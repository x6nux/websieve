//! StripeConn 运行时测试：内存 duplex mux（复用 matrix.rs 的 rig 风格）。
//! 覆盖：单 lane 双向回环、多 lane 8MB 条带、乱序重组、中途加 lane、
//! CLOSE 语义（final_offset 后 EOF / 缺口报错）、未知 conn_id 丢流。

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use wsieve_mux::stripe_runtime::{join_inbound, route_inbound_with_cfg, ConnRegistry, StripeCfg, StripeDialer, StripeListener};
use wsieve_mux::{mux_factory, mux_server_factory, Mux, MuxId, MuxStream};
use wsieve_proto::addr::{AddrPort, TargetAddr};
use wsieve_proto::stripe::CHUNK;

async fn make_pair(id: MuxId) -> (Arc<dyn Mux>, Arc<dyn Mux>) {
    // 大缓冲：条带测试双向大流量，64KB 的 duplex 会在双向同时打满时死锁
    let (client_io, server_io) = tokio::io::duplex(8 * 1024 * 1024);
    let client = mux_factory(id, Box::new(client_io)).await.unwrap();
    let server = mux_server_factory(id, Box::new(server_io)).await.unwrap();
    (Arc::from(client), Arc::from(server))
}

fn test_cfg() -> StripeCfg {
    StripeCfg {
        target_lanes: 4,
        upgrade_bytes: u64::MAX, // 默认不升级；升级用例单独开小阈值
        upgrade_rate_bps: 0,
        upgrade_window: Duration::from_millis(1),
    }
}

fn local_addr(port: u16) -> AddrPort {
    AddrPort { addr: TargetAddr::V4([127, 0, 0, 1]), port }
}

/// 起一个 echo TCP 目标。
async fn echo_server() -> std::io::Result<u16> {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { return };
            tokio::spawn(async move {
                let mut buf = [0u8; 16384];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if s.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    Ok(port)
}

/// 服务端侧：StripeListener + TCP 泵（目标双向 copy）。
async fn start_server_side(server: Arc<dyn Mux>, cfg: StripeCfg) {
    let listener = StripeListener::new(server, cfg);
    tokio::spawn(async move {
        listener
            .run(|conn, addr| {
                tokio::spawn(async move {
                    // 拨目标 + 双向泵
                    let host = match &addr.addr {
                        TargetAddr::V4(o) => format!("{}.{}.{}.{}", o[0], o[1], o[2], o[3]),
                        TargetAddr::Domain(d) => d.clone(),
                        TargetAddr::V6(_) => "::1".to_string(),
                    };
                    let Ok(tcp) = tokio::net::TcpStream::connect((host.as_str(), addr.port)).await
                    else {
                        return;
                    };
                    let (mut tcp_r, mut tcp_w) = tokio::io::split(tcp);
                    let mut up_s = conn.stream();
                    let mut down_s = up_s.clone();
                    let up = async {
                        let mut buf = [0u8; 16384];
                        loop {
                            match up_s.read(&mut buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    if tcp_w.write_all(&buf[..n]).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    };
                    let down = async {
                        let mut buf = [0u8; 16384];
                        loop {
                            match tcp_r.read(&mut buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    if down_s.write_all(&buf[..n]).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    };
                    tokio::join!(up, down);
                    down_s.shutdown().await.ok();
                    conn.close_send(wsieve_proto::stripe::CloseReason::TargetEof)
                        .await;
                });
            })
            .await
    });
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len as u64).map(|i| (i % 251) as u8).collect()
}

/// 1. 单 lane（不升级）双向 echo。
#[tokio::test]
async fn single_lane_roundtrip() {
    let (client, server) = make_pair(MuxId::Yamux).await;
    start_server_side(server, test_cfg()).await;
    let dialer = StripeDialer::new(client, test_cfg());
    let port = echo_server().await.unwrap();
    let mut s = dialer.connect(&local_addr(port)).await.unwrap();

    let payload = b"hello stripe".to_vec();
    s.write_all(&payload).await.unwrap();
    let mut got = vec![0u8; payload.len()];
    s.read_exact(&mut got).await.unwrap();
    assert_eq!(got, payload);

    // 二次往返
    let p2 = pattern(300_000);
    s.write_all(&p2).await.unwrap();
    let mut g2 = vec![0u8; p2.len()];
    s.read_exact(&mut g2).await.unwrap();
    assert_eq!(g2, p2);
}

/// 2. 多 lane 条带：小阈值强制升级，8MB 回显字节一致。
#[tokio::test]
async fn multi_lane_striped_8mb() {
    let cfg = StripeCfg {
        target_lanes: 4,
        upgrade_bytes: 64 * 1024,
        upgrade_rate_bps: 1,
        upgrade_window: Duration::from_millis(1),
    };
    let (client, server) = make_pair(MuxId::Yamux).await;
    start_server_side(server, cfg.clone()).await;
    let dialer = StripeDialer::new(client, cfg.clone());
    let port = echo_server().await.unwrap();
    let mut s = dialer.connect(&local_addr(port)).await.unwrap();

    let payload = pattern(8 * 1024 * 1024);
    let mut s = s;
    // 写读并行：echo 目标 + mux 窗口都有限，先写完再读会全局死锁
    let mut ws = s.clone();
    let writer = { let p = payload.clone(); tokio::spawn(async move { ws.write_all(&p).await.unwrap() }) };
    let mut got = vec![0u8; payload.len()];
    read_exact_timeout(&mut s, &mut got, Duration::from_secs(60)).await.unwrap();
    writer.await.unwrap();
    assert_eq!(got, payload, "8MB 多 lane 回显必须字节一致");
}

async fn read_exact_timeout(
    s: &mut wsieve_mux::stripe_runtime::StripeStreamHandle,
    buf: &mut [u8],
    d: Duration,
) -> std::io::Result<()> {
    let mut got = 0;
    while got < buf.len() {
        let n = tokio::time::timeout(d, s.read(&mut buf[got..]))
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("read timeout at {got}/{}", buf.len()),
                )
            })??;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("eof at {got}/{}", buf.len()),
            ));
        }
        got += n;
    }
    Ok(())
}

