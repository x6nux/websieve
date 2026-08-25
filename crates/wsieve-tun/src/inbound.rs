//! TUN 入站：把 netstack 产出的 TcpStream 接到既有的路由层与出站层上。
//!
//! **本模块不新建任何转发路径**（设计文档 §4.2 纪律②）。netstack 把 IP 包
//! 转成 `TcpStream` 之后，剩下的事与混合端口入站**逐字相同**：产出
//! `(AddrPort, 双向流)` 交给下游。TUN 只是又一个入口。
//!
//! 本模块唯一的增量是「目标地址怎么来」：
//!   - 目的 IP 已登记在 bypass 名单 → 这是转发器连向真实服务器的那条连接
//!   - 目的 IP 落在 fake-ip 段 → 反查回域名（这是 fake-ip 存在的全部理由）
//!   - 否则 → 就是真实 IP 目标
//!
//! # 为什么判定顺序是 bypass 优先
//!
//! `device.rs` 的 `netstack_does_not_filter_server_ip_by_itself` 已经实证：
//! 协议栈原样把发往服务器 IP 的连接交出来。bypass 路由生效时这条连接根本
//! 不该进 TUN；它一旦出现在这里，说明路由防线漏了。此时若先做 fake-ip 反查
//! 或直接判成普通目标，就会把它送进路由层 → 出站 → 转回本机，正是
//! §8.3.1 的死循环。**顺序不是风格问题，是环路的第二道防线。**

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use crate::bypass::BypassSet;
use crate::fakeip::FakeIpPool;

/// TUN 目标地址的判定结果。
#[derive(Debug, PartialEq, Eq)]
pub enum TunTarget {
    /// fake-ip 反查成功，按域名走路由（GEOSITE / DOMAIN-SUFFIX 才能生效）。
    Domain(String, u16),
    /// 真实 IP 目标，按 IP 走路由（GEOIP / IP-CIDR）。
    Ip(SocketAddr),
    /// **必须绕过 TUN 直连**：这是转发器连向真实服务器的那条连接。
    ///
    /// 走到这里说明 bypass 路由没生效（比如路由表被别的软件改了）。
    /// 不静默放行也不静默丢弃 —— 调用方直连并**报警**，否则用户看到的
    /// 是「代理莫名其妙卡死」而不是一条可查的日志。
    BypassLeak(SocketAddr),
    /// fake-ip 段内但查不到映射：DNS 缓存过期、或上一次运行残留的假 IP。
    /// 必须拒绝，不能当真实 IP 连出去 —— 198.18/15 在公网上不可达，
    /// 连出去只会挂到超时。
    StaleFakeIp(SocketAddr),
}

impl TunTarget {
    /// 该判定是否表示一件**不该发生**的事，需要在日志里报出来。
    ///
    /// 两种异常的成因完全不同（一个是路由漏了、一个是映射没了），但对
    /// 调用方是同一个动作：记一条可查的日志，而不是默默处理掉。把它做成
    /// 方法而不是让调用方各自 `matches!`，是为了漏一处就编译不过。
    pub fn is_anomaly(&self) -> bool {
        matches!(self, TunTarget::BypassLeak(_) | TunTarget::StaleFakeIp(_))
    }
}

/// 依目的地址判定 TUN 连接的去向。纯函数，可穷举单测。
pub fn classify(dst: SocketAddr, pool: &FakeIpPool, bypass: &BypassSet) -> TunTarget {
    // 顺序是关键：bypass 优先于一切。转发器的出网连接一旦被判成
    // 「走代理」，就是 §8.3.1 的死循环。
    if bypass.contains(&dst.ip()) {
        return TunTarget::BypassLeak(dst);
    }
    if let IpAddr::V4(v4) = dst.ip() {
        // 判据是**整段**而非可分配范围：`device.rs` 把 `198.18.0.0/15` 整段
        // 都配进了 TUN 接口，段内任何地址连出去都会被路由回本设备。差集里
        // 那 5 个地址（含 TUN 网关 `198.18.0.1`）反查必然落空，按 `Ip` 连
        // 出去就是一个静默黑洞 —— 必须一并判成 `StaleFakeIp` 拒掉。
        if FakeIpPool::in_segment(v4) {
            return match pool.lookup(v4) {
                Some(d) => TunTarget::Domain(d, dst.port()),
                None => TunTarget::StaleFakeIp(dst),
            };
        }
    }
    TunTarget::Ip(dst)
}

/// TUN 入站的运行参数。持有的都是**已存在**的组件引用，
/// 本模块不拥有路由与出站。
pub struct TunInbound {
    pub pool: Arc<FakeIpPool>,
    pub bypass: BypassSet,
}

impl TunInbound {
    pub fn new(pool: Arc<FakeIpPool>, bypass: BypassSet) -> Self {
        Self { pool, bypass }
    }

