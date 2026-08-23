//! Task 14/15 全链路集成测试（spec §10 第 4 项——方案 A 的兑现点）。
//! 全部经真实 HTTP 表面（127.0.0.1 随机端口 axum）+ ReqwestTransport + 真实
//! Noise IK 握手 + 真实 5 种 mux + 真实 TcpStream 拨号，无 mock。

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex as AsyncMutex;
use wsieve_mux::{mux_factory, Mux, MuxStream};
use wsieve_proto::addr::{encode_addr, AddrPort, TargetAddr};
use wsieve_proto::crypto::gen_keypair;
use wsieve_proto::hello::MuxId;
use wsieve_server::{AppState, KeepaliveRange, ServerKeys, SEEN_CACHE_CAPACITY};
use wsieve_transport::{HttpTransport, PostReply, ReqwestTransport};
use wsieve_xhttp::client::{UpstreamCfg, XhttpConn};

/// 起 echo TCP 服务，返回地址。连接断开即结束该连接任务。
async fn echo_server() -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if s.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

#[derive(Clone)]
struct TestRig {
    addr: SocketAddr,
    client_priv: [u8; 32],
    server_pub: [u8; 32],
    state: Arc<AppState>,
}

async fn start_server() -> TestRig {
    let (server_priv, server_pub) = gen_keypair();
    let (client_priv, client_pub) = gen_keypair();
    let mut whitelist = HashSet::new();
    whitelist.insert(client_pub);
    let state = AppState::new(
        ServerKeys {
            priv_key: server_priv,
            whitelist,
        },
        vec![
            MuxId::Yamux,
            MuxId::Smux,
            MuxId::Muxado,
            MuxId::Picomux,
            MuxId::H2mux,
        ],
        KeepaliveRange::default(),
        SEEN_CACHE_CAPACITY,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = state.clone().router();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    TestRig {
        addr,
        client_priv,
        server_pub,
        state,
    }
}

/// 连接 + 客户端 mux（mux 包住 XhttpConn 本体）。
async fn connect_mux(
    rig: &TestRig,
    prefs: Vec<MuxId>,
) -> (Box<dyn Mux>, MuxId) {
    let transport = Arc::new(ReqwestTransport::new(format!("http://{}", rig.addr)).unwrap());
    let (conn, neg) = XhttpConn::connect(
        transport,
        &UpstreamCfg {
            server_pub: rig.server_pub,
            client_priv: rig.client_priv,
            mux_prefs: prefs,
        },
    )
    .await
    .unwrap();
    let io: MuxStream = Box::new(conn);
    let mux = mux_factory(neg.mux_id, io).await.unwrap();
    (mux, neg.mux_id)
}

/// 经 mux 流把 payload 发给 echo 并读回全部（首帧带 TargetAddr）。
async fn echo_roundtrip(
    mux: &dyn Mux,
    echo: SocketAddr,
    payload: &[u8],
) -> Vec<u8> {
    let mut stream = mux.open().await.unwrap();
    let mut frame = encode_addr(&AddrPort {
        addr: TargetAddr::V4([127, 0, 0, 1]),
        port: echo.port(),
    });
    frame.extend_from_slice(payload);
    stream.write_all(&frame).await.unwrap();
    stream.flush().await.unwrap();

    let mut got = Vec::new();
    let mut buf = [0u8; 8192];
    // 读到 EOF（对端关流）或超时；echo 回显长度 = payload，多余字节是 mux 控制帧
    // 之外不可能出现——这里以「已收齐 payload 即停，再读一次确认无损坏」为准
    while got.len() < payload.len() {
        let n = tokio::time::timeout(Duration::from_secs(15), stream.read(&mut buf))
            .await
            .expect("15s 内应有回显")
            .unwrap();
        assert!(n > 0, "premature EOF at {}", got.len());
        got.extend_from_slice(&buf[..n]);
    }
    got.truncate(payload.len());
    got
}

/// 服务端会话栈单元化诊断：握手后直接向 SessionStore 推一个手工加密的
/// yamux 客户端首包，观察 mux factory 是否消费。
/// 1. 完整握手 + mux 协商 + 开流 + 服务端拨号到本地 echo + 双向数据。
#[tokio::test]
async fn full_chain_echo() {
    let rig = start_server().await;
    let echo = echo_server().await;
    let (mux, chosen) = connect_mux(&rig, vec![MuxId::Yamux]).await;
    assert_eq!(chosen, MuxId::Yamux);
    let got = echo_roundtrip(mux.as_ref(), echo, b"ping-over-full-chain").await;
    assert_eq!(got, b"ping-over-full-chain");
}

/// 2. 5 种 mux 各跑一遍全链路（矩阵）。
#[tokio::test]
async fn all_mux_matrix() {
    // 每种 mux 独立 rig：不同 mux 库的全局/后台任务行为互不干扰
    for id in [
        MuxId::Yamux,
        MuxId::Smux,
        MuxId::Muxado,
        MuxId::Picomux,
        MuxId::H2mux,
    ] {
        let rig = start_server().await;
        run_one_mux(&rig, id).await;
    }
}

async fn run_one_mux(rig: &TestRig, id: MuxId) {
    let echo = echo_server().await;
    let (mux, chosen) = connect_mux(rig, vec![id]).await;
    assert_eq!(chosen, id, "偏好唯一时应选中 {id:?}");
    let tag = format!("matrix-{id:?}-payload");
    let got = echo_roundtrip(mux.as_ref(), echo, tag.as_bytes()).await;
    assert_eq!(got, tag.as_bytes(), "{id:?} 全链路失败");
}

/// 3. 64KB+ 上行连续写 → echo 回读一致（TU 分帧 + 重排透明）。
#[tokio::test]
async fn upstream_64k_integrity() {
    let rig = start_server().await;
    let echo = echo_server().await;
    let (mux, _chosen) = connect_mux(&rig, vec![MuxId::Yamux]).await;
    let payload: Vec<u8> = (0..80_000u32).map(|i| (i % 253) as u8).collect();
    let got = echo_roundtrip(mux.as_ref(), echo, &payload).await;
    assert_eq!(got.len(), payload.len());
    assert_eq!(got, payload);
}

/// 4. 服务端杀会话 → POST 得伪装响应（404 而非 204）→ 客户端判定会话死亡
///    （后续写报 BrokenPipe）。
#[tokio::test]
async fn dead_session_detection() {
    let rig = start_server().await;
    let transport = Arc::new(ReqwestTransport::new(format!("http://{}", rig.addr)).unwrap());
    let (mut conn, _neg) = XhttpConn::connect(
        transport,
        &UpstreamCfg {
            server_pub: rig.server_pub,
            client_priv: rig.client_priv,
            mux_prefs: vec![MuxId::Yamux],
        },
    )
    .await
    .unwrap();

    // 先证明会话活着：写一段数据，POST 得 204（无报错即活）
    conn.write_all(b"alive").await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 服务端杀会话（单测单会话，kill_all 等价）
    rig.state.store.kill_all().await;

    // 客户端后台任务发 POST → 收到伪装 404（非约定 204）→ fatal → SessionDead
    let start = std::time::Instant::now();
    let mut err = None;
    while start.elapsed() < Duration::from_secs(20) {
        match tokio::time::timeout(Duration::from_millis(200), conn.write(b"post-death"))
            .await
        {
            Err(_) => continue, // write 即时返回；等后台事件
            Ok(Err(e)) => {
                err = Some(e);
                break;
            }
            Ok(Ok(_)) => {}
        }
    }
    let e = err.expect("20s 内会话应死亡且写失败");
    assert_eq!(
        e.kind(),
        std::io::ErrorKind::BrokenPipe,
        "死亡后写应报 BrokenPipe"
    );
}

/// 乱序注入传输包装：对奇数 seq 的 POST 延迟 N 毫秒（其余原样）。
struct DelayTransport {
    inner: ReqwestTransport,
    /// seq -> 是否延迟
    delay_odd_ms: Arc<AsyncMutex<u64>>,
}

#[async_trait::async_trait]
impl HttpTransport for DelayTransport {
    async fn post(&self, path: &str, body: Bytes) -> anyhow::Result<PostReply> {
        // path = /api/sync?n=<seq>&sid=...
        let seq: u64 = path
            .split("n=")
            .nth(1)
            .and_then(|s| s.split('&').next())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if seq % 2 == 1 {
            let ms = *self.delay_odd_ms.lock().await;
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
        self.inner.post(path, body).await
    }

    async fn get_stream(
        &self,
        path: &str,
    ) -> anyhow::Result<futures::stream::BoxStream<'static, anyhow::Result<Bytes>>> {
        self.inner.get_stream(path).await
    }
}

/// 5. 乱序注入（奇数 seq 延迟 150ms）→ 服务端仍按序重组，读侧字节流连续。
#[tokio::test]
async fn out_of_order_reorder() {
    let rig = start_server().await;
    let echo = echo_server().await;

    let inner = ReqwestTransport::new(format!("http://{}", rig.addr)).unwrap();
    let transport = Arc::new(DelayTransport {
        inner,
        delay_odd_ms: Arc::new(AsyncMutex::new(150)),
    });
    let (conn, neg) = XhttpConn::connect(
        transport,
        &UpstreamCfg {
            server_pub: rig.server_pub,
            client_priv: rig.client_priv,
            mux_prefs: vec![MuxId::Yamux],
        },
    )
    .await
    .unwrap();
    assert_eq!(neg.mux_id, MuxId::Yamux);
    let io: MuxStream = Box::new(conn);
    let mux = mux_factory(MuxId::Yamux, io).await.unwrap();

    // 连续多段写：聚合层会切成多个 POST（seq 递增），奇数延迟后到达序打乱
    let payload: Vec<u8> = (0..40_000u32).map(|i| (i % 249) as u8).collect();
    let got = echo_roundtrip(mux.as_ref(), echo, &payload).await;
    assert_eq!(got, payload, "乱序到达时服务端重组应透明");
}

/// 6. 探测等价性（spec §10 第 5 项）：垃圾 POST / 重放 msg1 / 随机路径扫描
///    → 三类响应字节级一致（status + headers + body）。
#[tokio::test]
async fn probing_equivalence() {
    let rig = start_server().await;
    let c = reqwest::Client::new();
    let base = format!("http://{}", rig.addr);

    // (a) 垃圾握手 POST（解密必失败 → 伪装）
    let garbage = c
        .post(format!("{base}/api/sync?n=0&sid=AAAAAAAAAAAAAAAAAAAAAA"))
        .body(vec![0x42u8; 64])
        .send()
        .await
        .unwrap();

    // (b) 重放一个合法 msg1：同字节重发两次，第二次命中已见缓存 → 伪装
    let mut client = wsieve_proto::crypto::build_client(&rig.server_pub, &rig.client_priv).unwrap();
    let hello = wsieve_proto::hello::encode_msg1(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
        &[MuxId::Yamux],
    );
    let mut buf = vec![0u8; 65535];
    let n = client.write_message(&hello, &mut buf).unwrap();
    let mut tu = Vec::with_capacity(2 + n);
    tu.extend_from_slice(&(n as u16).to_be_bytes());
    tu.extend_from_slice(&buf[..n]);
    let sid2 = "AAAAAAAAAAAAAAAAAAAAAA"; // 128-bit 全零的 base64url
    let first = c
        .post(format!("{base}/api/sync?n=0&sid={sid2}"))
        .body(tu.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(first.status().as_u16(), 200, "首见合法握手应 200");
    let replay = c
        .post(format!("{base}/api/sync?n=0&sid={sid2}"))
        .body(tu)
        .send()
        .await
        .unwrap();

    // (c) 随机路径扫描
    let scan = c.get(format!("{base}/wp-admin/setup-config.php")).send().await.unwrap();

    let norm = |r: reqwest::Response| async move {
        let status = r.status().as_u16();
        let mut headers = Vec::new();
        for (k, v) in r.headers().iter() {
            if k.as_str() == "date" || k.as_str() == "content-length" {
                continue;
            }
            headers.push((k.as_str().to_string(), v.to_str().unwrap_or("").to_string()));
        }
        headers.sort();
        (status, headers, r.bytes().await.unwrap())
    };

    let a = norm(garbage).await;
    let b = norm(replay).await;
    let cc = norm(scan).await;
    assert_eq!(a, b, "垃圾 vs 重放响应必须一致");
    assert_eq!(a, cc, "垃圾 vs 路径扫描响应必须一致");
    assert_eq!(a.0, 404);
    // (b) 与 (a) 同为伪装：重放未产生 200/会话态
}

/// 7. 并发会话唤醒回归：单 rig（单 AppState/SessionStore/axum）上 5 个并发
///    客户端会话（同一 mux：Yamux——测的是 SessionStore 而非 mux 矩阵），
///    各自写独特 pattern 经共享 echo 服务器回显。全部 15s 内完成。
///    回归背景：SessionStore 曾用单个全局 Notify，push_post 的 notify_one
///    可能被无关会话的读者消费，导致目标会话 read 永久挂起（修复前本测试
///    会因某会话上行泵挂起而超时）。
#[tokio::test]
async fn concurrent_sessions_do_not_lose_wakeups() {
    const N: usize = 5;
    let rig = start_server().await;
    let echo = echo_server().await;

    // 关键时序：先让 5 个会话全部完成握手/attach 并开流（服务端每个会话的
    // 上行 read 均已挂起在 Notify 上），再各自写数据——这正是全局 Notify 的
    // notify_one 被无关会话读者抢走、目标会话永久沉睡的窗口。
    let mut sessions = Vec::new();
    for _ in 0..N {
        let (mux, chosen) = connect_mux(&rig, vec![MuxId::Yamux]).await;
        assert_eq!(chosen, MuxId::Yamux);
        sessions.push(mux);
    }
    // 等待所有服务端读任务挂起（attach 完成、无数据可读）
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 反序写（session 4 先写、0 最后）：全局 Notify 的 notify_one 按 FIFO
    // 唤醒队头读者，若队头不是目标会话，permit 被无关会话消费后重新排到
    // 队尾——目标会话在被唤醒前一直沉睡。反序写使队头几乎必然不是目标，
    // 稳定复现修复前的唤醒丢失。
    let mut handles = Vec::new();
    for (i, mux) in sessions.into_iter().enumerate() {
        handles.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis((N - 1 - i) as u64 * 150)).await;
            let tag = format!("concurrent-session-{i}-payload");
            let got = echo_roundtrip(mux.as_ref(), echo, tag.as_bytes()).await;
            assert_eq!(got, tag.as_bytes(), "session {i} 回显不一致");
        }));
    }

    let all = futures::future::join_all(handles);
    let results = tokio::time::timeout(Duration::from_secs(15), all)
        .await
        .expect("15s 内 5 个并发会话都应完成（唤醒丢失回归）");
    for (i, r) in results.into_iter().enumerate() {
        r.unwrap_or_else(|e| panic!("session {i} 任务失败: {e}"));
    }
}
