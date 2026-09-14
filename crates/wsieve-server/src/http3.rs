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
/// 便利入口，供测试与不需要拆两步的调用方使用；启动编排走
/// [`bind_endpoint`] + [`serve`]，理由见 `bind_endpoint` 的注释。
pub async fn bind(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    listen: SocketAddr,
    router: Router,
    gate: crate::H3Gate,
) -> Result<SocketAddr> {
    let ep = bind_endpoint(certs, key, listen)?;
    let addr = ep.local_addr()?;
    serve(ep, router, gate);
    Ok(addr)
}

/// 只绑定 UDP，不开始服务。
///
/// 之所以要跟 [`serve`] 拆开：`Alt-Svc` 要宣告的端口取决于这里绑没绑成功，
/// 而 `AppState` 构造时就需要那个端口，`router()` 又出自 `AppState`——
/// 「端口 → AppState → router → 服务」是一条链，绑定必须先于 AppState。
/// 合成一步就成了 router 与端口互相等待的死结。
///
/// 绑定失败原样返回 `Err`：h3 是叠加能力，降不降级由调用方判断。
pub fn bind_endpoint(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    listen: SocketAddr,
) -> Result<quinn::Endpoint> {
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
    let mut server_cfg = quinn::ServerConfig::with_crypto(Arc::new(quic));

    // 拥塞控制换成 BBR。quinn 默认是 Cubic，而内核 TCP 面跑的是 BBR——两边
    // 不是一个算法，在有丢包的链路上会拉开数量级的差距：
    //
    //   2026-09-11 跨境实测，同一条链路同一时段
    //     TCP(BBR)   重传 8.1%，8MiB 下行 3.5 MiB/s
    //     QUIC(Cubic) 丢包 2.7%，8MiB 下行 0.4 MiB/s
    //
    // 丢得更少却慢一个数量级，因为 Cubic 每个丢包事件砍半窗口，按 Mathis
    // 上限 MSS/(RTT·√p) = 1350/(0.04·√0.027) ≈ 0.2 MiB/s，实测正好落在这个
    // 量级；BBR 不看丢包，按实测带宽发。这一行之前，h3 在任何有损链路上都
    // 只会比 TCP 慢，看起来像"QUIC 不行"，其实是默认值不匹配。
    let mut transport = quinn::TransportConfig::default();
    transport.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    server_cfg.transport_config(Arc::new(transport));

    quinn::Endpoint::server(server_cfg, listen)
        .with_context(|| format!("QUIC 监听 {listen} 失败"))
}

/// 在已绑定的 endpoint 上开始接受 h3 连接。
pub fn serve(endpoint: quinn::Endpoint, router: Router, gate: crate::H3Gate) {
    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            // 冷却期内**拒绝新的 QUIC 连接**，不能只靠"不宣告 + 关旧连接"。
            //
            // 2026-09-11 实测打脸：降级 18 秒后又冒出新的 h3 连接。原因是
            // `Alt-Svc: clear` 要搭响应才能送到客户端，而客户端在收到它之前
            // 就用旧记忆重连了 QUIC——服务端照单全收，冷却形同虚设。
            //
            // 在这里拒绝，客户端会立刻拿到连接失败并回落 TCP（WebKit 的握手
            // 失败回落是可靠的，慢才不可靠）。这比等 clear 送达确定得多。
            if gate.is_degraded() {
                incoming.refuse();
                continue;
            }
            let router = router.clone();
            let gate = gate.clone();
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
                if let Err(e) = serve_conn(conn, router, gate).await {
                    eprintln!("h3: 连接结束: {e}");
                }
            });
        }
    });
}

/// QUIC 质量采样间隔。
const SAMPLE_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

/// 一次采样的质量快照。
#[derive(Debug, Clone, Copy)]
pub struct LinkQuality {
    pub rtt_ms: u64,
    pub lost: u64,
    pub sent: u64,
    pub congestion_events: u64,
    pub black_holes: u64,
}

fn sample(conn: &quinn::Connection) -> LinkQuality {
    let p = conn.stats().path;
    LinkQuality {
        rtt_ms: p.rtt.as_millis() as u64,
        lost: p.lost_packets,
        sent: p.sent_packets,
        congestion_events: p.congestion_events,
        black_holes: p.black_holes_detected,
    }
}

/// 判为劣质的丢包率阈值。
///
/// 5% 是个保守取值：正常跨境链路的丢包通常在 1% 以下，而被 QoS 限速的 UDP
/// 往往远高于 5%。定得太低会把正常抖动误判成劣化，太高则失去意义。
const LOSS_BAD: f64 = 0.05;

/// 连续多少次采样判劣才降级（滞回）。
///
/// 没有这个计数，一次瞬时丢包就会触发降级，而降级要付一次断连代价——
/// 结果是在 h3/h2 之间反复横跳，比不降级更糟。
const BAD_STREAK: u32 = 3;

/// 一个采样窗口至少要有这么多发包，才谈得上丢包率。
///
/// 2026-09-12 真机翻车：连接刚建起来丢了 2 个包，窗口只有 24 个包，
/// 8.33% 直接过阈值，10 分钟冷却期就此关死 h3。跨境链路握手阶段丢一两个
/// 包是常态，样本小到 40 以下时 `lost>=2` 必然过 5% 线——那不是测量，是噪声。
///
/// 100 是按用途取的：这个机制要抓的是"运营商 QoS 掐 UDP"，那种劣化是持续
/// 20%+ 的，100 个包足够看出来；而正常传输 10 秒轻松几千包，够不着门槛的
/// 只有空闲连接，空闲连接本来就没有可谈的质量。
const MIN_SAMPLE: u64 = 100;

