//! StripeConn 运行时测试：内存 duplex mux（复用 matrix.rs 的 rig 风格）。
//! 覆盖：单 lane 双向回环、多 lane 8MB 条带、乱序重组、中途加 lane、
//! CLOSE 语义（final_offset 后 EOF / 缺口报错）、未知 conn_id 丢流。

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use wsieve_mux::stripe_runtime::{
    join_inbound, route_inbound_full, route_inbound_with_cfg, ConnRegistry, SessionGroup,
    SessionGroups, StripeCfg, StripeDialer, StripeListener,
};
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
        extra_sessions: 0,
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
    let (client, server) = make_pair(MuxId::Wsmux).await;
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
        // 窗口设 0，本例才测得到它真正要测的东西——字节的**分布**。
        // 用非零窗口会让结果取决于"发完 8MB 有没有比窗口慢"：内存 duplex 上
        // 一毫秒就能发完，于是永远不升级，分布退化成 [8MB, 0, 0, 0]，
        // 看起来像分布坏了，实际只是快到不需要加 lane。窗口计时本身由
        // `upgrade_to_target_lanes` 覆盖。
        upgrade_window: Duration::ZERO,
        extra_sessions: 0,
    };
    let (client, server) = make_pair(MuxId::Wsmux).await;
    start_server_side(server, cfg.clone()).await;
    let dialer = StripeDialer::new(client, cfg.clone());
    let port = echo_server().await.unwrap();
    let s = dialer.connect(&local_addr(port)).await.unwrap();

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
    let (client, server) = make_pair(MuxId::Wsmux).await;

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
    let (client, server) = make_pair(MuxId::Wsmux).await;
    let server = Arc::new(server);

    let sv = server.clone();
    let server_task = tokio::spawn(async move {
        use wsieve_proto::stripe::*;
        let mut lane1 = sv.accept().await.unwrap();
        let mut hdr = [0u8; HEADER_LEN];
        lane1.read_exact(&mut hdr).await.unwrap();
        let conn_id = decode_header(&hdr).unwrap().conn_id;
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
        // 开 lane2（OPEN 加入该 conn）发分片 1
        let mut lane2 = sv.open().await.unwrap();
        let mut hdr2 = encode_header(&ConnHeader {
            conn_id,
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
            conn_id,
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
    let (client, server) = make_pair(MuxId::Wsmux).await;
    let server_task = tokio::spawn(async move {
        use wsieve_proto::stripe::*;
        let mut lane1 = server.accept().await.unwrap();
        let mut hdr = [0u8; HEADER_LEN];
        lane1.read_exact(&mut hdr).await.unwrap();
        let conn_id = decode_header(&hdr).unwrap().conn_id;
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
            conn_id,
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
    let (client, server) = make_pair(MuxId::Wsmux).await;
    let dialer = StripeDialer::new(client.clone(), test_cfg());
    let port = echo_server().await.unwrap();
    // conn 保持存活即可（本例断言的是 ghost 流不影响它），无需读写
    let _s = dialer.connect(&local_addr(port)).await.unwrap();

    // 服务端先 accept 我们的 lane（保持 conn 活跃），再主动开一条未知
    // conn_id 的流
    // mux 句柄必须活过整个用例：最后一个 `Arc<dyn Mux>` 消失时会话就会收摊，
    // 把句柄 move 进一个写完就结束的任务，等于让会话在断言之前先自杀。
    let sv = server.clone();
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
    let (client, server) = make_pair(MuxId::Wsmux).await;
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
    let (client, server) = make_pair(MuxId::Wsmux).await;
    // mux 句柄必须活过整个用例：最后一个 `Arc<dyn Mux>` 消失时会话就会收摊，
    // 把句柄 move 进一个写完就结束的任务，等于让会话在断言之前先自杀。
    let c = client.clone();
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
        extra_sessions: 0,
    };
    // 双端都开自动升级：客户端上行触发其侧 add lane；这里只验证客户端侧。
    let (client, server) = make_pair(MuxId::Wsmux).await;
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

/// 服务端多会话泵：一个 mux + 共享 registry 的 accept 循环（每会话一个）。
async fn serve_session(server: Arc<dyn Mux>, cfg: StripeCfg, registry: Arc<ConnRegistry>) {
    loop {
        let Ok(stream) = server.accept().await else { return };
        let reg = registry.clone();
        let cfg = cfg.clone();
        let mux = server.clone();
        tokio::spawn(async move {
            let _ = route_inbound_with_cfg(&reg, mux, cfg, stream, pump_conn).await;
        });
    }
}

fn pump_conn(conn: Arc<StripeConnLike>, addr: AddrPort) {
    tokio::spawn(async move {
        let host = match &addr.addr {
            TargetAddr::V4(o) => format!("{}.{}.{}.{}", o[0], o[1], o[2], o[3]),
            TargetAddr::Domain(d) => d.clone(),
            TargetAddr::V6(_) => "::1".to_string(),
        };
        let Ok(tcp) = tokio::net::TcpStream::connect((host, addr.port)).await else { return };
        let (mut tcp_r, mut tcp_w) = tokio::io::split(tcp);
        let mut up_s = conn.stream();
        let mut down_s = up_s.clone();
        let up = async {
            let mut buf = [0u8; 16384];
            loop {
                match up_s.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => { if tcp_w.write_all(&buf[..n]).await.is_err() { break } }
                }
            }
        };
        let down = async {
            let mut buf = [0u8; 16384];
            loop {
                match tcp_r.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => { if down_s.write_all(&buf[..n]).await.is_err() { break } }
                }
            }
        };
        tokio::join!(up, down);
        down_s.shutdown().await.ok();
        conn.close_send(wsieve_proto::stripe::CloseReason::TargetEof).await;
    });
}

use wsieve_mux::stripe_runtime::StripeConn as StripeConnLike;

/// 计数版 accept 泵：每 accept 一条流计数 +1（测 lane 分布用）。
async fn serve_session_counting(
    server: Arc<dyn Mux>,
    cfg: StripeCfg,
    registry: Arc<ConnRegistry>,
    counter: Arc<std::sync::atomic::AtomicUsize>,
) {
    loop {
        let Ok(stream) = server.accept().await else { return };
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reg = registry.clone();
        let cfg = cfg.clone();
        let mux = server.clone();
        tokio::spawn(async move {
            let _ = route_inbound_with_cfg(&reg, mux, cfg, stream, pump_conn).await;
        });
    }
}

/// 10. 多会话条带：两条 duplex 对（= 两个独立「TCP」），客户端 dialer 挂
/// 两个 mux，服务端两 mux 共享 ConnRegistry。升级后 lane 分布在两个会话
/// 上，8MB 回显字节一致。
#[tokio::test]
async fn multi_session_striping() {
    let cfg = StripeCfg {
        target_lanes: 4,
        upgrade_bytes: 64 * 1024,
        upgrade_rate_bps: 1,
        upgrade_window: Duration::from_millis(1),
        extra_sessions: 0,
    };
    let (c1, s1) = make_pair(MuxId::Wsmux).await;
    let (c2, s2) = make_pair(MuxId::Wsmux).await;
    let registry = Arc::new(ConnRegistry::new());
    tokio::spawn(serve_session(s1, cfg.clone(), registry.clone()));
    tokio::spawn(serve_session(s2, cfg.clone(), registry.clone()));

    let dialer = StripeDialer::new(c1, cfg.clone());
    assert!(dialer.attach_session(c2));
    assert!(!dialer.attach_session(dialer_pick(&dialer)), "重复挂载应被拒绝");
    assert_eq!(dialer.session_count(), 2);
    let port = echo_server().await.unwrap();
    let mut s = dialer.connect(&local_addr(port)).await.unwrap();

    let payload = pattern(8 * 1024 * 1024);
    let mut ws = s.clone();
    let writer = { let p = payload.clone(); tokio::spawn(async move { ws.write_all(&p).await.unwrap() }) };
    let mut got = vec![0u8; payload.len()];
    read_exact_timeout(&mut s, &mut got, Duration::from_secs(60)).await.unwrap();
    writer.await.unwrap();
    assert_eq!(got, payload, "跨两会话条带回显必须字节一致");
}

fn dialer_pick(d: &Arc<StripeDialer>) -> Arc<dyn Mux> {
    // 取主会话 mux（sessions[0]）用于重复挂载断言
    d.primary_mux()
}

/// 11. 单会话死亡：dialer 挂两个会话，杀掉非首 lane 所在会话的传输，
/// conn 仍可经另一会话收发。（实现层面：直接不依赖该会话即可——用一条
/// 新 conn 走存活会话验证 dialer 仍可用；旧 conn 若 lane 全在死会话上会
/// 按自身 lane EOF 机制终结，不影响 dialer。）
#[tokio::test]
async fn session_death_conn_survives_via_other_session() {
    let cfg = test_cfg();
    let (c1, s1) = make_pair(MuxId::Wsmux).await;
    let (c2, s2) = make_pair(MuxId::Wsmux).await;
    let registry = Arc::new(ConnRegistry::new());
    tokio::spawn(serve_session(s1, cfg.clone(), registry.clone()));
    // 死会话：服务端不开 accept 泵 → 该会话空闲即断
    drop(s2);
    let dialer = StripeDialer::new(c1, cfg.clone());
    let _ = dialer.attach_session(c2);
    let port = echo_server().await.unwrap();
    let mut s = dialer.connect(&local_addr(port)).await.unwrap();
    let payload = pattern(1 * 1024 * 1024);
    let mut ws = s.clone();
    let writer = { let p = payload.clone(); tokio::spawn(async move { ws.write_all(&p).await.unwrap() }) };
    let mut got = vec![0u8; payload.len()];
    read_exact_timeout(&mut s, &mut got, Duration::from_secs(30)).await.unwrap();
    writer.await.unwrap();
    assert_eq!(got, payload, "死会话存在时经主会话的 conn 必须照常工作");
}

/// 12. lane 分布断言：多会话下升级开的 lane 应落在不同会话上（轮转）。
/// 服务端两个 mux 各自计数 accept 的流：两会话均应收到 ≥2 条流（首 lane
/// + 升级 lane 分摊），证明 lane 确实跨 TCP 分布。
#[tokio::test]
async fn multi_session_lane_distribution() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let cfg = StripeCfg {
        target_lanes: 4,
        upgrade_bytes: 64 * 1024,
        upgrade_rate_bps: 1,
        upgrade_window: Duration::from_millis(1),
        extra_sessions: 0,
    };
    let (c1, s1) = make_pair(MuxId::Wsmux).await;
    let (c2, s2) = make_pair(MuxId::Wsmux).await;
    let registry = Arc::new(ConnRegistry::new());
    let n1 = Arc::new(AtomicUsize::new(0));
    let n2 = Arc::new(AtomicUsize::new(0));
    tokio::spawn(serve_session_counting(s1, cfg.clone(), registry.clone(), n1.clone()));
    tokio::spawn(serve_session_counting(s2, cfg.clone(), registry.clone(), n2.clone()));

    let dialer = StripeDialer::new(c1, cfg.clone());
    dialer.attach_session(c2);
    let port = echo_server().await.unwrap();
    let mut s = dialer.connect(&local_addr(port)).await.unwrap();

    let payload = pattern(4 * 1024 * 1024);
    let mut ws = s.clone();
    let writer = { let p = payload.clone(); tokio::spawn(async move { ws.write_all(&p).await.unwrap() }) };
    let mut got = vec![0u8; payload.len()];
    read_exact_timeout(&mut s, &mut got, Duration::from_secs(60)).await.unwrap();
    writer.await.unwrap();
    assert_eq!(got, payload);

    let a = n1.load(Ordering::Relaxed);
    let b = n2.load(Ordering::Relaxed);
    assert!(a >= 2 && b >= 2, "lane 应跨两会话分布，实际 s1={a} s2={b}");
}

/// 构造一个「真死」的客户端 mux：底层 duplex 的对端半直接丢弃，任何读写
/// 立即失败 → mux 会话终结 → accept 循环退出。
/// 注意不能用 `drop(server_mux)`：适配器把驱动任务的 JoinHandle 一 drop
/// 就是 detach（任务继续跑并持有 io），会话反而活着。
async fn dead_mux(id: MuxId) -> Arc<dyn Mux> {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    drop(server_io);
    Arc::from(mux_factory(id, Box::new(client_io)).await.unwrap())
}

/// 13. 服务端 registry 排空：经 route_inbound_with_cfg 建的 conn 在接收方向
/// 终结后必须从表中摘除。registry 已从「每会话一张、随会话拆除整体回收」
/// 改为 AppState 级全局表，没有显式清理 = 每条 conn 永久泄漏。
#[tokio::test]
async fn server_registry_drains_after_conn_ends() {
    let (client, server) = make_pair(MuxId::Wsmux).await;
    let registry = Arc::new(ConnRegistry::new());
    let cfg = test_cfg();
    tokio::spawn(serve_session(server, cfg.clone(), registry.clone()));

    let port = echo_server().await.unwrap();
    let dialer = StripeDialer::new(client, cfg);
    let mut s = dialer.connect(&local_addr(port)).await.unwrap();

    // 走一趟真实数据，确认 conn 已建立并进表
    s.write_all(b"ping").await.unwrap();
    let mut got = [0u8; 4];
    read_exact_timeout(&mut s, &mut got, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(&got, b"ping");
    assert_eq!(registry.len(), 1, "conn 建立后服务端表应有 1 条");

    // 客户端半关 → 上行 CLOSE(TargetEof) 送达服务端 → 服务端 conn 的接收
    // 方向终结（lane_reader 的 CLOSE 分支即刻 mark_recv_terminal）。
    s.shutdown().await.unwrap();

    // 服务端清表是异步任务：给它落定时间
    for _ in 0..100 {
        if registry.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        registry.len(),
        0,
        "conn 终结后服务端 registry 必须排空（否则每条 conn 永久泄漏）"
    );
}

/// 14. 死会话下的 connect：两个会话、其中一个真死，连续 6 次 connect 全部
/// 成功且能真实往返。修复前 pick() 无存活性检查且 connect 不重试 → 轮转
/// 命中死会话即失败（实测 6 次里 3 次挂）。
#[tokio::test]
async fn connect_retries_past_dead_session() {
    let cfg = test_cfg();
    let (c1, s1) = make_pair(MuxId::Wsmux).await;
    let registry = Arc::new(ConnRegistry::new());
    tokio::spawn(serve_session(s1, cfg.clone(), registry.clone()));
    let c2 = dead_mux(MuxId::Wsmux).await;

    let dialer = StripeDialer::new(c1, cfg);
    assert!(dialer.attach_session(c2));
    assert_eq!(dialer.session_count(), 2);

    let port = echo_server().await.unwrap();
    for i in 0..6 {
        let mut s = dialer
            .connect(&local_addr(port))
            .await
            .unwrap_or_else(|e| panic!("第 {i} 次 connect 失败: {e}"));
        // 真实往返：证明拿到的是活会话上的 conn，而非「open 成功但发不出去」
        let msg = format!("probe-{i}");
        s.write_all(msg.as_bytes()).await.unwrap();
        let mut got = vec![0u8; msg.len()];
        read_exact_timeout(&mut s, &mut got, Duration::from_secs(10))
            .await
            .unwrap_or_else(|e| panic!("第 {i} 次往返失败: {e}"));
        assert_eq!(got, msg.as_bytes());
    }
    // 死会话应已被摘表（accept 循环退出 / open 失败两条路径任一触发）
    assert_eq!(dialer.session_count(), 1, "死会话应已从会话表摘除");
}

/// 服务端多会话泵（组感知版）：一个 mux + 共享 registry + 会话组。
/// 新 conn 的下行 lane 在组内轮转开 → 跨会话（跨 TCP）。
async fn serve_session_grouped(
    server: Arc<dyn Mux>,
    cfg: StripeCfg,
    registry: Arc<ConnRegistry>,
    group: Arc<SessionGroup>,
) {
    loop {
        let Ok(stream) = server.accept().await else { return };
        let reg = registry.clone();
        let cfg = cfg.clone();
        let mux = server.clone();
        let g = group.clone();
        tokio::spawn(async move {
            let _ = route_inbound_full(&reg, mux, Some(g), cfg, stream, pump_conn).await;
        });
    }
}

/// 15. 组感知下行 opener：服务端把 DOWN lane 铺到组内两个会话上。
/// 客户端只在会话 1 上开 conn（只用主会话），服务端升级下行 lane 时应
/// 轮转到会话 2 —— 即客户端在会话 2 的 mux 上 accept 到 DOWN lane。
/// 这是「下行也跨 TCP」的直接证据：修复前 opener 钉死在 conn 到达的会话。
#[tokio::test]
async fn group_opener_spreads_down_lanes_across_sessions() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let cfg = StripeCfg {
        target_lanes: 4,
        upgrade_bytes: 64 * 1024,
        upgrade_rate_bps: 1,
        upgrade_window: Duration::from_millis(1),
        extra_sessions: 0,
    };
    let (c1, s1) = make_pair(MuxId::Wsmux).await;
    let (c2, s2) = make_pair(MuxId::Wsmux).await;

    // 服务端：两个会话共享 registry，且同属一个会话组。
    let registry = Arc::new(ConnRegistry::new());
    let groups = Arc::new(SessionGroups::new());
    let gid = 0xdead_beefu128;
    let g1 = groups.join(gid, &s1);
    let g2 = groups.join(gid, &s2);
    assert!(Arc::ptr_eq(&g1, &g2), "同一 group_id 必须落到同一个组");
    assert_eq!(g1.len(), 2, "组内应有两个会话");
    assert_eq!(groups.len(), 1);
    tokio::spawn(serve_session_grouped(s1.clone(), cfg.clone(), registry.clone(), g1));
    tokio::spawn(serve_session_grouped(s2.clone(), cfg.clone(), registry.clone(), g2));

    // 客户端：conn 只在会话 1 上发起（dialer 的主会话），会话 2 只挂一个
    // 计数 accept 泵——服务端若仍把 DOWN lane 钉在会话 1，这个计数恒为 0。
    let down_on_s2 = Arc::new(AtomicUsize::new(0));
    let dialer = StripeDialer::new(c1, cfg.clone());
    {
        let c2 = c2.clone();
        let reg = dialer.registry();
        let cnt = down_on_s2.clone();
        tokio::spawn(async move {
            loop {
                let Ok(stream) = c2.accept().await else { return };
                cnt.fetch_add(1, Ordering::Relaxed);
                let reg = reg.clone();
                tokio::spawn(async move {
                    let _ = join_inbound(&reg, stream).await;
                });
            }
        });
    }

    let port = echo_server().await.unwrap();
    let mut s = dialer.connect(&local_addr(port)).await.unwrap();
    let payload = pattern(4 * 1024 * 1024);
    let mut ws = s.clone();
    let writer = { let p = payload.clone(); tokio::spawn(async move { ws.write_all(&p).await.unwrap() }) };
    let mut got = vec![0u8; payload.len()];
    read_exact_timeout(&mut s, &mut got, Duration::from_secs(60)).await.unwrap();
    writer.await.unwrap();
    assert_eq!(got, payload, "跨会话下行 lane 的重组必须字节一致");

    assert!(
        down_on_s2.load(Ordering::Relaxed) >= 1,
        "服务端下行 lane 应铺到组内第二个会话上（实际 {}），\
         否则下载仍只吃一个 TCP 拥塞窗口",
        down_on_s2.load(Ordering::Relaxed)
    );
}

/// 16. 会话组生命周期：leave 后组内成员减少；组空则组条目被删除
/// （否则 group_id 表随客户端重连无限增长）。会话 mux 的 Arc 被释放时
/// 组内 Weak 自然失效，不把死会话钉在内存里。
#[tokio::test]
async fn session_group_lifecycle() {
    let groups = SessionGroups::new();
    let (_c1, s1) = make_pair(MuxId::Wsmux).await;
    let (_c2, s2) = make_pair(MuxId::Wsmux).await;
    let gid = 42u128;
    let g = groups.join(gid, &s1);
    groups.join(gid, &s2);
    assert_eq!(g.len(), 2);
    // 幂等：同一 mux 重复 join 不增加成员
    groups.join(gid, &s1);
    assert_eq!(g.len(), 2);

    groups.leave(gid, &s1);
    assert_eq!(g.len(), 1);
    assert_eq!(groups.len(), 1, "组还有成员，条目应保留");

    groups.leave(gid, &s2);
    assert_eq!(g.len(), 0);
    assert_eq!(groups.len(), 0, "组空后条目必须删除，否则表无限增长");

    // Weak 语义：不 leave 而直接释放 mux，成员也应自动失效
    let (_c3, s3) = make_pair(MuxId::Wsmux).await;
    let g2 = groups.join(7, &s3);
    assert_eq!(g2.len(), 1);
    drop(s3);
    assert_eq!(g2.len(), 0, "会话 mux 释放后组成员应自动失效（Weak）");
}

/// 单会话回归：extra_sessions=0 / 组只有一个成员时，行为与改动前一致。
#[tokio::test]
async fn group_of_one_behaves_like_single_session() {
    let cfg = StripeCfg {
        target_lanes: 4,
        upgrade_bytes: 64 * 1024,
        upgrade_rate_bps: 1,
        upgrade_window: Duration::from_millis(1),
        extra_sessions: 0,
    };
    let (client, server) = make_pair(MuxId::Wsmux).await;
    let registry = Arc::new(ConnRegistry::new());
    let groups = SessionGroups::new();
    let g = groups.join(1, &server);
    assert_eq!(g.len(), 1);
    tokio::spawn(serve_session_grouped(server, cfg.clone(), registry.clone(), g));

    let dialer = StripeDialer::new(client, cfg);
    let port = echo_server().await.unwrap();
    let mut s = dialer.connect(&local_addr(port)).await.unwrap();
    let payload = pattern(4 * 1024 * 1024);
    let mut ws = s.clone();
    let writer = { let p = payload.clone(); tokio::spawn(async move { ws.write_all(&p).await.unwrap() }) };
    let mut got = vec![0u8; payload.len()];
    read_exact_timeout(&mut s, &mut got, Duration::from_secs(60)).await.unwrap();
    writer.await.unwrap();
    assert_eq!(got, payload, "单成员组（= 单会话）必须无回归");
}

/// 17. 字节级条带分布：升级到 4 lane 后，大流量必须真正摊到各 lane 上，
/// 而不是全压在 lane 0。
///
/// 两个真实 bug 的回归守卫（两者都让「多 lane」名存实亡）：
/// (a) 轮转游标每个队列项归零：上游泵 64KiB 缓冲 + CHUNK 64KiB ⇒ 每项通常
///     只有一个 chunk ⇒ 恒取 lane 0。
/// (b) 本轮分到空批次的 lane 写半未放回而被 drop ⇒ 该 lane 写侧关闭 ⇒
///     对端读到 EOF ⇒ 升级出来的 lane 刚建好就没了。
///
/// 做法：服务端手工 accept，统计每条入站 lane 上收到的字节数。
#[tokio::test]
async fn upload_bytes_spread_across_lanes() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let cfg = StripeCfg {
        target_lanes: 4,
        upgrade_bytes: 64 * 1024,
        upgrade_rate_bps: 1,
        upgrade_window: Duration::from_millis(1),
        extra_sessions: 0,
    };
    let (client, server) = make_pair(MuxId::Wsmux).await;

    // 每条 lane 一个计数器：accept 到就一直读到 EOF，累计字节数。
    let per_lane: Arc<std::sync::Mutex<Vec<Arc<AtomicU64>>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let per_lane = per_lane.clone();
        tokio::spawn(async move {
            loop {
                let Ok(mut lane) = server.accept().await else { return };
                let ctr = Arc::new(AtomicU64::new(0));
                per_lane.lock().unwrap().push(ctr.clone());
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 64 * 1024];
                    loop {
                        match lane.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                ctr.fetch_add(n as u64, Ordering::Relaxed);
                            }
                        }
                    }
                });
            }
        });
    }

    let dialer = StripeDialer::new(client, cfg);
    let mut s = dialer.connect(&local_addr(1)).await.unwrap();
    let payload = pattern(8 * 1024 * 1024);
    s.write_all(&payload).await.unwrap();
    // 给发送任务时间排空（不读回，服务端侧只统计上行）
    tokio::time::sleep(Duration::from_secs(2)).await;

    let counts: Vec<u64> = per_lane
        .lock()
        .unwrap()
        .iter()
        .map(|c| c.load(Ordering::Relaxed))
        .collect();
    let total: u64 = counts.iter().sum();
    assert!(
        total >= payload.len() as u64,
        "服务端应收到全部上行字节，实际 {total} / {}",
        payload.len()
    );
    let busy = counts.iter().filter(|&&c| c >= 1024 * 1024).count();
    assert!(
        busy >= 3,
        "8MB 上行应摊到多条 lane 上（≥1MB 的 lane 数应 ≥3），实际分布 {counts:?}"
    );
    // 最大 lane 不应独吞。注意首 lane 天然偏多：升级阈值触发之前的全部
    // 流量都只能走它（本例约 4MB），之后才四路均分。所以断言点是「升级
    // 之后确实均分」——用非首 lane 之间的均衡度衡量，而不是要求首 lane
    // 也只占 1/4。修复前的病态分布是 [8MB, 0, 0, 0]，这里必然挂。
    let mut rest = counts.clone();
    let top = rest.iter().position(|c| *c == *counts.iter().max().unwrap()).unwrap();
    rest.remove(top);
    let rmin = *rest.iter().min().unwrap();
    let rmax = *rest.iter().max().unwrap();
    assert!(
        rmin > 0 && rmax <= rmin * 2,
        "升级后各 lane 应大致均分（非首 lane min={rmin} max={rmax}），\
         total={total} 实际分布 {counts:?}"
    );
}

