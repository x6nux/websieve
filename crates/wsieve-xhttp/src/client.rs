//! XhttpConn：xhttp 客户端核心。spec §6.3/§6.4/§6.5。
//!
//! 架构：共享状态（tokio Mutex，持有握手后的 Noise `TransportState`）+
//! 后台聚合/心跳循环 + 每个 POST 一个发送任务（含重试策略）+ 下行解密任务，
//! 经 mpsc 事件通道向 `XhttpConn` 上抛数据与会话死亡。
//!
//! snow 0.10 的 `TransportState` 在握手完成后同时持有发送/接收两个
//! cipherstate（nonce 独立计数），`write_message` / `read_message` 分别走
//! 各自一侧——上行加密与下行解密共用这一份状态，由 Mutex 串行化。

use std::collections::HashMap;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{anyhow, Result};
use bytes::Bytes;
use futures::StreamExt;
use rand::RngCore;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, Mutex};
use tokio::time::{Instant, interval};

use wsieve_transport::HttpTransport;
use wsieve_proto::crypto::build_client;
use wsieve_proto::hello::{decode_msg2, encode_msg1, MuxId};
use wsieve_proto::tu::{decode_frame, encode_frame, Frame, TuDecoder, MAX_PAYLOAD};
use snow::TransportState;

const MAX_INFLIGHT: usize = 8;
const AGGREGATE_MS: u64 = 4;
const AGGREGATE_BYTES: usize = 64_000;
const IDLE_HEARTBEAT_MS: u64 = 60_000;
/// 空闲后首包立即发：距上次 flush 超过该阈值且缓冲非空 → 不等 4ms tick。
const IDLE_FLUSH_THRESHOLD: Duration = Duration::from_millis(50);
/// 单 POST body 上限 1 MB（spec §6.4）。单个 TU 密文 ≤ 65537 字节，
/// 15 个 TU（≤ 983 055 B）必然落在 1 MB 内。
const MAX_TUS_PER_POST: usize = 15;
const SID_LEN: usize = 16;

const RETRY_MAX: u8 = 2;
const RETRY_BACKOFF_INITIAL: Duration = Duration::from_millis(100);

pub struct UpstreamCfg {
    pub server_pub: [u8; 32],
    pub client_priv: [u8; 32],
    pub mux_prefs: Vec<MuxId>,
}

#[derive(Debug, Clone, Copy)]
pub struct Negotiated {
    pub mux_id: MuxId,
    pub fallback: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("session dead")]
pub struct SessionDead;

/// 后台任务命令
enum Command {
    WriteData(Vec<u8>),
}

/// 后台任务事件
enum Event {
    DataReceived(Vec<u8>),
    SessionDead,
}

pub struct XhttpConn {
    /// 写入命令发送器
    cmd_tx: mpsc::Sender<Command>,
    /// 读取数据接收器
    event_rx: mpsc::Receiver<Event>,
    /// 读取缓冲
    read_buffer: Vec<u8>,
    /// 会话是否死亡
    dead: bool,
}

/// 发送任务、聚合循环与下行任务共享的会话状态。
struct SharedState {
    sid_b64: String,
    /// 下一个上行 seq（握手用 0，数据/心跳从 1 起）
    next_seq: u64,
    /// 在途窗口：seq -> 完整 POST 字节。重试原样重发同一字节，
    /// 不重新加密（nonce 序 = TU 加密顺序 = seq 顺序，spec §6.4）。
    in_flight: HashMap<u64, Bytes>,
    dead: bool,
    last_write: Instant,
    last_flush: Instant,
    /// 握手后的 Noise 状态：write_message = 上行加密，read_message = 下行解密
    noise: TransportState,
}