    pub fn classify(&self, dst: SocketAddr) -> TunTarget {
        classify(dst, &self.pool, &self.bypass)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn fake_ip_resolves_back_to_domain() {
        let pool = Arc::new(FakeIpPool::new(vec![]));
        let ip = pool.allocate("example.com").unwrap();
        let t = TunInbound {
            pool: pool.clone(),
            bypass: BypassSet::new(),
        };
        assert_eq!(
            t.classify(SocketAddr::new(IpAddr::V4(ip), 443)),
            TunTarget::Domain("example.com".into(), 443)
        );
    }

    #[test]
    fn real_ip_stays_an_ip_target() {
        let t = TunInbound {
            pool: Arc::new(FakeIpPool::new(vec![])),
            bypass: BypassSet::new(),
        };
        assert_eq!(t.classify(sa("1.2.3.4:80")), TunTarget::Ip(sa("1.2.3.4:80")));
    }

    /// **环路回归（设计文档 §13 的专项用例）**。
    ///
    /// 转发器出网连接的目的地是已登记的服务器 IP。它必须被判成
    /// `BypassLeak` 而**绝不**是 `Ip`（走路由 → 走代理 → 回 WebView → 死循环）。
    #[test]
    fn forwarder_upstream_is_bypassed_never_routed() {
        let bypass = BypassSet::new();
        bypass.insert("jp", vec!["203.0.113.7".parse().unwrap()]);
        let t = TunInbound {
            pool: Arc::new(FakeIpPool::new(vec![])),
            bypass,
        };
        assert_eq!(
            t.classify(sa("203.0.113.7:443")),
            TunTarget::BypassLeak(sa("203.0.113.7:443")),
            "服务器 IP 被判成普通目标就是环路的第一步"
        );
    }

    #[test]
    fn bypass_wins_even_if_ip_is_inside_fake_range() {
        // 病态但可能：服务器真的部署在 198.18/15（内网基准测试环境）。
        // bypass 必须赢，否则转发器的连接会被当成 fake-ip 反查。
        let bypass = BypassSet::new();
        bypass.insert("lab", vec!["198.18.0.9".parse().unwrap()]);
        let t = TunInbound {
            pool: Arc::new(FakeIpPool::new(vec![])),
            bypass,
        };
        assert!(matches!(
            t.classify(sa("198.18.0.9:443")),
            TunTarget::BypassLeak(_)
        ));
    }

    /// 更狠的一版：那个 IP **同时**是某域名的有效 fake-ip 映射。
    ///
    /// 上一条里 198.18.0.9 只是碰巧落在段内、池里查不到，所以哪怕顺序写反
    /// 也会落到 `StaleFakeIp` 而不是 `Ip`——测试仍然「不是 Ip」，掩盖了错误。
    /// 这里让反查**能成功**：顺序一旦写反，结果就是 `Domain`，而 `Domain`
    /// 会被送进路由层，环路当场成立。
    #[test]
    fn bypass_wins_even_when_the_reverse_lookup_would_have_succeeded() {
        let pool = Arc::new(FakeIpPool::new(vec![]));
        let ip = pool.allocate("some-site.example").unwrap();
        let bypass = BypassSet::new();
        bypass.insert("lab", vec![IpAddr::V4(ip)]);
        let t = TunInbound {
            pool: pool.clone(),
            bypass,
        };
        let dst = SocketAddr::new(IpAddr::V4(ip), 443);
        assert_eq!(
            t.classify(dst),
            TunTarget::BypassLeak(dst),
            "反查成功也不能压过 bypass —— 那条连接是转发器的出网流量"
        );
    }

    #[test]
    fn stale_fake_ip_is_rejected_not_dialed() {
        // 未分配的 fake-ip 连出去必然超时。必须能被识别并拒绝。
        let t = TunInbound {
            pool: Arc::new(FakeIpPool::new(vec![])),
            bypass: BypassSet::new(),
        };
        assert_eq!(
            t.classify(sa("198.18.200.200:443")),
            TunTarget::StaleFakeIp(sa("198.18.200.200:443"))
        );
    }

    #[test]
    fn removing_outbound_stops_bypassing_its_ip() {
        // 出站下线后 bypass 必须同步失效，否则该 IP 永远绕过分流规则。
        let bypass = BypassSet::new();
        bypass.insert("jp", vec!["203.0.113.7".parse().unwrap()]);
        let t = TunInbound {
            pool: Arc::new(FakeIpPool::new(vec![])),
            bypass: bypass.clone(),
        };
        bypass.remove("jp");
        assert_eq!(
            t.classify(sa("203.0.113.7:443")),
            TunTarget::Ip(sa("203.0.113.7:443"))
        );
    }

    /// 共享 IP 的出站只下线一个时，bypass **必须仍然生效**。
    ///
    /// 这是 `BypassSet::remove` 只返回引用计数归零的 IP 的理由在判定层的
    /// 回声：原样删掉会让仍在线的那个出站当场掉进环路。
    #[test]
    fn a_shared_server_ip_stays_bypassed_while_any_outbound_holds_it() {
        let bypass = BypassSet::new();
        bypass.insert("jp", vec!["203.0.113.7".parse().unwrap()]);
        bypass.insert("sg", vec!["203.0.113.7".parse().unwrap()]);
        let t = TunInbound {
            pool: Arc::new(FakeIpPool::new(vec![])),
            bypass: bypass.clone(),
        };
        bypass.remove("jp");
        assert_eq!(
            t.classify(sa("203.0.113.7:443")),
            TunTarget::BypassLeak(sa("203.0.113.7:443")),
            "sg 还在线，这个 IP 仍然是转发器的出网目标"
        );
    }

    #[test]
    fn ipv6_target_is_never_treated_as_fake_ip() {
        let t = TunInbound {
            pool: Arc::new(FakeIpPool::new(vec![])),
            bypass: BypassSet::new(),
        };
        let a = sa("[2001:db8::1]:443");
        assert_eq!(t.classify(a), TunTarget::Ip(a));
    }

    /// 端口原样带过去，不做任何改写。
    ///
    /// 反查换掉的只是「主机」这一维；把 443 悄悄换成别的会让 fake-ip
    /// 变成一次隐式的端口转发 —— 那是入口层不该有的行为（§4.2 纪律②）。
    #[test]
    fn the_port_survives_reverse_lookup_verbatim() {
        let pool = Arc::new(FakeIpPool::new(vec![]));
        let ip = pool.allocate("odd-port.example").unwrap();
        let t = TunInbound::new(pool.clone(), BypassSet::new());
        assert_eq!(
            t.classify(SocketAddr::new(IpAddr::V4(ip), 8443)),
            TunTarget::Domain("odd-port.example".into(), 8443)
        );
    }

    /// 两种异常都要能被调用方一眼认出来，两种正常路径都不是异常。
    #[test]
    fn both_anomalies_are_flagged_and_normal_targets_are_not() {
        assert!(TunTarget::BypassLeak(sa("203.0.113.7:443")).is_anomaly());
        assert!(TunTarget::StaleFakeIp(sa("198.18.9.9:443")).is_anomaly());
        assert!(!TunTarget::Ip(sa("1.2.3.4:80")).is_anomaly());
        assert!(!TunTarget::Domain("example.com".into(), 443).is_anomaly());
    }

    /// TUN 自己的网关地址落在段内却永远不是有效映射。
    ///
    /// 它是 `198.18.0.1`，在池的起点 `198.18.0.4` 之前 —— 也就是说它落在
    /// **整段内但不在可分配范围内**。判据若用 `in_range`（可分配范围），
    /// 它会被当成普通真实 IP 连出去，而 `198.18.0.0/15` 整段都被路由进本
    /// 设备，包立刻绕回来：一个静默黑洞。必须用 `in_segment` 判、拒掉。
    #[test]
    fn the_tun_gateway_itself_is_never_a_valid_mapping() {
        let t = TunInbound::new(Arc::new(FakeIpPool::new(vec![])), BypassSet::new());
        let gw = sa("198.18.0.1:443");
        assert_eq!(t.classify(gw), TunTarget::StaleFakeIp(gw));
    }

    /// 可分配范围之外、但仍在 `/15` 段内的每一个地址都必须被拒。
    ///
    /// 这些是「路由进得来、反查出不去」的全部地址。逐个钉住，
    /// 免得日后有人把 `in_segment` 改回 `in_range` 而测试还是绿的。
    #[test]
    fn every_in_segment_address_outside_the_pool_is_refused() {
        let t = TunInbound::new(Arc::new(FakeIpPool::new(vec![])), BypassSet::new());
        for a in [
            "198.18.0.0:443",
            "198.18.0.1:443",
            "198.18.0.2:443",
            "198.18.0.3:443",
            "198.19.255.255:443",
        ] {
            assert_eq!(
                t.classify(sa(a)),
                TunTarget::StaleFakeIp(sa(a)),
                "{a} 会被路由进 TUN 却反查不到，按真实 IP 连出去就是黑洞"
            );
        }
    }

    /// 段的两侧邻居必须**照常**当真实 IP 处理。
    ///
    /// 上一条要求段内全拒，这一条防止拒过头：`198.17.255.255` 与
    /// `198.20.0.0` 都在 `/15` 之外，是公网可达地址，拒掉它们等于随手
    /// 黑洞掉两段真实互联网。
    #[test]
    fn addresses_just_outside_the_segment_are_ordinary_targets() {
        let t = TunInbound::new(Arc::new(FakeIpPool::new(vec![])), BypassSet::new());
        for a in ["198.17.255.255:443", "198.20.0.0:443"] {
            assert_eq!(t.classify(sa(a)), TunTarget::Ip(sa(a)), "{a} 在段外，是真实目标");
        }
    }
}