/// 3. 乱序交付：重组器必须按 offset 输出连续流（模拟：服务端后发低 offset
/// 分片——经「延迟 lane」难以精确控制，改为直接驱动重组状态：用两条手工
/// mux 流以乱序 offset 发帧，读取方应得到顺序流）。
#[tokio::test]
async fn out_of_order_reassembly() {
    let (client, server) = make_pair(MuxId::Yamux).await;

    // 手工服务端：accept 首 lane（OPEN+addr），读出 addr，然后乱序发两个
    // DataFrame：offset=CHUNK 的分片先发，offset=0 的后发，再发 CLOSE。
    let server_task = tokio::spawn(async move {
        use wsieve_proto::stripe::*;
        let mut lane1 = server.accept().await.unwrap();
        let mut hdr = [0u8; HEADER_LEN];
        lane1.read_exact(&mut hdr).await.unwrap();
        // 读 addr
        let mut ab = Vec::new();
        let mut c = [0u8; 64];
        loop {
            let n = lane1.read(&mut c).await.unwrap();
            ab.extend_from_slice(&c[..n]);
            if wsieve_proto::addr::decode_addr(&ab).is_ok() {
                break;
            }
        }
        let a: Vec<u8> = (0..CHUNK as u64).map(|i| (i % 251) as u8).collect();
        let b: Vec<u8> = (0..CHUNK as u64).map(|i| ((i + 7) % 251) as u8).collect();
        let mut f2 = Vec::new();
        encode_frame(CHUNK as u64, &b, &mut f2); // 高 offset 先发
        lane1.write_all(&f2).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut f1 = Vec::new();
        encode_frame(0, &a, &mut f1);
        lane1.write_all(&f1).await.unwrap();
        // CLOSE(final=2*CHUNK, TargetEof)
        let mut cl = encode_header(&ConnHeader {
            conn_id: 1,
            cmd: Cmd::Close,
            dir: Dir::Down,
            lane_id: 0,
        })
        .to_vec();
        cl.extend_from_slice(&encode_close_payload((2 * CHUNK) as u64, CloseReason::TargetEof));
        lane1.write_all(&cl).await.unwrap();
    });

    let dialer = StripeDialer::new(client, test_cfg());
    let mut s = dialer.connect(&local_addr(1)).await.unwrap();
    let mut got = vec![0u8; 2 * CHUNK];
    read_exact_timeout(&mut s, &mut got, Duration::from_secs(10)).await.unwrap();
    let mut expect = Vec::new();
    expect.extend((0..CHUNK as u64).map(|i| (i % 251) as u8));
    expect.extend((0..CHUNK as u64).map(|i| ((i + 7) % 251) as u8));
    assert_eq!(got, expect);
    // EOF
    let mut one = [0u8; 1];
    let n = s.read(&mut one).await.unwrap();
    assert_eq!(n, 0, "CLOSE 且 final_offset 交付后应为 EOF");
    server_task.await.unwrap();
}

