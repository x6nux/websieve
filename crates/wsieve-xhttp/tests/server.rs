//! SessionStore 服务端测试。spec §9.4。

use bytes::Bytes;
use std::time::Duration;
use tokio::time::timeout;
use wsieve_xhttp::server::{SessionGone, SessionStore, Sid};

#[tokio::test]
async fn reorder_and_dedup() {
    let s = SessionStore::new();
    let sid = Sid::random();

    // 创建会话（握手状态占位符）
    s.create(sid).await;

    // 乱序推送：3, 1, 2（数据 seq 从 1 起：n=0 是握手，spec §6.4）
    s.push_post(&sid, 3, Bytes::copy_from_slice(b"cc"))
        .await
        .unwrap();
    s.push_post(&sid, 1, Bytes::copy_from_slice(b"aa"))
        .await
        .unwrap();
    s.push_post(&sid, 2, Bytes::copy_from_slice(b"bb"))
        .await
        .unwrap();

    // 读取：应该按序返回 aa, bb, cc
    let mut buf = vec![0u8; 10];
    let mut total_n = 0;

    while total_n < 6 {
        let n = timeout(
            Duration::from_millis(100),
            s.read(&sid, &mut buf[total_n..]),
        )
        .await
        .expect("data already buffered; read returns immediately")
        .unwrap();
        total_n += n;
    }

    assert_eq!(&buf[..6], b"aabbcc");

    // 重复推送 seq 1 → 应该被去重，仍 Ok，不增加数据
    s.push_post(&sid, 1, Bytes::copy_from_slice(b"AA"))
        .await
        .unwrap();

    // 再次读取：应该没有新数据（缓冲已空）
    let n = timeout(Duration::from_millis(10), s.read(&sid, &mut buf)).await;
    assert!(n.is_err()); // 超时说明没有新数据
}

#[tokio::test(start_paused = true)]
async fn gc_attach_window() {
    // create 后 30s 内无 attach → SessionGone（spec §9.4 attach 窗口）
    let s = SessionStore::new();
    let sid = Sid::random();

    s.create(sid).await;

    // advance 31s（> 30s 窗口）；sleep 1.1s 让 GC task 的 1s tick 跑一轮
    //（paused clock 下 sleep 即 auto-advance）
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    tokio::task::yield_now().await;

    let mut buf = vec![0u8; 10];
    let result = s.read(&sid, &mut buf).await;
    assert!(matches!(result, Err(SessionGone)));
}

#[tokio::test(start_paused = true)]
async fn gc_upstream_idle() {
    // attach 后 180s 无上行 → SessionGone（spec §9.4 上行空闲 GC）
    let s = SessionStore::new();
    let sid = Sid::random();

    s.create(sid).await;
    let _handle = s.attach_downlink(&sid).await.unwrap(); // 不 drop：走空闲分支

    // advance 181s（> 180s 空闲）+ GC tick
    tokio::time::advance(Duration::from_secs(181)).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    tokio::task::yield_now().await;

    let mut buf = vec![0u8; 10];
    let result = s.read(&sid, &mut buf).await;
    assert!(matches!(result, Err(SessionGone)));
}

#[tokio::test(start_paused = true)]
async fn gc_upstream_idle_reset_by_post() {
    // 变体：attach 后中途有 POST → 计时重置，181s 从最后一次 POST 起算
    let s = SessionStore::new();
    let sid = Sid::random();

    s.create(sid).await;
    let _handle = s.attach_downlink(&sid).await.unwrap();

    // 过 100s 后来一个 POST（seq 0，落在 next_seq 上）
    tokio::time::advance(Duration::from_secs(100)).await;
    s.push_post(&sid, 1, Bytes::copy_from_slice(b"xx"))
        .await
        .unwrap();

    // 再过 100s（自 POST 起仅 100s < 180s）→ 会话仍活着
    tokio::time::advance(Duration::from_secs(100)).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    tokio::task::yield_now().await;

    // 读得到 POST 的数据（会话活着，且 seq 1 可消费）
    let mut buf = vec![0u8; 10];
    let n = timeout(Duration::from_millis(100), s.read(&sid, &mut buf))
        .await
        .expect("session alive, seq 1 buffered")
        .unwrap();
    assert_eq!(&buf[..n], b"xx");

    // 再过 81s（自 POST 起 181s）→ 会话被 GC
    tokio::time::advance(Duration::from_secs(81)).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    tokio::task::yield_now().await;

    let result = s.read(&sid, &mut buf).await;
    assert!(matches!(result, Err(SessionGone)));
}

#[tokio::test]
async fn buffer_overflow_kills() {
    let s = SessionStore::new();
    let sid = Sid::random();

    s.create(sid).await;

    // 推送 31 个乱序 POST（seq 2..=32，缺 1=next_seq，空洞）
    // 堆积超上限（30）且有空洞 → SessionGone
    for i in 2..=32 {
        let result = s.push_post(&sid, i, Bytes::from(vec![0u8; 10])).await;
        if i == 32 {
            assert!(matches!(result, Err(SessionGone)));
        } else {
            result.unwrap();
        }
    }

    // 会话已死亡
    let mut buf = vec![0u8; 10];
    let result = s.read(&sid, &mut buf).await;
    assert!(matches!(result, Err(SessionGone)));
}

#[tokio::test]
async fn double_attach_rejected() {
    let s = SessionStore::new();
    let sid = Sid::random();

    s.create(sid).await;

    // 第一次 attach，持有 handle 不 drop
    let _handle1 = s.attach_downlink(&sid).await.unwrap();

    // 第二次 attach → 走 attached 标志分支 → SessionGone
    let result = s.attach_downlink(&sid).await;
    assert!(matches!(result, Err(SessionGone)));
}

#[tokio::test]
async fn downlink_drop_kills() {
    let s = SessionStore::new();
    let sid = Sid::random();

    s.create(sid).await;

    // 绑定下行
    let handle = s.attach_downlink(&sid).await.unwrap();

    // Drop handle → 会话应该被 GC
    drop(handle);
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 尝试读取 → SessionGone
    let mut buf = vec![0u8; 10];
    let result = s.read(&sid, &mut buf).await;
    assert!(matches!(result, Err(SessionGone)));
}
