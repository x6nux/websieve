//! 本地多端口 TCP 转发器：多会话条带的落地件（配合 `hosts`）。
//!
//! WebView 看到的是同一域名的 N 个端口 ⇒ N 个 origin ⇒ N 条独立 TLS 连接
//! （h2 只在同 origin 内复用）。本转发器把每条入站连接原样搬到真实服务端
//! 的 :443，于是每个会话拿到自己的 TCP、自己的拥塞窗口。
//!
//! **绝不终结 TLS**：只做字节搬运，不解密、不看内容、不碰证书。一旦在这里
//! 终结 TLS，握手就变成 rustls 发的，整个项目「用真实浏览器指纹」的前提
//! 当场作废。ClientHello 与其后的一切都由 WebView 直接与真实服务端完成。
//!
//! 「一进一出、绝不池化」是本模块的核心不变量：若把多条入站连接汇聚到一条
//! 出站连接上，就等于自己把 h2 复用又做了一遍，整个特性归零。
//!
//! **预建 TCP（设计文档 §9.4 优化②）**：入站到达后才 connect 上游，等于把
//! 一次跨国 RTT（可达 200ms+）串在每条连接的关键路径上。预热池提前备好若干
//! 条，用掉即补。它**不违反**上面的不变量 —— 禁的是「多条入站汇聚到一条
//! 出站」的复用，而预建仍严格一进一出：每条入站独占一条预建连接，用过就
//! 丢，绝不还池。

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// 每个端口预建几条。
///
/// 少即是多：预建连接在服务端看来是一批建立后长时间不说话的 TCP，数量一多
/// 就与端口扫描难以区分。1–2 条足以覆盖「页面加载时几条会话几乎同时发起」
/// 这个真实场景。
///
/// `ponytail:` 2 是拍脑袋的初值，**待实测**（设计文档 §15 待实测项 #2）。
/// 升级路径：按实测的并发起始峰值调整，或做成配置项。
pub const PREWARM_DEPTH: usize = 2;

/// 预建连接的有效期：超过这么久没被用掉就丢弃重建。
///
/// 中间设备（NAT、防火墙、负载均衡）与服务端都会回收长时间空闲的连接，
/// 且多数是**静默**回收 —— 我们这端要到下次写才发现已经断了。与其把一条
/// 可疑的连接交给用户，不如定期换新。
///
/// `ponytail:` 45s 是拍脑袋的初值，**待实测**（同上）。典型 NAT 空闲超时在
/// 60s–300s 之间，取一个明显低于下限的值。
pub const PREWARM_TTL: Duration = Duration::from_secs(45);

/// 一条预建好的上游连接，连同它的出生时间。
struct Prewarmed {
    stream: TcpStream,
    born: Instant,
}

impl Prewarmed {
    fn expired(&self) -> bool {
        self.born.elapsed() >= PREWARM_TTL
    }

    /// 取用前探活：非阻塞读一次。
    ///
    /// 闲置期间被中间设备 RST 或被服务端 GC 掉的连接，在这里表现为「可读且
    /// 读到 0 字节（EOF）」或直接出错。不探的话，用户会遇到一次莫名其妙的
    /// 失败 —— 而且是**这次**请求失败，重试才好，最难查的那种。
    ///
    /// 健康的连接此刻应当无数据可读（服务端还没收到任何请求，不会主动说话），
    /// 即 `WouldBlock`。**读到了数据同样判为不健康**：上游在我们发出请求前
    /// 就说话，说明这不是一条干净的连接（或根本不是我们以为的那个服务）。
    fn is_healthy(&self) -> bool {
        let mut buf = [0u8; 1];
        match self.stream.try_read(&mut buf) {
            // EOF：对端已关闭
            Ok(0) => false,
            // 不该有的数据
            Ok(_) => false,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(_) => false,
        }
    }
}