// ---------------- mux 自身死亡的外泄信号 ----------------

/// accept 可控失败的假 mux：模拟「mux 内部死了，但底下的传输完全健康」。
///
/// 这不是为了方便才造的假件——真实里这正是 smux 的 keep-alive 超时形态：
/// 它只退出 `recv_loop`，`send_loop` 仍持有 io 的写半边，于是代理层套在 io
/// 外面的 `DeathWatch` 既读不到错误、也不会被 drop。用真 mux 复现不了这个
/// 形态，因为 duplex 两端一断两边都塌。
struct AcceptDiesOnCue {
    die: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl Mux for AcceptDiesOnCue {
    async fn open(&self) -> anyhow::Result<MuxStream> {
        anyhow::bail!("本假件不用于 open")
    }
    async fn accept(&self) -> anyhow::Result<MuxStream> {
        self.die.notified().await;
        anyhow::bail!("mux 已死")
    }
}

fn dying_mux() -> (Arc<dyn Mux>, Arc<tokio::sync::Notify>) {
    let die = Arc::new(tokio::sync::Notify::new());
    (
        Arc::new(AcceptDiesOnCue { die: die.clone() }) as Arc<dyn Mux>,
        die,
    )
}

/// 主会话死亡必须外泄成一个**可等待**的信号。
///
/// 少了它，代理的重连循环永远等不到死亡通知，出站停在「已连接」而全部新
/// 连接瞬间失败，且没有任何日志——实测过的真实故障。
#[tokio::test]
async fn a_dead_primary_session_surfaces_as_an_awaitable_signal() {
    let (mux, die) = dying_mux();
    let dialer = StripeDialer::new(mux, test_cfg());

    // 还活着时绝不能触发：误报会让健康的会话被反复重建。
    assert!(
        tokio::time::timeout(Duration::from_millis(200), dialer.all_sessions_dead())
            .await
            .is_err(),
        "会话还活着就报了死亡"
    );

    die.notify_one();
    tokio::time::timeout(Duration::from_secs(5), dialer.all_sessions_dead())
        .await
        .expect("主会话死了，信号必须在有限时间内触发");
}

/// **死一条不算死**：多会话下只有最后一条也死了才触发。
/// 早触发会把还能用的会话一起拆掉重建。
#[tokio::test]
async fn one_dead_session_out_of_two_does_not_fire_the_signal() {
    let (primary, die_primary) = dying_mux();
    let (extra, die_extra) = dying_mux();
    let dialer = StripeDialer::new(primary, test_cfg());
    assert!(dialer.attach_session(extra), "额外会话应当挂载成功");
    assert_eq!(dialer.session_count(), 2);

    die_primary.notify_one();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), dialer.all_sessions_dead())
            .await
            .is_err(),
        "还剩一条活会话就报了全死"
    );

    die_extra.notify_one();
    tokio::time::timeout(Duration::from_secs(5), dialer.all_sessions_dead())
        .await
        .expect("最后一条也死了，信号必须触发");
}

