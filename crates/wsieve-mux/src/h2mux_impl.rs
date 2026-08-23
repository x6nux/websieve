//! h2mux 适配（spec §7.3）：把 HTTP/2 双向流当作通用 mux 子流。
//! 每条子流是一条合成的 `POST / :authority wsieve` 请求：
//! 客户端写半 = 请求 SendStream，读半 = 响应 RecvStream；服务端反之。
//! 所有流完全相同，头部仅为满足 h2 语义，不含信息。

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::Request;
use tokio::sync::{mpsc, Mutex};

use crate::{Mux, MuxStream};

/// 客户端半：持有 h2 连接驱动 + 请求发送句柄。
pub struct H2ClientImpl {
    send_request: tokio::sync::Mutex<h2::client::SendRequest<Bytes>>,
    _driver: tokio::task::JoinHandle<()>,
}

impl H2ClientImpl {
    pub async fn new(io: MuxStream) -> anyhow::Result<Self> {
        let (send_request, connection) = h2::client::handshake(io).await?;
        let driver = tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(Self {
            send_request: tokio::sync::Mutex::new(send_request),
            _driver: driver,
        })
    }
}

#[async_trait::async_trait]
impl Mux for H2ClientImpl {
    async fn open(&self) -> anyhow::Result<MuxStream> {
        // 等待连接有可用流槽（避免 pending open 时半途卡住）。
        // 服务端 accept 时立即回 200 头，response 不会久等。
        let (response_fut, send) = {
            let mut sr = self.send_request.lock().await;
            sr.clone().ready().await?;
            let req = Request::builder()
                .method(http::Method::POST)
                .uri("http://wsieve/")
                .body(())?;
            sr.send_request(req, false)?
        };
        let response: http::Response<h2::RecvStream> = response_fut.await?;
        let recv = response.into_body();
        Ok(Box::new(H2Stream::new(send, recv)))
    }

    async fn accept(&self) -> anyhow::Result<MuxStream> {
        anyhow::bail!("h2mux client half does not accept streams")
    }
}

/// 服务端半：后台任务逐条 accept 请求并立即回 200，经 mpsc 交付。
pub struct H2ServerImpl {
    incoming: Mutex<mpsc::Receiver<MuxStream>>,
    _driver: tokio::task::JoinHandle<()>,
}

impl H2ServerImpl {
    pub async fn new(io: MuxStream) -> anyhow::Result<Self> {
        let mut conn = h2::server::handshake(io).await?;
        let (tx, rx) = mpsc::channel(32);
        let driver = tokio::spawn(async move {
            while let Some(Ok((req, mut respond))) = conn.accept().await {
                // 校验合成头（防御：非本协议流量回 400 继续跑）
                let ok = req.method() == http::Method::POST
                    && req.uri().path() == "/"
                    && req.uri().authority().map(|a| a.as_str()) == Some("wsieve");
                if !ok {
                    let resp = http::Response::builder().status(400).body(()).unwrap();
                    let _ = respond.send_response(resp, true);
                    continue;
                }
                let recv = req.into_body();
                let resp = http::Response::builder().status(200).body(()).unwrap();
                match respond.send_response(resp, false) {
                    Ok(send) => {
                        let stream: MuxStream = Box::new(H2Stream::new(send, recv));
                        if tx.send(stream).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => continue,
                }
            }
        });
        Ok(Self {
            incoming: Mutex::new(rx),
            _driver: driver,
        })
    }
}

#[async_trait::async_trait]
impl Mux for H2ServerImpl {
    async fn open(&self) -> anyhow::Result<MuxStream> {
        anyhow::bail!("h2mux server half does not open streams")
    }

    async fn accept(&self) -> anyhow::Result<MuxStream> {
        // mpsc 被 Drop（连接驱动退出）→ None；当作连接终结
        self.incoming
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("h2mux connection closed"))
    }
}