/// 转发器句柄：drop 即停（listener 任务随 abort 结束）。
pub struct Forwarder {
    ports: Vec<u16>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Forwarder {
    /// 实际监听到的端口（按会话序）。
    pub fn ports(&self) -> &[u16] {
        &self.ports
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// 在 127.0.0.1 的 `base_port..base_port+count` 上监听，每条入站连接配一条
/// 到 `upstream` 的**独立** TCP 并双向搬运。
///
/// 端口被占用即失败返回——静默跳过会让会话数与端口数对不上，条带按序取端口
/// 时就会连到别人的服务上。
pub async fn spawn(base_port: u16, count: usize, upstream: SocketAddr) -> anyhow::Result<Forwarder> {
    spawn_with_prewarm(base_port, count, upstream, PREWARM_DEPTH).await
}

/// 同 `spawn`，但可指定每个端口的预热深度（`0` = 关闭预建，退回懒连接）。
pub async fn spawn_with_prewarm(
    base_port: u16,
    count: usize,
    upstream: SocketAddr,
    prewarm: usize,
) -> anyhow::Result<Forwarder> {
    let mut ports = Vec::with_capacity(count);
    let mut tasks = Vec::with_capacity(count);
    for i in 0..count {
        let port = base_port
            .checked_add(i as u16)
            .ok_or_else(|| anyhow::anyhow!("端口号溢出: {base_port}+{i}"))?;
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|e| anyhow::anyhow!("监听 127.0.0.1:{port} 失败: {e}"))?;
        ports.push(port);

        // 每个端口一条预热通道。容量即目标数量：补充任务写满就阻塞，
        // 天然实现「用掉即补、不多不少」。
        let pool = if prewarm > 0 {
            let (tx, rx) = mpsc::channel::<Prewarmed>(prewarm);
            tasks.push(tokio::spawn(prewarm_loop(tx, upstream)));
            Some(rx)
        } else {
            None
        };
        tasks.push(tokio::spawn(accept_loop(listener, upstream, pool)));
    }
    tracing::info!("本地条带转发器就绪: {ports:?} -> {upstream}（每口预建 {prewarm} 条）");
    Ok(Forwarder { ports, tasks })
}

/// 持续把预热池补满。
///
/// `send` 在通道满时挂起，因此这个循环天然是「缺几条补几条」，不需要计数。
/// 连不上时退避重试而非放弃：上游只是暂时不可达的话，放弃就等于永久退回
/// 懒连接，而那正是本优化要消除的那一次 RTT。
async fn prewarm_loop(tx: mpsc::Sender<Prewarmed>, upstream: SocketAddr) {
    let mut backoff = Duration::from_millis(100);
    loop {
        // 先占坑再连：`reserve` 在池满时挂起，避免「连上了却没地方放」而
        // 白建一条连接扔掉——那在服务端看来就是无谓的连接抖动。
        let Ok(permit) = tx.reserve().await else {
            return; // 接收端没了 = 转发器停了
        };
        match TcpStream::connect(upstream).await {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                backoff = Duration::from_millis(100);
                permit.send(Prewarmed {
                    stream,
                    born: Instant::now(),
                });
            }
            Err(e) => {
                // 预建失败不影响可用性（取用时会即时新建），但绝不静默：
                // 上游不可达是真问题，只是不该由这条路径来报警。
                tracing::debug!("预建到 {upstream} 的连接失败（{e}），{backoff:?} 后重试");
                drop(permit);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
            }
        }
    }
}

/// 从池里取一条**健康且未过期**的连接。取不到就返回 None，由调用方即时新建。
///
/// 过期或不健康的一律丢弃并接着往下取：把一条可疑连接交给用户，换来的是
/// 一次无法复现的失败。
fn take_prewarmed(pool: &mut mpsc::Receiver<Prewarmed>) -> Option<TcpStream> {
    while let Ok(c) = pool.try_recv() {
        if c.expired() {
            tracing::debug!("丢弃过期的预建连接（{:?}）", c.born.elapsed());
            continue;
        }
        if !c.is_healthy() {
            tracing::debug!("丢弃已失效的预建连接（对端已关闭或有意外数据）");
            continue;
        }
        return Some(c.stream);
    }
    None
}

async fn accept_loop(
    listener: TcpListener,
    upstream: SocketAddr,
    mut pool: Option<mpsc::Receiver<Prewarmed>>,
) {
    loop {
        let Ok((inbound, _)) = listener.accept().await else {
            return;
        };
        // 每条入站连接一条**全新**出站连接：这正是多拥塞窗口的来源，
        // 任何形式的复用都会让特性归零。预建只是把「新建」这个动作提前，
        // 取走的连接不还池、不共享。
        let ready = pool.as_mut().and_then(take_prewarmed);
        tokio::spawn(async move {
            if let Err(e) = relay(inbound, upstream, ready).await {
                tracing::debug!("转发结束: {e}");
            }
        });
    }
}

async fn relay(
    mut inbound: TcpStream,
    upstream: SocketAddr,
    ready: Option<TcpStream>,
) -> anyhow::Result<()> {
    // 有预建的就用，没有（池空/刚被丢弃）就即时新建——预建是优化，
    // 不是前提，池空绝不能让连接失败。
    let mut outbound = match ready {
        Some(s) => s,
        None => TcpStream::connect(upstream).await?,
    };
    let _ = inbound.set_nodelay(true);
    let _ = outbound.set_nodelay(true);
    // 纯字节搬运：TLS 记录原样过境，握手是 WebView 与真实服务端之间的事。
    tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await?;
    Ok(())
}

/// 解析真实服务端地址。
///
/// **必须走 bootstrap 解析器，绝不能走我们自己的 DNS**（设计文档 §7.2 纪律①）。
/// 也**必须在写 hosts 之前调用**：hosts 一旦把域名指向 127.0.0.1，解析器
/// 就会返回本地地址，转发器再解析就指向自己，形成死循环。
///
/// 两道防线针对的是两件不同的事，缺一不可：
/// - 「先解析后写 hosts」防的是**我们自己**写进 hosts 的那一行
/// - 「走 bootstrap」防的是阶段 6 的 **fake-ip**：届时系统 DNS 查询会被
///   劫持并返回 198.18.x.x，转发器会连向虚空，且完全静默 —— 表现为
///   「握手一直不成功」，没有任何一条日志会说是解析出了假 IP
///
/// 收的是 `&TokioResolver`（bootstrap 那一族）而**不是** `DnsResolver`，
/// 这不是风格问题：类型不同，判决用的那个解析器根本递不进来，纪律①因此
/// 由编译器把关，而不是靠调用方自觉。
pub async fn resolve_upstream(
    boot: &wsieve_dns::TokioResolver,
    host: &str,
    port: u16,
) -> anyhow::Result<SocketAddr> {
    let lookup = boot
        .lookup_ip(host)
        .await
        .map_err(|e| anyhow::anyhow!("bootstrap 解析 {host} 失败: {e}"))?;
    let addrs: Vec<SocketAddr> = lookup.iter().map(|ip| SocketAddr::new(ip, port)).collect();
    // 优先 IPv4：hosts 里我们只写 127.0.0.1，链路两端保持同族更少意外。
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.first())
        .copied()
        .ok_or_else(|| anyhow::anyhow!("{host}:{port} 解析不到地址"))
}