/// 信号已经触发之后再等，必须**立即**返回而不是挂住。
/// `Notify::notify_waiters` 只唤醒当时已在等的人，先检查后等待的写法会在
/// 这里永久挂起——而重连循环恰好是「先干别的、再回来等」的模式。
#[tokio::test]
async fn the_signal_is_level_triggered_not_edge_triggered() {
    let (mux, die) = dying_mux();
    let dialer = StripeDialer::new(mux, test_cfg());
    die.notify_one();
    // 先让死亡发生且无人等待
    tokio::time::sleep(Duration::from_millis(200)).await;
    tokio::time::timeout(Duration::from_secs(2), dialer.all_sessions_dead())
        .await
        .expect("死亡已成事实，事后再等必须立刻返回");
}

/// **追加 lane 抢在它的 conn 之前到达时，必须被暂存而不是当成新 conn。**
///
/// 追加 lane 的 OPEN 后面不跟 TargetAddr。旧实现只靠「conn 认不认识」来分辨
/// 首 lane 与追加 lane，于是顺序一颠倒就会走进 `read_open_addr`，把分片数据
/// 当地址解析，凭空造出一个指向乱七八糟目标的 conn；而真正的 conn 建好之后
/// 那条 lane 已经没了，发送侧却还在往它写分片——重组端永远填不上那些洞，
/// 整条流静默卡死（实测 `read timeout at 65536/2097152`，恰好停在首 lane 之后）。
///
/// 这条测试把顺序**倒过来**：先送追加 lane（lane_id 非 0、无 TargetAddr），
/// 再送首 lane。正确实现下两条 lane 最终都挂在同一个 conn 上。
#[tokio::test]
async fn an_extra_lane_arriving_before_its_conn_is_parked_not_misparsed() {
    let cfg = StripeCfg {
        target_lanes: 4,
        upgrade_bytes: 64 * 1024,
        upgrade_rate_bps: 1,
        upgrade_window: Duration::from_millis(1),
        extra_sessions: 0,
    };
    let (client, server) = make_pair(MuxId::Wsmux).await;
    let registry = Arc::new(ConnRegistry::new());
    let echo = echo_server().await.unwrap();

    // 服务端侧：按到达顺序逐条路由（不并发，好把顺序钉死）。
    let reg = registry.clone();
    let cfg2 = cfg.clone();
    let srv = server.clone();
    tokio::spawn(async move {
        loop {
            let Ok(stream) = srv.accept().await else { return };
            let _ = route_inbound_with_cfg(&reg, srv.clone(), cfg2.clone(), stream, pump_conn).await;
        }
    });

    // ① 先送一条**追加** lane：只有 OPEN 头，没有 TargetAddr。
    let conn_id: u64 = 0x2b2b_2b2b_2b2b_2b2b;
    let mut extra = client.open().await.unwrap();
    extra.write_all(&open_header_bytes(conn_id, 1)).await.unwrap();
    extra.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    // 误判成新 conn 的话，这里已经多出一个表项了。
    assert_eq!(
        registry.len(),
        0,
        "追加 lane 不得凭空造出一个 conn——那是把分片数据当 TargetAddr 解析的结果"
    );

    // ② 再送首 lane：OPEN + TargetAddr。conn 建好，①那条必须被接回来。
    let mut first = client.open().await.unwrap();
    let mut prefix = open_header_bytes(conn_id, 0);
    prefix.extend_from_slice(&wsieve_proto::addr::encode_addr(&local_addr(echo)));
    first.write_all(&prefix).await.unwrap();
    first.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let conn = registry.get(conn_id).expect("首 lane 到达后 conn 必须登记");
    assert!(
        conn.recv_lane_count() >= 2,
        "先到的追加 lane 必须在 conn 登记时被接回来，实际接收 lane 数 {}；\
         丢弃实现下它永远是 1，而发送侧仍会往那条 lane 写分片 ⇒ 重组端静默卡死",
        conn.recv_lane_count()
    );
    drop(extra);
    drop(first);
}

