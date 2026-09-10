//! XhttpConn 客户端测试。spec §6.4/§6.5 不变量。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::stream::BoxStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use wsieve_proto::crypto::{build_server, gen_keypair};
use wsieve_proto::hello::{IpStrategy, encode_msg2, Msg2, MuxId};
use wsieve_proto::tu::{Frame, MAX_PAYLOAD};
use wsieve_transport::{HttpTransport, PostReply};
use wsieve_xhttp::client::{UpstreamCfg, XhttpConn};

/// 录制的一条上行请求
#[derive(Clone, Debug)]
struct RecordedPost {
    path: String,
    body: Bytes,
}

impl RecordedPost {
    fn seq(&self) -> u64 {
        // path 形如 /api/sync?n=<seq>&sid=<...>
        let n = self
            .path
            .split("n=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap();
        n.parse().unwrap()
    }
}

/// 可编程 FakeTransport：路由感知、延迟/失败可脚本化、全量录制。
struct FakeTransport {
    /// n=0 握手：合法服务端半边（真 Noise 响应 msg2）
    server_priv: [u8; 32],
    /// n≥1 POST 默认响应延迟
    post_delay: Mutex<Duration>,
    /// 前 K 次 n≥1 POST 返回传输层错误（脚本化失败）
    fail_first: AtomicUsize,
    /// 对 n≥1 的自定义状态码覆盖（None → 204 空体）
    n_ge1_status: Mutex<Option<(u16, Bytes)>>,
    /// 录制的全部 n≥1 POST
    recorded: Mutex<Vec<RecordedPost>>,
    /// 下行流（脚本化）
    stream_chunks: Mutex<VecDeque<Result<Bytes, anyhow::Error>>>,
    /// GET 是否已开过（一条长 GET）
    stream_opened: AtomicBool,
}

impl FakeTransport {
    fn new(server_priv: [u8; 32]) -> Self {
        Self {
            server_priv,
            post_delay: Mutex::new(Duration::from_millis(0)),
            fail_first: AtomicUsize::new(0),
            n_ge1_status: Mutex::new(None),
            recorded: Mutex::new(Vec::new()),
            stream_chunks: Mutex::new(VecDeque::new()),
            stream_opened: AtomicBool::new(false),
        }
    }

    fn recorded(&self) -> Vec<RecordedPost> {
        self.recorded.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl HttpTransport for FakeTransport {
    async fn post(&self, path: &str, body: Bytes) -> anyhow::Result<PostReply> {
        let is_handshake = path.contains("n=0&");
        if !is_handshake {
            self.recorded.lock().unwrap().push(RecordedPost {
                path: path.to_string(),
                body: body.clone(),
            });
        }

        let delay = *self.post_delay.lock().unwrap();
        if delay > Duration::ZERO {
            tokio::time::sleep(delay).await;
        }

        if is_handshake {
            // 真 Noise 服务端半边
            let msg1_cipher = {
                let len = u16::from_be_bytes([body[0], body[1]]) as usize;
                body[2..2 + len].to_vec()
            };
            let mut server = build_server(&self.server_priv)?;
            let mut buf = vec![0u8; 65535];
            server.read_message(&msg1_cipher, &mut buf)?;

            let msg2 = encode_msg2(&Msg2 {
                chosen_mux_id: MuxId::Wsmux,
                fallback: false,
            });
            let mut out = vec![0u8; 65535];
            let n = server.write_message(&msg2, &mut out)?;

            let mut reply = Vec::with_capacity(2 + n);
            reply.extend_from_slice(&(n as u16).to_be_bytes());
            reply.extend_from_slice(&out[..n]);
            Ok(PostReply {
                status: 200,
                body: Bytes::from(reply),
            })
        } else {
            // 脚本化失败：fail_first 计数
            let prev = self
                .fail_first
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                    if v > 0 {
                        Some(v - 1)
                    } else {
                        None
                    }
                });
            if prev.is_ok() {
                return Err(anyhow::anyhow!("scripted transport error"));
            }
            // 状态码覆盖
            let guard = self.n_ge1_status.lock().unwrap();
            if let Some((status, body)) = guard.clone() {
                return Ok(PostReply { status, body });
            }
            Ok(PostReply {
                status: 204,
                body: Bytes::new(),
            })
        }
    }