/// 若解析结果落在环回段，说明 hosts 已被（上一次运行的残留）劫持，
/// 此时启动转发器只会自己转给自己。
pub fn is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

/// 本地条带的运行态：一批转发器 + 一份 hosts 托管，drop 时自动摘除 hosts 条目。
///
/// 摘除动作不在这里写 —— 它归 `CustodyGuard<HostsCustody>`（设计文档 §10）。
/// 本结构只负责「转发器与 hosts 条目同生共死」：字段顺序即 drop 顺序，
/// **先摘 hosts 再停转发器**，反过来的话中间那一小段时间里域名已经指向
/// 一个刚被 abort 的监听口，本机访问该域名会失败。
///
/// 持有的是**一批**转发器而非一个：`shard_setup::plan_many` 把多个出站的
/// hosts 写入合并成一次原子操作（见该模块的文档），因此它们的转发器也必须
/// 同生共死——都随这一份 hosts 托管一起摘除，而不是各自一份 guard。
pub struct ShardGuard {
    /// 持有即生效：drop 时摘除 hosts 托管条目。
    _hosts: crate::custody::CustodyGuard<crate::custody::hosts::HostsCustody>,
    /// 持有即保活：drop 时每个 listener 任务被 abort。
    _forwarders: Vec<Forwarder>,
}

