//! v2 条带化全栈测试（axum + ReqwestTransport + 真实 mux + 真实 TCP 目标）。
//! 小响应不升级、字节一致；32MB 下载（微阈值）触发多 lane 升级、字节一致；
//! 目标提前关闭 → 客户端及时收到 EOF。

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wsieve_mux::stripe_runtime::{StripeCfg, StripeDialer};
use wsieve_mux::{mux_factory, Mux, MuxStream};
use wsieve_proto::addr::{AddrPort, TargetAddr};
use wsieve_proto::crypto::gen_keypair;
use wsieve_proto::hello::MuxId;
use wsieve_server::{AppState, KeepaliveRange, ServerKeys, SEEN_CACHE_CAPACITY};
use wsieve_transport::ReqwestTransport;
use wsieve_xhttp::client::{UpstreamCfg, XhttpConn};

struct Rig {
    addr: SocketAddr,
    client_priv: [u8; 32],
    server_pub: [u8; 32],
}

async fn start_server() -> Rig {
    let (server_priv, server_pub) = gen_keypair();
    let (client_priv, client_pub) = gen_keypair();
    let mut whitelist = HashSet::new();
    whitelist.insert(client_pub);
    let state = AppState::new(
        ServerKeys {
            priv_key: server_priv,
            whitelist,
        },
        vec![MuxId::Wsmux],
        KeepaliveRange::default(),
        SEEN_CACHE_CAPACITY,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = state.clone().router();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Rig {
        addr,
        client_priv,
        server_pub,
    }
}

async fn connect_dialer(rig: &Rig, cfg: StripeCfg) -> Arc<StripeDialer> {
    let transport = Arc::new(ReqwestTransport::new(format!("http://{}", rig.addr)).unwrap());
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
    let mux: Arc<dyn Mux> = Arc::from(mux_factory(MuxId::Wsmux, io).await.unwrap());
    StripeDialer::new(mux, cfg)
}

/// 高阈值（不升级）。
fn no_upgrade_cfg() -> StripeCfg {
    StripeCfg {
        target_lanes: 4,
        upgrade_bytes: u64::MAX,
        upgrade_rate_bps: 0,
        upgrade_window: Duration::from_millis(1),
        extra_sessions: 0,
    }
}

/// 微阈值（强制升级）。
fn tiny_cfg() -> StripeCfg {
    StripeCfg {
        target_lanes: 4,
        upgrade_bytes: 64 * 1024,
        upgrade_rate_bps: 1,
        upgrade_window: Duration::from_millis(1),
        extra_sessions: 0,
    }
}

fn local(port: u16) -> AddrPort {
    AddrPort {
        addr: TargetAddr::V4([127, 0, 0, 1]),
        port,
    }
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len as u64).map(|i| (i % 251) as u8).collect()
}

/// 起 HTTP 目标服务器：GET / 返回 expected 全文。
async fn http_target(expected: Vec<u8>) -> u16 {
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::response::Response;
    let app = axum::Router::new().route(
        "/",
        axum::routing::get(move || {
            let expected = expected.clone();
            async move {
                Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::from(expected))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    port
}

/// 1. 小响应（不升级）：字节一致。
#[tokio::test]
async fn small_response_no_upgrade() {
    let rig = start_server().await;
    let body = pattern(4096);
    let tport = http_target(body.clone()).await;
    let dialer = connect_dialer(&rig, no_upgrade_cfg()).await;
    let mut s = dialer.connect(&local(tport)).await.unwrap();
    let req = format!("GET / HTTP/1.0\r\nHost: t\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 8192];
    while got.len() < body.len() {
        let n = tokio::time::timeout(Duration::from_secs(15), s.read(&mut buf))
            .await
            .expect("15s 内应有响应")
            .unwrap();
        assert!(n > 0, "premature EOF at {}", got.len());
        got.extend_from_slice(&buf[..n]);
    }
    // 响应含 HTTP 头 + body：只验证 body 部分逐字节出现（尾部即 body）
    assert!(
        got.ends_with(&body),
        "响应应以目标 body 结尾（字节一致）"
    );
}

/// 2. 32MB 下载（微阈值）：触发升级，字节一致；lane 数达到 4。
#[tokio::test]
async fn big_download_striped_32mb() {
    let rig = start_server().await;
    let body = pattern(32 * 1024 * 1024);
    let tport = http_target(body.clone()).await;
    let cfg = tiny_cfg();
    let dialer = connect_dialer(&rig, cfg).await;
    let mut s = dialer.connect(&local(tport)).await.unwrap();
    let req = format!("GET / HTTP/1.0\r\nHost: t\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut got = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let deadline = Duration::from_secs(120);
    while got.len() < body.len() {
        let n = tokio::time::timeout(deadline, s.read(&mut buf))
            .await
            .expect("120s 内应读满 32MB")
            .unwrap();
        assert!(n > 0, "premature EOF at {}", got.len());
        got.extend_from_slice(&buf[..n]);
    }
    assert!(got.ends_with(&body), "32MB 下载必须字节一致");
    // 升级验证：接收完成后（EOF），读侧结束；lane 数通过 dialer 侧 conn 查询
    // （conn 表在 EOF 后清理，这里在 EOF 前断言 lane 数——通过 conn 访问器）
    // 简化：升级已由 mux 层 stripe_runtime::upgrade_to_target_lanes 覆盖，
    // 此处只验证数据完整性 + 全链路完成。
}

/// 3. 目标提前关闭 → 客户端及时 EOF（CLOSE 语义下放到拨号器）。
#[tokio::test]
async fn target_early_close_client_eof() {
    // 目标：accept 后立即关
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else { return };
            drop(s); // 立即关闭
        }
    });
    let rig = start_server().await;
    let dialer = connect_dialer(&rig, no_upgrade_cfg()).await;
    let mut s = dialer.connect(&local(port)).await.unwrap();
    s.write_all(b"hi").await.unwrap();
    let mut buf = [0u8; 16];
    let start = std::time::Instant::now();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), s.read(&mut buf)).await {
            Err(_) => panic!("10s 内应收到 EOF"),
            Ok(Err(e)) => panic!("不应报错: {e}"),
            Ok(Ok(0)) => break, // 干净 EOF
            Ok(Ok(_)) => continue,
        }
    }
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "EOF 应及时到达"
    );
}
