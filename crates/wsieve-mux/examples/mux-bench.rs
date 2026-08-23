//! 一次性 mux 基准。spec §7.6。用后即弃，结果回填 spec §7.7。
//!
//! RTT/丢包在内存管道注入（不碰系统网络）：两条 tokio duplex 经中继任务
//! 串接，中继的每个写操作 sleep(RTT/2) 模拟单向传播延迟；按概率（固定
//! 种子 ChaCha12，可复现）把该次写额外延迟 2×RTT，模拟 TCP 丢包重传——
//! 因为底层是可靠字节流，丢包的正确建模就是「重传带来的额外时延」而非
//! 真丢字节。被测的 mux 实现、帧处理、流控全部真实。
//!
//! 场景 × 实现矩阵见 `SCENARIOS`。每格指标：
//!   - 首字节延迟 P50/P99（open 完成到 echo 首字节，跨流统计）
//!   - 总吞吐（双向聚合字节 / makespan）
//!   - 公平性：各流完成时间极差（max−min），小 = 快慢流不被互相拖死
//!
//! paritytech `yamux`（第 6 项 A/B）：未加入。它暴露 futures 0.3 的
//! `Yamux` wrapper，与本项目 tokio IO + object-safe `Mux` trait 对接需要
//! 一套独立的 futures↔tokio 双向桥接适配器，远超 30 行预算；且其线格式
//! 与 tokio-yamux 同为标准 yamux，不构成「选哪个协议/实现」的额外信息。
//! 跳过，见 spec §7.7 说明。
//!
//! 实现特定 harness 行为（源于实测的 crate 缺陷，详见 spec §7.7）：
//!   - picomux 0.2.1：单流 FIN 会终结整个会话 → 写满后不发 shutdown，
//!     保持写半存活至读侧收满再统一 abort；
//!   - muxado 0.5.4：≥8 流并发上传即停滞（sentinel 懒 SYN + 串行 accept +
//!     流控依赖），全场景超时记失败——这本身就是基准结论的一部分；
//!   - h2mux：h2 默认连接级流控窗口 64KB 全流共享，高并发停滞。

use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha12Rng;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::{mpsc, Mutex};
use wsieve_mux::{mux_factory, mux_server_factory, MuxId};