impl ShardGuard {
    pub fn new(
        forwarders: Vec<Forwarder>,
        hosts: crate::custody::CustodyGuard<crate::custody::hosts::HostsCustody>,
    ) -> Self {
        Self {
            _hosts: hosts,
            _forwarders: forwarders,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 假上游：回显 + 统计 accept 次数。
    async fn echo_upstream(accepts: Arc<AtomicUsize>) -> SocketAddr {
        let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                accepts.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
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
        addr
    }

    /// 核心不变量：N 条入站 ⇒ N 条独立出站。若被池化，整个多拥塞窗口
    /// 的前提就没了，特性归零。
    ///
    /// 这里刻意关掉预建（深度 0），好让计数是**精确**的而非「至少」：
    /// 预建会往上游发起与入站无关的连接，混在一起就只能断言下界，而下界
    /// 断言恰恰放得过复用（复用少建的那几条会被预建补回来，看不出来）。
    /// 开着预建时的同一条不变量由 `each_inbound_still_gets_its_own_upstream_connection`
    /// 负责，两条合起来才把这条不变量锁死。
    #[tokio::test]
    async fn each_inbound_gets_its_own_upstream_connection() {
        let accepts = Arc::new(AtomicUsize::new(0));
        let upstream = echo_upstream(accepts.clone()).await;
        // 端口 0 不能用于连续分配，取一段大概率空闲的高端口
        let base = 39411;
        let fwd = spawn_with_prewarm(base, 3, upstream, 0).await.unwrap();
        assert_eq!(fwd.ports(), &[base, base + 1, base + 2]);

        let mut conns = Vec::new();
        for (i, p) in fwd.ports().iter().enumerate() {
            let mut c = TcpStream::connect(("127.0.0.1", *p)).await.unwrap();
            let msg = format!("hello-{i}");
            c.write_all(msg.as_bytes()).await.unwrap();
            let mut buf = vec![0u8; msg.len()];
            c.read_exact(&mut buf).await.unwrap();
            assert_eq!(String::from_utf8(buf).unwrap(), msg, "端口 {p} 回显不符");
            conns.push(c);
        }
        assert_eq!(accepts.load(Ordering::SeqCst), 3, "每条入站必须对应一条独立出站");

        // 同一端口再开一条，仍应是新的出站连接（绝不复用）
        let mut extra = TcpStream::connect(("127.0.0.1", fwd.ports()[0])).await.unwrap();
        extra.write_all(b"again").await.unwrap();
        let mut buf = [0u8; 5];
        extra.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"again");
        assert_eq!(accepts.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn bidirectional_payload_is_relayed_intact() {
        let accepts = Arc::new(AtomicUsize::new(0));
        let upstream = echo_upstream(accepts.clone()).await;
        let fwd = spawn(39421, 1, upstream).await.unwrap();
        let mut c = TcpStream::connect(("127.0.0.1", fwd.ports()[0])).await.unwrap();
        // 大于单次读缓冲，确保跨多次 read/write 仍字节精确
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let w = payload.clone();
        let (mut r, mut wr) = c.split();
        let writer = async move { wr.write_all(&w).await.unwrap() };
        let mut got = vec![0u8; payload.len()];
        let reader = async { r.read_exact(&mut got).await.unwrap() };
        tokio::join!(writer, reader);
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn port_conflict_is_reported_not_skipped() {
        // 端口被占却静默跳过的话，会话按序取端口就会连到别人的服务上。
        let blocker = TcpListener::bind(("127.0.0.1", 39431)).await.unwrap();
        let accepts = Arc::new(AtomicUsize::new(0));
        let upstream = echo_upstream(accepts).await;
        let e = match spawn(39431, 2, upstream).await {
            Ok(_) => panic!("端口被占用时必须报错"),
            Err(e) => e,
        };
        assert!(e.to_string().contains("39431"), "错误里应指明冲突端口: {e}");
        drop(blocker);
    }

    // ── 预建 TCP（优化②）──

    /// 起一个带预热的转发器。
    async fn spawn_prewarmed(
        base: u16,
        count: usize,
        upstream: SocketAddr,
        prewarm: usize,
    ) -> anyhow::Result<Forwarder> {
        spawn_with_prewarm(base, count, upstream, prewarm).await
    }

    #[tokio::test]
    async fn prewarmed_connection_is_used_and_replenished() {
        let accepts = Arc::new(AtomicUsize::new(0));
        let upstream = echo_upstream(accepts.clone()).await;
        let fwd = spawn_prewarmed(39441, 1, upstream, 2).await.unwrap();

        // 预热完成后，上游应已看到 2 条连接，而客户端一条都没发起
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(accepts.load(Ordering::SeqCst), 2, "应预建 2 条");

        // 用掉一条
        let mut c = TcpStream::connect(("127.0.0.1", fwd.ports()[0])).await.unwrap();
        c.write_all(b"hi").await.unwrap();
        let mut b = [0u8; 2];
        c.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"hi", "预建连接必须真的能用来搬字节");

        // 补回来
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(accepts.load(Ordering::SeqCst), 3, "用掉一条要补一条");
    }

    #[tokio::test]
    async fn each_inbound_still_gets_its_own_upstream_connection() {
        // 核心不变量不能被预建破坏：绝不复用
        let accepts = Arc::new(AtomicUsize::new(0));
        let upstream = echo_upstream(accepts.clone()).await;
        let fwd = spawn_prewarmed(39451, 1, upstream, 2).await.unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        let base = accepts.load(Ordering::SeqCst);

        let mut conns = Vec::new();
        for _ in 0..3 {
            conns.push(TcpStream::connect(("127.0.0.1", fwd.ports()[0])).await.unwrap());
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        // 3 条入站 ⇒ 至少 3 条新出站（预建的被消耗 + 补充）
        assert!(
            accepts.load(Ordering::SeqCst) >= base + 3,
            "绝不能复用：3 条入站至少要 3 条新出站，实际只多了 {}",
            accepts.load(Ordering::SeqCst) - base
        );

        // 而且每条入站都要独立可用——复用的话字节会串到别人那里去
        for (i, c) in conns.iter_mut().enumerate() {
            let msg = format!("c{i}");
            c.write_all(msg.as_bytes()).await.unwrap();
            let mut buf = vec![0u8; msg.len()];
            c.read_exact(&mut buf).await.unwrap();
            assert_eq!(String::from_utf8(buf).unwrap(), msg);
        }
    }

    #[tokio::test]
    async fn stale_prewarmed_connection_is_discarded_not_served() {
        // 闲置太久的预建连接可能已被中间设备 RST。取用前必须探活，
        // 否则用户会遇到一次莫名其妙的失败。
        //
        // 构造法：让上游在 accept 后立刻关掉连接（模拟被 GC / RST），
        // 于是池里躺着的全是死连接。取用时必须识别出来并即时新建，
        // 而不是把死连接交给客户端。
        let accepts = Arc::new(AtomicUsize::new(0));
        let kill_first = Arc::new(AtomicBool::new(true));

        let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let upstream = l.local_addr().unwrap();
        let a2 = accepts.clone();
        let k2 = kill_first.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                a2.fetch_add(1, Ordering::SeqCst);
                if k2.load(Ordering::SeqCst) {
                    // 立刻关闭：这条进池后就是一具尸体
                    drop(s);
                    continue;
                }
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
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

        let fwd = spawn_prewarmed(39461, 1, upstream, 2).await.unwrap();
        // 等预热跑几轮，池里攒下死连接
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(accepts.load(Ordering::SeqCst) >= 2, "应已预建过");

        // 从现在起上游正常服务
        kill_first.store(false, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(150)).await;

        // 客户端此刻发起请求：即使池里可能还有死连接，也必须回显成功。
        // 不探活的话，这里会读到 EOF 而不是回显。
        let mut c = TcpStream::connect(("127.0.0.1", fwd.ports()[0])).await.unwrap();
        c.write_all(b"alive?").await.unwrap();
        let mut buf = [0u8; 6];
        tokio::time::timeout(Duration::from_secs(5), c.read_exact(&mut buf))
            .await
            .expect("死掉的预建连接必须被丢弃并即时新建，不能拿去服务")
            .expect("回显应当成功");
        assert_eq!(&buf, b"alive?");
    }

    #[tokio::test]
    async fn an_expired_prewarmed_connection_is_never_served() {
        // 有效期是第二道防线：连接看着还活（探活过得去），但躺得太久，
        // 中间设备随时可能在下一次写时把它 RST 掉。宁可换新。
        let accepts = Arc::new(AtomicUsize::new(0));
        let upstream = echo_upstream(accepts.clone()).await;
        let (tx, mut rx) = mpsc::channel::<Prewarmed>(2);
        let stream = TcpStream::connect(upstream).await.unwrap();
        tx.send(Prewarmed {
            stream,
            // 出生于 TTL 之前 —— 已经过期
            born: Instant::now() - PREWARM_TTL - Duration::from_secs(1),
        })
        .await
        .unwrap();
        assert!(
            take_prewarmed(&mut rx).is_none(),
            "过期的连接必须被丢弃，而不是交给用户"
        );
    }

    #[tokio::test]
    async fn a_fresh_prewarmed_connection_passes_the_health_check() {
        // 反向对照：探活不能把好连接也判死，否则预建就完全白做了
        // （每次都丢弃重建，还多了一次无谓的连接）。
        let accepts = Arc::new(AtomicUsize::new(0));
        let upstream = echo_upstream(accepts.clone()).await;
        let (tx, mut rx) = mpsc::channel::<Prewarmed>(2);
        let stream = TcpStream::connect(upstream).await.unwrap();
        tx.send(Prewarmed {
            stream,
            born: Instant::now(),
        })
        .await
        .unwrap();
        assert!(
            take_prewarmed(&mut rx).is_some(),
            "刚建好的健康连接必须能被取用"
        );
    }

    #[tokio::test]
    async fn an_empty_pool_falls_back_to_connecting_on_demand() {
        // 预建是优化，不是前提。池空（或预热深度为 0）时必须照常工作，
        // 否则一次上游抖动就会让转发器彻底失灵。
        let accepts = Arc::new(AtomicUsize::new(0));
        let upstream = echo_upstream(accepts.clone()).await;
        let fwd = spawn_prewarmed(39471, 1, upstream, 0).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(accepts.load(Ordering::SeqCst), 0, "深度 0 就不该预建任何连接");

        let mut c = TcpStream::connect(("127.0.0.1", fwd.ports()[0])).await.unwrap();
        c.write_all(b"lazy").await.unwrap();
        let mut buf = [0u8; 4];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"lazy");
        assert_eq!(accepts.load(Ordering::SeqCst), 1, "退回懒连接，照常工作");
    }

    #[tokio::test]
    async fn the_pool_never_grows_beyond_its_depth() {
        // 数量要少：在服务端看来，一批建立后长时间不说话的连接与端口扫描
        // 难以区分。补充循环若不受限，闲置时会无限建连。
        let accepts = Arc::new(AtomicUsize::new(0));
        let upstream = echo_upstream(accepts.clone()).await;
        let _fwd = spawn_prewarmed(39481, 2, upstream, 2).await.unwrap();
        // 给足时间：若补充循环失控，这段时间足够建出几十条
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            accepts.load(Ordering::SeqCst),
            4,
            "2 个端口 × 深度 2 = 4 条，一条不多"
        );
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback(&"127.0.0.1:443".parse().unwrap()));
        assert!(is_loopback(&"[::1]:443".parse().unwrap()));
        assert!(!is_loopback(&"1.2.3.4:443".parse().unwrap()));
    }

    // ── 纪律①：出站域名走 bootstrap 解析器（设计文档 §7.2）──

    /// 解析确实由 **bootstrap 解析器**完成，且 IPv4 优先仍然生效。
    ///
    /// 这里用真解析器（`wsieve_dns::bootstrap()`，读系统配置的那一个）解
    /// `localhost` —— 脱外网、每台机器都有、结果确定。
    ///
    /// 挑 `localhost` 不是图省事，是因为它**双栈且 IPv6 在前**：本机实测
    /// `bootstrap()` 返回 `[::1, 127.0.0.1]`。于是「优先 IPv4」这条逻辑真的
    /// 被考到了 —— 若谁把它改成朴素的 `.first()`，拿到的会是 `[::1]`，
    /// 下面的断言立刻变红。换个单栈域名来测，这条断言就成了摆设。
    #[tokio::test]
    async fn resolve_upstream_goes_through_bootstrap_and_prefers_ipv4() {
        let boot = wsieve_dns::bootstrap().expect("系统 DNS 配置应可读");
        let addr = resolve_upstream(&boot, "localhost", 8443)
            .await
            .expect("localhost 在任何机器上都解析得出来");
        assert_eq!(
            addr,
            "127.0.0.1:8443".parse::<SocketAddr>().unwrap(),
            "双栈结果里必须挑 IPv4；拿到 [::1] 说明「优先 IPv4」被改坏了"
        );
    }

    /// 解析失败**必须报错**，绝不静默返回一个凑合的地址。
    ///
    /// 上游指向本机一个**没人监听**的 UDP 端口 —— 不用 RFC 5737 的
    /// `192.0.2.1`：本机实测那个地址 5ms 就返回了 `[fc00::d1, 198.18.0.211]`
    /// （链路上的 DNS 拦截给的 fake-ip，198.18.0.0/15 恰是 fake-ip 段），
    /// 拿它当黑洞会让测试在别人的机器上随机变红。本地端口是我们自己选的，
    /// 没有任何中间人能替它回话。
    ///
    /// 这条锁的是 `shard_setup` 的降级路径有东西可降级：解析失败要能被
    /// `match ... Err(e)` 接住并打出告警，而不是拿到一个错的地址继续往下跑
    /// —— 后者会把转发器指向虚空，且完全静默。
    #[tokio::test]
    async fn a_bootstrap_failure_is_reported_not_swallowed() {
        // 先占下一个 UDP 端口拿到号，再释放：这样端口号确定且几乎不可能
        // 在测试期间被别人抢去回话。
        let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let dead_port = probe.local_addr().unwrap().port();
        drop(probe);

        let boot = wsieve_dns::bootstrap_with(&[format!("udp://127.0.0.1:{dead_port}")])
            .expect("上游是 IP 字面量，应能建起来");
        let e = resolve_upstream(&boot, "no-such-host.invalid", 443)
            .await
            .expect_err("上游不可达时必须报错，不能返回一个凑合的地址");
        let msg = e.to_string();
        assert!(
            msg.contains("no-such-host.invalid"),
            "错误要点名是解析谁失败了，否则排查时无从下手：{msg}"
        );
    }
}
