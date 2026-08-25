//! TUN 设备与 netstack 的对接（设计文档 §8.3）。
//!
//! netstack-smoltcp 0.2.4 的 `build()` 返回四元组
//! `(Stack, Option<Runner>, Option<UdpSocket>, Option<TcpListener>)`：
//!   - `Stack` 同时是 `Stream<Item = io::Result<Vec<u8>>>`（协议栈发出的包）
//!     与 `Sink<Vec<u8>>`（喂给协议栈的包）。`AnyIpPktFrame = Vec<u8>`
//!   - `Runner` 是 `BoxFuture<'static, io::Result<()>>`，直接 `tokio::spawn`
//!     即可。**只有 enable_tcp 时才是 Some**
//!   - `TcpListener` 实现 `Stream<Item = (TcpStream, SocketAddr, SocketAddr)>`，
//!     元组是 `(流, 本地地址, 远端地址)`。**远端地址就是路由判决的输入**
//!   - `TcpStream` 实现 tokio 的 `AsyncRead + AsyncWrite`，因此下游能直接
//!     `copy_bidirectional`，与混合端口入站完全一致
//!
//! 泵送是两条独立任务：TUN→栈、栈→TUN。tun-rs 的 `DeviceFramed` 提供
//! `Stream<Item = io::Result<BytesMut>>` 与 `Sink<Bytes>`，注意**收发的
//! 字节类型不同**（`BytesMut` 进、`Bytes` 出），需显式转换。
//!
//! # 本模块能证明什么，不能证明什么
//!
//! `spawn_netstack()` 需要 root（创建 utun），因此**它本身没有单测**。
//! 但下面三条测试不碰设备也能跑，它们验的是**协议栈侧**的事实：
//! 四元组形状、ICMP 的依赖约束，以及最要紧的那条 ——
//! `netstack_does_not_filter_server_ip_by_itself`：一个发往服务器真实 IP
//! 的 SYN 喂进栈，`TcpListener` 原样把它当成一条连接吐出来。
//!
//! 这条测试**证伪**了一个诱人的错误假设（「netstack 会帮我们挡掉环路」），
//! 从而说明 §8.3.1 的 bypass 路由是必需的、且是唯一防线。它是在
//! 「无 root 也能实证」这个边界内能拿到的最强证据：真实设备上的环路
//! 是否成立仍须手工验证（计划文档手工验证清单 M4）。

use std::sync::Arc;

use netstack_smoltcp::{StackBuilder, TcpListener};

/// 建好的 netstack 三件套。
pub struct NetStack {
    pub tcp: TcpListener,
    pub udp: netstack_smoltcp::UdpSocket,
    /// 内核分配到的设备名（macOS 上形如 `utun7`）。
    ///
    /// 留在这里不是为了好看：手工验证清单 M1/M2 要拿它去 `ifconfig` 与
    /// `netstat -rn` 里对账，而设备名是内核分配的、事先无从知晓。
    pub if_name: String,
}

/// TUN 设备的 MTU。1500 是以太网默认值；TUN 是虚拟设备，可以更大，
/// 但对端物理链路仍是 1500，调大只会让 smoltcp 分片。保持一致最省事。
pub const TUN_MTU: u16 = 1500;

/// fake-ip 段的网关地址：TUN 接口自己的地址。
pub const TUN_ADDR: &str = "198.18.0.1";

/// TUN 接口地址的前缀长度：整个 `198.18.0.0/15` 都进这个设备。
///
/// 这就是 `fakedns::screen_upstream` 把**任何**段内 A 记录都当污染的原因 ——
/// 判据是**段的归属**而非池成员资格：整段都被路由进本设备，一个不在池里
/// 的段内地址连出去只会掉进黑洞。
pub const TUN_PREFIX: u8 = 15;

