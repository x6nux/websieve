//! NoiseStream：snow `TransportState` + 底层 TU 字节管道 → AsyncRead+AsyncWrite。
//!
//! 设计（Task 15 服务端协议栈接线）：构造时传入「握手后的 TransportState」与
//! 「搬运 TU 字节的底层 AsyncRead+AsyncWrite」。服务端形态：底层管道 =
//! tokio duplex；上行半由会话上行泵写入（POST body 即 TU 字节流），下行半由
//! 下行泵读出送 GET 响应流；保活任务经 [`PadHandle`] 在同一 nonce 序列上
//! 插入 PADDING TU。
//!
//! - 写侧：上层字节切成 ≤ MAX_PAYLOAD 的 Frame::Data，逐帧 encode_frame →
//!   `write_message` 加密 → `u16 BE 长度 + 密文` 进 cipher_buffer；
//! - 读侧：底层管道字节 → `TuDecoder` 剥完整 TU → `read_message` 解密 →
//!   `decode_frame`，Frame::Data 载荷按序进读缓冲，Frame::Padding 丢弃。
//!   底层 EOF（对端写半关闭）如实上抛为流 EOF。
//!
//! 锁分三块：`state`/`rd` 为 std Mutex（临界区纯 CPU、不跨 await）；`io` 为
//! tokio Mutex（cipher_buffer 的写出可能 await）。snow 收发 cipherstate 独立
//! 计数，读写可并发。客户端 XhttpConn 的 TU 编解码结构上内联（HTTP 请求/
//! 响应形态不同），本类型供服务端全栈使用。

use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll};

use snow::TransportState;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Mutex;

use crate::tu::{decode_frame, encode_frame, Frame, TuDecoder, MAX_PAYLOAD};

/// 读缓冲堆积上限（上层不读时的背压阈值）：2 个满帧。
const READ_BUF_CAP: usize = MAX_PAYLOAD * 2;

/// 底层管道 trait（AsyncRead + AsyncWrite + Unpin）。
trait AsyncReadWrite: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> AsyncReadWrite for T {}

struct Inner {
    io: Pin<Box<dyn AsyncReadWrite>>,
    decoder: TuDecoder,
    /// 已加密、尚未写到 io 的 TU 字节（PadHandle 写 PADDING 也暂存于此）
    cipher_buffer: Vec<u8>,
}

struct ReadSide {
    /// 已解密、尚未被上层读走的字节
    buffer: Vec<u8>,
    /// 解密失败（认证不过 = 流被篡改/错位）→ 置位，后续读写全部报错
    broken: bool,
}

struct WriteSide {
    /// 上层写入、尚未加密的字节
    pending: Vec<u8>,
}

pub struct NoiseStream {
    state: Arc<StdMutex<TransportState>>,
    io: Arc<Mutex<Inner>>,
    rd: Arc<StdMutex<ReadSide>>,
    wr: Arc<StdMutex<WriteSide>>,
}

impl NoiseStream {
    pub fn new(
        state: TransportState,
        io: impl AsyncRead + AsyncWrite + Send + Unpin + 'static,
    ) -> Self {
        Self {
            state: Arc::new(StdMutex::new(state)),
            io: Arc::new(Mutex::new(Inner {
                io: Box::pin(io),
                decoder: TuDecoder::new(),
                cipher_buffer: Vec::new(),
            })),
            rd: Arc::new(StdMutex::new(ReadSide {
                buffer: Vec::new(),
                broken: false,
            })),
            wr: Arc::new(StdMutex::new(WriteSide {
                pending: Vec::new(),
            })),
        }
    }

    /// 保活句柄：流被 mux 持有的同时，向同一 nonce 序列插入 PADDING TU。
    /// 加密（state 锁）与 TU 落盘（io 锁）各自成段，与数据 TU 的相对顺序 =
    /// state 锁获取顺序 = 线上顺序，nonce 序不乱。
    pub fn pad_handle(&self) -> PadHandle {
        PadHandle {
            state: self.state.clone(),
            io: self.io.clone(),
        }
    }

    fn broken_err() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "noise stream broken")
    }

    /// 加密 wr.pending 的全部字节 → 追加到 io.cipher_buffer（纯 CPU，不 await）。
    fn encrypt_pending(&self) -> std::io::Result<()> {
        let mut wr = self.wr.lock().unwrap();
        if wr.pending.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock().unwrap();
        let mut io = match self.io.try_lock() {
            Ok(g) => g,
            Err(_) => return Ok(()), // io 被占：数据留在 pending，由 flush 驱动
        };
        while !wr.pending.is_empty() {
            let take = wr.pending.len().min(MAX_PAYLOAD);
            let chunk: Vec<u8> = wr.pending.drain(..take).collect();
            let plain = encode_frame(&Frame::Data(chunk), &mut rand::rng())
                .map_err(std::io::Error::other)?;
            let mut cipher_buf = vec![0u8; 65535];
            let cipher_len = state
                .write_message(&plain, &mut cipher_buf)
                .map_err(std::io::Error::other)?;
            io.cipher_buffer
                .extend_from_slice(&(cipher_len as u16).to_be_bytes());
            io.cipher_buffer.extend_from_slice(&cipher_buf[..cipher_len]);
        }
        Ok(())
    }
}

