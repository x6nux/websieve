//! wsmux 自身的行为测试（不经过 stripe 层）。
//!
//! `matrix.rs` 只覆盖了"客户端 open、服务端 accept"这一个方向。这里补上另一
//! 半：服务端主动开流、双向同时开流、以及句柄丢弃时的收尾语义——条带层会用到
//! 全部这些，出问题时先在这一层定位，比在 stripe 用例里猜要快得多。

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wsieve_mux::{mux_factory, mux_server_factory, Mux, MuxId};

async fn pair() -> (Arc<dyn Mux>, Arc<dyn Mux>) {
    let (c_io, s_io) = tokio::io::duplex(8 * 1024 * 1024);
    let c = mux_factory(MuxId::Wsmux, Box::new(c_io)).await.unwrap();
    let s = mux_server_factory(MuxId::Wsmux, Box::new(s_io))
        .await
        .unwrap();
    (Arc::from(c), Arc::from(s))
}

/// 两端各用各的窗口配置建一对会话。
async fn skewed_pair(client_win: u32, server_win: u32) -> (Arc<dyn Mux>, Arc<dyn Mux>) {
    use wsieve_mux::wsmux::{Config, Session};
    let mk = |w: u32| Config {
        window: w,
        keepalive: std::time::Duration::from_secs(15),
    };
    let (c_io, s_io) = tokio::io::duplex(8 * 1024 * 1024);
    let c: Arc<dyn Mux> = Arc::new(Session::with_config(Box::new(c_io), false, mk(client_win)));
    let s: Arc<dyn Mux> = Arc::new(Session::with_config(Box::new(s_io), true, mk(server_win)));
    (c, s)
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| ((i + seed as usize) % 251) as u8).collect()
}

#[tokio::test]
async fn the_server_can_open_a_stream_and_the_client_accepts_it() {
    let (client, server) = pair().await;
    let payload = pattern(256 * 1024, 3);
    let expect = payload.clone();

    let sv = server.clone();
    let t = tokio::spawn(async move {
        let mut s = sv.open().await.unwrap();
        s.write_all(&payload).await.unwrap();
        s.shutdown().await.unwrap();
    });

    let mut c = client.accept().await.unwrap();
    let mut got = Vec::new();
    c.read_to_end(&mut got).await.unwrap();
    assert_eq!(got, expect, "服务端开的流必须完整送达客户端");
    t.await.unwrap();
}

#[tokio::test]
async fn both_sides_can_open_at_once_without_their_stream_ids_colliding() {
    // sid 靠奇偶分区，两端各占一半空间；分错了会表现为两条流互相串数据。
    let (client, server) = pair().await;

    let sv = server.clone();
    let srv = tokio::spawn(async move {
        // 服务端同时做两件事：开自己的流，接客户端的流。
        let mut mine = sv.open().await.unwrap();
        mine.write_all(b"from-server").await.unwrap();
        mine.shutdown().await.unwrap();

        let mut theirs = sv.accept().await.unwrap();
        let mut buf = Vec::new();
        theirs.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"from-client");
    });

    let mut mine = client.open().await.unwrap();
    mine.write_all(b"from-client").await.unwrap();
    mine.shutdown().await.unwrap();

    let mut theirs = client.accept().await.unwrap();
    let mut buf = Vec::new();
    theirs.read_to_end(&mut buf).await.unwrap();
    assert_eq!(buf, b"from-server");
    srv.await.unwrap();
}

#[tokio::test]
async fn data_written_just_before_the_session_handle_drops_still_arrives() {
    // `poll_write` 返回 Ok 只表示帧进了出站缓冲。会话句柄被丢弃时如果直接
    // abort 写任务，这些帧就没了——对端看到的是数据凭空少一截。
    let (client, server) = pair().await;
    let payload = pattern(512 * 1024, 11);
    let expect = payload.clone();

    let sv = server.clone();
    tokio::spawn(async move {
        let mut s = sv.open().await.unwrap();
        s.write_all(&payload).await.unwrap();
        s.shutdown().await.unwrap();
        // 这里 sv、s 全部离开作用域：写缓冲里多半还压着数据。
    });
    drop(server); // 连主体这份也放掉，让会话真的只剩后台任务。

    let mut c = client.accept().await.unwrap();
    let mut got = Vec::new();
    c.read_to_end(&mut got).await.unwrap();
    assert_eq!(got.len(), expect.len(), "收尾必须排空写缓冲，不能丢尾巴");
    assert_eq!(got, expect);
}

#[tokio::test]
async fn a_half_closed_stream_still_carries_data_in_the_other_direction() {
    // FIN 是半关闭：对端不再发，但仍在收，也仍会回窗口更新。如果实现把 FIN
    // 当成整条流结束、把流表项摘掉，那些窗口更新就会找不到归属被丢弃，写侧
    // 的信用再也涨不回来——表现为传够一个窗口之后永久挂死。
    let (client, server) = pair().await;
    // 取大于默认窗口（4 MiB）的量，逼出至少一轮窗口回补。
    let bulk = pattern(6 * 1024 * 1024, 5);
    let expect_len = bulk.len();

    let sv = server.clone();
    let srv = tokio::spawn(async move {
        let mut s = sv.accept().await.unwrap();
        // 服务端先说"我不发了"，然后一路只读。
        s.shutdown().await.unwrap();
        let mut got = Vec::new();
        s.read_to_end(&mut got).await.unwrap();
        got.len()
    });

    let mut c = client.open().await.unwrap();
    // 先把对端的 FIN 读出来，确认半关闭确实已经发生。
    let mut probe = [0u8; 1];
    assert_eq!(c.read(&mut probe).await.unwrap(), 0, "应当读到对端的 FIN");
    c.write_all(&bulk).await.unwrap();
    c.shutdown().await.unwrap();

    let got = tokio::time::timeout(std::time::Duration::from_secs(20), srv)
        .await
        .expect("半关闭之后写侧不该挂死")
        .unwrap();
    assert_eq!(got, expect_len);
}

#[tokio::test]
async fn two_ends_configured_with_different_windows_still_transfer() {
    // 开流的一方无从知道对端窗口。早先它拿本端窗口当发送信用，两端配置一旦
    // 不一致就会超发，被对端的入站上限判成协议违规——握手看着是通的，然后
    // 一个字节都过不去。窗口是可调参数，两端不同步是迟早的事，必须撑住。
    let (client, server) = skewed_pair(16 * 1024 * 1024, 1024 * 1024).await;
    let bulk = pattern(5 * 1024 * 1024, 23);
    let expect_len = bulk.len();

    let sv = server.clone();
    let srv = tokio::spawn(async move {
        let mut s = sv.accept().await.unwrap();
        let mut got = Vec::new();
        s.read_to_end(&mut got).await.unwrap();
        s.write_all(b"ack").await.unwrap();
        s.shutdown().await.unwrap();
        got.len()
    });

    let mut c = client.open().await.unwrap();
    c.write_all(&bulk).await.unwrap();
    c.shutdown().await.unwrap();
    let mut back = Vec::new();
    c.read_to_end(&mut back).await.unwrap();
    assert_eq!(back, b"ack", "反向也要通");

    let got = tokio::time::timeout(std::time::Duration::from_secs(20), srv)
        .await
        .expect("窗口不对称不该把链路卡死")
        .unwrap();
    assert_eq!(got, expect_len);
}