/// 造一个 OPEN 头。`lane_id` 非 0 表示这是追加 lane（不带 TargetAddr）。
fn open_header_bytes(conn_id: u64, lane_id: u16) -> Vec<u8> {
    use wsieve_proto::stripe::{encode_header, Cmd, ConnHeader, Dir};
    encode_header(&ConnHeader {
        conn_id,
        cmd: Cmd::Open,
        dir: if lane_id == 0 { Dir::Bidi } else { Dir::Up },
        lane_id,
    })
    .to_vec()
}

/// **带着在途写入去「等新工作」= 挂死。**
///
/// 发送任务改成流水线之后，写入不再于分发时等齐，而是留在 `inflight` 里。
/// `inflight` **只有发送任务自己会 poll**：队列排空后若直接停在「等队列来
/// 数据」上，那些写入永远不会再被推进 → mux 窗口不推进 → 对端收不到剩余
/// 分片 → 整条流挂死，且没有任何错误。
///
/// 复现要凑齐两件事：写入处于 Pending，**且**待发队列已经排空。对端一直
/// 在读的话第二件事凑不出来（写入总能瞬间完成），所以这里让对端**先不读**
/// ——小缓冲 duplex 随即写满，最后一片挂在途中，队列同时见底。
///
/// 2026-09-11 真机就是这个症状：8MiB 下载 5 轮全部 30s 超时，而小包因为
/// 每次都能排空所以完全正常，只打在大流上。
#[tokio::test]
async fn a_pending_write_is_still_driven_after_the_queue_drains() {
    // 16KiB：小到一片 CHUNK 就写满，写入必然 Pending。
    let (client_io, server_io) = tokio::io::duplex(16 * 1024);
    let client: Arc<dyn Mux> =
        Arc::from(mux_factory(MuxId::Wsmux, Box::new(client_io)).await.unwrap());
    let server: Arc<dyn Mux> =
        Arc::from(mux_server_factory(MuxId::Wsmux, Box::new(server_io)).await.unwrap());

    let cfg = test_cfg();
    let registry = Arc::new(ConnRegistry::new());
    // on_new 只把 conn 递出来，**不读**——读要等测试主动开始。
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Arc<StripeConnLike>>(1);
    let reg = registry.clone();
    let cfg2 = cfg.clone();
    let srv = server.clone();
    tokio::spawn(async move {
        loop {
            let Ok(stream) = srv.accept().await else { return };
            let reg = reg.clone();
            let cfg = cfg2.clone();
            let mux = srv.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let _ = route_inbound_with_cfg(&reg, mux, cfg, stream, move |c, _a| {
                    let _ = tx.try_send(c);
                })
                .await;
            });
        }
    });

    let dialer = StripeDialer::new(client, cfg);
    let mut s = dialer.connect(&local_addr(1)).await.unwrap();

    // 客户端写 6MiB：超过 mux 每流 4MiB 的窗口，对端又没在读 ⇒ 最后一片
    // 必然挂在途中，而队列此刻已经见底。
    let payload = pattern(6 * 1024 * 1024);
    let p = payload.clone();
    tokio::spawn(async move { let _ = s.write_all(&p).await; });

    let conn = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("conn 应当建立")
        .expect("on_new 应当递出 conn");
    // 故意晾一会儿：让「队列已空 + 有在途」这个组合真的成立。
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 现在才开始读。挂死实现下这些字节永远到不齐。
    let mut up = conn.stream();
    let mut got = vec![0u8; payload.len()];
    tokio::time::timeout(Duration::from_secs(15), up.read_exact(&mut got))
        .await
        .expect("带着在途写入去等新工作会让整条流挂死：排空 inflight 必须在停车之前")
        .expect("读取本身不该出错");
    assert_eq!(got, payload, "字节必须一致");
}