/// 保活句柄（见 [`NoiseStream::pad_handle`]）。
pub struct PadHandle {
    state: Arc<StdMutex<TransportState>>,
    io: Arc<Mutex<Inner>>,
}

impl PadHandle {
    /// 加密一个 PADDING TU 并写到底层管道（阻塞直至写完；含冲刷已缓冲 TU）。
    pub async fn write_padding(&self) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt as _;
        let plain =
            encode_frame(&Frame::Padding, &mut rand::rng()).map_err(std::io::Error::other)?;
        let tu = {
            let mut state = self.state.lock().unwrap();
            let mut cipher_buf = vec![0u8; 65535];
            let cipher_len = state
                .write_message(&plain, &mut cipher_buf)
                .map_err(std::io::Error::other)?;
            let mut tu = Vec::with_capacity(2 + cipher_len);
            tu.extend_from_slice(&(cipher_len as u16).to_be_bytes());
            tu.extend_from_slice(&cipher_buf[..cipher_len]);
            tu
        };
        let mut io = self.io.lock().await;
        io.cipher_buffer.extend_from_slice(&tu);
        while !io.cipher_buffer.is_empty() {
            let chunk = io.cipher_buffer.clone();
            io.io.as_mut().write_all(&chunk).await?;
            let n = chunk.len();
            io.cipher_buffer.drain(..n);
        }
        Ok(())
    }
}