impl XhttpConn {
    pub async fn connect<T: HttpTransport + 'static>(
        transport: Arc<T>,
        cfg: &UpstreamCfg,
    ) -> Result<(Self, Negotiated)> {
        let mut sid = [0u8; SID_LEN];
        rand::rng().fill_bytes(&mut sid);
        let sid_b64 = base64_url::encode(&sid);

        let mut client = build_client(&cfg.server_pub, &cfg.client_priv)?;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;

        let hello = encode_msg1(now_ms, &cfg.mux_prefs);

        let mut msg1_buf = vec![0u8; 65535];
        let msg1_len = client.write_message(&hello, &mut msg1_buf)?;
        let msg1_cipher = &msg1_buf[..msg1_len];

        let mut msg1_tu = Vec::with_capacity(2 + msg1_cipher.len());
        msg1_tu.extend_from_slice(&(msg1_cipher.len() as u16).to_be_bytes());
        msg1_tu.extend_from_slice(msg1_cipher);

        let path = format!("/api/sync?n=0&sid={}", sid_b64);
        let reply = transport.post(&path, Bytes::from(msg1_tu)).await?;

        if reply.status != 200 {
            return Err(anyhow!("handshake failed: status {}", reply.status));
        }

        if reply.body.len() < 2 {
            return Err(anyhow!("msg2 body too short"));
        }
        let msg2_len = u16::from_be_bytes([reply.body[0], reply.body[1]]) as usize;
        if reply.body.len() < 2 + msg2_len {
            return Err(anyhow!("msg2 truncated"));
        }
        let msg2_cipher = &reply.body[2..2 + msg2_len];

        let mut msg2_buf = vec![0u8; 65535];
        let msg2_plain_len = client.read_message(msg2_cipher, &mut msg2_buf)?;
        let msg2_plain = &msg2_buf[..msg2_plain_len];
        let msg2 = decode_msg2(msg2_plain)?;

        let shared = Arc::new(Mutex::new(SharedState {
            sid_b64,
            next_seq: 1,
            in_flight: HashMap::new(),
            dead: false,
            last_write: Instant::now(),
            last_flush: Instant::now(),
            noise: client.into_transport_mode()?,
        }));

        let (cmd_tx, cmd_rx) = mpsc::channel(128);
        let (event_tx, event_rx) = mpsc::channel(128);

        // 启动后台任务
        let transport_clone = transport.clone();
        tokio::spawn(async move {
            Self::background_task(shared, transport_clone, cmd_rx, event_tx).await;
        });

        Ok((Self {
            cmd_tx,
            event_rx,
            read_buffer: Vec::new(),
            dead: false,
        }, Negotiated {
            mux_id: msg2.chosen_mux_id,
            fallback: msg2.fallback,
        }))
    }

    async fn background_task<T: HttpTransport + 'static>(
        shared: Arc<Mutex<SharedState>>,
        transport: Arc<T>,
        mut cmd_rx: mpsc::Receiver<Command>,
        event_tx: mpsc::Sender<Event>,
    ) {
        let mut agg_buffer = Vec::new();
        let mut ticker = interval(Duration::from_millis(AGGREGATE_MS));
        let mut heartbeat_interval = interval(Duration::from_millis(IDLE_HEARTBEAT_MS));

        // 启动下行任务
        let downlink_path = format!("/api/events?sid={}", shared.lock().await.sid_b64);
        let transport_clone = transport.clone();
        let shared_clone = shared.clone();
        let downlink_event_tx = event_tx.clone();
        tokio::spawn(async move {
            Self::downlink_task(transport_clone, downlink_path, shared_clone, downlink_event_tx).await;
        });

        loop {
            if shared.lock().await.dead {
                let _ = event_tx.try_send(Event::SessionDead);
                return;
            }

            tokio::select! {
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(Command::WriteData(data)) => {
                            let mut st = shared.lock().await;
                            st.last_write = Instant::now();
                            agg_buffer.extend_from_slice(&data);
                            // 空闲后首包立即发：距上次 flush 超阈值 → 不等 4ms tick
                            if st.last_flush.elapsed() >= IDLE_FLUSH_THRESHOLD {
                                Self::flush(&shared, &mut st, &mut agg_buffer, &transport, &event_tx);
                            }
                        }
                        None => {
                            return;
                        }
                    }
                }
                _ = ticker.tick() => {
                    let mut st = shared.lock().await;
                    let elapsed = st.last_write.elapsed();
                    let should_send =
                        agg_buffer.len() >= AGGREGATE_BYTES || elapsed >= Duration::from_millis(AGGREGATE_MS);
                    if should_send {
                        Self::flush(&shared, &mut st, &mut agg_buffer, &transport, &event_tx);
                    }
                }
                _ = heartbeat_interval.tick() => {
                    let mut st = shared.lock().await;
                    let idle = st.last_write.elapsed() >= Duration::from_millis(IDLE_HEARTBEAT_MS);
                    if idle {
                        // 心跳 PADDING TU：与数据完全相同的窗口/seq 路径，无旁路（spec §6.5）
                        let tu = Self::encode_single_tu(&mut st, &Frame::Padding);
                        let seq = st.next_seq;
                        st.next_seq += 1;
                        st.in_flight.insert(seq, tu.clone());
                        let sid_b64 = st.sid_b64.clone();
                        drop(st);
                        Self::spawn_send(transport.clone(), shared.clone(), sid_b64, seq, tu, event_tx.clone());
                    }
                }
            }
        }
    }

    /// 把聚合缓冲编码为一个 POST 的 TU 串并发送（窗口允许时）。
    /// 单 POST ≤ MAX_TUS_PER_POST 个 TU（≤ 1 MB），剩余留给下次 flush。
    fn flush<T: HttpTransport + 'static>(
        shared: &Arc<Mutex<SharedState>>,
        st: &mut SharedState,
        agg_buffer: &mut Vec<u8>,
        transport: &Arc<T>,
        event_tx: &mpsc::Sender<Event>,
    ) {
        if agg_buffer.is_empty() || st.dead || st.in_flight.len() >= MAX_INFLIGHT {
            return;
        }
        let cap = MAX_TUS_PER_POST * MAX_PAYLOAD;
        let take = agg_buffer.len().min(cap);
        let data: Vec<u8> = agg_buffer.drain(..take).collect();

        let body = Self::encode_tus(&mut st.noise, &data);
        let seq = st.next_seq;
        st.next_seq += 1;
        st.in_flight.insert(seq, body.clone());
        st.last_flush = Instant::now();
        let sid_b64 = st.sid_b64.clone();

        Self::spawn_send(transport.clone(), shared.clone(), sid_b64, seq, body, event_tx.clone());
    }

    fn spawn_send<T: HttpTransport + 'static>(
        transport: Arc<T>,
        shared: Arc<Mutex<SharedState>>,
        sid_b64: String,
        seq: u64,
        body: Bytes,
        event_tx: mpsc::Sender<Event>,
    ) {
        tokio::spawn(async move {
            if send_post(transport, sid_b64, seq, body, shared).await == PostOutcome::Fatal {
                let _ = event_tx.try_send(Event::SessionDead);
            }
        });
    }

    async fn downlink_task<T: HttpTransport + 'static>(
        transport: Arc<T>,
        path: String,
        shared: Arc<Mutex<SharedState>>,
        event_tx: mpsc::Sender<Event>,
    ) {
        let stream = match transport.get_stream(&path).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("downlink GET failed: {}", e);
                kill_session(&shared).await;
                let _ = event_tx.try_send(Event::SessionDead);
                return;
            }
        };

        let mut decoder = TuDecoder::new();
        let mut stream = stream;

        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("downlink stream error: {}", e);
                    break;
                }
            };

            // TuDecoder 返回的每个元素是完整 TU（含 2 字节长度前缀）：
            // 先剥前缀取密文体，再走 Noise transport-state read_message 解密
            //（spec §6.1，生产路径，禁止跳过解密直接解帧）。
            for tu in decoder.push(&chunk) {
                if tu.len() < 2 {
                    continue;
                }
                let tu_cipher = &tu[2..];
                let frame = {
                    let mut st = shared.lock().await;
                    if st.dead {
                        return;
                    }
                    let mut plain_buf = vec![0u8; 65535];
                    let n = match st.noise.read_message(tu_cipher, &mut plain_buf) {
                        Ok(n) => n,
                        Err(_) => {
                            eprintln!("downlink TU decrypt failed");
                            drop(st);
                            kill_session(&shared).await;
                            let _ = event_tx.try_send(Event::SessionDead);
                            return;
                        }
                    };
                    match decode_frame(&plain_buf[..n]) {
                        Ok(f) => f,
                        Err(_) => {
                            eprintln!("downlink frame decode failed");
                            drop(st);
                            kill_session(&shared).await;
                            let _ = event_tx.try_send(Event::SessionDead);
                            return;
                        }
                    }
                };

                match frame {
                    Frame::Data(data) => {
                        if event_tx.try_send(Event::DataReceived(data)).is_err() {
                            return;
                        }
                    }
                    Frame::Padding => {}
                }
            }
        }

        // 流结束/错误 → 断会话（spec §9.1）
        kill_session(&shared).await;
        let _ = event_tx.try_send(Event::SessionDead);
    }

    fn encode_single_tu(st: &mut SharedState, frame: &Frame) -> Bytes {
        let plain = encode_frame(frame, &mut rand::rng()).expect("padding frame encodes");
        let mut cipher_buf = vec![0u8; 65535];
        let cipher_len = st.noise.write_message(&plain, &mut cipher_buf)
            .expect("transport encrypt");
        let mut tu = Vec::with_capacity(2 + cipher_len);
        tu.extend_from_slice(&(cipher_len as u16).to_be_bytes());
        tu.extend_from_slice(&cipher_buf[..cipher_len]);
        Bytes::from(tu)
    }

    fn encode_tus(noise: &mut TransportState, data: &[u8]) -> Bytes {
        let mut out = Vec::new();
        let mut remaining = data;

        while !remaining.is_empty() {
            let chunk_size = remaining.len().min(MAX_PAYLOAD);
            let chunk = &remaining[..chunk_size];
            remaining = &remaining[chunk_size..];

            let frame = Frame::Data(chunk.to_vec());
            let plain = encode_frame(&frame, &mut rand::rng()).unwrap();

            let mut cipher_buf = vec![0u8; 65535];
            let cipher_len = noise.write_message(&plain, &mut cipher_buf).unwrap();
            let cipher = &cipher_buf[..cipher_len];

            out.extend_from_slice(&(cipher_len as u16).to_be_bytes());
            out.extend_from_slice(cipher);
        }

        Bytes::from(out)
    }
}

