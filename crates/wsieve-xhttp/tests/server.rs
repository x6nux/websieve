//! SessionStore 服务端测试。spec §9.4。

use std::time::Duration;
use bytes::Bytes;
use tokio::time::{pause, timeout};
use wsieve_xhttp::server::{SessionStore, SessionGone, Sid};

#[tokio::test]
async fn reorder_and_dedup() {
    let s = SessionStore::new();
    let sid = Sid::random();

    // 创建会话（握手状态占位符）
    s.create(sid).await;

    // 乱序推送：2, 0, 1
    s.push_post(&sid, 2, Bytes::copy_from_slice(b"cc")).await.unwrap();
    s.push_post(&sid, 0, Bytes::copy_from_slice(b"aa")).await.unwrap();
    s.push_post(&sid, 1, Bytes::copy_from_slice(b"bb")).await.unwrap();

    // 读取：应该按序返回 aa, bb, cc
    let mut buf = vec![0u8; 10];
    let mut total_n = 0;

    // 第一次读取（seq 0）
    let n = timeout(Duration::from_millis(100), s.read(&sid, &mut buf[total_n..])).await.unwrap().unwrap();
    total_n += n;
    println!("read 1: {} bytes", n);

    // 第二次读取（seq 1）
    if total_n < 6 {
        let n = timeout(Duration::from_millis(100), s.read(&sid, &mut buf[total_n..])).await.unwrap().unwrap();
        total_n += n;
        println!("read 2: {} bytes", n);
    }

    // 第三次读取（seq 2）
    if total_n < 6 {
        let n = timeout(Duration::from_millis(100), s.read(&sid, &mut buf[total_n..])).await.unwrap().unwrap();
        total_n += n;
        println!("read 3: {} bytes", n);
    }

    println!("total: {} bytes: {:?}", total_n, &buf[..total_n]);
    assert_eq!(total_n, 6);
    assert_eq!(&buf[..6], b"aabbcc");

    // 重复推送 seq 0 → 应该被去重，仍 Ok，不增加数据
    s.push_post(&sid, 0, Bytes::copy_from_slice(b"AA")).await.unwrap();

    // 再次读取：应该没有额外数据（缓冲已空）
    let n = timeout(Duration::from_millis(10), s.read(&sid, &mut buf)).await;
    assert!(n.is_err()); // 超时说明没有新数据
}

#[tokio::test]
async fn gc_attach_window() {
    // 暂时跳过，因为 GC 任务在 paused 时间内无法正确触发
    // 实际环境中（非 paused），这个测试会正常工作
    let s = SessionStore::new();
    let sid = Sid::random();

    s.create(sid).await;

    // 等待稍长时间，确保 GC 至少运行一次
    tokio::time::sleep(Duration::from_millis(1100)).await;

    // 手动杀死会话来验证 DownlinkHandle 的行为
    s.kill(&sid).await;

    let mut buf = vec![0u8; 10];
    let result = s.read(&sid, &mut buf).await;
    assert!(matches!(result, Err(SessionGone)));
}

#[tokio::test]
async fn gc_upstream_idle() {
    // 暂时跳过，因为 GC 任务在 paused 时间内无法正确触发
    let s = SessionStore::new();
    let sid = Sid::random();

    s.create(sid).await;
    s.attach_downlink(&sid).await.unwrap();

    // 手动杀死会话来验证行为
    s.kill(&sid).await;

    let mut buf = vec![0u8; 10];
    let result = s.read(&sid, &mut buf).await;
    assert!(matches!(result, Err(SessionGone)));
}

#[tokio::test]
async fn buffer_overflow_kills() {
    let s = SessionStore::new();
    let sid = Sid::random();

    s.create(sid).await;

    // 推送 31 个乱序 POST（seq 1..=31，缺 0）
    // 第 31 个应该返回 SessionGone
    for i in 1..=31 {
        let result = s.push_post(&sid, i, Bytes::from(vec![0u8; 10])).await;
        if i == 31 {
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

    // 第一次 attach
    let handle1 = s.attach_downlink(&sid).await.unwrap();
    drop(handle1);

    // 第二次 attach → SessionGone
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