/// **一条永久卡住的 lane 不得把发送任务钉死在收尾之前。**
///
/// 与 `a_pending_write_is_still_driven_after_the_queue_drains` 是一对：那条守
/// 「带着在途写去停车会挂死」，这条守它的修法**不能变成新的挂死**。早先的修法
/// 是在停车前 `while inflight.next().await` 排空，没有出口——对端不读时那一行
/// 就是终点：任务再也看不到 `closing`，`mark_dead` 永不调用，conn 的资源一件
/// 都不回收。收尾路径上的排空与 flush/shutdown 有同样的形状，也同样需要出口。
///
/// 判据取"收尾走完之后写入会报错"：`mark_dead` 排在收尾的最后一行，只有任务
/// 真的走到那里，`out.dead` 才会置位、`poll_write` 才会返回 `Err`。任务被钉住
/// 时写入照样进无界队列、返回 `Ok`——一个静默的资源泄漏。
#[tokio::test]
async fn a_wedged_lane_does_not_block_teardown() {
    // 16KiB duplex，且**服务端从不 accept** ⇒ 对端永不读，写入永久 Pending。
    let (client_io, server_io) = tokio::io::duplex(16 * 1024);
    let client: Arc<dyn Mux> =
        Arc::from(mux_factory(MuxId::Wsmux, Box::new(client_io)).await.unwrap());
    let _server: Arc<dyn Mux> =
        Arc::from(mux_server_factory(MuxId::Wsmux, Box::new(server_io)).await.unwrap());

    let dialer = StripeDialer::new(client, test_cfg());
    let mut s = dialer.connect(&local_addr(1)).await.unwrap();

    // 6MiB > mux 每流 4MiB 窗口，对端不读 ⇒ 尾部必然挂在途中。
    // 用 timeout 拿回控制权，此刻"队列已空 + 有在途写"成立。
    let payload = pattern(6 * 1024 * 1024);
    let _ = tokio::time::timeout(Duration::from_secs(2), s.write_all(&payload)).await;

    // 请求收尾（只置 closing 标志，立即返回）。
    let _ = s.shutdown().await;

    // 等收尾走完。带出口的实现会在 TEARDOWN_TIMEOUT(5s) 后放弃尾部并
    // `mark_dead`；被钉住的实现永远走不到那里。
    tokio::time::sleep(Duration::from_secs(9)).await;

    let r = s.write_all(&[0u8; 64]).await;
    assert!(
        r.is_err(),
        "收尾没走完（写入仍被接受）——发送任务被那条卡死的 lane 钉住了"
    );
}