/// send_post 的结局
#[derive(Debug, PartialEq, Eq)]
enum PostOutcome {
    /// 约定的成功响应（n≥1: 204+空body；n=0: 200+合法 msg2）→ 已释放窗口槽
    Delivered,
    /// 会话已死亡：传输层错误/超时/5xx 重试 ≤2 次（指数退避）仍失败，
    /// 或收到非约定响应（立即，不重试）→ 调用方据此上抛 SessionDead
    Fatal,
}

/// 发送单个 POST（含完整重试策略，spec §6.4）。窗口槽持有完整 POST 字节，
/// 重试原样重发同一 seq 同一字节——绝不重新加密，nonce 序不乱。
async fn send_post<T: HttpTransport + 'static>(
    transport: Arc<T>,
    sid_b64: String,
    seq: u64,
    body: Bytes,
    shared: Arc<Mutex<SharedState>>,
) -> PostOutcome {
    let path = format!("/api/sync?n={}&sid={}", seq, sid_b64);

    let mut attempt: u8 = 0;
    let mut backoff = RETRY_BACKOFF_INITIAL;

    loop {
        let retryable = match transport.post(&path, body.clone()).await {
            // 传输层错误/超时 → 不知是否送达 → 重试
            Err(_) => true,
            Ok(reply) => {
                if seq == 0 {
                    // n=0 约定响应：200 + 合法 msg2（msg2 合法性在 connect 里验证）
                    if reply.status == 200 {
                        let mut st = shared.lock().await;
                        st.in_flight.remove(&seq);
                        return PostOutcome::Delivered;
                    }
                    return fatal(&shared).await;
                }
                if reply.status == 204 && reply.body.is_empty() {
                    // 送达（或被去重，等价）→ 释放窗口槽
                    let mut st = shared.lock().await;
                    st.in_flight.remove(&seq);
                    return PostOutcome::Delivered;
                }
                // 5xx → 不知是否送达 → 重试；其余一切 → 会话失效，不重试
                reply.status >= 500
            }
        };

        if retryable && attempt < RETRY_MAX {
            attempt += 1;
            tokio::time::sleep(backoff).await;
            backoff *= 4; // 100ms → 400ms
            continue;
        }

        // 重试耗尽或非约定响应 → 断会话
        return fatal(&shared).await;
    }
}

