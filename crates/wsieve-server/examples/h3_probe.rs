//! 对着一个真实服务端打一次 h3 请求，回答「服务端的 QUIC 到底能不能用」。
//!
//! 存在的理由：客户端不走 h3 时，「服务端坏了」和「WebKit 不肯升级」两种
//! 可能长得一模一样——都是零 UDP 包。用一个自己控制的 QUIC 客户端去打一次，
//! 就能把这两件事分开。2026-09-12 排查 h3 用不了时，正是缺这一刀。
//!
//! 用法: cargo run -p wsieve-server --example h3_probe -- <host> <port> [path]

use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Buf;
use tokio_rustls::rustls;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let host = args.get(1).cloned().unwrap_or_else(|| "localhost".into());
    let port: u16 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(443);
    let path = args.get(3).cloned().unwrap_or_else(|| "/".into());

    let _ = rustls::crypto::ring::default_provider().install_default();
    // 用系统根证书：这里探的是真实服务端，证书链是否可信也是被探的一部分。
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut crypto = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    crypto.alpn_protocols = vec![b"h3".to_vec()];

    let cfg = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?,
    ));
    let mut ep = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    ep.set_default_client_config(cfg);

    let addr = format!("{host}:{port}")
        .to_socket_addrs()?
        .find(|a| a.is_ipv4())
        .ok_or_else(|| anyhow::anyhow!("{host}:{port} 解析不到 IPv4"))?;
    println!("探测 {addr}（SNI={host}，ALPN=h3）…");

    let t0 = Instant::now();
    let conn = match tokio::time::timeout(Duration::from_secs(15), ep.connect(addr, &host)?).await {
        Err(_) => {
            println!("✗ QUIC 握手超时（15s）——服务端没回，或 UDP 被路径丢弃");
            return Ok(());
        }
        Ok(Err(e)) => {
            println!("✗ QUIC 握手失败: {e}");
            return Ok(());
        }
        Ok(Ok(c)) => c,
    };
    println!("✓ QUIC 握手成功，耗时 {:?}", t0.elapsed());
    println!("  RTT={:?}", conn.rtt());

    let (mut driver, mut send) = h3::client::new(h3_quinn::Connection::new(conn)).await?;
    let drive = tokio::spawn(async move { std::future::poll_fn(|cx| driver.poll_close(cx)).await });

    let req = axum::http::Request::get(format!("https://{host}:{port}{path}")).body(())?;
    let mut stream = send.send_request(req).await?;
    stream.finish().await?;
    let resp = stream.recv_response().await?;
    println!("✓ HTTP/3 响应: {}", resp.status());
    let mut n = 0usize;
    while let Some(chunk) = stream.recv_data().await? {
        n += chunk.remaining();
    }
    println!("  body {n} 字节");
    drop(send);
    let _ = tokio::time::timeout(Duration::from_secs(2), drive).await;
    Ok(())
}
