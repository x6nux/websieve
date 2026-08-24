//! 有损 TCP 中继：在客户端与服务端之间注入 RTT 与丢包，测真实全栈吞吐。
//!
//! 拓扑：
//!   e2e_reqwest 客户端 → 127.0.0.1:RELAY → （延迟+丢包管道）→ 127.0.0.1:SRV
//!
//! 丢包建模：环回上无法真丢 TCP 段（需 root 动 pfctl/dummynet），本中继按
//! TCP 语义近似——每个读到的块：
//!   1. 以概率 loss 触发「丢一次」：额外延迟 RTO（2×RTT + 100ms，混合
//!      快速重传与超时重传的量级）
//!   2. 以概率 loss² 触发「连丢两次」：再 +RTO（重传本身也可能丢）
//!   3. 顺序保持（TCP 队头阻塞语义——前面丢了，后面到达也要等）
//!   4. 正常块延迟 RTT/2
//! 管道化：多块可在途（延迟在每块的独立 future 上，不阻塞后续块的读取），
//! 但写入严格按入队顺序（保序）。
//!
//! 用法（由 scripts/loss-matrix.sh 驱动）：
//!   WSIEVE_LOSSY_UPSTREAM 127.0.0.1:8080   真实服务端地址
//!   WSIEVE_LOSSY_LISTEN   127.0.0.1:9090   中继监听
//!   WSIEVE_LOSS_RTT_MS    80                RTT（默认 80ms）
//!   WSIEVE_LOSS_PCT       1                 丢包百分比（默认 1）

use std::time::Duration;

use rand::Rng;
use rand_chacha::ChaCha12Rng;
use rand::SeedableRng;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn envs() -> (String, String, Duration, f64) {
    let up = std::env::var("WSIEVE_LOSSY_UPSTREAM").expect("WSIEVE_LOSSY_UPSTREAM");
    let listen = std::env::var("WSIEVE_LOSSY_LISTEN").unwrap_or_else(|_| "127.0.0.1:9090".into());
    let rtt = Duration::from_millis(
        std::env::var("WSIEVE_LOSS_RTT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(80),
    );
    let pct: f64 = std::env::var("WSIEVE_LOSS_PCT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);
    (up, listen, rtt, pct / 100.0)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (up, listen, rtt, loss) = envs();
    let up = up.parse::<std::net::SocketAddr>()?;
    println!(
        "lossy relay {listen} -> {up}  rtt={:?} loss={:.1}%",
        rtt,
        loss * 100.0
    );
    let listener = tokio::net::TcpListener::bind(&listen).await?;

    let mut conn_id = 0u64;
    loop {
        let (mut client, _addr) = listener.accept().await?;
        let server = match tokio::net::TcpStream::connect(up).await {
            Ok(s) => s,
            Err(e) => {
                let _ = client.shutdown().await;
                eprintln!("relay: connect upstream failed: {e}");
                continue;
            }
        };
        conn_id += 1;
        println!("relay: conn #{conn_id} established");
        let (cr, cw) = client.into_split();
        let (sr, sw) = server.into_split();
        tokio::spawn(direction(cr, sw, rtt, loss, ChaCha12Rng::seed_from_u64(conn_id)));
        tokio::spawn(direction(sr, cw, rtt, loss, ChaCha12Rng::seed_from_u64(conn_id ^ 0x9e37)));
    }
}

/// 单向泵：顺序读块，每块按丢包模型计算送达时刻，管道化发送
/// （读取不被在途块阻塞），写入按顺序执行（保序）。
async fn direction(
    mut src: tokio::net::tcp::OwnedReadHalf,
    dst: tokio::net::tcp::OwnedWriteHalf,
    rtt: Duration,
    loss: f64,
    mut rng: ChaCha12Rng,
) {
    // 每块一个 (送达时刻, 数据) 消息进队列；写侧按序等到时刻再写。
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(std::time::Instant, Vec<u8>)>(256);
    let mut dst = Some(dst);
    let writer = tokio::spawn(async move {
        while let Some((deliver_at, data)) = rx.recv().await {
            let now = std::time::Instant::now();
            if deliver_at > now {
                tokio::time::sleep(deliver_at - now).await;
            }
            let Some(d) = dst.as_mut() else { break };
            if d.write_all(&data).await.is_err() {
                break;
            }
        }
        if let Some(mut d) = dst.take() {
            let _ = d.shutdown().await;
        }
    });

    let rto = rtt + Duration::from_millis(100);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match src.read(&mut buf).await {
            Ok(0) => {
                drop(tx);
                let _ = writer.await; // writer 内部持有 dst；其退出即写半关闭
                break;
            }
            Ok(n) => {
                // 丢包模型：初始送达时刻 = now + RTT/2；每丢一次 +RTO。
                let mut deliver = std::time::Instant::now() + rtt / 2;
                let r = rng.random::<f64>();
                if loss > 0.0 && r < loss {
                    deliver += rto;
                    if rng.random::<f64>() < loss {
                        deliver += rto;
                    }
                }
                if tx.send((deliver, buf[..n].to_vec())).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}
