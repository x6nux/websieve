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

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};

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

/// 在 127.0.0.1 的 `base_port..base_port+count` 上监听，每条入站连接新建一条
/// 到 `upstream` 的 TCP 并双向搬运。
///
/// 端口被占用即失败返回——静默跳过会让会话数与端口数对不上，条带按序取端口
/// 时就会连到别人的服务上。
pub async fn spawn(base_port: u16, count: usize, upstream: SocketAddr) -> anyhow::Result<Forwarder> {
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
        tasks.push(tokio::spawn(accept_loop(listener, upstream)));
    }
    tracing::info!("本地条带转发器就绪: {ports:?} -> {upstream}");
    Ok(Forwarder { ports, tasks })
}

async fn accept_loop(listener: TcpListener, upstream: SocketAddr) {
    loop {
        let Ok((inbound, _)) = listener.accept().await else {
            return;
        };
        // 每条入站连接一条**全新**出站连接：这正是多拥塞窗口的来源，
        // 任何形式的复用都会让特性归零。
        tokio::spawn(async move {
            if let Err(e) = relay(inbound, upstream).await {
                tracing::debug!("转发结束: {e}");
            }
        });
    }
}

async fn relay(mut inbound: TcpStream, upstream: SocketAddr) -> anyhow::Result<()> {
    let mut outbound = TcpStream::connect(upstream).await?;
    let _ = inbound.set_nodelay(true);
    let _ = outbound.set_nodelay(true);
    // 纯字节搬运：TLS 记录原样过境，握手是 WebView 与真实服务端之间的事。
    tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await?;
    Ok(())
}

/// 解析真实服务端地址。
///
/// **必须在写 hosts 之前调用**：hosts 一旦把域名指向 127.0.0.1，系统解析器
/// 就会返回本地地址，转发器再解析就指向自己，形成死循环。
pub async fn resolve_upstream(host: &str, port: u16) -> anyhow::Result<SocketAddr> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await?.collect();
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

/// 本地条带的运行态：转发器 + 已写入的 hosts，drop 时自动摘除 hosts 条目。
pub struct ShardGuard {
    /// 持有即保活：drop 时 listener 任务被 abort。
    _forwarder: Forwarder,
    hosts: Arc<crate::hosts::HostsFile>,
}

impl ShardGuard {
    pub fn new(forwarder: Forwarder, hosts: Arc<crate::hosts::HostsFile>) -> Self {
        Self { _forwarder: forwarder, hosts }
    }
}

impl Drop for ShardGuard {
    fn drop(&mut self) {
        // 不摘除的话，域名会一直指向已经不在跑的转发器 —— 本机之后访问
        // 该域名全部失败。崩溃路径由启动时的 clear_managed 兜底。
        if let Err(e) = self.hosts.clear_managed() {
            tracing::warn!("清理 hosts 托管条目失败: {e}");
        } else {
            tracing::info!("已清理 hosts 托管条目");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
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
    #[tokio::test]
    async fn each_inbound_gets_its_own_upstream_connection() {
        let accepts = Arc::new(AtomicUsize::new(0));
        let upstream = echo_upstream(accepts.clone()).await;
        // 端口 0 不能用于连续分配，取一段大概率空闲的高端口
        let base = 39411;
        let fwd = spawn(base, 3, upstream).await.unwrap();
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

    #[test]
    fn loopback_detection() {
        assert!(is_loopback(&"127.0.0.1:443".parse().unwrap()));
        assert!(is_loopback(&"[::1]:443".parse().unwrap()));
        assert!(!is_loopback(&"1.2.3.4:443".parse().unwrap()));
    }
}