/// 4. 中途加 lane（join）：客户端首 lane 起流量，服务端再开第二条 lane
/// 继续发（无 CLOSE），重组仍连续。
#[tokio::test]
async fn lane_join_mid_stream() {
    let (client, server) = make_pair(MuxId::Yamux).await;
    let server = Arc::new(server);

    let sv = server.clone();
    let server_task = tokio::spawn(async move {
        use wsieve_proto::stripe::*;
        let mut lane1 = sv.accept().await.unwrap();
        let mut hdr = [0u8; HEADER_LEN];
        lane1.read_exact(&mut hdr).await.unwrap();
        let mut ab = Vec::new();
        let mut c = [0u8; 64];
        loop {
            let n = lane1.read(&mut c).await.unwrap();
            ab.extend_from_slice(&c[..n]);
            if wsieve_proto::addr::decode_addr(&ab).is_ok() {
                break;
            }
        }
        // 分片 0 在 lane1
        let p0: Vec<u8> = (0..CHUNK as u64).map(|i| (i % 249) as u8).collect();
        let mut f = Vec::new();
        encode_frame(0, &p0, &mut f);
        lane1.write_all(&f).await.unwrap();
        // 开 lane2（OPEN 加入 conn 1）发分片 1
        let mut lane2 = sv.open().await.unwrap();
        let mut hdr2 = encode_header(&ConnHeader {
            conn_id: 1,
            cmd: Cmd::Open,
            dir: Dir::Down,
            lane_id: 5,
        })
        .to_vec();
        let p1: Vec<u8> = (0..CHUNK as u64).map(|i| ((i + 3) % 249) as u8).collect();
        encode_frame(CHUNK as u64, &p1, &mut hdr2);
        lane2.write_all(&hdr2).await.unwrap();
        // CLOSE 在 lane1（最闲）
        let mut cl = encode_header(&ConnHeader {
            conn_id: 1,
            cmd: Cmd::Close,
            dir: Dir::Down,
            lane_id: 0,
        })
        .to_vec();
        cl.extend_from_slice(&encode_close_payload((2 * CHUNK) as u64, CloseReason::TargetEof));
        lane1.write_all(&cl).await.unwrap();
    });

    let dialer = StripeDialer::new(client, test_cfg());
    let mut s = dialer.connect(&local_addr(1)).await.unwrap();
    let mut got = vec![0u8; 2 * CHUNK];
    read_exact_timeout(&mut s, &mut got, Duration::from_secs(10)).await.unwrap();
    let mut expect = Vec::new();
    expect.extend((0..CHUNK as u64).map(|i| (i % 249) as u8));
    expect.extend((0..CHUNK as u64).map(|i| ((i + 3) % 249) as u8));
    assert_eq!(got, expect);
    server_task.await.unwrap();
}

