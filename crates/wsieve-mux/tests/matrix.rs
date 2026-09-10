//! 四种 mux 实现的行为矩阵测试：4 个用例 × 4 实现。

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use wsieve_mux::{mux_factory, mux_server_factory, Mux, MuxId, MuxStream};

async fn make_pair(id: MuxId) -> (Box<dyn Mux>, Box<dyn Mux>) {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let client = mux_factory(id, Box::new(client_io))
        .await
        .expect("client mux");
    let server = mux_server_factory(id, Box::new(server_io))
        .await
        .expect("server mux");
    (client, server)
}

/// 拿一对直连的流：client.open() + server.accept()（两个方向，供 echo 用）。
async fn stream_pair(client: &dyn Mux, server: &dyn Mux) -> (MuxStream, MuxStream) {
    let c = client.open().await.expect("open");
    let s = server.accept().await.expect("accept");
    (c, s)
}

async fn matrix(name: &str, id: MuxId) {
    let (c, s) = make_pair(id).await;
    open_and_echo(name, c, s).await;
}

// ---------- 用例 1：open + echo ----------

async fn open_and_echo(name: &str, client: Box<dyn Mux>, server: Box<dyn Mux>) {
    let (mut c, mut s) = stream_pair(client.as_ref(), server.as_ref()).await;
    c.write_all(b"hello websieve").await.unwrap();
    let mut buf = [0u8; 14];
    s.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello websieve", "{name}: forward payload");

    s.write_all(b"pong").await.unwrap();
    let mut buf = [0u8; 4];
    c.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"pong", "{name}: backward payload");
}

#[tokio::test]
async fn wsmux_open_and_echo() {
    matrix("wsmux", MuxId::Wsmux).await;
}

// ---------- 用例 2：32 条并发流，各写 4KB 独立模式 ----------

fn pattern_for(i: usize, len: usize) -> Vec<u8> {
    (0..len).map(|j| ((i * 31 + j) % 251) as u8).collect()
}

async fn concurrent_streams(name: &'static str, id: MuxId) {
    const N: usize = 32;
    const LEN: usize = 4 * 1024;

    let (client, server) = make_pair(id).await;

    // 服务端：逐条 accept，交给后台任务验证
    let verifier = tokio::spawn(async move {
        let mut handlers = Vec::new();
        for _ in 0..N {
            let mut stream = server.accept().await.expect("accept");
            handlers.push(tokio::spawn(async move {
                let mut expected_seed: Option<usize> = None;
                let mut buf = Vec::with_capacity(LEN);
                let mut chunk = [0u8; 1024];
                loop {
                    match stream.read(&mut chunk).await {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        Err(e) => panic!("server read error: {e}"),
                    }
                }
                // 判断属于哪条流：与全部 pattern 匹配
                for i in 0..N {
                    if buf == pattern_for(i, LEN) {
                        expected_seed = Some(i);
                        break;
                    }
                }
                assert!(expected_seed.is_some(), "{name}: cross-stream corruption?");
            }));
        }
        for h in handlers {
            h.await.expect("verifier join");
        }
    });

    // 客户端：并发打开 32 条流并写各自的 pattern
    let mut writers = Vec::new();
    for i in 0..N {
        let mut stream = client.open().await.expect("open");
        writers.push(tokio::spawn(async move {
            let data = pattern_for(i, LEN);
            stream.write_all(&data).await.expect("write");
            stream.shutdown().await.ok();
        }));
    }
    for w in writers {
        w.await.expect("writer join");
    }
    verifier.await.expect("verifier join");
}

#[tokio::test]
async fn wsmux_concurrent_streams() {
    concurrent_streams("wsmux", MuxId::Wsmux).await;
}

// ---------- 用例 3：慢流不饿死快流 ----------

