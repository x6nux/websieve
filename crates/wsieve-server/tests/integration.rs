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
use wsieve_mux::stripe_runtime::{StripeCfg, StripeDialer};
use wsieve_mux::{mux_factory, Mux, MuxStream};
use wsieve_proto::addr::{AddrPort, TargetAddr};
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
            MuxId::Wsmux,
            MuxId::Wsmux,
            MuxId::Wsmux,
            MuxId::Wsmux,
            MuxId::Wsmux,
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
) -> (std::sync::Arc<dyn Mux>, MuxId) {
    let transport = Arc::new(ReqwestTransport::new(format!("http://{}", rig.addr)).unwrap());
    let (conn, neg) = XhttpConn::connect(
        transport,
        &UpstreamCfg {
            server_pub: rig.server_pub,
            client_priv: rig.client_priv,
            mux_prefs: prefs,
            group_id: wsieve_xhttp::client::random_group_id(),
            ip_strategy: wsieve_proto::hello::IpStrategy::Auto,
            profile: Default::default(),
        },
    )
    .await
    .unwrap();
    let io: MuxStream = Box::new(conn);
    let mux = mux_factory(neg.mux_id, io).await.unwrap();
    (std::sync::Arc::from(mux), neg.mux_id)
}

/// v2：经 StripeDialer 开 conn（BIDI OPEN + TargetAddr），发 payload 读回全部。
/// 高阈值（不升级）以贴近小流量路径；乱序/大流量由 stripe 专项测试覆盖。
async fn echo_roundtrip(
    mux: std::sync::Arc<dyn Mux>,
    echo: SocketAddr,
    payload: &[u8],
) -> Vec<u8> {
    let dialer = StripeDialer::new(
        mux,
        StripeCfg {
            target_lanes: 4,
            upgrade_bytes: u64::MAX,
            upgrade_rate_bps: 0,
            upgrade_window: Duration::from_millis(1),
            extra_sessions: 0,
        },
    );
    let mut s = dialer
        .connect(&AddrPort {
            addr: TargetAddr::V4([127, 0, 0, 1]),
            port: echo.port(),
        })
        .await
        .unwrap();
    s.write_all(payload).await.unwrap();
    let mut got = vec![0u8; payload.len()];
    let mut done = 0;
    while done < payload.len() {
        let n = tokio::time::timeout(Duration::from_secs(15), s.read(&mut got[done..]))
            .await
            .expect("15s 内应有回显")
            .unwrap();
        assert!(n > 0, "premature EOF at {done}");
        done += n;
    }
    got
}

/// 服务端会话栈单元化诊断：握手后直接向 SessionStore 推一个手工加密的
/// 客户端首包，观察 mux factory 是否消费。
/// 1. 完整握手 + mux 协商 + 开流 + 服务端拨号到本地 echo + 双向数据。
#[tokio::test]
async fn full_chain_echo() {
    let rig = start_server().await;
    let echo = echo_server().await;
    let (mux, chosen) = connect_mux(&rig, vec![MuxId::Wsmux]).await;
    assert_eq!(chosen, MuxId::Wsmux);
    let got = echo_roundtrip(mux, echo, b"ping-over-full-chain").await;
    assert_eq!(got, b"ping-over-full-chain");
}

/// 2. 5 种 mux 各跑一遍全链路（矩阵）。
#[tokio::test]
async fn all_mux_matrix() {
    // 每种 mux 独立 rig：不同 mux 库的全局/后台任务行为互不干扰
    for id in [
        MuxId::Wsmux,
        MuxId::Wsmux,
        MuxId::Wsmux,
        MuxId::Wsmux,
        MuxId::Wsmux,
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
    let got = echo_roundtrip(mux, echo, tag.as_bytes()).await;
    assert_eq!(got, tag.as_bytes(), "{id:?} 全链路失败");
}

/// 3. 64KB+ 上行连续写 → echo 回读一致（TU 分帧 + 重排透明）。
#[tokio::test]
async fn upstream_64k_integrity() {
    let rig = start_server().await;
    let echo = echo_server().await;
    let (mux, _chosen) = connect_mux(&rig, vec![MuxId::Wsmux]).await;
    let payload: Vec<u8> = (0..80_000u32).map(|i| (i % 253) as u8).collect();
    let got = echo_roundtrip(mux, echo, &payload).await;
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
            mux_prefs: vec![MuxId::Wsmux],
            group_id: wsieve_xhttp::client::random_group_id(),
            ip_strategy: wsieve_proto::hello::IpStrategy::Auto,
            profile: Default::default(),
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
            mux_prefs: vec![MuxId::Wsmux],
            group_id: wsieve_xhttp::client::random_group_id(),
            ip_strategy: wsieve_proto::hello::IpStrategy::Auto,
            profile: Default::default(),
        },
    )
    .await
    .unwrap();
    assert_eq!(neg.mux_id, MuxId::Wsmux);
    let io: MuxStream = Box::new(conn);
    let mux: std::sync::Arc<dyn Mux> = std::sync::Arc::from(mux_factory(MuxId::Wsmux, io).await.unwrap());

    // 连续多段写：聚合层会切成多个 POST（seq 递增），奇数延迟后到达序打乱
    let payload: Vec<u8> = (0..40_000u32).map(|i| (i % 249) as u8).collect();
    let got = echo_roundtrip(mux, echo, &payload).await;
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
        wsieve_xhttp::client::random_group_id(),
        &[MuxId::Wsmux],
        wsieve_proto::hello::IpStrategy::Auto,
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
        let (mux, chosen) = connect_mux(&rig, vec![MuxId::Wsmux]).await;
        assert_eq!(chosen, MuxId::Wsmux);
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
            let got = echo_roundtrip(mux, echo, tag.as_bytes()).await;
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