impl AsyncRead for NoiseStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        // 快路径：读缓冲有货 → 直接交付
        {
            let mut rd = this.rd.lock().unwrap();
            if rd.broken {
                return Poll::Ready(Err(Self::broken_err()));
            }
            if !rd.buffer.is_empty() {
                let n = rd.buffer.len().min(buf.remaining());
                let data: Vec<u8> = rd.buffer.drain(..n).collect();
                buf.put_slice(&data);
                return Poll::Ready(Ok(()));
            }
        }

        // 从底层 io 拉一批密文（tokio 锁 try_lock，忙则让出）
        let cipher = {
            let mut io = match this.io.try_lock() {
                Ok(g) => g,
                Err(_) => {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
            };
            let mut tmp = [0u8; 16384];
            let mut rb = ReadBuf::new(&mut tmp);
            match io.io.as_mut().poll_read(cx, &mut rb) {
                Poll::Ready(Ok(())) => rb.filled().to_vec(),
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        };
        if cipher.is_empty() {
            // 底层 EOF：对端写半已关闭
            return Poll::Ready(Ok(()));
        }

        // 解密所有完整 TU → 本地
        let mut state = this.state.lock().unwrap();
        let mut io = this.io.try_lock().unwrap();
        let tus = io.decoder_push(&cipher);
        drop(io);
        let mut decrypted: Vec<u8> = Vec::new();
        for tu in tus {
            if tu.len() < 2 {
                continue;
            }
            let mut plain = vec![0u8; 65535];
            let n = match state.read_message(&tu[2..], &mut plain) {
                Ok(n) => n,
                Err(_) => {
                    this.rd.lock().unwrap().broken = true;
                    return Poll::Ready(Err(Self::broken_err()));
                }
            };
            match decode_frame(&plain[..n]) {
                Ok(Frame::Data(d)) => decrypted.extend_from_slice(&d),
                Ok(Frame::Padding) => {}
                Err(_) => {
                    this.rd.lock().unwrap().broken = true;
                    return Poll::Ready(Err(Self::broken_err()));
                }
            }
        }
        drop(state);

        let mut rd = this.rd.lock().unwrap();
        rd.buffer.extend_from_slice(&decrypted);
        if !rd.buffer.is_empty() {
            let n = rd.buffer.len().min(buf.remaining());
            let data: Vec<u8> = rd.buffer.drain(..n).collect();
            buf.put_slice(&data);
            Poll::Ready(Ok(()))
        } else {
            // 本批只有 PADDING / 不完整 TU：继续等下一批
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

impl Inner {
    fn decoder_push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.decoder.push(chunk)
    }
}

impl AsyncWrite for NoiseStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        {
            let rd = this.rd.lock().unwrap();
            if rd.broken {
                return Poll::Ready(Err(Self::broken_err()));
            }
            // 背压：读缓冲堆积（上层不读）时不再接受写入，防内存无界
            if rd.buffer.len() > READ_BUF_CAP {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        }
        // 写入即接收：数据进 pending 后无论 flush 是否写完 io，都算本 buf 已
        // 消费——调用方不会重发。flush 未完成的部分留在 pending/cipher_buffer
        // 里由后续 poll_flush 逐步写出（pending 是持久缓冲，不是暂存）。
        this.wr.lock().unwrap().pending.extend_from_slice(buf);
        let _ = this.encrypt_pending();
        let _ = Pin::new(&mut *this).poll_flush(cx);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        this.encrypt_pending()?;
        // 写出 cipher_buffer：poll_fn 风格委托（Pending 时数据留在 buffer）
        let mut io = match this.io.try_lock() {
            Ok(g) => g,
            Err(_) => return Poll::Pending,
        };
        while !io.cipher_buffer.is_empty() {
            let chunk = io.cipher_buffer.clone();
            let n = match io.io.as_mut().poll_write(_cx, &chunk) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "underlying io wrote zero",
                    )));
                }
                Poll::Ready(Ok(n)) => n,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            io.cipher_buffer.drain(..n);
        }
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut *this).poll_flush(cx) {
            Poll::Ready(Ok(())) => {
                let mut io = match this.io.try_lock() {
                    Ok(g) => g,
                    Err(_) => return Poll::Pending,
                };
                io.io.as_mut().poll_shutdown(cx)
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn handshake_pair() -> (TransportState, TransportState) {
        let (s_priv, s_pub) = crate::crypto::gen_keypair();
        let (c_priv, _c_pub) = crate::crypto::gen_keypair();
        let mut cli = crate::crypto::build_client(&s_pub, &c_priv).unwrap();
        let mut srv = crate::crypto::build_server(&s_priv).unwrap();
        let hello = crate::hello::encode_msg1(0, 0, &[], crate::hello::IpStrategy::Auto);
        let mut b1 = vec![0u8; 65535];
        let n1 = cli.write_message(&hello, &mut b1).unwrap();
        let mut p1 = vec![0u8; 65535];
        srv.read_message(&b1[..n1], &mut p1).unwrap();
        let mut b2 = vec![0u8; 65535];
        let n2 = srv.write_message(b"ok", &mut b2).unwrap();
        let mut p2 = vec![0u8; 65535];
        cli.read_message(&b2[..n2], &mut p2).unwrap();
        (
            cli.into_transport_mode().unwrap(),
            srv.into_transport_mode().unwrap(),
        )
    }

    fn stream_pair() -> (NoiseStream, NoiseStream) {
        let (cs, ss) = handshake_pair();
        let (a, b) = tokio::io::duplex(8192);
        (NoiseStream::new(cs, a), NoiseStream::new(ss, b))
    }

    #[tokio::test]
    async fn roundtrip_small() {
        let (a, b) = stream_pair();
        let mut a = a;
        let mut b = b;
        a.write_all(b"hello websieve").await.unwrap();
        a.flush().await.unwrap();
        let mut buf = vec![0u8; 64];
        let n = b.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello websieve");
    }

    #[tokio::test]
    async fn roundtrip_large_multiframe() {
        let (a, b) = stream_pair();
        let mut a = a;
        let mut b = b;
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let total = payload.len();
        let expect = payload.clone();
        tokio::spawn(async move {
            a.write_all(&payload).await.unwrap();
            a.flush().await.unwrap();
        });
        let mut got = Vec::new();
        let mut buf = [0u8; 8192];
        while got.len() < total {
            let n = b.read(&mut buf).await.unwrap();
            assert!(n > 0, "premature EOF at {}", got.len());
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, expect);
    }

    #[tokio::test]
    async fn padding_transparent_and_bidirectional() {
        let (a, b) = stream_pair();
        let mut a = a;
        let mut b = b;
        b.write_all(b"data1").await.unwrap();
        b.flush().await.unwrap();
        // 经 PadHandle 注入 PADDING（与保活路径同型）：a 读侧应无感
        b.pad_handle().write_padding().await.unwrap();
        b.write_all(b"data2").await.unwrap();
        b.flush().await.unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 64];
        while got.len() < 10 {
            let n = a.read(&mut buf).await.unwrap();
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(&got, b"data1data2");
    }

    #[tokio::test]
    async fn underlying_eof_propagates() {
        let (cs, ss) = handshake_pair();
        let (a, b) = tokio::io::duplex(8192);
        let (_c_io, _s_io2) = tokio::io::duplex(8192);
        drop(NoiseStream::new(cs, _c_io)); // 客户端持有方（不发数据）
        let mut s = NoiseStream::new(ss, b);
        drop(a); // 对端整个关闭 → 底层 EOF
        let mut buf = [0u8; 8];
        let n = s.read(&mut buf).await.unwrap();
        assert_eq!(n, 0, "expected EOF");
    }

    #[tokio::test]
    async fn tamper_breaks_stream() {
        let (cs, ss) = handshake_pair();
        let garbage = [[0u8, 16].as_slice(), &[0xABu8; 16][..]].concat();
        let (a, b) = tokio::io::duplex(8192);
        let (_c_io, _s_io2) = tokio::io::duplex(8192);
        drop(NoiseStream::new(cs, _c_io));
        let mut s = NoiseStream::new(ss, b);
        let mut raw_a = a;
        raw_a.write_all(&garbage).await.unwrap();
        let mut buf = [0u8; 8];
        assert!(s.read(&mut buf).await.is_err());
        // broken 后写也失败
        assert!(s.write_all(b"x").await.is_err());
    }
}