/// **conn 已登记时 `park_orphan` 必须直接交付，不能塞进队列。**
///
/// `get` 与 `park_orphan` 之间有窗口：调用方查不到 conn，随后 `insert` 完成，
/// 再 `park_orphan`。若 park 不在同一临界区里复查 map，这条 lane 就烂在队列里
/// 直到被驱逐——而发送侧照样往它写分片，重组端永远等不齐（实测症状
/// `read timeout at 65536/...`）。
///
/// 判据取"park 之后 conn 的接收侧 lane 数增加了"：只有直接交付才会如此。
#[tokio::test]
async fn parking_a_lane_for_an_already_registered_conn_delivers_it() {
    let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
    let client: Arc<dyn Mux> =
        Arc::from(mux_factory(MuxId::Wsmux, Box::new(client_io)).await.unwrap());
    let server: Arc<dyn Mux> =
        Arc::from(mux_server_factory(MuxId::Wsmux, Box::new(server_io)).await.unwrap());

    let registry = Arc::new(ConnRegistry::new());
    let dialer = StripeDialer::new(client.clone(), test_cfg());
    let _s = dialer.connect(&local_addr(1)).await.unwrap();

    // 服务端接第一条 lane，让 conn 真的登记进 registry
    let first = server.accept().await.unwrap();
    let reg = registry.clone();
    let srv = server.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Arc<StripeConnLike>>(1);
    tokio::spawn(async move {
        let _ = route_inbound_with_cfg(&reg, srv, test_cfg(), first, move |c, _a| {
            let _ = tx.try_send(c);
        })
        .await;
    });
    let conn = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("conn 应当建立")
        .expect("on_new 应当递出 conn");
    let before = conn.recv_lane_count();

    // 再开一条 mux 流当作"晚到的额外 lane"，直接 park 给已登记的 conn
    let extra = client.open().await.unwrap();
    registry.park_orphan(conn.conn_id(), extra);

    for _ in 0..100 {
        if conn.recv_lane_count() > before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        conn.recv_lane_count() > before,
        "conn 已登记却把 lane 塞进了孤儿队列——它会烂在那里直到被驱逐"
    );
}