/// 建 TUN 设备并把它与 netstack 对接起来。
///
/// **需要 root（macOS utun）/ 管理员（Windows wintun）/ CAP_NET_ADMIN（Linux）。**
/// 无权限时 `build_async()` 返回 `EPERM`，本函数原样上报，不降级 ——
/// 静默降级会让用户以为 TUN 开着而实际全部流量在裸奔（与 §6.4
/// 「禁止回退直连」同源）。
#[cfg(target_os = "macos")]
pub async fn spawn_netstack() -> std::io::Result<NetStack> {
    use futures::{SinkExt, StreamExt};
    use tun_rs::async_framed::{BytesCodec, DeviceFramed};
    use tun_rs::DeviceBuilder;

    let builder = DeviceBuilder::new()
        // 不指定 name：macOS 由内核分配下一个可用 utunN，硬编码
        // utun8 会在别的 VPN 已占用时直接失败。
        .ipv4(TUN_ADDR, TUN_PREFIX, None)
        .mtu(TUN_MTU)
        // macOS：关掉 4 字节 packet information 头，让收到的就是裸 IP 包
        // （netstack 期待的正是裸包）；同时关掉 tun-rs 的自动路由 ——
        // 路由由我们的 ManagedSystemState 统一托管，两边都写会互相踩。
        .with(|o| {
            o.packet_information(false).associate_route(false);
        });
    let dev = Arc::new(builder.build_async()?);
    // 设备名取不到就报错，不用空串糊过去：M1/M2 的对账全靠它，
    // 而「TUN 起来了但不知道是哪个接口」等于没法验证路由写对没有。
    let if_name = dev.name()?;

    let (stack, runner, udp, tcp) = StackBuilder::default()
        .enable_tcp(true)
        .enable_udp(true)
        .enable_icmp(true)
        .mtu(TUN_MTU as usize)
        .build()?;
    // enable_tcp(true) ⇒ 三者必然是 Some；expect 而非 unwrap，
    // 以便真出问题时错误信息能指出是哪一个。
    let runner = runner.expect("enable_tcp(true) 必产出 runner");
    let udp = udp.expect("enable_udp(true) 必产出 udp socket");
    let tcp = tcp.expect("enable_tcp(true) 必产出 tcp listener");
    tokio::spawn(runner);

    // 注意顺序：`DeviceFramed::split()` 返回 **(读, 写)**，
    // 与 `futures::StreamExt::split()` 的 **(写, 读)** 正好相反。
    // 接反会得到「DeviceFramedRead 没有 send 方法」这类费解的编译错误。
    let (mut tun_stream, mut tun_sink) = DeviceFramed::new(dev, BytesCodec::new()).split();
    let (mut stack_sink, mut stack_stream) = stack.split();

    // 栈 → TUN
    tokio::spawn(async move {
        while let Some(pkt) = stack_stream.next().await {
            // 读错误不能当成「没数据」跳过：栈侧读失败意味着泵已经断了，
            // 继续循环只会空转。报出来再退出，让日志里留下因果。
            let pkt = match pkt {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("读协议栈失败，栈→TUN 泵停止: {e}");
                    break;
                }
            };
            if let Err(e) = tun_sink.send(bytes::Bytes::from(pkt)).await {
                tracing::warn!("写 TUN 失败，栈→TUN 泵停止: {e}");
                break;
            }
        }
    });
    // TUN → 栈
    tokio::spawn(async move {
        while let Some(pkt) = tun_stream.next().await {
            let pkt = match pkt {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("读 TUN 失败，TUN→栈 泵停止: {e}");
                    break;
                }
            };
            if let Err(e) = stack_sink.send(pkt.to_vec()).await {
                tracing::warn!("写协议栈失败，TUN→栈 泵停止: {e}");
                break;
            }
        }
    });

    Ok(NetStack { tcp, udp, if_name })
}