/// 双向子流适配器：h2 的 Bytes-chunk API → AsyncRead/AsyncWrite。
///
/// - 写：按当前 send capacity 分片发送；无容量则 reserve + poll_capacity 挂起
///   （h2 流控标准模式，缓冲无界我们不做）
/// - 读：poll_data 取 Bytes 块进本地缓冲，按需拷出并 release_capacity
/// - 关闭：空 DATA + end_of_stream
struct H2Stream {
    send: Option<h2::SendStream<Bytes>>,
    recv: Option<h2::RecvStream>,
    /// poll_data 取到、尚未被上层读走的字节
    buffer: Bytes,
    /// END_STREAM 已发送（h2 无"写完"通知，Drop-race 由 send_data 语义规避不了，
    /// 见下方 poll_shutdown 注释）
    shutdown_sent: bool,
}

impl H2Stream {
    fn new(send: h2::SendStream<Bytes>, recv: h2::RecvStream) -> Self {
        Self {
            send: Some(send),
            recv: Some(recv),
            buffer: Bytes::new(),
            shutdown_sent: false,
        }
    }
}

impl tokio::io::AsyncWrite for H2Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let mut capacity = self.send.as_ref().expect("send half").capacity();
        if capacity == 0 {
            self.send.as_mut().expect("send half").reserve_capacity(buf.len());
            capacity = match self.send.as_mut().expect("send half").poll_capacity(cx) {
                Poll::Ready(Some(Ok(n))) => n,
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Err(std::io::Error::other(e)))
                }
                Poll::Ready(None) => {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "h2 stream closed",
                    )))
                }
                Poll::Pending => return Poll::Pending,
            };
        }
        let n = buf.len().min(capacity);
        let chunk = Bytes::copy_from_slice(&buf[..n]);
        self.send
            .as_mut()
            .expect("send half")
            .send_data(chunk, false)
            .map_err(std::io::Error::other)?;
        Poll::Ready(Ok(n))
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(())) // h2 内部自动刷新
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        if !self.shutdown_sent {
            self.send
                .as_mut()
                .expect("send half")
                .send_data(Bytes::new(), true)
                .map_err(std::io::Error::other)?;
            self.shutdown_sent = true;
        }
        Poll::Ready(Ok(()))
    }
}

impl Drop for H2Stream {
    fn drop(&mut self) {
        let Some(mut send) = self.send.take() else { return };
        let recv = self.recv.take();
        if !self.shutdown_sent {
            // 未正常关闭就 Drop：显式取消，释放对端资源
            send.send_reset(h2::Reason::CANCEL);
            return;
        }
        // 关键：END_STREAM 排队后立即 Drop，h2 会把 SendStream 的 Drop 视为
        // "发送端失去兴趣"，对尚未写出的数据帧 RST_STREAM(CANCEL) 清队
        // （h2 streams.rs maybe_cancel）。把两个半流移交 linger 任务持有：
        // 对端 RST/关流时 poll_reset 唤醒、正常释放；对端不响应则 30s 超时
        // 兜底。期间连接驱动持续写出排队帧，数据不丢。
        tokio::spawn(async move {
            let _ = recv; // 读半一并持有：维持流引用计数
            let linger = futures::future::poll_fn(move |cx| send.poll_reset(cx));
            let _ = tokio::time::timeout(std::time::Duration::from_secs(30), linger).await;
        });
    }
}

impl tokio::io::AsyncRead for H2Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.buffer.is_empty() {
            match self.recv.as_mut().expect("recv half").poll_data(cx) {
                Poll::Ready(Some(Ok(chunk))) => self.buffer = chunk,
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Err(std::io::Error::other(e)))
                }
                Poll::Ready(None) => return Poll::Ready(Ok(())), // EOF
                Poll::Pending => return Poll::Pending,
            }
        }
        if self.buffer.is_empty() {
            // 拿到空 DATA 块：继续等下一块
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        let n = self.buffer.len().min(buf.remaining());
        buf.put_slice(&self.buffer[..n]);
        let consumed = self.buffer.split_to(n).len();
        self.recv
            .as_mut()
            .expect("recv half")
            .flow_control()
            .release_capacity(consumed)
            .map_err(std::io::Error::other)?;
        Poll::Ready(Ok(()))
    }
}

