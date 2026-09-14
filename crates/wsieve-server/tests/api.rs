//! Task 13 集成测试：auth-before-routing + 伪装回退（spec §6.2/§6.3/§8）。
//! 9 个用例全部走真实 Noise IK 握手 / 真实会话 / 真实加密 PADDING TU；
//! keepalive 区间与缓存容量是配置旋钮（非 mock）。

use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use futures::StreamExt;
use rand::RngCore;
use wsieve_proto::crypto::{build_client, gen_keypair};
use wsieve_proto::hello::{IpStrategy, decode_msg2, encode_msg1, MuxId};
use wsieve_proto::tu::{decode_frame, Frame};
use wsieve_server::{AppState, DisguiseCfg, KeepaliveRange, ServerKeys, SEEN_CACHE_CAPACITY};
use wsieve_xhttp::server::Sid;

struct TestServer {
    addr: SocketAddr,
    client_priv: [u8; 32],
    server_pub: [u8; 32],
}

/// 起一个真实 TCP 上的 axum 服务（随机端口），keepalive 调小使流测试可等。
async fn start_server(seen_capacity: usize, keepalive: KeepaliveRange) -> TestServer {
    start_server_with_disguise(seen_capacity, keepalive, DisguiseCfg::default()).await
}

/// 同上，但可指定伪装配置（Alt-Svc 测试需要非默认的 alt_svc_port）。
async fn start_server_with_disguise(
    seen_capacity: usize,
    keepalive: KeepaliveRange,
    disguise: DisguiseCfg,
) -> TestServer {
    let (server_priv, server_pub) = gen_keypair();
    let (client_priv, client_pub) = gen_keypair();
    let mut whitelist = HashSet::new();
    whitelist.insert(client_pub);
    let state = AppState::with_disguise(
        ServerKeys {
            priv_key: server_priv,
            whitelist,
        },
        vec![MuxId::Wsmux, MuxId::Wsmux],
        keepalive,
        seen_capacity,
        disguise,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, state.router()).await.unwrap();
    });
    TestServer {
        addr,
        client_priv,
        server_pub,
    }
}

fn http(_ts: &TestServer) -> reqwest::Client {
    reqwest::Client::builder()
        .build()
        .unwrap()
}