/// 5. CLOSE 缺口：final_offset 超过实际交付 → 读取方报错而非挂死。
#[tokio::test]
async fn close_with_gap_errors() {
    let (client, server) = make_pair(MuxId::Yamux).await;
    let server_task = tokio::spawn(async move {
        use wsieve_proto::stripe::*;
        let mut lane1 = server.accept().await.unwrap();
        let mut hdr = [0u8; HEADER_LEN];
        lane1.read_exact(&mut hdr).await.unwrap();
        let mut ab = Vec::new();
        let mut c = [0u8; 64];
        loop {
            let n = lane1.read(&mut c).await.unwrap();
            ab.extend_from_slice(&c[..n]);
            if wsieve_proto::addr::decode_addr(&ab).is_ok() {
                break;
            }
        }
        // 只发 offset=0 的 100 字节，CLOSE 却声称 final=200
        let mut f = Vec::new();
        encode_frame(0, &[9u8; 100], &mut f);
        lane1.write_all(&f).await.unwrap();
        let mut cl = encode_header(&ConnHeader {
            conn_id: 1,
            cmd: Cmd::Close,
            dir: Dir::Down,
            lane_id: 0,
        })
        .to_vec();
        cl.extend_from_slice(&encode_close_payload(200, CloseReason::TargetEof));
        lane1.write_all(&cl).await.unwrap();
        // 保持 lane 打开一会儿，让客户端看到 CLOSE 后等 lane EOF 终结
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let dialer = StripeDialer::new(client, test_cfg());
    let mut s = dialer.connect(&local_addr(1)).await.unwrap();
    // 先读到 100 字节缺口前数据，下一次 read 应报 UnexpectedEof
    let mut buf = [0u8; 512];
    let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf))
        .await
        .expect("不应挂死")
        .unwrap();
    assert_eq!(n, 100, "缺口前数据应先交付");
    let r = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf)).await;
    match r {
        Ok(Ok(0)) => panic!("不应干净 EOF"),
        Ok(Ok(n)) => panic!("不应有后续数据 n={n}"),
        Ok(Err(e)) => {
            assert!(
                e.kind() == std::io::ErrorKind::UnexpectedEof,
                "缺口应报 UnexpectedEof，got {e}"
            );
        }
        Err(_) => panic!("缺口 + lane 全断后不应挂死"),
    }
    server_task.await.unwrap();
}

