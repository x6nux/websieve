//! 诊断：内存 delay 管道上直接对比 yamux 单流 vs 多流吞吐。
//! 排除 XHTTP/Noise 层，隔离 mux 层车道并行度。

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wsieve_mux::{mux_factory, mux_server_factory};

fn delay_pipe(rtt_ms: u64) -> (tokio::io::DuplexStream, tokio::io::DuplexStream) {
    let (a, b1) = tokio::io::duplex(1024 * 1024);
    let (b2, c) = tokio::io::duplex(1024 * 1024);
    let half = Duration::from_millis(rtt_ms / 2);
    tokio::spawn(async move {
        let (mut r, w) = tokio::io::split(b1);
        let (mut r2, mut w2) = tokio::io::split(b2);
        let mut w = Some(w);
        let mut w2o = Some(w2);
        let mut buf1 = vec![0u8; 65536];
        let mut buf2 = vec![0u8; 65536];
        loop {
            tokio::select! {
                n = r.read(&mut buf1) => {
                    match n { Ok(0) | Err(_) => break, Ok(n) => {
                        tokio::time::sleep(half).await;
                        let mut out = Vec::with_capacity(n);
                        out.extend_from_slice(&buf1[..n]);
                        if let Some(ww) = w2o.as_mut() { if ww.write_all(&out).await.is_err() { break; } }
                    } }
                }
                n = r2.read(&mut buf2) => {
                    match n { Ok(0) | Err(_) => break, Ok(n) => {
                        tokio::time::sleep(half).await;
                        let mut out = Vec::with_capacity(n);
                        out.extend_from_slice(&buf2[..n]);
                        if let Some(ww) = w.as_mut() { if ww.write_all(&out).await.is_err() { break; } }
                    } }
                }
            }
        }
    });
    (a, c)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    for n_streams in [1usize, 4] {
        let (ca, sa) = delay_pipe(80);
        let client = Arc::new(mux_factory(wsieve_proto::hello::MuxId::Yamux, Box::new(ca)).await?);
        let server = mux_server_factory(wsieve_proto::hello::MuxId::Yamux, Box::new(sa)).await?;

        tokio::spawn(async move {
            loop {
                let Ok(mut s) = server.accept().await else { break };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    loop {
                        match s.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if s.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });

        let total_per_stream = 8u64 * 1024 * 1024;
        let start = std::time::Instant::now();
        let mut handles = Vec::new();
        for _ in 0..n_streams {
            let client = Arc::clone(&client);
            handles.push(tokio::spawn(async move {
                let mut s = client.open().await?;
                let chunk = vec![0x42u8; 65536];
                let mut remaining = total_per_stream;
                let mut verify = vec![0u8; 65536];
                while remaining > 0 {
                    s.write_all(&chunk).await?;
                    s.read_exact(&mut verify).await?;
                    remaining -= chunk.len() as u64;
                }
                anyhow::Ok(())
            }));
        }
        for h in handles {
            h.await??;
        }
        let elapsed = start.elapsed().as_secs_f64();
        let mbps = (total_per_stream * n_streams as u64) as f64 / elapsed / 1048576.0;
        println!("yamux {n_streams} 流 × 8MB (echo): {mbps:.1} MB/s");
    }
    Ok(())
}
