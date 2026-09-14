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

/// 降级冷却期内必须**拒绝新的 QUIC 连接**。
///
/// 2026-09-11 真机实测暴露的缺陷：只做"不宣告 Alt-Svc + 关掉旧连接"是不够的
/// ——`Alt-Svc: clear` 要搭响应才能送到客户端，而客户端在收到它之前就会用
/// 旧记忆重连 QUIC。当时降级 18 秒后就又冒出新的 h3 连接，冷却形同虚设。
///
/// 在 accept 处拒绝，客户端会立刻拿到失败并回落 TCP——WebKit 的握手失败
/// 回落是可靠的（不可靠的是"慢"不触发回落）。
#[tokio::test]
async fn degraded_gate_refuses_new_quic_connections() {
    let (cert, key) = self_signed();
    let gate = wsieve_server::H3Gate::new();
    let router = axum::Router::new().route("/hello", axum::routing::get(|| async { "hi" }));
    let addr = wsieve_server::http3::bind(
        vec![cert.clone()],
        key,
        "127.0.0.1:0".parse().unwrap(),
        router,
        gate.clone(),
    )
    .await
    .unwrap();

    let ep = client_endpoint(cert);
    // 未降级：连得上
    ep.connect(addr, "localhost").unwrap().await.expect("降级前应当连得上");

    // 降级后：新连接必须被拒
    gate.degrade_for(Duration::from_secs(600));
    let r = ep.connect(addr, "localhost").unwrap().await;
    assert!(
        r.is_err(),
        "冷却期内必须拒绝新 QUIC 连接，否则客户端会用旧记忆绕过降级"
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
        wsieve_server::H3Gate::new(),
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
        wsieve_server::H3Gate::new(),
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

/// 判劣必须看**窗口增量**，不能看累计率。
///
/// 这三组数字是 2026-09-12 真机日志的原样抄录：链路在第一次采样后一个包
/// 都没再丢，但累计率被早期那 2 个丢包钉在 5% 线以上，于是同一个事实被判
/// 了 3 次劣，`BAD_STREAK` 的滞回失效，h3 被自己关进 10 分钟冷却期——
/// 客户端表现为"h3 完全用不了"，而服务端 QUIC 其实完全健康。
#[test]
fn a_single_early_loss_burst_is_not_counted_three_times() {
    use wsieve_server::http3::{judge, LinkQuality, Verdict};
    let q = |lost, sent| LinkQuality {
        rtt_ms: 48,
        lost,
        sent,
        congestion_events: 2,
        black_holes: 0,
    };
    // 真机窗口太小（24/4/3 个包），任何一个都不该构成判劣证据。
    for (prev, now) in [(q(0, 0), q(2, 24)), (q(2, 24), q(2, 28)), (q(2, 28), q(2, 31))] {
        assert_eq!(
            judge(&prev, &now),
            Verdict::NotEnoughSamples,
            "{}/{} 个包的窗口不足以判丢包率",
            now.lost - prev.lost,
            now.sent - prev.sent
        );
    }
}

/// 样本够大时该判的还是要判，否则上面那条修法就成了"永不降级"。
#[test]
fn a_sustained_loss_over_a_large_window_still_degrades() {
    use wsieve_server::http3::{judge, LinkQuality, Verdict};
    let q = |lost, sent| LinkQuality {
        rtt_ms: 48,
        lost,
        sent,
        congestion_events: 9,
        black_holes: 0,
    };
    // 窗口 1000 包丢 200 个：这才是被 QoS 掐 UDP 的样子。
    assert_eq!(judge(&q(0, 0), &q(200, 1000)), Verdict::Bad);
    // 同样大的窗口、正常丢包率 → 明确 Ok，会把滞回计数清零。
    assert_eq!(judge(&q(0, 0), &q(5, 1000)), Verdict::Ok);
}

/// 一条跑满的连接**恢复**之后，累计率还高但增量已正常，必须判 Ok。
///
/// 这正是累计率写法看不见的东西：它会把恢复后的连接一直判劣到死。
#[test]
fn a_recovered_link_reads_as_ok_even_while_its_lifetime_ratio_is_bad() {
    use wsieve_server::http3::{judge, LinkQuality, Verdict};
    let q = |lost, sent| LinkQuality {
        rtt_ms: 48,
        lost,
        sent,
        congestion_events: 30,
        black_holes: 0,
    };
    // 累计 500/1000 = 50%，远超阈值；但这个窗口 0/1000。
    assert_eq!(judge(&q(500, 1000), &q(500, 2000)), Verdict::Ok);
}

/// 黑洞不受最小样本量约束：路径彻底不通时根本发不出 100 个包。
#[test]
fn a_black_hole_degrades_without_waiting_for_the_sample_floor() {
    use wsieve_server::http3::{judge, LinkQuality, Verdict};
    let q = |sent, black_holes| LinkQuality {
        rtt_ms: 48,
        lost: 0,
        sent,
        congestion_events: 0,
        black_holes,
    };
    assert_eq!(judge(&q(10, 0), &q(13, 1)), Verdict::Bad);
}

/// 黑洞一旦出现就**持续**判劣，不是只在出现的那一窗。
///
/// 按增量判会变成边沿触发：黑洞链路本来就发不出几个包，下一窗计数没再涨就
/// 落回 `NotEnoughSamples`，`streak` 永远卡在 1，降级再也不触发——而那正是
/// 降级机制该起作用的场景。丢包率按增量、黑洞按累计，两者性质不同。
#[test]
fn a_detected_black_hole_keeps_reading_as_bad() {
    use wsieve_server::http3::{judge, LinkQuality, Verdict};
    let q = |sent, black_holes| LinkQuality {
        rtt_ms: 48,
        lost: 0,
        sent,
        congestion_events: 0,
        black_holes,
    };
    // 第一窗检测到黑洞
    assert_eq!(judge(&q(10, 0), &q(13, 1)), Verdict::Bad);
    // 之后计数不再增长（包都发不出去了），仍然必须判劣
    assert_eq!(judge(&q(13, 1), &q(15, 1)), Verdict::Bad, "黑洞退化成了边沿触发");
    assert_eq!(judge(&q(15, 1), &q(16, 1)), Verdict::Bad);
}

/// 样本不足的窗口**不推进基线**，否则低速率链路永远攒不够证据。
///
/// 被限速到每 10 秒几十个包的链路，如果每个窗口都重置基线，增量永远够不着
/// `MIN_SAMPLE`，降级判据对它彻底失效。这条模拟那种链路：连续多个小窗口
/// 累积之后，必须能攒够样本并给出真正的判定。
#[test]
fn a_throttled_link_accumulates_across_windows_instead_of_resetting() {
    use wsieve_server::http3::{judge, LinkQuality, Verdict};
    let q = |lost, sent| LinkQuality {
        rtt_ms: 48,
        lost,
        sent,
        congestion_events: 5,
        black_holes: 0,
    };
    // 基线固定在起点，每 10 秒只多发 30 个包、丢 5 个（16.7% 丢包）
    let base = q(0, 0);
    assert_eq!(judge(&base, &q(5, 30)), Verdict::NotEnoughSamples);
    assert_eq!(judge(&base, &q(10, 60)), Verdict::NotEnoughSamples);
    assert_eq!(judge(&base, &q(15, 90)), Verdict::NotEnoughSamples);
    // 第四个窗口跨过 MIN_SAMPLE，证据够了 —— 必须判劣
    assert_eq!(
        judge(&base, &q(20, 120)),
        Verdict::Bad,
        "攒够样本后仍未判劣，被限速的链路就永远降不了级"
    );
}