/// 每条流上传并 echo 回验的数据量。
fn payload_size() -> usize {
    std::env::var("BENCH_PAYLOAD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256 * 1024)
}
const PAYLOAD: usize = 256 * 1024;
/// 中继/写侧分片。
const CHUNK: usize = 16 * 1024;
/// 底层 duplex 缓冲。
const LINK_BUF: usize = 256 * 1024;

struct Scenario {
    name: &'static str,
    rtt: Duration,
    loss: f64,
    streams: usize,
}

const SCENARIOS: [Scenario; 4] = [
    Scenario { name: "local", rtt: Duration::from_millis(1), loss: 0.00, streams: 8 },
    Scenario { name: "cross", rtt: Duration::from_millis(80), loss: 0.00, streams: 32 },
    Scenario { name: "weak", rtt: Duration::from_millis(250), loss: 0.01, streams: 32 },
    Scenario { name: "burst", rtt: Duration::from_millis(80), loss: 0.00, streams: 128 },
];

const IMPLS: [(&str, MuxId); 5] = [
    ("tokio-yamux", MuxId::Yamux),
    ("smux", MuxId::Smux),
    ("muxado", MuxId::Muxado),
    ("picomux", MuxId::Picomux),
    ("h2mux", MuxId::H2mux),
];

// ---------- 延迟/丢包注入链路 ----------

enum PumpMsg {
    Data(Vec<u8>),
    Fin,
}

/// 单向延迟泵：保持顺序（TCP 语义），每包 RTT/2 传播延迟，
/// 概率 loss 触发 +2×RTT 重传延迟。
async fn pump_direction(
    mut src: tokio::io::ReadHalf<DuplexStream>,
    mut dst: tokio::io::WriteHalf<DuplexStream>,
    rtt: Duration,
    loss: f64,
    rng: Arc<Mutex<ChaCha12Rng>>,
) {
    let (tx, mut rx) = mpsc::channel::<PumpMsg>(64);
    // 写入半：注入延迟
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                PumpMsg::Data(buf) => {
                    if rtt.is_zero() {
                        // debug: zero-delay passthrough
                    } else {
                        tokio::time::sleep(rtt / 2).await;
                    }
                    if loss > 0.0 && rng.lock().await.random::<f64>() < loss {
                        tokio::time::sleep(rtt * 2).await; // 重传
                    }
                    if dst.write_all(&buf).await.is_err() {
                        break;
                    }
                }
                PumpMsg::Fin => {
                    let _ = dst.shutdown().await;
                    break;
                }
            }
        }
    });
    // 读取半：透传到泵
    let mut buf = vec![0u8; CHUNK];
    loop {
        match src.read(&mut buf).await {
            Ok(0) => {
                let _ = tx.send(PumpMsg::Fin).await;
                break;
            }
            Ok(n) => {
                if tx.send(PumpMsg::Data(buf[..n].to_vec())).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// 建一条注入 RTT/丢包的双向内存链路，返回（客户端 IO，服务端 IO）。
fn delayed_link(rtt: Duration, loss: f64, seed: u64) -> (DuplexStream, DuplexStream) {
    let (client_io, client_relay) = tokio::io::duplex(LINK_BUF);
    let (server_relay, server_io) = tokio::io::duplex(LINK_BUF);
    let rng_c2s = Arc::new(Mutex::new(ChaCha12Rng::seed_from_u64(seed)));
    let rng_s2c = Arc::new(Mutex::new(ChaCha12Rng::seed_from_u64(seed ^ 0x9e37)));
    let (cr_read, cr_write) = tokio::io::split(client_relay);
    let (sr_read, sr_write) = tokio::io::split(server_relay);
    tokio::spawn(pump_direction(cr_read, sr_write, rtt, loss, rng_c2s));
    tokio::spawn(pump_direction(sr_read, cr_write, rtt, loss, rng_s2c));
    (client_io, server_io)
}

// ---------- 工作负载 ----------

/// 每字节的确定性图案：stream_id ^ chunk序号 ^ 块内偏移（读侧按全局偏移重算，兼容 mux 合并交付）。
fn pattern_byte(stream_id: u8, abs: usize) -> u8 {
    stream_id
        ^ ((abs / CHUNK) as u8).wrapping_mul(31)
        ^ ((abs % CHUNK) as u8).wrapping_mul(7)
}

fn fill_pattern(buf: &mut [u8], stream_id: u8, chunk: usize) {
    for (k, b) in buf.iter_mut().enumerate() {
        *b = pattern_byte(stream_id, chunk * CHUNK + k);
    }
}

struct StreamResult {
    ttfb_ms: f64,
    done_ms: f64, // 自 open 完成起算的整流完成时间
    ok: bool,
}

#[derive(Default, Clone, Copy)]
struct Metrics {
    p50_ttfb: f64,
    p99_ttfb: f64,
    makespan_ms: f64,
    spread_ms: f64,
    mbps: f64,
    failures: usize,
}

/// 单格总超时：超过即判失败（防某个 mux 死锁拖住整场 bench）。
const CELL_TIMEOUT: Duration = Duration::from_secs(180);

async fn run_cell(id: MuxId, sc: &Scenario, seed: u64) -> anyhow::Result<Metrics> {
    match tokio::time::timeout(CELL_TIMEOUT, run_cell_inner(id, sc, seed)).await {
        Ok(r) => r,
        Err(_) => Ok(Metrics { failures: sc.streams, ..Default::default() }),
    }
}

async fn run_cell_inner(id: MuxId, sc: &Scenario, seed: u64) -> anyhow::Result<Metrics> {
    // picomux 0.2.1：任何流的 FIN 会终结整个会话（无半关闭），写满后保持写半存活
    let no_fin = id == MuxId::Picomux;
    let (client_io, server_io) = delayed_link(sc.rtt, sc.loss, seed);
    let client = Arc::new(mux_factory(id, Box::new(client_io)).await?);
    let server = mux_server_factory(id, Box::new(server_io)).await?;

    // 服务端：持续 accept 并 echo 到 EOF。循环不设上界、`server` 句柄在
    // 本函数作用域存活到 cell 结束——muxado/h2mux/picomux 的会话生命周期
    // 绑定在句柄/driver 上，提前 drop 会把连接整条杀掉（首版 harness 的 bug）。
    let server = Arc::new(server);
    {
        let server = server.clone();
        tokio::spawn(async move {
            let mut accepted = 0usize;
            loop {
                match server.accept().await {
                    Ok(s) => {
                        accepted += 1;
                        let _sid = accepted - 1;
                        tokio::spawn(async move {
                            let mut s = s;
                            let mut buf = vec![0u8; CHUNK];
                            loop {
                                match s.read(&mut buf).await {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        if s.write_all(&buf[..n]).await.is_err() {
                                            break;
                                        }
                                    }
                                    Err(_) => {
                                        break;
                                    }
                                }
                            }
                        });
                    }
                    Err(_) => break,
                }
            }
        });
    }

    // muxado 的 open() 串行持锁（Mutex 包 session），8 流逐个 RTT 才能开完，
    // 其他实现并发开流——统一给足 60s 开流窗口，超时按剩余流记失败。
    const OPEN_TIMEOUT: Duration = Duration::from_secs(120);
    let mut handles = Vec::new();
    for _ in 0..sc.streams {
        let client = client.clone();
        handles.push(tokio::spawn(async move {
            match tokio::time::timeout(OPEN_TIMEOUT, client.open()).await {
                Ok(Ok(s)) => {
                    Some(s)
                }
                Ok(Err(e)) => {
                    eprintln!("open failed: {e}");
                    None
                }
                Err(_) => {
                    eprintln!("open timed out after {OPEN_TIMEOUT:?}");
                    None
                }
            }
        }));
    }
    let (tx, mut rx) = mpsc::channel::<StreamResult>(sc.streams);
    let mut streams = Vec::new();
    for h in handles.drain(..) {
        streams.push(match h.await.unwrap() {
            Some(s) => s,
            None => {
                eprintln!("open returned None for one stream");
                let _ = tx.send(StreamResult { ttfb_ms: 0.0, done_ms: 0.0, ok: false }).await;
                continue;
            }
        });
    }

    let t_start = Instant::now();
    let mut handles = Vec::new();
    for (i, s) in streams.into_iter().enumerate() {
        let tx = tx.clone();
        handles.push(tokio::spawn(async move {
            let t_stream = Instant::now();
            // 拆读写两半：写任务推图案，读任务收并校验
            let (mut rh, mut wh) = tokio::io::split(s);
            let writer = tokio::spawn(async move {
                let mut chunk = [0u8; CHUNK];
                let mut sent = 0usize;
                while sent < payload_size() {
                    let n = CHUNK.min(payload_size() - sent);
                    fill_pattern(&mut chunk[..n], i as u8, sent / CHUNK);
                    if let Err(e) = wh.write_all(&chunk[..n]).await {
                        eprintln!("writer err: {e}");
                        break;
                    }
                    sent += n;
                }
                if no_fin {
                    std::future::pending::<()>().await;
                } else {
                    let _ = wh.shutdown().await;
                }
            });
            #[allow(unused_mut)]
            // 读端：收满并校验，产出该流结果
            let mut buf = vec![0u8; CHUNK];
            let mut got = 0usize;
            let mut ttfb: Option<f64> = None;
            while got < payload_size() {
                match rh.read(&mut buf).await {
                    Ok(0) => {
                        break;
                    }
                    Err(_) => {
                        break;
                    }
                    Ok(n) => {
                        let now = t_stream.elapsed().as_secs_f64() * 1000.0;
                        if ttfb.is_none() {
                            ttfb = Some(now);
                        }
                        // 读可能跨 chunk 边界（mux 可能合并交付）：按全局绝对偏移逐字节校验
                        let mut ok = true;
                        for k in 0..n {
                            if buf[k] != pattern_byte(i as u8, got + k) {
                                ok = false;
                                break;
                            }
                        }
                        if !ok {
                            break;
                        }
                        got += n;
                    }
                }
            }
            let done = t_stream.elapsed().as_secs_f64() * 1000.0;
            writer.abort();
            let _ = tx.send(StreamResult {
                ttfb_ms: ttfb.unwrap_or(done),
                done_ms: done,
                ok: got == payload_size(),
            }).await;
        }));
    }
    drop(tx);
    let mut results = Vec::new();
    while let Some(r) = rx.recv().await {
        results.push(r);
    }
    for h in handles {
        let _ = h.await;
    }
    drop((server, client)); // cell 结束，释放链路
    let makespan_ms = t_start.elapsed().as_secs_f64() * 1000.0;

    let failures = results.iter().filter(|r| !r.ok).count();
    let mut ttfbs: Vec<f64> = results.iter().filter(|r| r.ok).map(|r| r.ttfb_ms).collect();
    let mut dones: Vec<f64> = results.iter().filter(|r| r.ok).map(|r| r.done_ms).collect();
    ttfbs.sort_by(|a, b| a.total_cmp(b));
    dones.sort_by(|a, b| a.total_cmp(b));
    let pct = |v: &[f64], p: f64| -> f64 {
        if v.is_empty() { 0.0 } else { v[((v.len() as f64 - 1.0) * p).round() as usize] }
    };
    let spread_ms = dones.last().copied().unwrap_or(0.0)
        - dones.first().copied().unwrap_or(0.0);
    let total_bytes = results.iter().filter(|r| r.ok).count() * payload_size() * 2;
    Ok(Metrics {
        p50_ttfb: pct(&ttfbs, 0.50),
        p99_ttfb: pct(&ttfbs, 0.99),
        makespan_ms,
        spread_ms,
        mbps: total_bytes as f64 / (makespan_ms / 1000.0) / 1024.0 / 1024.0,
        failures,
    })
}

#[tokio::main]
async fn main() {
    println!("mux-bench (one-shot, spec §7.6) — payload {PAYLOAD} B/stream, echo, integrity-checked\n");
    let t_all = Instant::now();
    // 调试用过滤器：BENCH_IMPL / BENCH_SCEN 只跑子集
    let impl_filter = std::env::var("BENCH_IMPL").ok();
    let scen_filter = std::env::var("BENCH_SCEN").ok();
    for sc in &SCENARIOS {
        if let Some(f) = &scen_filter {
            if f != sc.name {
                continue;
            }
        }
        println!(
            "== 场景 {}：RTT {}ms, 丢包 {:.0}%, {} 流 ==",
            sc.name,
            sc.rtt.as_millis(),
            sc.loss * 100.0,
            sc.streams
        );
        println!(
            "{:<12} {:>10} {:>10} {:>10} {:>10} {:>9} {:>7}",
            "impl", "p50 ttfb", "p99 ttfb", "makespan", "spread", "MB/s", "fail"
        );
        for (idx, (name, id)) in IMPLS.iter().enumerate() {
            if let Some(f) = &impl_filter {
                if f != name {
                    continue;
                }
            }
            let seed = 0x5EED_0000u64 ^ (idx as u64 + 1);
            let cell_start = Instant::now();
            let m = match run_cell(*id, sc, seed).await {
                Ok(m) => m,
                Err(e) => {
                    println!("{:<12} SETUP/PROTO FAIL: {e}", name);
                    continue;
                }
            };
            println!(
                "{:<12} {:>9.1}ms {:>9.1}ms {:>8.0}ms {:>8.0}ms {:>9.1} {:>7}",
                name, m.p50_ttfb, m.p99_ttfb, m.makespan_ms, m.spread_ms, m.mbps, m.failures
            );
            println!("             (cell wall time: {:.1}s)", cell_start.elapsed().as_secs_f64());
        }
        println!();
    }
    println!("total bench time: {:.1}s", t_all.elapsed().as_secs_f64());
}