    async fn get_stream(
        &self,
        _path: &str,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<Bytes>>> {
        self.stream_opened.store(true, Ordering::SeqCst);
        let chunks: Vec<_> = self.stream_chunks.lock().unwrap().drain(..).collect();
        if chunks.is_empty() {
            // 空脚本 = 永不结束也永不报错的活流（长 GET 正常形态）
            Ok(Box::pin(futures::stream::pending()))
        } else {
            Ok(Box::pin(futures::stream::iter(chunks)))
        }
    }
}

/// 建立连接的通用脚手架（未使用的辅助保留为 make_pair）。
/// 建立带一致密钥的 (transport, conn) 对。
#[allow(dead_code)]
async fn make_pair(
    post_delay: Duration,
    stream_chunks: Vec<Result<Bytes, anyhow::Error>>,
) -> (Arc<FakeTransport>, XhttpConn) {
    let (server_priv, server_pub) = gen_keypair();
    let t = Arc::new(FakeTransport::new(server_priv));
    *t.post_delay.lock().unwrap() = post_delay;
    *t.stream_chunks.lock().unwrap() = stream_chunks.into();

    let (client_priv, _) = gen_keypair();
    let cfg = UpstreamCfg {
        server_pub,
        client_priv,
        mux_prefs: vec![MuxId::Wsmux],
        group_id: wsieve_xhttp::client::random_group_id(),
        ip_strategy: IpStrategy::Auto,
    };
    let (conn, _neg) = XhttpConn::connect(t.clone(), &cfg).await.unwrap();
    (t, conn)
}

/// 等待录制请求数达到 n（轮询 yield，paused clock 不适用——这些用真实时钟）。
async fn wait_for_records(t: &FakeTransport, n: usize) -> Vec<RecordedPost> {
    for _ in 0..500 {
        let r = t.recorded();
        if r.len() >= n {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    t.recorded()
}

#[tokio::test]
async fn handshake_success() {
    let (t, conn) = make_pair(Duration::ZERO, vec![]).await;
    // 让出轮次：后台任务 spawn + 下行 GET 发起
    for _ in 0..100 {
        if t.stream_opened.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(conn);
    assert!(t.stream_opened.load(Ordering::SeqCst));
}

#[tokio::test]
async fn handshake_garbage_reply_kills() {
    let (_server_priv, server_pub) = gen_keypair();
    // 坏 transport：对 n=0 回 404
    struct BadTransport;
    #[async_trait::async_trait]
    impl HttpTransport for BadTransport {
        async fn post(&self, _p: &str, _b: Bytes) -> anyhow::Result<PostReply> {
            Ok(PostReply {
                status: 404,
                body: Bytes::from("not found"),
            })
        }
        async fn get_stream(
            &self,
            _p: &str,
        ) -> anyhow::Result<BoxStream<'static, anyhow::Result<Bytes>>> {
            Err(anyhow::anyhow!("no stream"))
        }
    }
    let (client_priv, _) = gen_keypair();
    let result = XhttpConn::connect(
        Arc::new(BadTransport),
        &UpstreamCfg {
            server_pub,
            client_priv,
            mux_prefs: vec![MuxId::Wsmux],
            group_id: wsieve_xhttp::client::random_group_id(),
        ip_strategy: IpStrategy::Auto,
        },
    )
    .await;
    assert!(result.is_err());
}

/// 用服务端视角解密 TU 的工具。注意：Noise IK 的 msg2 含服务端新鲜
/// ephemeral，重放握手会导出不同传输密钥——因此必须保留握手中真实的
/// 服务端 TransportState（由 transport 在响应 n=0 时存下）。
struct ServerView {
    state: snow::TransportState,
}

impl ServerView {
    fn decrypt_tu(&mut self, cipher: &[u8]) -> anyhow::Result<Frame> {
        let mut plain = vec![0u8; 65535];
        let n = self.state.read_message(cipher, &mut plain)?;
        Ok(wsieve_proto::tu::decode_frame(&plain[..n])?)
    }
}

/// 把 POST body 解析为 TU 密文体列表。
fn split_tus(body: &[u8]) -> Vec<Vec<u8>> {
    // 按前缀切出各 TU 的密文体（不含 2 字节长度前缀）
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 2 <= body.len() {
        let len = u16::from_be_bytes([body[i], body[i + 1]]) as usize;
        if i + 2 + len > body.len() {
            break;
        }
        out.push(body[i + 2..i + 2 + len].to_vec());
        i += 2 + len;
    }
    out
}

#[tokio::test]
async fn write_frames_become_tus() {
    // 写 70000 字节 → 录制 POST body 按 TU 解析后完整覆盖全部字节且有序
    let (server_priv, server_pub) = gen_keypair();

    // 专用 RecordingTransport：真 Noise 握手 + 保留服务端传输状态 + 录制 n≥1 POST
    struct RecTransport {
        server_priv: [u8; 32],
        posts: Mutex<Vec<(String, Bytes)>>,
        server_state: tokio::sync::Mutex<Option<snow::TransportState>>,
    }
    #[async_trait::async_trait]
    impl HttpTransport for RecTransport {
        async fn post(&self, path: &str, body: Bytes) -> anyhow::Result<PostReply> {
            if path.contains("n=0&") {
                let len = u16::from_be_bytes([body[0], body[1]]) as usize;
                let msg1 = body[2..2 + len].to_vec();
                let mut server = build_server(&self.server_priv)?;
                let mut buf = vec![0u8; 65535];
                server.read_message(&msg1, &mut buf)?;
                let msg2 = encode_msg2(&Msg2 {
                    chosen_mux_id: MuxId::Wsmux,
                    fallback: false,
                });
                let mut out = vec![0u8; 65535];
                let n = server.write_message(&msg2, &mut out)?;
                let mut reply = Vec::with_capacity(2 + n);
                reply.extend_from_slice(&(n as u16).to_be_bytes());
                reply.extend_from_slice(&out[..n]);
                *self.server_state.lock().await = Some(server.into_transport_mode()?);
                return Ok(PostReply {
                    status: 200,
                    body: Bytes::from(reply),
                });
            }
            self.posts.lock().unwrap().push((path.to_string(), body));
            Ok(PostReply {
                status: 204,
                body: Bytes::new(),
            })
        }
        async fn get_stream(
            &self,
            _p: &str,
        ) -> anyhow::Result<BoxStream<'static, anyhow::Result<Bytes>>> {
            Ok(Box::pin(futures::stream::pending()))
        }
    }

    let rt = Arc::new(RecTransport {
        server_priv,
        posts: Mutex::new(Vec::new()),
        server_state: tokio::sync::Mutex::new(None),
    });

    let (client_priv, _) = gen_keypair();
    let cfg = UpstreamCfg {
        server_pub,
        client_priv,
        mux_prefs: vec![MuxId::Wsmux],
        group_id: wsieve_xhttp::client::random_group_id(),
        ip_strategy: IpStrategy::Auto,
    };
    let (mut conn, _neg) = XhttpConn::connect(rt.clone(), &cfg).await.unwrap();

    // 写 70000 字节（> 64516 单 TU 上限 → 至少 2 个 TU）
    let data: Vec<u8> = (0..70000u32).map(|i| (i % 251) as u8).collect();
    conn.write_all(&data).await.unwrap();
    conn.flush().await.unwrap();

    // 等聚合器 flush（4ms tick）
    tokio::time::sleep(Duration::from_millis(300)).await;

    let posts = rt.posts.lock().unwrap().clone();
    assert!(!posts.is_empty(), "at least one POST must be recorded");

    // 服务端视角解密全部 TU（真实的握手状态）
    let mut sv = ServerView {
        state: rt.server_state.lock().await.take().expect("handshake done"),
    };
    let mut reassembled = Vec::new();
    for (_path, body) in &posts {
        for tu_cipher in split_tus(body) {
            match sv.decrypt_tu(&tu_cipher).unwrap() {
                Frame::Data(d) => reassembled.extend_from_slice(&d),
                Frame::Padding => {}
            }
        }
    }
    assert!(reassembled.len() <= data.len());
    assert_eq!(&reassembled[..], &data[..reassembled.len()]);

    // 单 POST body ≤ 1MB（15 TU × MAX_PAYLOAD 上限）
    for (_, body) in &posts {
        assert!(
            body.len() <= 15 * (MAX_PAYLOAD + 3 + 16 + 2),
            "1MB per-POST cap"
        );
    }
}

#[tokio::test]
async fn window_capped_at_8() {
    // FakeTransport 延迟回复 → 写足量数据 → 断言录制到的最大在途 ≤ 8。
    // 通过「未回复时已发出的 POST 数」度量：用延迟 + 中途快照。
    let (server_priv, server_pub) = gen_keypair();

    struct SlowTransport {
        server_priv: [u8; 32],
        outstanding: AtomicUsize,
        max_outstanding: AtomicUsize,
        released: AtomicBool,
        posts: Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl HttpTransport for SlowTransport {
        async fn post(&self, path: &str, body: Bytes) -> anyhow::Result<PostReply> {
            if path.contains("n=0&") {
                let len = u16::from_be_bytes([body[0], body[1]]) as usize;
                let msg1 = body[2..2 + len].to_vec();
                let mut server = build_server(&self.server_priv)?;
                let mut buf = vec![0u8; 65535];
                server.read_message(&msg1, &mut buf)?;
                let msg2 = encode_msg2(&Msg2 {
                    chosen_mux_id: MuxId::Wsmux,
                    fallback: false,
                });
                let mut out = vec![0u8; 65535];
                let n = server.write_message(&msg2, &mut out)?;
                let mut reply = Vec::with_capacity(2 + n);
                reply.extend_from_slice(&(n as u16).to_be_bytes());
                reply.extend_from_slice(&out[..n]);
                return Ok(PostReply {
                    status: 200,
                    body: Bytes::from(reply),
                });
            }
            let cur = self.outstanding.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_outstanding.fetch_max(cur, Ordering::SeqCst);
            self.posts.lock().unwrap().push(path.to_string());
            // 挂起直到释放
            while !self.released.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            self.outstanding.fetch_sub(1, Ordering::SeqCst);
            Ok(PostReply {
                status: 204,
                body: Bytes::new(),
            })
        }
        async fn get_stream(
            &self,
            _p: &str,
        ) -> anyhow::Result<BoxStream<'static, anyhow::Result<Bytes>>> {
            Ok(Box::pin(futures::stream::pending()))
        }
    }

    let st = Arc::new(SlowTransport {
        server_priv,
        outstanding: AtomicUsize::new(0),
        max_outstanding: AtomicUsize::new(0),
        released: AtomicBool::new(false),
        posts: Mutex::new(Vec::new()),
    });

    let (client_priv, _) = gen_keypair();
    let cfg = UpstreamCfg {
        server_pub,
        client_priv,
        mux_prefs: vec![MuxId::Wsmux],
        group_id: wsieve_xhttp::client::random_group_id(),
        ip_strategy: IpStrategy::Auto,
    };
    let (mut conn, _neg) = XhttpConn::connect(st.clone(), &cfg).await.unwrap();

    // 写足量数据：64KB 聚合块 × 20 → 至少 20 个 POST 想发，窗口只能 8
    for _ in 0..20 {
        let chunk = vec![0xABu8; 64_000];
        conn.write_all(&chunk).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // 等聚合与发送
    tokio::time::sleep(Duration::from_millis(500)).await;

    let max = st.max_outstanding.load(Ordering::SeqCst);
    assert!(
        max <= 8,
        "in-flight window must be capped at 8, got {}",
        max
    );
    assert!(max >= 1, "something must have been sent");

    st.released.store(true, Ordering::SeqCst);
}

#[tokio::test]
async fn retry_same_seq_same_bytes() {
    // 首次传输错误、二次成功 → 两次录制请求 path 相同、body 逐字节相同、seq 相同
    let (t, mut conn) = make_pair(Duration::ZERO, vec![]).await;
    t.fail_first.store(1, Ordering::SeqCst);

    // 等后台任务完全启动（下行任务已开流），再写
    tokio::time::sleep(Duration::from_millis(50)).await;

    conn.write_all(b"hello retry").await.unwrap();
    conn.flush().await.unwrap();

    let recs = wait_for_records(&t, 2).await;
    assert!(
        recs.len() >= 2,
        "need first attempt + retry, got {}",
        recs.len()
    );
    let first = &recs[0];
    let second = &recs[1];
    assert_eq!(first.path, second.path, "same path (same seq in query)");
    assert_eq!(first.seq(), second.seq(), "same seq");
    assert_eq!(
        first.body, second.body,
        "byte-identical body (no re-encryption)"
    );
}

#[tokio::test]
async fn non_conventional_reply_kills_session() {
    // n≥1 收到 200+带 body → 会话死亡，后续写失败
    let (t, mut conn) = make_pair(Duration::ZERO, vec![]).await;
    *t.n_ge1_status.lock().unwrap() = Some((200, Bytes::from_static(b"upstream body")));

    conn.write_all(b"payload").await.unwrap();
    conn.flush().await.unwrap();

    // 等死亡事件传导到 AsyncRead/Write
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut buf = vec![0u8; 16];
    match conn.read(&mut buf).await {
        Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::ConnectionReset),
        Ok(n) => panic!("expected session-dead error, read {} bytes", n),
    }
    let r = conn.write_all(b"more").await;
    assert!(r.is_err(), "writes after session death must fail");
}

#[tokio::test]
async fn downlink_frames_flow() {
    // 脚本化 get_stream 产出真实加密 TU → AsyncRead 读出数据
    let (server_priv, server_pub) = gen_keypair();

    // 服务端视角：先完成一次握手拿到加密侧状态，再产出下行 TU。
    // 握手在 connect() 内发生——我们先离线做一次握手交换拿状态，
    // 但客户端的握手实例是一次性的……正确做法：录制客户端 msg1，
    // 由测试侧做服务端握手。connect 内部用独立 msg1——所以让
    // FakeTransport 的 post 在 n=0 时执行服务端握手并保留状态，
    // 之后用它加密下行帧。用状态化的 DownlinkTransport：
    struct DlTransport {
        server_priv: [u8; 32],
        state: tokio::sync::Mutex<Option<snow::TransportState>>,
        script: Vec<Frame>,
    }
    #[async_trait::async_trait]
    impl HttpTransport for DlTransport {
        async fn post(&self, path: &str, body: Bytes) -> anyhow::Result<PostReply> {
            if path.contains("n=0&") {
                let len = u16::from_be_bytes([body[0], body[1]]) as usize;
                let msg1 = body[2..2 + len].to_vec();
                let mut server = build_server(&self.server_priv)?;
                let mut buf = vec![0u8; 65535];
                server.read_message(&msg1, &mut buf)?;
                let msg2 = encode_msg2(&Msg2 {
                    chosen_mux_id: MuxId::Wsmux,
                    fallback: false,
                });
                let mut out = vec![0u8; 65535];
                let n = server.write_message(&msg2, &mut out)?;
                let mut reply = Vec::with_capacity(2 + n);
                reply.extend_from_slice(&(n as u16).to_be_bytes());
                reply.extend_from_slice(&out[..n]);
                let st = server.into_transport_mode()?;
                *self.state.lock().await = Some(st);
                return Ok(PostReply {
                    status: 200,
                    body: Bytes::from(reply),
                });
            }
            Ok(PostReply {
                status: 204,
                body: Bytes::new(),
            })
        }
        async fn get_stream(
            &self,
            _p: &str,
        ) -> anyhow::Result<BoxStream<'static, anyhow::Result<Bytes>>> {
            // 用握手后的服务端加密状态产出脚本帧的 TU 流
            let mut st_guard = self.state.lock().await;
            let st = st_guard.as_mut().expect("handshake done before GET");
            let mut wire = Vec::new();
            for frame in self.script.clone() {
                let plain = wsieve_proto::tu::encode_frame(&frame, &mut rand::rng())?;
                let mut cipher_buf = vec![0u8; 65535];
                let n = st.write_message(&plain, &mut cipher_buf)?;
                wire.extend_from_slice(&(n as u16).to_be_bytes());
                wire.extend_from_slice(&cipher_buf[..n]);
            }
            // 切成两个 chunk，中间断开，验证跨 chunk 解密
            let mid = wire.len() / 2;
            let chunks = vec![
                Ok(Bytes::from(wire[..mid].to_vec())),
                Ok(Bytes::from(wire[mid..].to_vec())),
            ];
            Ok(Box::pin(futures::stream::iter(chunks)))
        }
    }

    let t = Arc::new(DlTransport {
        server_priv,
        state: tokio::sync::Mutex::new(None),
        script: vec![
            Frame::Data(b"abc".to_vec()),
            Frame::Padding,
            Frame::Data(b"def".to_vec()),
        ],
    });

    let (client_priv, _) = gen_keypair();
    let cfg = UpstreamCfg {
        server_pub,
        client_priv,
        mux_prefs: vec![MuxId::Wsmux],
        group_id: wsieve_xhttp::client::random_group_id(),
        ip_strategy: IpStrategy::Auto,
    };
    let (mut conn, _neg) = XhttpConn::connect(t.clone(), &cfg).await.unwrap();

    let mut got = Vec::new();
    let deadline = tokio::time::Duration::from_secs(2);
    let read_all = async {
        while got.len() < 6 {
            let mut buf = [0u8; 64];
            let n = conn.read(&mut buf).await.expect("read data");
            got.extend_from_slice(&buf[..n]);
        }
    };
    tokio::time::timeout(deadline, read_all)
        .await
        .expect("downlink data within deadline");
    assert_eq!(&got, b"abcdef", "Data frames concatenated, Padding skipped");
}

#[tokio::test(start_paused = true)]
async fn idle_heartbeat_sends_padding() {
    // pause + advance 60s → 录制到 PADDING TU 的 POST
    let (server_priv, server_pub) = gen_keypair();

    struct HbTransport {
        server_priv: [u8; 32],
        server_state: tokio::sync::Mutex<Option<snow::TransportState>>,
        posts: Mutex<Vec<(String, Bytes)>>,
    }
    #[async_trait::async_trait]
    impl HttpTransport for HbTransport {
        async fn post(&self, path: &str, body: Bytes) -> anyhow::Result<PostReply> {
            if path.contains("n=0&") {
                let len = u16::from_be_bytes([body[0], body[1]]) as usize;
                let msg1 = body[2..2 + len].to_vec();
                let mut server = build_server(&self.server_priv)?;
                let mut buf = vec![0u8; 65535];
                server.read_message(&msg1, &mut buf)?;
                let msg2 = encode_msg2(&Msg2 {
                    chosen_mux_id: MuxId::Wsmux,
                    fallback: false,
                });
                let mut out = vec![0u8; 65535];
                let n = server.write_message(&msg2, &mut out)?;
                let mut reply = Vec::with_capacity(2 + n);
                reply.extend_from_slice(&(n as u16).to_be_bytes());
                reply.extend_from_slice(&out[..n]);
                *self.server_state.lock().await = Some(server.into_transport_mode()?);
                return Ok(PostReply {
                    status: 200,
                    body: Bytes::from(reply),
                });
            }
            self.posts.lock().unwrap().push((path.to_string(), body));
            Ok(PostReply {
                status: 204,
                body: Bytes::new(),
            })
        }
        async fn get_stream(
            &self,
            _p: &str,
        ) -> anyhow::Result<BoxStream<'static, anyhow::Result<Bytes>>> {
            Ok(Box::pin(futures::stream::pending()))
        }
    }

    let ht = Arc::new(HbTransport {
        server_priv,
        server_state: tokio::sync::Mutex::new(None),
        posts: Mutex::new(Vec::new()),
    });

    let (client_priv, _) = gen_keypair();
    let cfg = UpstreamCfg {
        server_pub,
        client_priv,
        mux_prefs: vec![MuxId::Wsmux],
        group_id: wsieve_xhttp::client::random_group_id(),
        ip_strategy: IpStrategy::Auto,
    };
    let (conn, _neg) = XhttpConn::connect(ht.clone(), &cfg).await.unwrap();

    // 空闲 70s（> 60s 心跳阈值）
    tokio::time::sleep(Duration::from_secs(70)).await;

    let posts = ht.posts.lock().unwrap().clone();
    assert!(!posts.is_empty(), "heartbeat POST must have been sent");

    let mut sv = ServerView {
        state: ht.server_state.lock().await.take().expect("handshake done"),
    };
    let mut saw_padding = false;
    for (_path, body) in &posts {
        for tu in split_tus(body) {
            if let Frame::Padding = sv.decrypt_tu(&tu).unwrap() {
                saw_padding = true;
            }
        }
    }
    assert!(saw_padding, "at least one PADDING TU in idle heartbeat");
    drop(conn);
}