/// CORS 头只出现在**认证成功**的响应上 —— 判据是「认证与否」，不是「同不同域」。
///
/// 两种场景都需要跨源 fetch：多端口条带（同域名不同端口）与单 WebView 承载
/// 多出站（彻底不同域名，设计文档 §9.1）。服务端无从预知客户端把哪台机器当
/// 宿主，故 Origin 一律回显。真正的防线是「未认证请求走伪装路径、绝不带头」：
/// CORS 头出现在 nginx 默认页上本身就是可探测特征，而探测者发不出合法 msg1
/// 就永远看不到任何 CORS 痕迹。
#[tokio::test]
async fn cors_headers_only_on_authenticated_responses() {
    let rig = start_server().await;
    let c = reqwest::Client::new();
    let base = format!("http://{}", rig.addr);
    let host = rig.addr.to_string();
    // 彻底不同域名的 Origin —— 单 WebView 承载多出站产生的正是这种形态
    let cross_domain_origin = "https://wsieve-host-a.example".to_string();

    // 1) 未认证的协议路径 → 走伪装，绝不带 CORS 头
    let r = c
        .post(format!("{base}/api/sync?n=7&sid=AAAAAAAAAAAAAAAAAAAAAA"))
        .header("Origin", &cross_domain_origin)
        .header("Host", &host)
        .body("garbage")
        .send()
        .await
        .unwrap();
    assert!(
        r.headers().get("access-control-allow-origin").is_none(),
        "未认证响应不得带 CORS 头（否则成为探测特征）"
    );

    // 2) 完全无关的路径（纯伪装页）→ 同样不带
    let r = c
        .get(format!("{base}/"))
        .header("Origin", &cross_domain_origin)
        .header("Host", &host)
        .send()
        .await
        .unwrap();
    assert!(r.headers().get("access-control-allow-origin").is_none());

    // 3) 认证成功的握手响应 → 带 CORS 头，且回显具体 Origin（非 `*`，
    //    因为 emitter 用 credentials:'include'）
    let transport = std::sync::Arc::new(CorsProbeTransport {
        client: c.clone(),
        base: base.clone(),
        origin: cross_domain_origin.clone(),
        host: host.clone(),
        seen: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    let seen = transport.seen.clone();
    let _ = XhttpConn::connect(
        transport,
        &UpstreamCfg {
            server_pub: rig.server_pub,
            client_priv: rig.client_priv,
            mux_prefs: vec![MuxId::Wsmux],
            group_id: wsieve_xhttp::client::random_group_id(),
            ip_strategy: wsieve_proto::hello::IpStrategy::Auto,
            profile: Default::default(),
        },
    )
    .await
    .expect("握手应成功");
    let recorded = seen.lock().unwrap().clone();
    let handshake_hdrs = recorded.first().expect("应记录到握手响应头");
    assert_eq!(
        handshake_hdrs.0.as_deref(),
        Some(cross_domain_origin.as_str()),
        "认证成功的响应应回显具体 Origin"
    );
    assert_eq!(handshake_hdrs.1.as_deref(), Some("true"), "应允许携带凭据");
}

/// 记录响应 CORS 头的 transport（只用于上面的测试）。
struct CorsProbeTransport {
    client: reqwest::Client,
    base: String,
    origin: String,
    host: String,
    /// (allow-origin, allow-credentials, expose-headers, server-timing)
    ///
    /// 后两项是自适应流控的回传通道所需：`server-timing` 是载荷，
    /// `expose-headers` 是跨域 JS 能否读到它的前提。两者缺一，通道就静默失效。
    seen: std::sync::Arc<
        std::sync::Mutex<Vec<(Option<String>, Option<String>, Option<String>, Option<String>)>>,
    >,
}

#[async_trait::async_trait]
impl HttpTransport for CorsProbeTransport {
    async fn post(&self, path: &str, body: bytes::Bytes) -> anyhow::Result<PostReply> {
        let resp = self
            .client
            .post(format!("{}{}", self.base, path))
            .header("Origin", &self.origin)
            .header("Host", &self.host)
            .body(body)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let hv = |n: &str| {
            resp.headers()
                .get(n)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        };
        self.seen.lock().unwrap().push((
            hv("access-control-allow-origin"),
            hv("access-control-allow-credentials"),
            hv("access-control-expose-headers"),
            hv("server-timing"),
        ));
        let body = resp.bytes().await?;
        Ok(PostReply { status, body, peer: None })
    }

    async fn get_stream(
        &self,
        path: &str,
    ) -> anyhow::Result<futures::stream::BoxStream<'static, anyhow::Result<bytes::Bytes>>> {
        let resp = self
            .client
            .get(format!("{}{}", self.base, path))
            .header("Origin", &self.origin)
            .header("Host", &self.host)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("GET {path} -> {}", resp.status().as_u16());
        }
        use futures::StreamExt;
        Ok(resp
            .bytes_stream()
            .map(|r| r.map_err(anyhow::Error::new))
            .boxed())
    }
}