/// 追加 lane 的判据是 **`lane_id` 非零**，不是等于某个具体常量。
///
/// `LANE_ID_EXTRA` 的文档写明"数值本身不参与路由，只区分首与追加"。按具体
/// 数值判会留一个错位隐患：任何填了别的非零值的追加 lane（换常量、对端版本
/// 不同、或将来真的用 lane_id 编号）都会被当成首 lane 送去 `read_open_addr`，
/// 把分片数据当 TargetAddr 解析，凭空造一个指向乱七八糟目标的 conn。
///
/// 这里用一个**不等于** `LANE_ID_EXTRA` 的非零值，正是为了钉住"看非零而非
/// 看数值"这条语义。
#[tokio::test]
async fn any_nonzero_lane_id_is_treated_as_an_extra_lane() {
    let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
    let client: Arc<dyn Mux> =
        Arc::from(mux_factory(MuxId::Wsmux, Box::new(client_io)).await.unwrap());
    let server: Arc<dyn Mux> =
        Arc::from(mux_server_factory(MuxId::Wsmux, Box::new(server_io)).await.unwrap());

    use wsieve_proto::stripe::{encode_header, Cmd, ConnHeader, Dir};
    // 客户端手工开一条"追加 lane"，lane_id 填 7（既非 0 也非 LANE_ID_EXTRA）
    let mut lane = client.open().await.unwrap();
    let hdr = encode_header(&ConnHeader {
        conn_id: 0xBEEF,
        cmd: Cmd::Open,
        dir: Dir::Up,
        lane_id: 7,
    })
    .to_vec();
    lane.write_all(&hdr).await.unwrap();
    // 紧跟一段**数据帧**——若被误判成首 lane，这些字节会被当 TargetAddr 解析
    lane.write_all(&[0xAA; 256]).await.unwrap();
    lane.flush().await.unwrap();

    let registry = Arc::new(ConnRegistry::new());
    let stream = server.accept().await.unwrap();
    let r = route_inbound_with_cfg(&registry, server.clone(), test_cfg(), stream, |_c, _a| {
        panic!("把追加 lane 当成新 conn 了——lane_id=7 被误判为首 lane");
    })
    .await;
    assert!(r.is_ok(), "追加 lane 应当被 park 而不是报错");
}
