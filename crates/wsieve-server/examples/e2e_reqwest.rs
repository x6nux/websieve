//! Task 18 E2E 客户端（webview 之外的全栈）：连接真实 wsieve-server，
//! 完成真实 Noise 握手 + mux，然后在 127.0.0.1:11080 提供 SOCKS5。
//! 唯一非真实件：fetch 由 ReqwestTransport 顶替（WebView 边界之内的一切
//! ——emitter/IPC——需要 GUI，见 scripts/e2e.sh 的 --with-app 阶段）。
//!
//! 环境变量：
//!   WSIEVE_E2E_SERVER       server base URL（如 http://127.0.0.1:18443）
//!   WSIEVE_E2E_SERVER_PUB   服务端静态公钥（64 hex）
//!   WSIEVE_E2E_CLIENT_PRIV  客户端静态私钥（64 hex，须在白名单内）
//!   WSIEVE_E2E_SOCKS        SOCKS5 监听地址（默认 127.0.0.1:11080）
//!   WSIEVE_EXTRA_SESSIONS   额外 XHTTP 会话数（多 TCP 条带，默认 0）
//!
//! 运行：`cargo run -p wsieve-server --example e2e_reqwest`（由 scripts/e2e.sh 驱动）

use std::sync::Arc;

use wsieve_transport::ReqwestTransport;
use wsieve_xhttp::client::{random_group_id, UpstreamCfg, XhttpConn};

fn hex32(s: &str) -> anyhow::Result<[u8; 32]> {
    let s = s.trim();
    let mut out = [0u8; 32];
    if s.len() != 64 {
        anyhow::bail!("expected 64 hex chars, got {}", s.len());
    }
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)?;
    }
    Ok(out)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let base = std::env::var("WSIEVE_E2E_SERVER").expect("WSIEVE_E2E_SERVER");
    let server_pub = hex32(&std::env::var("WSIEVE_E2E_SERVER_PUB").expect("WSIEVE_E2E_SERVER_PUB"))?;
    let client_priv =
        hex32(&std::env::var("WSIEVE_E2E_CLIENT_PRIV").expect("WSIEVE_E2E_CLIENT_PRIV"))?;
    let socks: String = std::env::var("WSIEVE_E2E_SOCKS")
        .unwrap_or_else(|_| "127.0.0.1:11080".to_string());

    let transport = Arc::new(ReqwestTransport::new(base.clone())?);
    // 会话组 id：本客户端开的全部会话（主 + 全部额外）共用同一值，服务端
    // 据此把它们归为一组并跨会话铺下行 lane。整个进程只生成一次。
    let group_id = random_group_id();
    let mux_prefs = std::env::var("WSIEVE_E2E_MUX")
        .ok()
        .and_then(|m| wsieve_proto::hello::MuxId::from_u8(match m.as_str() {
            _ => 0x01, // 只剩 wsmux 一种
        }))
        .map(|m| vec![m])
        .unwrap_or_else(|| vec![wsieve_proto::hello::MuxId::Wsmux]);
    let (conn, neg) = XhttpConn::connect(
        transport,
        &UpstreamCfg {
            server_pub,
            client_priv,
            mux_prefs: mux_prefs.clone(),
            group_id,
            ip_strategy: wsieve_proto::hello::IpStrategy::Auto,
        },
    )
    .await?;
    println!("handshake ok against {base}, mux = {:?}", neg.mux_id);

    let io: wsieve_mux::MuxStream = Box::new(conn);
    let mux: Arc<dyn wsieve_mux::Mux> = Arc::from(wsieve_mux::mux_factory(neg.mux_id, io).await?);

    // v2 条带化：StripeDialer 负责 conn_id 分配、OPEN+TargetAddr 首 lane、
    // 以及后台 accept 归并服务端新开的 DOWN lane。
    // 多 TCP 条带（aria2 效应）：extra_sessions > 0 时急切建立额外 XHTTP
    // 会话（各自独立 TCP），lane 跨会话轮转 → 每 lane 独立拥塞窗口。
    let cfg = wsieve_mux::stripe_runtime::StripeCfg::with_env();
    let extra = cfg.extra_sessions;
    let dialer = wsieve_mux::stripe_runtime::StripeDialer::new(mux, cfg);
    for i in 0..extra {
        let t = Arc::new(ReqwestTransport::new(base.clone())?);
        let (conn, neg2) = XhttpConn::connect(
            t,
            &UpstreamCfg {
                server_pub,
                client_priv,
                mux_prefs: mux_prefs.clone(),
                group_id,
                ip_strategy: wsieve_proto::hello::IpStrategy::Auto,
            },
        )
        .await?;
        if neg2.mux_id != neg.mux_id {
            anyhow::bail!("extra session {i} mux mismatch: {:?} vs {:?}", neg2.mux_id, neg.mux_id);
        }
        let io: wsieve_mux::MuxStream = Box::new(conn);
        let m: Arc<dyn wsieve_mux::Mux> =
            Arc::from(wsieve_mux::mux_factory(neg.mux_id, io).await?);
        dialer.attach_session(m);
    }
    if extra > 0 {
        println!("multi-session striping: {} sessions", dialer.session_count());
    }

    let listener = tokio::net::TcpListener::bind(&socks).await?;
    println!("SOCKS5 listening on {socks}");
    // 与 src-tauri/src/proxy.rs 相同的 handler 路径（v2：StripeConn 出口）。
    let _ = wsieve_socks5::serve(listener, move |target| {
        let dialer = dialer.clone();
        Box::pin(async move {
            let stream = dialer
                .connect(&target)
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let (local, mut remote_end) = tokio::io::duplex(64 * 1024);
            tokio::spawn(async move {
                let mut stream = stream;
                let _ = tokio::io::copy_bidirectional(&mut remote_end, &mut stream).await;
            });
            Ok(local)
        })
    })
    .await?;
    Ok(())
}