async fn fatal(shared: &Arc<Mutex<SharedState>>) -> PostOutcome {
    let mut st = shared.lock().await;
    st.dead = true;
    st.in_flight.clear();
    PostOutcome::Fatal
}

async fn kill_session(shared: &Arc<Mutex<SharedState>>) {
    let mut st = shared.lock().await;
    st.dead = true;
    st.in_flight.clear();
}

impl AsyncRead for XhttpConn {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // 先检查事件通道
        loop {
            match std::pin::Pin::new(&mut self.event_rx).poll_recv(cx) {
                Poll::Ready(Some(Event::DataReceived(data))) => {
                    self.read_buffer.extend_from_slice(&data);
                }
                Poll::Ready(Some(Event::SessionDead)) => {
                    self.dead = true;
                }
                Poll::Ready(None) => {
                    self.dead = true;
                }
                Poll::Pending => break,
            }
        }

        // 先吐缓冲里的数据再报死亡（会话死亡前到达的数据仍可读）
        if !self.read_buffer.is_empty() {
            let n = self.read_buffer.len().min(buf.remaining());
            let data = self.read_buffer.drain(..n).collect::<Vec<_>>();
            buf.put_slice(&data);
            return Poll::Ready(Ok(()));
        }

        if self.dead {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "session dead",
            )));
        }

        Poll::Pending
    }
}

impl AsyncWrite for XhttpConn {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.dead {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "session dead",
            )));
        }

        let data = buf.to_vec();

        // 直接尝试发送
        match self.cmd_tx.try_send(Command::WriteData(data)) {
            Ok(_) => Poll::Ready(Ok(buf.len())),
            Err(mpsc::error::TrySendError::Full(_)) => Poll::Pending,
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "channel closed",
                )))
            }
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.dead = true;
        Poll::Ready(Ok(()))
    }
}

mod base64_url {
    pub fn encode(data: &[u8]) -> String {
        use base64::prelude::*;
        BASE64_URL_SAFE_NO_PAD.encode(data)
    }
}
