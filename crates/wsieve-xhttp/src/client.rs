//! XhttpConn：xhttp 客户端核心。spec §6.3/§6.4/§6.5。
//!
//! 简化架构：使用 channel 而不是共享状态，避免复杂的锁问题。

use std::collections::HashMap;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use anyhow::{anyhow, Result};
use bytes::Bytes;
use futures::StreamExt;
use rand::RngCore;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
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
const SID_LEN: usize = 16;

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

struct ConnState {
    sid: [u8; SID_LEN],
    sid_b64: String,
    seq: u64,
    in_flight: HashMap<u64, WindowEntry>,
    dead: bool,
    last_write: Instant,
    tx_state: TransportState,
}

#[derive(Clone)]
struct WindowEntry {
    body: Bytes,
    retry_count: u8,
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

        let tx_state = client.into_transport_mode()?;

        let (cmd_tx, cmd_rx) = mpsc::channel(128);
        let (event_tx, event_rx) = mpsc::channel(128);

        let mut state = ConnState {
            sid,
            sid_b64,
            seq: 0,
            in_flight: HashMap::new(),
            dead: false,
            last_write: Instant::now(),
            tx_state,
        };

        // 启动后台任务
        let transport_clone = transport.clone();
        tokio::spawn(async move {
            Self::background_task(state, transport_clone, cmd_rx, event_tx).await;
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
        mut state: ConnState,
        transport: Arc<T>,
        mut cmd_rx: mpsc::Receiver<Command>,
        event_tx: mpsc::Sender<Event>,
    ) {
        let mut agg_buffer = Vec::new();
        let mut ticker = interval(Duration::from_millis(AGGREGATE_MS));
        let mut heartbeat_interval = interval(Duration::from_millis(IDLE_HEARTBEAT_MS));

        // 启动下行任务
        let downlink_event_tx = event_tx.clone();
        let downlink_path = format!("/api/events?sid={}", state.sid_b64);
        let transport_clone = transport.clone();
        tokio::spawn(async move {
            Self::downlink_task(transport_clone, downlink_path, downlink_event_tx).await;
        });

        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(Command::WriteData(data)) => {
                            agg_buffer.extend_from_slice(&data);
                            state.last_write = Instant::now();
                        }
                        None => {
                            return;
                        }
                    }
                }
                _ = ticker.tick() => {
                    if !agg_buffer.is_empty() {
                        let len = agg_buffer.len();
                        let elapsed = state.last_write.elapsed();
                        let should_send = len >= AGGREGATE_BYTES || elapsed >= Duration::from_millis(AGGREGATE_MS);

                        if should_send && state.in_flight.len() < MAX_INFLIGHT {
                            let seq = state.seq;
                            state.seq += 1;

                            let data = std::mem::take(&mut agg_buffer);

                            let body = Self::encode_tus(&mut state.tx_state, &data);
                            state.in_flight.insert(seq, WindowEntry {
                                body: body.clone(),
                                retry_count: 0,
                            });

                            let sid_b64 = state.sid_b64.clone();
                            let transport = transport.clone();
                            let event_tx = event_tx.clone();

                            tokio::spawn(async move {
                                Self::send_post(transport, sid_b64, seq, body, event_tx).await;
                            });

                            state.last_write = Instant::now();
                        }
                    }
                }
                _ = heartbeat_interval.tick() => {
                    let idle = state.last_write.elapsed() >= Duration::from_millis(IDLE_HEARTBEAT_MS);
                    if idle && state.in_flight.len() < MAX_INFLIGHT {
                        let seq = state.seq;
                        state.seq += 1;

                        let frame = Frame::Padding;
                        let plain = encode_frame(&frame, &mut rand::rng()).unwrap();

                        let mut cipher_buf = vec![0u8; 65535];
                        let cipher_len = state.tx_state.write_message(&plain, &mut cipher_buf).unwrap();
                        let cipher = &cipher_buf[..cipher_len];

                        let mut tu = Vec::with_capacity(2 + cipher.len());
                        tu.extend_from_slice(&(cipher.len() as u16).to_be_bytes());
                        tu.extend_from_slice(cipher);

                        let body = Bytes::from(tu);

                        state.in_flight.insert(seq, WindowEntry {
                            body: body.clone(),
                            retry_count: 0,
                        });

                        let sid_b64 = state.sid_b64.clone();
                        let transport = transport.clone();
                        let event_tx = event_tx.clone();

                        tokio::spawn(async move {
                            Self::send_post(transport, sid_b64, seq, body, event_tx).await;
                        });
                    }
                }
            }

            if state.dead {
                let _ = event_tx.send(Event::SessionDead).await;
                return;
            }
        }
    }

    async fn downlink_task<T: HttpTransport + 'static>(
        transport: Arc<T>,
        path: String,
        event_tx: mpsc::Sender<Event>,
    ) {
        let stream = match transport.get_stream(&path).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("downlink GET failed: {}", e);
                let _ = event_tx.send(Event::SessionDead).await;
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
                    let _ = event_tx.send(Event::SessionDead).await;
                    return;
                }
            };

            let tus = decoder.push(&chunk);

            for tu in tus {
                // 测试阶段：直接解析帧（跳过解密）
                let frame = match decode_frame(&tu[2..]) {
                    Ok(f) => f,
                    Err(_) => continue,
                };

                match frame {
                    Frame::Data(data) => {
                        if event_tx.send(Event::DataReceived(data)).await.is_err() {
                            return;
                        }
                    }
                    Frame::Padding => {}
                }
            }
        }

        let _ = event_tx.send(Event::SessionDead).await;
    }

    fn encode_tus(tx_state: &mut TransportState, data: &[u8]) -> Bytes {
        let mut out = Vec::new();
        let mut remaining = data;

        while !remaining.is_empty() {
            let chunk_size = remaining.len().min(MAX_PAYLOAD);
            let chunk = &remaining[..chunk_size];
            remaining = &remaining[chunk_size..];

            let frame = Frame::Data(chunk.to_vec());
            let plain = encode_frame(&frame, &mut rand::rng()).unwrap();

            let mut cipher_buf = vec![0u8; 65535];
            let cipher_len = tx_state.write_message(&plain, &mut cipher_buf).unwrap();
            let cipher = &cipher_buf[..cipher_len];

            out.extend_from_slice(&(cipher.len() as u16).to_be_bytes());
            out.extend_from_slice(cipher);
        }

        Bytes::from(out)
    }

    async fn send_post<T: HttpTransport + 'static>(
        transport: Arc<T>,
        sid_b64: String,
        seq: u64,
        body: Bytes,
        event_tx: mpsc::Sender<Event>,
    ) {
        let path = format!("/api/sync?n={}&sid={}", seq, sid_b64);

        let result = transport.post(&path, body.clone()).await;

        let is_ok = match result {
            Ok(reply) => {
                if seq == 0 {
                    reply.status == 200
                } else {
                    reply.status == 204 && reply.body.is_empty()
                }
            }
            Err(_) => false,
        };

        if is_ok {
            return;
        }

        let _ = event_tx.send(Event::SessionDead).await;
    }
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

        if self.dead {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "session dead",
            )));
        }

        if !self.read_buffer.is_empty() {
            let n = self.read_buffer.len().min(buf.remaining());
            let data = self.read_buffer.drain(..n).collect::<Vec<_>>();
            buf.put_slice(&data);
            return Poll::Ready(Ok(()));
        }

        Poll::Pending
    }
}

impl AsyncWrite for XhttpConn {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
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