/// 一个采样窗口的判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 窗口内确实劣化。
    Bad,
    /// 窗口内正常。
    Ok,
    /// 发包太少，这个窗口不构成证据——既不判劣也不清零滞回计数。
    NotEnoughSamples,
}

/// 判定**两次采样之间**的链路质量。
///
/// 必须比较增量，不能直接看累计率。quinn 的 `lost_packets`/`sent_packets`
/// 都是连接生命周期的累计值，拿累计率判劣会让早期的一次丢包被反复计入：
/// 2026-09-12 真机上就是 `2/24 → 2/28 → 2/31`，实际第一次采样之后一个包
/// 都没再丢，却被判了 3 次劣，`BAD_STREAK` 的滞回意图（要 3 次**独立**判劣）
/// 退化成"同一个事实数 3 遍"，直接触发降级。
pub fn judge(prev: &LinkQuality, now: &LinkQuality) -> Verdict {
    // 黑洞按**累计**判，与丢包率相反——这不是疏忽，两者的性质不同。
    //
    // 丢包率必须看增量，否则早期的一次丢包会被反复计入（见函数文档）。
    // 但 `black_holes_detected` 只在 quinn 确认"发出去的包整批消失"时才加，
    // 是路径已经不通的**确定性**信号，不是一个会自己好转的比率。按增量判
    // 就变成了边沿触发：只有出现黑洞的那一窗判劣，下一窗计数没再涨就落回
    // `NotEnoughSamples`（黑洞链路本来就发不出几个包），`streak` 永远卡在 1，
    // 降级再也不会触发——而这正是降级机制存在的那个场景。
    if now.black_holes > 0 {
        return Verdict::Bad;
    }
    let sent = now.sent.saturating_sub(prev.sent);
    if sent < MIN_SAMPLE {
        return Verdict::NotEnoughSamples;
    }
    let lost = now.lost.saturating_sub(prev.lost);
    if lost as f64 / sent as f64 >= LOSS_BAD {
        Verdict::Bad
    } else {
        Verdict::Ok
    }
}

/// 降级后的冷却时长：这段时间内不再宣告 h3。
///
/// 劣化多半是暂时的（运营商 QoS 有时段性），到点自动恢复，不需要人工干预。
const COOLDOWN: std::time::Duration = std::time::Duration::from_secs(600);

/// 周期采样 QUIC 链路质量，劣化则触发降级。
///
/// 服务端自己就能测，**不需要客户端配合**——这是降级判据的来源。客户端侧
/// 那条路（`nextHopProtocol`）只用于让人看见现状，判定不依赖它。
///
/// 判劣后做两件事，缺一不可（见 `H3Gate` 的文档）：
///   1. 置位闸门 → 后续响应改发 `Alt-Svc: clear`，清掉客户端的 h3 记忆
///   2. 主动关闭本连接 → 逼客户端重连；记忆已清，重连自然落到 TCP
/// 顺序不能反：先断后清的话，客户端重连时记忆还在，又会选 h3。
fn spawn_quality_probe(conn: quinn::Connection, gate: crate::H3Gate) {
    tokio::spawn(async move {
        let mut streak = 0u32;
        let mut prev = sample(&conn);
        loop {
            tokio::time::sleep(SAMPLE_EVERY).await;
            if conn.close_reason().is_some() {
                return;
            }
            let q = sample(&conn);
            let verdict = judge(&prev, &q);
            let sent = q.sent.saturating_sub(prev.sent);
            let lost = q.lost.saturating_sub(prev.lost);
            // 样本不足不打日志也不动 streak：空闲连接每 10 秒刷一行
            // "没样本"既没信息量，又会把真实的劣化序列冲散。
            //
            // **基线也不能推进**。这一句曾经在 `continue` 之前，于是每个窗口
            // 都把 `prev` 重置一次：一条被限速到每 10 秒几十个包的链路，
            // 永远攒不到 `MIN_SAMPLE`，降级判据对它彻底失效——而那恰恰是最
            // 需要降级的链路。留着 `prev` 不动，窗口就会一直变宽直到攒够证据。
            if verdict == Verdict::NotEnoughSamples {
                continue;
            }
            prev = q;
            eprintln!(
                "h3: 链路质量 rtt={}ms 窗口丢包={:.2}% ({}/{}) 累计拥塞事件={} 黑洞={}{}",
                q.rtt_ms,
                if sent == 0 {
                    0.0
                } else {
                    lost as f64 / sent as f64 * 100.0
                },
                lost,
                sent,
                q.congestion_events,
                q.black_holes,
                if verdict == Verdict::Bad {
                    "  [判劣]"
                } else {
                    ""
                }
            );
            if verdict == Verdict::Ok {
                streak = 0;
                continue;
            }
            streak += 1;
            if streak < BAD_STREAK {
                continue;
            }
            eprintln!(
                "h3: 连续 {BAD_STREAK} 次判劣，降级到 h2 并冷却 {}s",
                COOLDOWN.as_secs()
            );
            // 1) 先清记忆
            gate.degrade_for(COOLDOWN);
            // 2) 给 clear 一点发出去的机会，再断连。这一小段等待不是凑数：
            //    闸门置位后，要等客户端发来下一个请求、由响应把 clear 带回去，
            //    降级才算真正生效。立刻断连会让 clear 根本没机会发出。
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            conn.close(0u32.into(), b"h3 link degraded");
            return;
        }
    });
}

async fn serve_conn(conn: quinn::Connection, router: Router, gate: crate::H3Gate) -> Result<()> {
    spawn_quality_probe(conn.clone(), gate);
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
