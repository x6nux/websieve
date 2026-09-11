//! HTTP/3 端到端往返 + 下行流式。
//!
//! 流式那条是硬要求而非锦上添花：xhttp 的下行是长流（`GET /api/events`
//! 一直开着），适配层若把 response body 收集完再发，下行就从流式退化成
//! 一次性返回——条带、保活、低延迟会一起失效，而且**不报错**，只表现为
//! "慢"。没有这条测试，这种退化能一路活到线上。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Buf, Bytes};
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// 自签一张 localhost 证书；同时作为客户端的信任根，这样不必动 rustls 的
/// dangerous API 去关掉证书校验（关掉校验的测试会连"证书根本没配对"这类
/// 真实故障一起放过）。
fn self_signed() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let ka = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .unwrap()
        .self_signed(&ka)
        .unwrap();
    let key = PrivateKeyDer::try_from(ka.serialize_der()).unwrap();
    (cert.der().clone(), key)
}

/// 建一个信任该证书的 QUIC 客户端 endpoint。
fn client_endpoint(ca: CertificateDer<'static>) -> quinn::Endpoint {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca).unwrap();
    let mut crypto = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    crypto.alpn_protocols = vec![b"h3".to_vec()];
    let cfg = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto).unwrap(),
    ));
    let mut ep = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    ep.set_default_client_config(cfg);
    ep
}

/// 发一个 h3 GET，把每个 body chunk 的到达时刻一并带回。
async fn h3_get(
    ep: &quinn::Endpoint,
    addr: SocketAddr,
    path: &str,
) -> (u16, Vec<u8>, Vec<Instant>) {
    let conn = ep.connect(addr, "localhost").unwrap().await.unwrap();
    let (mut driver, mut send) = h3::client::new(h3_quinn::Connection::new(conn))
        .await
        .unwrap();
    // driver 必须被轮询，否则连接不前进
    let drive = tokio::spawn(async move { std::future::poll_fn(|cx| driver.poll_close(cx)).await });

    let req = axum::http::Request::get(format!("https://localhost{path}"))
        .body(())
        .unwrap();
    let mut stream = send.send_request(req).await.unwrap();
    stream.finish().await.unwrap();

    let resp = stream.recv_response().await.unwrap();
    let status = resp.status().as_u16();

    let mut body = Vec::new();
    let mut marks = Vec::new();
    while let Some(mut chunk) = stream.recv_data().await.unwrap() {
        marks.push(Instant::now());
        while chunk.has_remaining() {
            let s = chunk.chunk().to_vec();
            body.extend_from_slice(&s);
            chunk.advance(s.len());
        }
    }
    drop(send);
    let _ = tokio::time::timeout(Duration::from_secs(1), drive).await;
    (status, body, marks)
}

/// UDP 端口被占用时必须**明确报错**，不能静默成功。
///
/// 启动编排完全依赖这个 `Err`：拿不到它就会以为 h3 起来了，进而宣告一个
/// 连不上的 Alt-Svc 端点——客户端此后每次连接都要先试 QUIC 超时再回落，
/// 比根本不支持 h3 还糟。
#[tokio::test]
async fn bind_fails_loudly_when_udp_port_is_taken() {
    let squatter = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let taken = squatter.local_addr().unwrap();
    let (cert, key) = self_signed();
    let err = wsieve_server::http3::bind_endpoint(vec![cert], key, taken)
        .expect_err("端口已被占用，绑定必须失败");
    assert!(
        format!("{err:#}").contains("QUIC 监听"),
        "错误信息要指明是 QUIC 监听失败，实际: {err:#}"
    );
}

#[tokio::test]
async fn h3_round_trip_over_quic() {
    let (cert, key) = self_signed();
    let router = axum::Router::new().route(
        "/hello",
        axum::routing::get(|| async { "hello-h3" }),
    );
    let addr = wsieve_server::http3::bind(
        vec![cert.clone()],
        key,
        "127.0.0.1:0".parse().unwrap(),
        router,
    )
    .await
    .unwrap();

    let ep = client_endpoint(cert);
    let (status, body, _) = h3_get(&ep, addr, "/hello").await;
    assert_eq!(status, 200);
    assert_eq!(&body[..], b"hello-h3");
}

#[tokio::test]
async fn downstream_is_streamed_not_buffered() {
    // 造一个"先发一块、隔 300ms、再发一块"的 body。若适配层缓冲了整个
    // body，两块会在同一时刻到达，间隔趋近 0——这正是要钉死的退化。
    let (cert, key) = self_signed();
    let router = axum::Router::new().route(
        "/slow",
        axum::routing::get(|| async {
            let s = futures::stream::unfold(0u8, |i| async move {
                if i >= 2 {
                    return None;
                }
                if i > 0 {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                Some((Ok::<_, std::io::Error>(Bytes::from_static(b"chunk")), i + 1))
            });
            axum::body::Body::from_stream(s)
        }),
    );
    let addr = wsieve_server::http3::bind(
        vec![cert.clone()],
        key,
        "127.0.0.1:0".parse().unwrap(),
        router,
    )
    .await
    .unwrap();

    let ep = client_endpoint(cert);
    let (status, body, marks) = h3_get(&ep, addr, "/slow").await;
    assert_eq!(status, 200);
    assert_eq!(&body[..], b"chunkchunk");
    assert!(
        marks.len() >= 2,
        "应当分多次收到 body，实收 {} 次——适配层把下行缓冲成了一次性返回",
        marks.len()
    );
    let span = *marks.last().unwrap() - marks[0];
    assert!(
        span >= Duration::from_millis(150),
        "首末块间隔仅 {span:?}，说明下行被缓冲后一次性发出，流式已退化"
    );
}