/// 其余平台的占位。**明确报错而不是静默什么都不做**——后者会让用户
/// 以为 TUN 开着。
#[cfg(not(target_os = "macos"))]
pub async fn spawn_netstack() -> std::io::Result<NetStack> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "TUN 目前仅支持 macOS（阶段 6 的范围声明）。\
         Linux 需要 CAP_NET_ADMIN 与 netlink 路由实现，\
         Windows 需要 wintun.dll 与 IPHLPAPI 路由实现，二者均待补",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};

    /// 不需要 TUN 设备也能验的部分：`StackBuilder` 的四元组形状。
    #[tokio::test]
    async fn builder_tuple_shape_matches_expectation() {
        let (_stack, runner, udp, tcp) = StackBuilder::default()
            .enable_tcp(true)
            .enable_udp(true)
            .enable_icmp(true)
            .mtu(TUN_MTU as usize)
            .build()
            .unwrap();
        assert!(runner.is_some(), "enable_tcp ⇒ runner 必为 Some");
        assert!(udp.is_some());
        assert!(tcp.is_some());
    }

    #[tokio::test]
    async fn icmp_without_tcp_is_rejected() {
        // netstack 的约束：ICMP 由 TCP 的 Interface 驱动。单开 ICMP 会报错，
        // 我们的配置必须永远同时打开二者。
        let r = StackBuilder::default().enable_icmp(true).build();
        assert!(r.is_err(), "只开 ICMP 应当报错");
    }

    /// **环路回归的协议栈侧**（设计文档 §13 的专项之一）。
    ///
    /// 把一个目的地为服务器真实 IP 的 SYN 包喂进栈，确认 `TcpListener` 会把
    /// 它当成一条连接吐出来 —— 也就是说，只靠 netstack 自己**不会**避开环路，
    /// bypass 路由是唯一防线。
    ///
    /// 这条测试的价值在于它证伪了一个诱人的错误假设（「netstack 会帮我们
    /// 挡掉」），从而说明 §8.3.1 的 bypass 是必需的。
    #[tokio::test]
    async fn netstack_does_not_filter_server_ip_by_itself() {
        use etherparse::PacketBuilder;
        let (mut stack, runner, _udp, tcp) = StackBuilder::default()
            .enable_tcp(true)
            .mtu(TUN_MTU as usize)
            .build()
            .unwrap();
        tokio::spawn(runner.unwrap());
        let mut tcp = tcp.unwrap();

        // 一个发往「服务器真实 IP」的 SYN。
        let b = PacketBuilder::ipv4([10, 0, 0, 2], [203, 0, 113, 7], 64)
            .tcp(50000, 443, 0, 65535)
            .syn();
        let mut pkt = Vec::with_capacity(b.size(0));
        b.write(&mut pkt, &[]).unwrap();
        stack.send(pkt).await.unwrap();

        let got = tokio::time::timeout(std::time::Duration::from_secs(3), tcp.next()).await;
        let (_s, _local, remote) = got
            .expect("netstack 应当接受这条连接")
            .expect("listener 不应提前结束");
        assert_eq!(
            remote.to_string(),
            "203.0.113.7:443",
            "netstack 原样交出服务器 IP —— 它不会替我们避环路"
        );
    }

    /// 上一条的对照：**在 bypass 生效的世界里，这个包根本不会到达协议栈。**
    ///
    /// netstack 自带 `ip_filter` 钩子，用它模拟「bypass 路由把服务器 IP 的
    /// 包挡在 TUN 之外」——包被丢弃，`TcpListener` 上什么都不会出现。
    /// 两条测试合起来才完整：前者说明防线**必需**，后者说明防线**有效**，
    /// 且有效的那一侧是在 IP 层拦掉，而不是在连接层补救。
    #[tokio::test]
    async fn a_filter_at_the_ip_layer_is_what_actually_stops_the_loop() {
        use etherparse::PacketBuilder;
        let server: std::net::IpAddr = "203.0.113.7".parse().unwrap();
        let (mut stack, runner, _udp, tcp) = StackBuilder::default()
            .enable_tcp(true)
            .mtu(TUN_MTU as usize)
            // 这就是 bypass 路由在协议栈里的等价物：目的地是服务器 IP 的包
            // 一律不进栈。真实系统里由路由表完成，此处用过滤器等价复现。
            .add_ip_filter_fn(move |_src, dst| *dst != server)
            .build()
            .unwrap();
        tokio::spawn(runner.unwrap());
        let mut tcp = tcp.unwrap();

        let b = PacketBuilder::ipv4([10, 0, 0, 2], [203, 0, 113, 7], 64)
            .tcp(50000, 443, 0, 65535)
            .syn();
        let mut pkt = Vec::with_capacity(b.size(0));
        b.write(&mut pkt, &[]).unwrap();
        stack.send(pkt).await.unwrap();

        let got = tokio::time::timeout(std::time::Duration::from_millis(500), tcp.next()).await;
        assert!(
            got.is_err(),
            "包被挡在 IP 层，listener 上不该出现任何连接（否则环路仍然成立）"
        );
    }

    /// 反过来的对照：**别的目的地照常放行。**
    ///
    /// 没有这条，上一条的「什么都没出现」就可能只是因为过滤器把一切都拦了 ——
    /// 一个全拦的防线不是防线，是断网。
    #[tokio::test]
    async fn the_same_filter_still_lets_ordinary_traffic_through() {
        use etherparse::PacketBuilder;
        let server: std::net::IpAddr = "203.0.113.7".parse().unwrap();
        let (mut stack, runner, _udp, tcp) = StackBuilder::default()
            .enable_tcp(true)
            .mtu(TUN_MTU as usize)
            .add_ip_filter_fn(move |_src, dst| *dst != server)
            .build()
            .unwrap();
        tokio::spawn(runner.unwrap());
        let mut tcp = tcp.unwrap();

        // 一个普通目标（不是服务器 IP）。
        let b = PacketBuilder::ipv4([10, 0, 0, 2], [93, 184, 216, 34], 64)
            .tcp(50001, 443, 0, 65535)
            .syn();
        let mut pkt = Vec::with_capacity(b.size(0));
        b.write(&mut pkt, &[]).unwrap();
        stack.send(pkt).await.unwrap();

        let (_s, _local, remote) = tokio::time::timeout(std::time::Duration::from_secs(3), tcp.next())
            .await
            .expect("普通目标不该被挡")
            .expect("listener 不应提前结束");
        assert_eq!(remote.to_string(), "93.184.216.34:443");
    }

    /// fake-ip 段内的目标同样原样交出 —— 反查是**我们**的活。
    ///
    /// 协议栈对 `198.18.x.x` 没有任何特殊待遇：它既不知道那是保留段，
    /// 也不会拒绝。因此 `inbound::classify` 的 `StaleFakeIp` 分支不是
    /// 防御性编程，是真会走到的路径。
    #[tokio::test]
    async fn fake_ip_destinations_reach_the_listener_untouched() {
        use etherparse::PacketBuilder;
        let (mut stack, runner, _udp, tcp) = StackBuilder::default()
            .enable_tcp(true)
            .mtu(TUN_MTU as usize)
            .build()
            .unwrap();
        tokio::spawn(runner.unwrap());
        let mut tcp = tcp.unwrap();

        let b = PacketBuilder::ipv4([198, 18, 0, 1], [198, 18, 7, 9], 64)
            .tcp(50002, 443, 0, 65535)
            .syn();
        let mut pkt = Vec::with_capacity(b.size(0));
        b.write(&mut pkt, &[]).unwrap();
        stack.send(pkt).await.unwrap();

        let (_s, _local, remote) = tokio::time::timeout(std::time::Duration::from_secs(3), tcp.next())
            .await
            .expect("fake-ip 目标应当照常到达 listener")
            .expect("listener 不应提前结束");
        assert_eq!(
            remote.to_string(),
            "198.18.7.9:443",
            "协议栈不认识 fake-ip —— 反查必须由 inbound::classify 做"
        );
    }

    /// TUN 接口地址必须落在 fake-ip 段内，且前缀盖住整段。
    ///
    /// 这两个常量是 `fakedns::screen_upstream`「整段皆污染」判据的前提：
    /// 前缀若不是 /15，段内就会有地址不进本设备，那条判据立刻失去依据。
    #[test]
    fn tun_address_covers_the_whole_fake_ip_segment() {
        let addr: std::net::Ipv4Addr = TUN_ADDR.parse().expect("TUN_ADDR 必须是合法 IPv4");
        assert_eq!(TUN_PREFIX, 15, "前缀必须盖住 198.18.0.0/15 整段");
        assert_eq!(addr.octets()[0], 198);
        assert_eq!(addr.octets()[1], 18);
        // 网关自己不该被分配出去：池的起点必须在它之后。
        assert!(
            crate::fakeip::FakeIpPool::new(vec![]).lookup(addr).is_none(),
            "TUN 网关地址不该出现在池的映射里"
        );
    }
}
