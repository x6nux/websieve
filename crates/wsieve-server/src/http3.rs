//! HTTP/3 监听面：QUIC endpoint + h3 → axum 适配。
//!
//! 与 TCP 面（[`crate::tls`]）是**叠加关系而非替代**。UDP 443 在跨境 QoS、
//! 企业防火墙、iCloud Private Relay 下都可能不通，客户端会自动回落到 TCP
//! 上的 h2/h1，所以本模块的任何失败都不得影响 TCP 面——`bind` 把错误原样
//! 返回，要不要因此降级是启动编排的决定，不是这里的。
//!
//! # 为什么值得做
//!
//! 2026-09-11 的对照实测（同节点同链路，唯一变量是 ALPN 是否宣告 h2）：
//! h1 的 64MiB 单流中位 11.0 MB/s，h2 是 35.5 MB/s——**3.22 倍**。原因是
//! HTTP/1.1 的队头阻塞叠加长流占用：xhttp 的下行 `GET /api/events` 会永久
//! 占住一条 TCP，几条 lane 就吃掉大半连接池，上行 POST 只能排队。
//!
//! h3 是同一方向的进一步优化：同为单连接多路复用，但 QUIC 的 stream 之间
//! **没有队头阻塞**——一次丢包只影响所属 stream，而 h2 的一次丢包会卡住
//! 该 TCP 上的全部 stream。跨境链路有丢包，这个差别应当是实的。

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use bytes::{Buf, Bytes, BytesMut};
use futures::StreamExt;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tower::ServiceExt;

/// 单个请求允许的上行 body 上限。
///
/// 与 `fallback` 里 `to_bytes(.., 1 << 20)` 的上限对齐：xhttp 的上行 TU 远
/// 小于此，给一个明确上限是为了不让畸形请求把内存吃光。
const MAX_UPLINK: usize = 1 << 20;

/// 绑定 UDP 并 spawn accept 循环，返回**实际**绑定地址（传 `:0` 时测试用）。
///
/// 绑定失败原样返回 `Err`：h3 是叠加能力，降不降级由调用方判断。
pub async fn bind(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    listen: SocketAddr,
    router: Router,
) -> Result<SocketAddr> {
    // QUIC 面**单独**构建 rustls config：ALPN 只有 h3，且 early_data 必须是
    // 0 或 u32::MAX——TCP 面那个 16384 拿过来会让 try_from 直接失败。这正是
    // 两个监听面不能共用一份 config 的原因（见 tls::rustls_config 的注释）。
    let tls = crate::tls::rustls_config(
        certs,
        key,
        crate::tls::alpn_vec(&crate::tls::ALPN_QUIC),
        u32::MAX,
    )?;
    let quic = quinn::crypto::rustls::QuicServerConfig::try_from(Arc::new(tls))
        .context("rustls config 不满足 QUIC 要求（需 TLS1.3，且 early_data 为 0 或 u32::MAX）")?;
    let server_cfg = quinn::ServerConfig::with_crypto(Arc::new(quic));

    let endpoint = quinn::Endpoint::server(server_cfg, listen)
        .with_context(|| format!("QUIC 监听 {listen} 失败"))?;
    let addr = endpoint.local_addr()?;

    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            let router = router.clone();
            tokio::spawn(async move {
                // 握手失败是常态（端口扫描、UDP 源地址伪造、被中途丢包的
                // 客户端），debug 级别足够，不该刷 warn。
                let conn = match incoming.await {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("h3: QUIC 握手失败: {e}");
                        return;
                    }
                };
                if let Err(e) = serve_conn(conn, router).await {
                    eprintln!("h3: 连接结束: {e}");
                }
            });
        }
    });
    Ok(addr)
}

async fn serve_conn(conn: quinn::Connection, router: Router) -> Result<()> {
    let mut h3_conn = h3::server::builder()
        .build(h3_quinn::Connection::new(conn))
        .await?;
    loop {
        match h3_conn.accept().await {
            Ok(Some(resolver)) => {
                let router = router.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve_request(resolver, router).await {
                        eprintln!("h3: 请求处理失败: {e}");
                    }
                });
            }
            // 对端正常关闭连接
            Ok(None) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
}

/// 一个 h3 请求 → axum Router → **流式**写回。
///
/// h3 的 API 是手动 `accept` 取 `(Request, RequestStream)`，与 hyper/axum 的
/// `Service` 模型不同，这个函数就是两种模型的边界。
async fn serve_request(
    resolver: h3::server::RequestResolver<h3_quinn::Connection, Bytes>,
    router: Router,
) -> Result<()> {
    let (req, mut stream) = resolver.resolve_request().await?;
    let (parts, _) = req.into_parts();

    // 先收完上行 body。xhttp 的上行是一次性的 Uint8Array（见 ui/emitter.js
    // 里的 fetch 调用，没有用 duplex streaming），所以收完再处理是安全的；
    // 下行则**必须**流式，见下方。
    let mut body = BytesMut::new();
    while let Some(mut chunk) = stream.recv_data().await? {
        if body.len() + chunk.remaining() > MAX_UPLINK {
            anyhow::bail!("上行 body 超过 {MAX_UPLINK} 字节上限");
        }
        while chunk.has_remaining() {
            let s = chunk.chunk();
            body.extend_from_slice(s);
            let n = s.len();
            chunk.advance(n);
        }
    }

    let resp = router
        .oneshot(axum::http::Request::from_parts(
            parts,
            axum::body::Body::from(body.freeze()),
        ))
        .await
        .map_err(|e| anyhow::anyhow!("router 失败: {e}"))?;
    let (parts, body) = resp.into_parts();

    // 先发响应头，再**逐块**发 body。逐块是硬要求：xhttp 的下行是长流
    // （GET /api/events 会一直开着），若在这里把 body 收集完再发，下行就从
    // 流式退化成一次性返回——条带、保活、低延迟会一起失效，而且不会报错，
    // 只会表现为"慢"。tests/h3_roundtrip.rs 有一条测试专门钉住这点。
    stream
        .send_response(axum::http::Response::from_parts(parts, ()))
        .await?;
    let mut data = body.into_data_stream();
    while let Some(chunk) = data.next().await {
        stream
            .send_data(chunk.context("下行 body 读取失败")?)
            .await?;
    }
    stream.finish().await?;
    Ok(())
}