fn url(ts: &TestServer, path: &str) -> String {
    format!("http://{}{}", ts.addr, path)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// 构造真实 msg1 TU：Noise IK 客户端 + 0-RTT payload(encode_msg1)。
fn make_msg1(
    ts: &TestServer,
    ts_ms: u64,
) -> (Vec<u8>, snow::HandshakeState, Sid, String) {
    let mut client = build_client(&ts.server_pub, &ts.client_priv).unwrap();
    let hello = encode_msg1(ts_ms, wsieve_xhttp::client::random_group_id(), &[MuxId::Wsmux, MuxId::Wsmux], IpStrategy::Auto);
    let mut buf = vec![0u8; 65535];
    let n = client.write_message(&hello, &mut buf).unwrap();
    let mut tu = Vec::with_capacity(2 + n);
    tu.extend_from_slice(&(n as u16).to_be_bytes());
    tu.extend_from_slice(&buf[..n]);
    let sid = Sid::random();
    let sid_b64 = URL_SAFE_NO_PAD.encode(sid.0);
    (tu, client, sid, sid_b64)
}

/// 1. GET / → 200 nginx 页。
#[tokio::test]
async fn root_serves_nginx_page() {
    let ts = start_server(SEEN_CACHE_CAPACITY, KeepaliveRange::default()).await;
    let resp = http(&ts).get(url(&ts, "/")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["server"], "nginx");
    let body = resp.text().await.unwrap();
    assert!(body.contains("Welcome to nginx"), "body: {body}");
}

/// 2. 垃圾握手请求与随机路径的伪装响应无差别（同 status、同 body 形状）。
#[tokio::test]
async fn garbage_handshake_indistinguishable_from_404() {
    let ts = start_server(SEEN_CACHE_CAPACITY, KeepaliveRange::default()).await;
    let c = http(&ts);
    let sid = Sid::random();
    let sid_b64 = URL_SAFE_NO_PAD.encode(sid.0);

    let mut garbage = vec![0u8; 64];
    rand::rng().fill_bytes(&mut garbage);
    let resp = c
        .post(url(&ts, &format!("/api/sync?n=0&sid={sid_b64}")))
        .body(garbage)
        .send()
        .await
        .unwrap();
    let other = c.get(url(&ts, "/random-path")).send().await.unwrap();

    // 两者都应是伪装出口（此处均为 404 + nginx 页形状）
    assert_eq!(resp.status(), other.status());
    let b1 = resp.bytes().await.unwrap();
    let b2 = other.bytes().await.unwrap();
    assert_eq!(b1, b2, "垃圾握手与随机路径响应必须无差别");
}

/// 3. 合法 msg1 → 200，body 可解密出合法 msg2。
#[tokio::test]
async fn valid_handshake_returns_msg2() {
    let ts = start_server(SEEN_CACHE_CAPACITY, KeepaliveRange::default()).await;
    let (tu, mut client, _, sid_b64) = make_msg1(&ts, now_ms());
    let resp = http(&ts)
        .post(url(&ts, &format!("/api/sync?n=0&sid={sid_b64}")))
        .body(tu)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.bytes().await.unwrap();
    let len = u16::from_be_bytes([body[0], body[1]]) as usize;
    let msg2_cipher = &body[2..2 + len];
    let mut plain = vec![0u8; 65535];
    let n = client.read_message(msg2_cipher, &mut plain).unwrap();
    let msg2 = decode_msg2(&plain[..n]).unwrap();
    assert_eq!(msg2.chosen_mux_id, MuxId::Wsmux);
    assert!(!msg2.fallback);
}

/// Alt-Svc 必须覆盖**数据面**响应，不能只加在伪装页面上。
///
/// 承载页自 2026-09-10 起是本机 http 壳，WebView 的数据面请求直接走
/// `/api/sync` 与 `/api/events`，**从不请求伪装页面**。若这个头只加在
/// `disguise_resp` 里，客户端就永远看不到它——而 Apple 的网络栈不做推测性
/// QUIC 尝试，没看到 Alt-Svc 就永远不会升级到 h3，等于 h3 白做。
#[tokio::test]
async fn alt_svc_covers_data_plane_responses() {
    let ts = start_server_with_disguise(
        SEEN_CACHE_CAPACITY,
        KeepaliveRange::default(),
        DisguiseCfg {
            upstream: None,
            alt_svc_port: Some(443),
            ..DisguiseCfg::default()
        },
    )
    .await;
    let (tu, _, _, sid_b64) = make_msg1(&ts, now_ms());
    let resp = http(&ts)
        .post(url(&ts, &format!("/api/sync?n=0&sid={sid_b64}")))
        .body(tu)
        .send()
        .await
        .unwrap();
    // 200 = 握手成功，这条响应由 handshake() 直接产出，不经 disguise_resp
    assert_eq!(resp.status(), 200, "前提：这必须是一条数据面响应");
    let v = resp
        .headers()
        .get("alt-svc")
        .expect("数据面响应必须带 Alt-Svc")
        .to_str()
        .unwrap()
        .to_string();
    assert!(v.contains(r#"h3=":443""#), "实际: {v}");
}

/// 伪装响应仍然带 Alt-Svc（把加头位置上移不能弄丢原有覆盖面）。
#[tokio::test]
async fn alt_svc_still_covers_disguise_responses() {
    let ts = start_server_with_disguise(
        SEEN_CACHE_CAPACITY,
        KeepaliveRange::default(),
        DisguiseCfg {
            upstream: None,
            alt_svc_port: Some(8443),
            ..DisguiseCfg::default()
        },
    )
    .await;
    let resp = http(&ts).get(url(&ts, "/")).send().await.unwrap();
    let v = resp
        .headers()
        .get("alt-svc")
        .expect("伪装响应原本就带这个头，不得回归")
        .to_str()
        .unwrap()
        .to_string();
    assert!(v.contains(r#"h3=":8443""#), "实际: {v}");
}

/// 没有可用的 h3 端口时必须完全沉默。
///
/// 宣告一个连不上的 QUIC 端点，会让客户端此后每次连接都先试 QUIC 超时
/// 再回落，白白多一轮延迟——比不宣告更糟。
#[tokio::test]
async fn alt_svc_absent_when_h3_unavailable() {
    let ts = start_server(SEEN_CACHE_CAPACITY, KeepaliveRange::default()).await;
    let resp = http(&ts).get(url(&ts, "/")).send().await.unwrap();
    assert!(resp.headers().get("alt-svc").is_none());
}

/// 闸门置位后，响应必须改发 `Alt-Svc: clear`。
///
/// 这是降级的第一步。RFC 7838 规定 `clear` 让客户端作废该 origin 的全部
/// 替代服务记录——但它**只约束新连接**，拆不掉已建立的那条 h3，所以服务端
/// 侧还必须主动断连（见 http3.rs 的 spawn_quality_probe）。
#[tokio::test]
async fn degraded_gate_switches_alt_svc_to_clear() {
    let gate = wsieve_server::H3Gate::new();
    let ts = start_server_with_disguise(
        SEEN_CACHE_CAPACITY,
        KeepaliveRange::default(),
        DisguiseCfg {
            upstream: None,
            alt_svc_port: Some(443),
            h3_gate: gate.clone(),
            ..DisguiseCfg::default()
        },
    )
    .await;

    // 未降级：正常宣告 h3
    let v = http(&ts).get(url(&ts, "/")).send().await.unwrap().headers()["alt-svc"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(v.contains(r#"h3=":443""#), "降级前应正常宣告，实际: {v}");

    // 置位闸门后：同一个 server、同一个 router，响应必须变成 clear。
    // 闸门是运行期状态，若实现时在构建 Router 那一刻就把头算死，这里就会失败。
    gate.degrade_for(Duration::from_secs(600));
    let v2 = http(&ts).get(url(&ts, "/")).send().await.unwrap().headers()["alt-svc"]
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(v2, "clear", "降级后必须发 clear，实际: {v2}");

    // 冷却到期后自动恢复宣告，不需要人工干预
    gate.reset();
    let v3 = http(&ts).get(url(&ts, "/")).send().await.unwrap().headers()["alt-svc"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(v3.contains(r#"h3=":443""#), "冷却结束应自动恢复，实际: {v3}");
}

/// 重复降级取较晚的截止时刻，短冷却不得缩短长冷却。
#[test]
fn degrade_takes_the_later_deadline() {
    let gate = wsieve_server::H3Gate::new();
    gate.degrade_for(Duration::from_secs(600));
    // 第二次用**零长冷却**，而不是"短一点的冷却"：后者截止时刻仍在未来，
    // 用 store 还是 fetch_max 实现都会让 is_degraded() 为真，测不出区别。
    // 零长冷却的截止时刻就是此刻，已然过期——若实现用了 store，它会覆盖掉
    // 前面那 600 秒，这里立刻变 false。
    gate.degrade_for(Duration::from_secs(0));
    assert!(
        gate.is_degraded(),
        "后置位的零长冷却不得抹掉前一次的 600 秒"
    );
}

/// Timing-Allow-Origin 必须无条件出现，且与 h3 是否可用无关。
///
/// 承载页（本机 http 壳）与数据面是跨 origin，而 WebKit 自 2022 年起把
/// `PerformanceResourceTiming.nextHopProtocol` 置于 TAO 保护之下——缺这个头
/// 时它一律返回空字符串。没有它，客户端就无从知道自己跑在 h1/h2/h3 的哪
/// 一个上，"h3 是不是变慢了"也就无从判断。
#[tokio::test]
async fn timing_allow_origin_is_always_present() {
    // 连 h3 都没开的情况下也要有——它服务的是观测，不是 h3
    let ts = start_server(SEEN_CACHE_CAPACITY, KeepaliveRange::default()).await;
    let resp = http(&ts).get(url(&ts, "/")).send().await.unwrap();
    assert_eq!(
        resp.headers()
            .get("timing-allow-origin")
            .expect("伪装响应必须带 Timing-Allow-Origin"),
        "*"
    );

    // 数据面响应同样要有
    let (tu, _, _, sid_b64) = make_msg1(&ts, now_ms());
    let resp2 = http(&ts)
        .post(url(&ts, &format!("/api/sync?n=0&sid={sid_b64}")))
        .body(tu)
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status(), 200, "前提：这必须是一条数据面响应");
    assert_eq!(
        resp2
            .headers()
            .get("timing-allow-origin")
            .expect("数据面响应必须带 Timing-Allow-Origin"),
        "*"
    );
}

/// `ma` 必须可配，且默认是 86400。
///
/// 这是降级保护的一个旋钮：客户端按 `ma` 记住 h3，期间即使 h3 变慢也不会
/// 自己回到 TCP（WebKit 只在握手失败时回落）。调小能缩短劣化窗口，代价是
/// 偏离真实世界的普遍取值。
#[tokio::test]
async fn alt_svc_ma_is_configurable_and_defaults_to_a_day() {
    assert_eq!(wsieve_server::ALT_SVC_MA_DEFAULT, 86_400);

    let ts = start_server_with_disguise(
        SEEN_CACHE_CAPACITY,
        KeepaliveRange::default(),
        DisguiseCfg {
            upstream: None,
            alt_svc_port: Some(443),
            alt_svc_ma: 600,
            ..DisguiseCfg::default()
        },
    )
    .await;
    let resp = http(&ts).get(url(&ts, "/")).send().await.unwrap();
    let v = resp.headers()["alt-svc"].to_str().unwrap().to_string();
    assert!(v.contains("ma=600"), "实际: {v}");

    // 默认构造仍是一天
    let ts2 = start_server_with_disguise(
        SEEN_CACHE_CAPACITY,
        KeepaliveRange::default(),
        DisguiseCfg {
            alt_svc_port: Some(443),
            ..DisguiseCfg::default()
        },
    )
    .await;
    let resp2 = http(&ts2).get(url(&ts2, "/")).send().await.unwrap();
    let v2 = resp2.headers()["alt-svc"].to_str().unwrap().to_string();
    assert!(v2.contains("ma=86400"), "实际: {v2}");
}

/// 4. 同一 msg1 字节重放 → 伪装（404，同随机路径）。
#[tokio::test]
async fn replayed_msg1_goes_to_disguise() {
    let ts = start_server(SEEN_CACHE_CAPACITY, KeepaliveRange::default()).await;
    let c = http(&ts);
    let (tu, _client, _, sid_b64) = make_msg1(&ts, now_ms());
    let p = format!("/api/sync?n=0&sid={sid_b64}");
    let first = c.post(url(&ts, &p)).body(tu.clone()).send().await.unwrap();
    assert_eq!(first.status(), 200);
    let replay = c.post(url(&ts, &p)).body(tu).send().await.unwrap();
    assert_eq!(replay.status(), 404);
    let other = c.get(url(&ts, "/random-path")).send().await.unwrap();
    assert_eq!(
        replay.bytes().await.unwrap(),
        other.bytes().await.unwrap()
    );
}

/// 5. ts 超窗（now - 400s）→ 伪装。
#[tokio::test]
async fn stale_ts_goes_to_disguise() {
    let ts = start_server(SEEN_CACHE_CAPACITY, KeepaliveRange::default()).await;
    let (tu, _, _, sid_b64) = make_msg1(&ts, now_ms() - 400_000);
    let resp = http(&ts)
        .post(url(&ts, &format!("/api/sync?n=0&sid={sid_b64}")))
        .body(tu)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

/// 6. 无会话的 GET /api/events → 伪装。
#[tokio::test]
async fn events_without_session_goes_to_disguise() {
    let ts = start_server(SEEN_CACHE_CAPACITY, KeepaliveRange::default()).await;
    let sid_b64 = URL_SAFE_NO_PAD.encode(Sid::random().0);
    let resp = http(&ts)
        .get(url(&ts, &format!("/api/events?sid={sid_b64}")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let other = http(&ts).get(url(&ts, "/random-path")).send().await.unwrap();
    assert_eq!(
        resp.bytes().await.unwrap(),
        other.bytes().await.unwrap()
    );
}

/// 7. 完整挂载：握手 → GET /api/events → 200 + 三头 + 至少一个可解密 PADDING TU
///    （keepalive 调到 20–80ms，真实 TransportState 双端）。
#[tokio::test]
async fn full_attach_streams_real_padding() {
    let ts = start_server(
        SEEN_CACHE_CAPACITY,
        KeepaliveRange {
            min_ms: 20,
            max_ms: 80,
        },
    )
    .await;
    let (tu, mut client, _, sid_b64) = make_msg1(&ts, now_ms());
    let c = http(&ts);
    let resp = c
        .post(url(&ts, &format!("/api/sync?n=0&sid={sid_b64}")))
        .body(tu)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.bytes().await.unwrap();
    let len = u16::from_be_bytes([body[0], body[1]]) as usize;
    let mut plain = vec![0u8; 65535];
    let n = client
        .read_message(&body[2..2 + len], &mut plain)
        .unwrap();
    let _msg2 = decode_msg2(&plain[..n]).unwrap();
    let client = client.into_transport_mode().unwrap();

    let resp = c
        .get(url(&ts, &format!("/api/events?sid={sid_b64}")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    assert_eq!(resp.headers()["cache-control"], "no-store");
    assert_eq!(resp.headers()["x-accel-buffering"], "no");

    // 读流直到解密出一个 PADDING 帧（Task 15 后 mux 数据帧与保活帧同流，
    // 语义是「keepalive 在产出真实可解密的 PADDING TU」——逐 TU 扫描）。
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut client = client;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let chunk = stream.next().await.unwrap().unwrap();
            buf.extend_from_slice(&chunk);
            while buf.len() >= 2 {
                let len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
                if buf.len() < 2 + len {
                    break;
                }
                let mut plain = vec![0u8; 65535];
                let n = client.read_message(&buf[2..2 + len], &mut plain).unwrap();
                if decode_frame(&plain[..n]).unwrap() == Frame::Padding {
                    return; // 命中保活帧
                }
                buf.drain(..2 + len); // 数据帧（mux 控制/子流），继续扫
            }
        }
    })
    .await
    .expect("10s 内应收到至少一个保活 TU");
}

/// 8. 二次 GET /api/events（已挂载）→ 伪装。
#[tokio::test]
async fn double_attach_goes_to_disguise() {
    let ts = start_server(SEEN_CACHE_CAPACITY, KeepaliveRange::default()).await;
    let (tu, _, _, sid_b64) = make_msg1(&ts, now_ms());
    let c = http(&ts);
    let resp = c
        .post(url(&ts, &format!("/api/sync?n=0&sid={sid_b64}")))
        .body(tu)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let p = format!("/api/events?sid={sid_b64}");
    let first = c.get(url(&ts, &p)).send().await.unwrap();
    assert_eq!(first.status(), 200);
    let second = c.get(url(&ts, &p)).send().await.unwrap();
    assert_eq!(second.status(), 404);
    let other = c.get(url(&ts, "/random-path")).send().await.unwrap();
    assert_eq!(
        second.bytes().await.unwrap(),
        other.bytes().await.unwrap()
    );
}

/// 9. 容量 fail-closed：缓存满（容量 1，已有 1 条）后，**新的合法** msg1
///    （不同 ephemeral、在窗内）也被转伪装——不驱逐、不顶替，窗口过后自然
///    恢复。被顶掉的条目若被驱逐，其 msg1 可在窗内重放（洞）；fail-closed
///    堵死该路径。
#[tokio::test]
async fn full_cache_fails_closed() {
    let ts = start_server(1, KeepaliveRange::default()).await;
    let c = http(&ts);
    let (tu1, _, _, sid1) = make_msg1(&ts, now_ms());
    let resp = c
        .post(url(&ts, &format!("/api/sync?n=0&sid={sid1}")))
        .body(tu1)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // 第二个全新的合法握手：ephemeral 不同 → 不构成重放，但缓存已满。
    let (tu2, _, _, sid2) = make_msg1(&ts, now_ms());
    let resp = c
        .post(url(&ts, &format!("/api/sync?n=0&sid={sid2}")))
        .body(tu2)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "容量满时新握手应 fail-closed");
}
