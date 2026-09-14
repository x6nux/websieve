//! WebView 专用的本地 SOCKS5 代理：多会话条带的**免提权**落地件。
//!
//! # 它替代了什么
//!
//! 多会话条带要的是「同域名 N 个端口 ⇒ N 个 origin ⇒ N 条独立 TCP」。
//! 原本靠 hosts 劫持实现：把域名指向 127.0.0.1，WebView 连 `domain:8443`
//! 就落到本地转发器的 8443 口上。代价是要管理员权限去写 `/etc/hosts`，
//! 而且那是**全系统**生效的——浏览器、curl、别的程序一并受影响。
//!
//! 本模块把同一件事搬进进程内：给 WebView 设一个 SOCKS5 代理
//! （`WKWebsiteDataStore.proxyConfigurations`，macOS 14+；Windows 走
//! `--proxy-server=`），WebView 于是把「连 domain:8443」交给我们，我们照
//! [`routes`](WebviewProxy::set_routes) 改写成 `127.0.0.1:8443`。**没写过
//! 的目标原样直连**——承载页自己的 http 壳就走这条路。
//!
//! 相比 hosts 劫持的三个实质差别：
//! - 不要管理员权限，不碰系统文件，进程崩了不留残留
//! - 作用域只在我们自己的 WebView，系统其余部分完全不知情
//! - **纯 IP 服务端也能用**：hosts 只能映射「域名→IP」，映射不了「IP→IP」，
//!   所以 `shard_setup::hijackable` 直接拒绝 IP 字面量；而这里根本不做名字
//!   解析，只按 (host, port) 查表改写，IP 服务端一样拿得到多会话
//!
//! # 绝不终结 TLS
//!
//! 与 `shard` 同一条纪律：只做字节搬运，不解密、不看内容、不碰证书。
//! SOCKS5 的握手发生在 TLS **之外**——WebView 先跟我们谈好去哪，然后它的
//! ClientHello 原样穿过这条隧道抵达真实服务端。一旦在这里终结 TLS，整个
//! 项目「用真实浏览器指纹」的前提当场作废。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

/// 改写表：WebView 请求的 `(host, port)` → 实际要拨的地址。
///
/// 键里的 host 原样保留 WebView 给的形式（域名或 IP 字面量），大小写归一
/// 到小写——DNS 本就大小写不敏感，而 WebView 给的大小写不由我们决定。
pub type Routes = HashMap<(String, u16), SocketAddr>;

/// 把 host 归一成查表用的键。
fn key(host: &str, port: u16) -> (String, u16) {
    // 剥掉 IPv6 字面量的方括号再做 key。
    //
    // 两边的写法天生不同：URL 的 authority 里是 `[2001:db8::1]`（RFC 3986
    // 要求），而 SOCKS5 的 IPv6 地址类型（atyp=0x04）解出来是裸地址
    // `2001:db8::1`。不归一化
    // 到同一种写法，IPv6 服务端的改写就永远匹配不上——请求会落到"未命中"
    // 分支，而那条分支只允许回环，于是直接被拒。
    let h = host.trim_start_matches('[').trim_end_matches(']');
    (h.to_ascii_lowercase(), port)
}

/// 代理句柄：drop 即停。
pub struct WebviewProxy {
    port: u16,
    routes: Arc<RwLock<Routes>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for WebviewProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl WebviewProxy {
    /// 在 127.0.0.1 的随机高端口上起代理。
    ///
    /// 端口随机而非固定：这个端口只有我们自己用（通过 `url()` 交给 WebView
    /// 构建器），没有任何理由去抢一个固定号，也就没有端口冲突这类启动失败。
    pub async fn spawn() -> anyhow::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| anyhow::anyhow!("WebView 代理监听失败: {e}"))?;
        let port = listener.local_addr()?.port();
        let routes = Arc::new(RwLock::new(Routes::new()));
        let task = tokio::spawn(accept_loop(listener, routes.clone()));
        tracing::info!("WebView 代理就绪: socks5://127.0.0.1:{port}");
        Ok(Self { port, routes, task })
    }

    #[cfg(test)]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 交给 WebView 构建器的代理 URL。
    pub fn url(&self) -> String {
        format!("socks5://127.0.0.1:{}", self.port)
    }

    /// **整体替换**改写表。
    ///
    /// 整体替换而非增量合并：条带每次重建都会换一批端口，增量合并会把上一代
    /// 的映射留下来，而那些端口上的转发器已经随上一代 `Forwarder` 一起
    /// abort 了——表现为「换代之后偶发连到一个死端口」，且只在旧 origin 恰好
    /// 被复用时才出现，极难复现。
    pub fn set_routes(&self, routes: Routes) {
        let n = routes.len();
        *self.routes.write().expect("改写表锁中毒") = routes;
        tracing::debug!("WebView 代理改写表已更新：{n} 条");
    }

    /// 当前改写表的条数（测试用）。
    #[cfg(test)]
    pub fn route_count(&self) -> usize {
        self.routes.read().expect("改写表锁中毒").len()
    }
}

