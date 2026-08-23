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
use wsieve_proto::hello::{decode_msg2, encode_msg1, MuxId};
use wsieve_proto::tu::{decode_frame, Frame};
use wsieve_server::{AppState, KeepaliveRange, ServerKeys, SEEN_CACHE_CAPACITY};
use wsieve_xhttp::server::Sid;

struct TestServer {
    addr: SocketAddr,
    client_priv: [u8; 32],
    server_pub: [u8; 32],
}

/// 起一个真实 TCP 上的 axum 服务（随机端口），keepalive 调小使流测试可等。
async fn start_server(seen_capacity: usize, keepalive: KeepaliveRange) -> TestServer {
    let (server_priv, server_pub) = gen_keypair();
    let (client_priv, client_pub) = gen_keypair();
    let mut whitelist = HashSet::new();
    whitelist.insert(client_pub);
    let state = AppState::new(
        ServerKeys {
            priv_key: server_priv,
            whitelist,
        },
        vec![MuxId::Yamux, MuxId::Smux],
        keepalive,
        seen_capacity,
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
    let hello = encode_msg1(ts_ms, &[MuxId::Smux, MuxId::Yamux]);
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
    assert_eq!(msg2.chosen_mux_id, MuxId::Smux);
    assert!(!msg2.fallback);
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

    // 读流直到攒够一个完整 TU，解密须得 PADDING 帧。
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let chunk = stream.next().await.unwrap().unwrap();
            buf.extend_from_slice(&chunk);
            if buf.len() >= 2 {
                let len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
                if buf.len() >= 2 + len {
                    break;
                }
            }
        }
    })
    .await
    .expect("10s 内应收到至少一个保活 TU");
    let len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
    let mut client = client;
    let mut plain = vec![0u8; 65535];
    let n = client.read_message(&buf[2..2 + len], &mut plain).unwrap();
    assert_eq!(decode_frame(&plain[..n]).unwrap(), Frame::Padding);
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