async fn slow_stream_does_not_starve_fast(name: &'static str, id: MuxId) {
    let (client, server) = make_pair(id).await;

    // 快流：客户端写 64KB，服务端全速读
    let (mut fast_c, mut fast_s) = stream_pair(client.as_ref(), server.as_ref()).await;
    // 慢流：客户端写 64KB，服务端每次只读 1KB 后睡 50ms
    let (mut slow_c, mut slow_s) = stream_pair(client.as_ref(), server.as_ref()).await;

    const LEN: usize = 64 * 1024;

    let fast_writer = tokio::spawn(async move {
        let data = pattern_for(100, LEN);
        fast_c.write_all(&data).await.expect("fast write");
        fast_c.shutdown().await.ok();
        fast_c
    });

    let slow_writer = tokio::spawn(async move {
        let data = pattern_for(200, LEN);
        slow_c.write_all(&data).await.expect("slow write");
        slow_c.shutdown().await.ok();
        slow_c
    });

    let fast_reader = tokio::spawn(async move {
        read_all(&mut fast_s, LEN).await;
        fast_s
    });

    let slow_reader = tokio::spawn(async move {
        let mut got = 0usize;
        let mut chunk = [0u8; 1024];
        let mut done = false;
        while !done {
            tokio::time::sleep(Duration::from_millis(50)).await;
            match slow_s.read(&mut chunk).await {
                Ok(0) => done = true,
                Ok(n) => got += n,
                Err(_) => done = true,
            }
        }
        got
    });

    // 快流在慢流读满之前完成
    let fast_done = tokio::time::timeout(Duration::from_secs(10), fast_reader)
        .await
        .expect("{name}: fast stream starved (timeout)");
    let _ = fast_done.expect("fast reader join");
    let _fast_c = fast_writer.await.expect("fast writer join");

    let slow_got = slow_reader.await.expect("slow reader join");
    let _slow_c = slow_writer.await.expect("slow writer join");
    assert_eq!(slow_got, LEN, "{name}: slow stream should still complete");
}

async fn read_all(stream: &mut MuxStream, expect: usize) {
    let mut got = 0usize;
    let mut chunk = [0u8; 8192];
    while got < expect {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) => panic!("read error: {e}"),
        }
    }
    assert_eq!(got, expect, "stream length mismatch");
}

#[tokio::test]
async fn wsmux_slow_stream_does_not_starve_fast() {
    slow_stream_does_not_starve_fast("wsmux", MuxId::Wsmux).await;
}

// ---------- 用例 4：关闭传播 ----------
// 客户端 shutdown() 一条流 → 服务端 read 得到 EOF（Ok(0)）或 io 错误，
// 两种都算关闭成功（各 crate 语义不同），但必须"终止"而不是永久挂起。

async fn close_propagates(name: &'static str, id: MuxId) {
    let (client, server) = make_pair(id).await;
    let (mut c, mut s) = stream_pair(client.as_ref(), server.as_ref()).await;

    c.write_all(b"bye").await.unwrap();
    let mut buf = [0u8; 3];
    s.read_exact(&mut buf).await.unwrap();

    c.shutdown().await.expect("client shutdown");

    let mut chunk = [0u8; 16];
    let outcome = tokio::time::timeout(Duration::from_secs(5), s.read(&mut chunk)).await;
    match outcome {
        // EOF 或 错误 均视为关闭已传播
        Ok(Ok(0)) | Ok(Err(_)) => {}
        Ok(Ok(n)) => panic!("{name}: expected EOF after shutdown, got {n} bytes"),
        Err(_) => panic!("{name}: server read hangs after client shutdown"),
    }
}

#[tokio::test]
async fn wsmux_close_propagates() {
    close_propagates("wsmux", MuxId::Wsmux).await;
}

// ---------- h2mux 专项：发送窗口耗尽应挂起而非报错 ----------
// 客户端写超过初始流控窗口的数据且服务端暂不读：write 必须挂起而非报错，
// 窗口释放后数据完整到达。