/// `Server-Timing` 与 CORS 头受同一条防线约束：**只出现在认证成功的响应上**。
///
/// 这个头是自适应流控的回传通道（服务端把处理耗时和上行 seq 空洞告诉客户端）。
/// 它一旦漏到伪装页上就是个可探测特征——nginx 默认页发 `server-timing` 里带
/// 一个 `q;desc="0-0"`，是任何真实 nginx 都不会有的形状。
///
/// 与 `cors_headers_only_on_authenticated_responses` 分成两条而不是并进去：
/// 两个头挂在不同的调用点上，将来任何一个被挪走，都该有自己的那条测试变红。
#[tokio::test]
async fn server_timing_only_on_authenticated_responses() {
    let rig = start_server().await;
    let c = reqwest::Client::new();
    let base = format!("http://{}", rig.addr);
    let host = rig.addr.to_string();

    // 1) 未认证的协议路径 → 伪装，不得带
    let r = c
        .post(format!("{base}/api/sync?n=7&sid=AAAAAAAAAAAAAAAAAAAAAA"))
        .header("Host", &host)
        .body("garbage")
        .send()
        .await
        .unwrap();
    assert!(
        r.headers().get("server-timing").is_none(),
        "未认证响应带了 server-timing，成为探测特征"
    );

    // 2) 纯伪装页 → 同样不得带
    let r = c.get(format!("{base}/")).header("Host", &host).send().await.unwrap();
    assert!(r.headers().get("server-timing").is_none());
}

/// `Server-Timing` 必须同时被 `Access-Control-Expose-Headers` 放行。
///
/// **两个头管的是两件不同的事，这正是原先搞混的地方**：
///   - `Timing-Allow-Origin` 放行 `PerformanceResourceTiming`（走 Performance API）
///   - `Access-Control-Expose-Headers` 放行 `Response.headers.get()`（走 fetch）
///
/// 自适应流控的回传读的是后者。少了它，跨域的
/// `resp.headers.get('server-timing')` 恒为 null——不报错，只表现为服务端
/// 观测永远是"无数据"，整个回传通道静默失效。
///
/// reqwest 不做 CORS 过滤，所以"能读到这个头"的测试抓不到这个缺陷；
/// 必须直接断言放行头本身存在且包含它。
#[tokio::test]
async fn server_timing_is_exposed_to_cross_origin_javascript() {
    let rig = start_server().await;
    let c = reqwest::Client::new();
    let base = format!("http://{}", rig.addr);
    let host = rig.addr.to_string();
    let origin = "https://wsieve-host-a.example".to_string();

    let transport = std::sync::Arc::new(CorsProbeTransport {
        client: c.clone(),
        base: base.clone(),
        origin: origin.clone(),
        host: host.clone(),
        seen: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    let seen = transport.seen.clone();
    let (mut conn, _neg) = XhttpConn::connect(
        transport,
        &UpstreamCfg {
            server_pub: rig.server_pub,
            client_priv: rig.client_priv,
            mux_prefs: vec![MuxId::Wsmux],
            group_id: wsieve_xhttp::client::random_group_id(),
            ip_strategy: wsieve_proto::hello::IpStrategy::Auto,
            profile: Default::default(),
        },
    )
    .await
    .expect("握手应当成功");

    // 触发一个 n≥1 的数据面 POST —— Server-Timing 只挂在那上面。
    use tokio::io::AsyncWriteExt;
    conn.write_all(&[1u8; 64]).await.unwrap();
    for _ in 0..200 {
        if seen.lock().unwrap().iter().any(|(_, _, _, st)| st.is_some()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let headers = seen.lock().unwrap().clone();
    let (_, _, expose, _) = headers
        .iter()
        .find(|(_, _, _, st)| st.is_some())
        .expect("数据面响应上应当有 server-timing")
        .clone();
    let expose = expose
        .expect("缺 Access-Control-Expose-Headers —— 跨域 JS 读不到 server-timing，回传通道静默失效")
        .to_ascii_lowercase();
    assert!(
        expose.contains("server-timing"),
        "放行列表里没有 server-timing，实为「{expose}」"
    );
}