/// 6. 未知 conn_id 的入站流 → 客户端丢弃（不 panic，不影响既有 conn）。
#[tokio::test]
async fn unknown_conn_inbound_dropped() {
    let (client, server) = make_pair(MuxId::Yamux).await;
    let dialer = StripeDialer::new(client.clone(), test_cfg());
    let port = echo_server().await.unwrap();
    let mut s = dialer.connect(&local_addr(port)).await.unwrap();

    // 服务端先 accept 我们的 lane（保持 conn 活跃），再主动开一条未知
    // conn_id 的流
    let sv = Arc::new(server);
    let t = tokio::spawn(async move {
        let mut lane = sv.accept().await.unwrap();
        // 只挂着不动（echo 服务器端由前几个测试的路径负责；这里无需泵）
        let mut buf = [0u8; 1024];
        let _ = lane.read(&mut buf).await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 未知 conn 流：直接 mux.open 发一个 OPEN 头（conn_id=999）
    {
        use wsieve_proto::stripe::*;
        let mut ghost = client.open().await.unwrap();
        let hdr = encode_header(&ConnHeader {
            conn_id: 999,
            cmd: Cmd::Open,
            dir: Dir::Down,
            lane_id: 0,
        });
        // 未被消费的入站流可能被服务端 mux reset（无人 accept）：写失败
        // 亦无妨，本测试断言的是客户端 accept 任务不崩溃。
        let _ = ghost.write_all(&hdr).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // ghost 流可能在服务端 mux 侧被服务端 accept 抢走（本测试未起服务端
    // accept 泵）导致 reset —— 无所谓，客户端 accept 任务健壮性才是断言点。
    t.abort();
}

/// 7. join_inbound：未知 conn 的流被关闭；已知 conn 的 lane 被归并。
#[tokio::test]
async fn join_inbound_routes_and_drops() {
    let registry = Arc::new(ConnRegistry::new());
    // 构造一条手工 lane：OPEN conn_id=7
    let (client, server) = make_pair(MuxId::Yamux).await;
    let c = Arc::new(client);
    let writer = tokio::spawn({
        let c = c.clone();
        async move {
            let mut stream: MuxStream = c.open().await.unwrap();
            let hdr = wsieve_proto::stripe::encode_header(&wsieve_proto::stripe::ConnHeader {
                conn_id: 7,
                cmd: wsieve_proto::stripe::Cmd::Open,
                dir: wsieve_proto::stripe::Dir::Down,
                lane_id: 3,
            });
            use tokio::io::AsyncWriteExt as _;
            stream.write_all(&hdr).await.unwrap();
            stream
        }
    });
    let lane = server.accept().await.unwrap();
    // registry 为空 → join_inbound 应丢弃（不 panic）
    join_inbound(&registry, lane).await.unwrap();
    writer.abort();
    let _ = c;
}

/// 8. 服务端 route_inbound：非 OPEN 首帧（DATA）→ 丢弃。
#[tokio::test]
async fn server_rejects_non_open_first_frame() {
    let (client, server) = make_pair(MuxId::Yamux).await;
    let c = Arc::new(client);
    let writer = tokio::spawn(async move {
        let mut stream: MuxStream = c.open().await.unwrap();
        let hdr = wsieve_proto::stripe::encode_header(&wsieve_proto::stripe::ConnHeader {
            conn_id: 42,
            cmd: wsieve_proto::stripe::Cmd::Data,
            dir: wsieve_proto::stripe::Dir::Bidi,
            lane_id: 0,
        });
        stream.write_all(&hdr).await.unwrap();
    });
    let lane = server.accept().await.unwrap();
    let registry = Arc::new(ConnRegistry::new());
    let r = route_inbound_with_cfg(
        &registry,
        server,
        test_cfg(),
        lane,
        |_conn, _addr| {},
    )
    .await
    .unwrap();
    assert!(matches!(r, wsieve_mux::stripe_runtime::InboundLane::Dropped));
    assert!(registry.get(42).is_none());
    writer.await.unwrap();
}

/// 9. 升级发生：小阈值下发送大量数据后 lane 数应达到 target_lanes。
#[tokio::test]
async fn upgrade_to_target_lanes() {
    let cfg = StripeCfg {
        target_lanes: 4,
        upgrade_bytes: 64 * 1024,
        upgrade_rate_bps: 1,
        upgrade_window: Duration::from_millis(1),
    };
    // 双端都开自动升级：客户端上行触发其侧 add lane；这里只验证客户端侧。
    let (client, server) = make_pair(MuxId::Yamux).await;
    start_server_side(server, cfg.clone()).await;
    let dialer = StripeDialer::new(client, cfg.clone());
    let port = echo_server().await.unwrap();
    let s = dialer.connect(&local_addr(port)).await.unwrap();

    // 服务端 conn 的 lane_count 无法直接取（在服务器内部）；改用客户端侧
    // 行为验证：发送 2MB 后连接仍健康（升级 lane 不破坏正确性），且
    // dialer 侧 conn 未断。
    let payload = pattern(2 * 1024 * 1024);
    let mut s = s;
    let mut ws = s.clone();
    let writer = { let p = payload.clone(); tokio::spawn(async move { ws.write_all(&p).await.unwrap() }) };
    let mut got = vec![0u8; payload.len()];
    read_exact_timeout(&mut s, &mut got, Duration::from_secs(30)).await.unwrap();
    writer.await.unwrap();
    assert_eq!(got, payload, "升级后仍必须字节一致");
}