/// SOCKS5 协商阶段的超时。
///
/// 没有它，一个连上来什么都不发的本地进程（或一次被页面跳转抛弃的 WebView
/// 连接）会永久占住一个任务和两个 fd，且**任何日志级别都看不到**。
/// `tls.rs` 的 `HANDSHAKE_TIMEOUT` 防的是同一件事，这里当时漏了。
///
/// 5 秒对回环上的协商绰绰有余——这段路径上没有任何网络往返。
const NEGOTIATE_TIMEOUT: Duration = Duration::from_secs(5);

/// 连续多少次 accept 失败才真的放弃。
///
/// 给瞬时的 fd 压力留出恢复窗口（每次退避 100ms，合计约 6 秒），同时保证
/// 监听口真的坏了时不会无声空转。
const MAX_ACCEPT_ERRS: u32 = 64;

async fn accept_loop(listener: TcpListener, routes: Arc<RwLock<Routes>>) {
    let mut consecutive_errs = 0u32;
    loop {
        let inbound = match listener.accept().await {
            Ok((s, _)) => s,
            Err(e) => {
                // **accept 出错要退避重试，不能当成"监听结束"。**
                //
                // 原先是 `let Ok(..) else { return }`，于是 EMFILE（fd 用尽）
                // 这种瞬时压力会永久杀死 accept 循环：此后每一个 WebView 请求
                // ——数据面和承载页外壳——全部失败，而 `WebviewProxy` 句柄还
                // 活着，没有任何地方会发现。
                //
                // 不去辨别具体 errno：这个循环里**没有一种错误值得立刻放弃**，
                // fd 压力会随别处释放而缓解，而真正的致命情况（监听 socket 被
                // 关掉）会稳定复现，由连续计数兜住。
                consecutive_errs += 1;
                if consecutive_errs > MAX_ACCEPT_ERRS {
                    tracing::error!("WebView 代理连续 {consecutive_errs} 次 accept 失败，停止: {e}");
                    return;
                }
                tracing::warn!("WebView 代理 accept 失败（{e}），退避后继续");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        consecutive_errs = 0;
        let routes = routes.clone();
        tokio::spawn(async move {
            if let Err(e) = serve(inbound, routes).await {
                // 浏览器关标签页、页面跳转都会让隧道中途断开，属常态噪声，
                // 因此是 debug 而非 warn。真正的配置错误表现为「改写表查不到、
                // 直连又被拒」，那条路径下面会单独 warn。
                tracing::debug!("WebView 代理会话结束: {e:#}");
            }
        });
    }
}

/// 处理一条 SOCKS5 会话：握手 → 取目标 → 查表 → 建隧道 → 搬字节。
async fn serve(mut inbound: TcpStream, routes: Arc<RwLock<Routes>>) -> anyhow::Result<()> {
    // 协商借 `wsieve_socks5::negotiate`——同一个 RFC 1928 子集不该在这个仓库
    // 里解两遍。它只解析、不替我们决定怎么回复，所以下面的"未命中就拒"那套
    // 策略仍然在这里。
    //
    // 协商阶段带超时；建好隧道之后的搬运不设上限（长连接是常态）。
    let target = tokio::time::timeout(NEGOTIATE_TIMEOUT, wsieve_socks5::negotiate(&mut inbound))
        .await
        .map_err(|_| anyhow::anyhow!("SOCKS5 协商超过 {NEGOTIATE_TIMEOUT:?}"))??;
    // `None` = 对端中途断开 / 不是 SOCKS5 / 用了不支持的命令或地址类型。
    // 后两种 `negotiate` 已按 RFC 回过错误码了，这里丢掉连接就行。
    let Some(target) = target else {
        return Ok(());
    };
    let (host, port) = (host_string(&target.addr), target.port);

    // 查表改写。查不到就原样直连——承载页自己的 http 壳走的正是这条。
    let mapped = routes.read().expect("改写表锁中毒").get(&key(&host, port)).copied();
    let mut outbound = match mapped {
        Some(addr) => {
            tracing::debug!("WebView 代理改写: {host}:{port} -> {addr}");
            TcpStream::connect(addr).await.map_err(|e| {
                anyhow::anyhow!("连本地转发器 {addr} 失败（{host}:{port} 的改写目标）: {e}")
            })?
        }
        // **未命中只允许回环。**
        //
        // 这个监听口是无认证的，任何本地进程都能连上它。若未命中就原样外拨，
        // 它就成了一个通用出网中继：别的进程能借 websieve 的身份连任意
        // host:port，从而绕过按应用划分的防火墙规则（Little Snitch、macOS
        // 本地网络授权），还能顺带探测局域网。
        //
        // 而合法目标**全都在回环上**：路由表把数据面 origin 改写到
        // `127.0.0.1:<转发端口>`，未映射的那条只有承载页自己的 http 壳
        // （也是 127.0.0.1）。限制到回环不损失任何功能，却把"任意出网"这个
        // 能力降到零——本地进程本来就能直连回环，通过这里绕一圈拿不到新东西。
        None if is_loopback_host(&host) => TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|e| anyhow::anyhow!("直连 {host}:{port} 失败: {e}"))?,
        None => {
            tracing::warn!("WebView 代理拒绝非回环目标 {host}:{port}（不是出网中继）");
            reply_refused(&mut inbound).await?;
            anyhow::bail!("拒绝非回环目标 {host}:{port}");
        }
    };

    reply_success(&mut inbound).await?;

    let _ = inbound.set_nodelay(true);
    let _ = outbound.set_nodelay(true);
    // 纯字节搬运：TLS 记录原样过境。
    tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await?;
    Ok(())
}

// ── SOCKS5（RFC 1928）──
//
// 只实现 CONNECT + 无认证。WebView 是唯一的客户端，它两样都支持，多实现
// 一种认证方式只是多一份没人走的代码。

const VER: u8 = 0x05;
const ATYP_IPV4: u8 = 0x01;
const REP_OK: u8 = 0x00;
/// RFC 1928 的 `connection not allowed by ruleset`。
const REP_NOT_ALLOWED: u8 = 0x02;

/// 目标是否落在本机回环上。
///
/// 覆盖三种写法：IPv4 的 127.0.0.0/8、IPv6 的 ::1、以及字面量 "localhost"。
/// 不做 DNS 解析——名字解析的结果可能随时变，而这是一道安全闸，判据必须
/// 只依赖眼前这个字符串。
fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // IPv6 字面量在 SOCKS 请求里不带方括号，两种都收。
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// 回一个"规则不允许"的 SOCKS5 应答。
///
/// 明确回绝而不是直接断开：客户端拿到 0x02 会立刻报错，断开只表现为
/// "页面一直转圈"。
async fn reply_refused(s: &mut TcpStream) -> anyhow::Result<()> {
    s.write_all(&[VER, REP_NOT_ALLOWED, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
        .await?;
    Ok(())
}

/// 把 `TargetAddr` 渲染成查表用的主机串。
///
/// IPv6 渲染成**裸地址**（不带方括号）：改写表的 key 就是这个写法，
/// 见 `key()` 的注释。
fn host_string(a: &wsieve_proto::addr::TargetAddr) -> String {
    use wsieve_proto::addr::TargetAddr;
    match a {
        TargetAddr::V4(o) => std::net::Ipv4Addr::from(*o).to_string(),
        TargetAddr::V6(o) => std::net::Ipv6Addr::from(*o).to_string(),
        TargetAddr::Domain(d) => d.clone(),
    }
}

/// 回成功。
///
/// BND.ADDR/BND.PORT 填 `0.0.0.0:0`：那两个字段只对 BIND/UDP ASSOCIATE 有
/// 意义，CONNECT 下客户端不看。填真实地址反而会把本地转发器的端口号告诉
/// 客户端，没有好处。
async fn reply_success(s: &mut TcpStream) -> anyhow::Result<()> {
    s.write_all(&[VER, REP_OK, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncReadExt;

    // 下面这三个只有测试里那个手搓的 SOCKS5 **客户端**用得到：解析那一半
    // 现在归 `wsieve_socks5::negotiate`，生产代码只需要回复用的 VER/ATYP_IPV4。
    const METHOD_NO_AUTH: u8 = 0x00;
    const CMD_CONNECT: u8 = 0x01;
    const ATYP_DOMAIN: u8 = 0x03;

    /// 回显靶：回显收到的字节，并统计 accept 次数。
    async fn echo(accepts: Arc<AtomicUsize>) -> SocketAddr {
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

    /// 以 SOCKS5 客户端身份连上代理并请求 `host:port`，返回建好的隧道。
    async fn socks_connect(proxy: u16, host: &str, port: u16) -> anyhow::Result<TcpStream> {
        let mut s = TcpStream::connect(("127.0.0.1", proxy)).await?;
        s.write_all(&[VER, 1, METHOD_NO_AUTH]).await?;
        let mut r = [0u8; 2];
        s.read_exact(&mut r).await?;
        anyhow::ensure!(r == [VER, METHOD_NO_AUTH], "方法协商失败: {r:?}");

        let mut req = vec![VER, CMD_CONNECT, 0x00, ATYP_DOMAIN, host.len() as u8];
        req.extend_from_slice(host.as_bytes());
        req.extend_from_slice(&port.to_be_bytes());
        s.write_all(&req).await?;

        let mut rep = [0u8; 10];
        s.read_exact(&mut rep).await?;
        anyhow::ensure!(rep[1] == REP_OK, "CONNECT 被拒: rep={:#04x}", rep[1]);
        Ok(s)
    }

    /// 核心能力：改写表命中时，连的是表里的地址而不是请求里的那个。
    ///
    /// 这条正是 hosts 劫持的等价替代——不成立的话整个免提权方案归零。
    /// 用一个**不存在的域名**（`.invalid` 是 RFC 2606 保留后缀，保证解析
    /// 不出来）作请求目标：若改写没生效而去直连，必然失败，于是这条断言
    /// 无法靠「碰巧连对了」蒙混过去。
    #[tokio::test]
    async fn a_mapped_destination_is_rewritten_not_dialed_directly() {
        let accepts = Arc::new(AtomicUsize::new(0));
        let target = echo(accepts.clone()).await;
        let proxy = WebviewProxy::spawn().await.unwrap();

        let mut routes = Routes::new();
        routes.insert(("websieve.example.invalid".into(), 8443), target);
        proxy.set_routes(routes);

        let mut t = socks_connect(proxy.port(), "websieve.example.invalid", 8443)
            .await
            .expect("改写命中时必须连得上，失败说明改写没生效");
        t.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        t.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
        assert_eq!(accepts.load(Ordering::SeqCst), 1);
    }

    /// 查表大小写不敏感：WebView 给的大小写不由我们决定，漏了归一就是
    /// 「配置看着没错，条带却静默退化成单会话」。
    #[tokio::test]
    async fn route_lookup_ignores_host_case() {
        let accepts = Arc::new(AtomicUsize::new(0));
        let target = echo(accepts.clone()).await;
        let proxy = WebviewProxy::spawn().await.unwrap();
        let mut routes = Routes::new();
        routes.insert(key("WebSieve.Example.Invalid", 8443), target);
        proxy.set_routes(routes);

        let mut t = socks_connect(proxy.port(), "websieve.EXAMPLE.invalid", 8443)
            .await
            .expect("大小写不同也必须命中同一条改写");
        t.write_all(b"x").await.unwrap();
        let mut b = [0u8; 1];
        t.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"x");
    }

    /// 表里没有的目标原样直连——承载页自己的 http 壳走的正是这条路。
    /// 若改成「查不到就拒绝」，承载页根本加载不起来。
    #[tokio::test]
    async fn an_unmapped_destination_is_dialed_as_requested() {
        let accepts = Arc::new(AtomicUsize::new(0));
        let target = echo(accepts.clone()).await;
        let proxy = WebviewProxy::spawn().await.unwrap();
        // 改写表故意留空
        assert_eq!(proxy.route_count(), 0);

        let mut t = socks_connect(proxy.port(), "127.0.0.1", target.port())
            .await
            .expect("没配改写的目标必须能直连");
        t.write_all(b"direct").await.unwrap();
        let mut buf = [0u8; 6];
        t.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"direct");
        assert_eq!(accepts.load(Ordering::SeqCst), 1);
    }

    /// 每条 SOCKS5 会话一条独立出站——这是多拥塞窗口的前提，与 `shard`
    /// 的「一进一出、绝不池化」是同一条不变量。代理层若偷偷复用，条带在
    /// 上游看来又并回一条 TCP，特性归零且完全静默。
    #[tokio::test]
    async fn each_session_gets_its_own_upstream_connection() {
        let accepts = Arc::new(AtomicUsize::new(0));
        let target = echo(accepts.clone()).await;
        let proxy = WebviewProxy::spawn().await.unwrap();
        let mut routes = Routes::new();
        routes.insert(("a.invalid".into(), 8443), target);
        proxy.set_routes(routes);

        let mut conns = Vec::new();
        for i in 0..3u8 {
            let mut t = socks_connect(proxy.port(), "a.invalid", 8443).await.unwrap();
            t.write_all(&[b'a' + i]).await.unwrap();
            let mut b = [0u8; 1];
            t.read_exact(&mut b).await.unwrap();
            // 字节没串台：复用的话回显会错位
            assert_eq!(b[0], b'a' + i);
            conns.push(t);
        }
        assert_eq!(
            accepts.load(Ordering::SeqCst),
            3,
            "3 条会话必须对应 3 条独立出站，复用会让多拥塞窗口归零"
        );
    }

    /// 大载荷跨多次读写仍字节精确——数据面搬的是 TLS 记录，错一个字节
    /// 就是握手失败或解密失败，而那两种失败都不会说是代理搬错了。
    #[tokio::test]
    async fn large_payload_is_relayed_intact() {
        let accepts = Arc::new(AtomicUsize::new(0));
        let target = echo(accepts).await;
        let proxy = WebviewProxy::spawn().await.unwrap();
        let mut routes = Routes::new();
        routes.insert(("big.invalid".into(), 443), target);
        proxy.set_routes(routes);

        let mut t = socks_connect(proxy.port(), "big.invalid", 443).await.unwrap();
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let (mut r, mut w) = t.split();
        let p2 = payload.clone();
        let writer = async move { w.write_all(&p2).await.unwrap() };
        let mut got = vec![0u8; payload.len()];
        let reader = async { r.read_exact(&mut got).await.unwrap() };
        tokio::join!(writer, reader);
        assert_eq!(got, payload);
    }

    /// 换代时必须**整体替换**改写表。
    ///
    /// 增量合并的话，上一代的端口映射会留在表里，而那些端口上的转发器已随
    /// 上一代 `Forwarder` 一起 abort——表现为「换代之后偶发连到死端口」，
    /// 只在旧 origin 恰好被复用时才出现，极难复现。
    #[tokio::test]
    async fn setting_routes_replaces_rather_than_merges() {
        let proxy = WebviewProxy::spawn().await.unwrap();
        let mut gen1 = Routes::new();
        gen1.insert(("old.invalid".into(), 8443), "127.0.0.1:1".parse().unwrap());
        gen1.insert(("old.invalid".into(), 8444), "127.0.0.1:2".parse().unwrap());
        proxy.set_routes(gen1);
        assert_eq!(proxy.route_count(), 2);

        let mut gen2 = Routes::new();
        gen2.insert(("new.invalid".into(), 9443), "127.0.0.1:3".parse().unwrap());
        proxy.set_routes(gen2);
        assert_eq!(
            proxy.route_count(),
            1,
            "换代必须整体替换；留着上一代的映射会连到已 abort 的死端口"
        );
    }

    /// **`macos-proxy` 与 macOS 最低版本必须同步。**
    ///
    /// 这个 feature 链进来的 `nw_proxy_config_create_*` 是 macOS 14 才有的
    /// 符号，13 及以下即使一次都不调用也可能在 dyld 绑定阶段就起不来。两处
    /// 一旦失步，症状都很坏且都没有线索：
    ///   - 开了 feature 却没声明下限 ⇒ macOS 13 用户遇到一次无提示的启动失败
    ///   - 声明了下限却删了 feature ⇒ macOS 13 用户被无谓地挡在门外
    ///
    /// 两份配置分别在 `Cargo.toml` 与 `tauri.conf.json`，编译器管不到它们的
    /// 一致性，所以由这条测试来管。
    #[test]
    fn the_macos_floor_matches_the_proxy_feature() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
        let conf = std::fs::read_to_string(root.join("tauri.conf.json")).unwrap();

        // 只看真正生效的那一行（tauri 的依赖声明），不是注释里提到的名字。
        let feature_on = manifest
            .lines()
            .any(|l| l.starts_with("tauri = ") && l.contains("macos-proxy"));
        let declares_14 = conf.contains("\"minimumSystemVersion\": \"14.0\"");

        assert_eq!(
            feature_on, declares_14,
            "Cargo.toml 的 macos-proxy（={feature_on}）与 tauri.conf.json 的 \
             minimumSystemVersion 14.0（={declares_14}）必须同时在或同时不在"
        );
    }

    /// 非 SOCKS5 的客户端要明确失败，不能把垃圾字节当成目标地址去连。
    #[tokio::test]
    async fn a_non_socks5_client_is_rejected() {
        let proxy = WebviewProxy::spawn().await.unwrap();
        let mut s = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        // HTTP 请求打头的字节是 'G'(0x47)，不是 0x05
        s.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
        let mut buf = [0u8; 2];
        // 要么读到明确的拒绝、要么连接被关闭；绝不能当成一次成功的 CONNECT
        let r = tokio::time::timeout(std::time::Duration::from_secs(2), s.read_exact(&mut buf)).await;
        // 关闭（Ok(Err)）或超时（Err）都算正确处理，只有"读到了完整应答"才需要查。
        if let Ok(Ok(_)) = r {
            assert_ne!(buf[0], VER, "不该给非 SOCKS5 客户端回一个成功的协商");
        }
    }
}

#[cfg(test)]
mod relay_guard_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// **这个代理不是通用出网中继。**
    ///
    /// 监听口无认证，任何本地进程都能连上。若未命中路由表就原样外拨，别的
    /// 进程就能借 websieve 的身份连任意 host:port——绕过按应用划分的防火墙
    /// 规则（Little Snitch、macOS 本地网络授权），还能探测局域网。
    ///
    /// 合法目标全在回环上（路由表改写到 `127.0.0.1:<转发端口>`，未映射的只有
    /// 承载页自己的 http 壳），所以这道闸不损失任何功能。
    #[tokio::test]
    async fn a_non_loopback_destination_is_refused() {
        let p = WebviewProxy::spawn().await.unwrap();
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", p.port())).await.unwrap();
        s.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut m = [0u8; 2];
        s.read_exact(&mut m).await.unwrap();
        assert_eq!(m, [0x05, 0x00]);

        // CONNECT 到一个公网地址（198.51.100.7 是 RFC 5737 文档用网段，不会真连上）
        s.write_all(&[0x05, 0x01, 0x00, 0x01, 198, 51, 100, 7, 0x01, 0xBB])
            .await
            .unwrap();
        let mut rep = [0u8; 10];
        s.read_exact(&mut rep).await.unwrap();
        assert_eq!(
            rep[1], REP_NOT_ALLOWED,
            "非回环目标被放行了——这是个通用出网中继，不是改写表"
        );
    }

    /// 回环判据要覆盖三种写法，且**不做 DNS 解析**。
    ///
    /// 名字解析的结果随时可变，而这是一道安全闸，判据只能依赖眼前的字符串。
    #[test]
    fn loopback_detection_covers_the_three_spellings() {
        for ok in ["127.0.0.1", "127.1.2.3", "localhost", "LOCALHOST", "::1", "[::1]"] {
            assert!(is_loopback_host(ok), "{ok} 应当算回环");
        }
        for bad in ["198.51.100.7", "10.0.0.1", "example.com", "0.0.0.0", "169.254.1.1"] {
            assert!(!is_loopback_host(bad), "{bad} 不该算回环");
        }
    }
}
