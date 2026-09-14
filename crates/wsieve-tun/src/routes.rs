//! TUN 路由表命令构造（设计文档 §10 的第三个 `ManagedSystemState` 的下半截）。
//!
//! 本模块**只构造命令、只解析输出**，一条都不执行 —— 执行在 `managed.rs`。
//! 这条缝是被实测逼出来的：`route`(8) **即使加 `-t`（test-only 模式）也要求
//! root**（实测 `rc=77`，`route: must be root to alter routing table`）。
//! 构造是纯函数因而可穷举单测，执行只能进手工验证清单。
//!
//! 两类路由，职责完全不同：
//!
//! | 类别 | 内容 | 目的 |
//! |---|---|---|
//! | **默认路由** | `0.0.0.0/1` + `128.0.0.0/1` 指向 TUN | 抢在系统 `default` 之前接管全部流量。用两条 /1 而不是改 `default`，是因为改 `default` 会把原网关记录冲掉，崩溃后恢复不了 |
//! | **bypass 路由** | 每个服务器 IP 一条 `-host` 指向**物理网关** | 环路防线（§8.3.1）。转发器出网连接因此不进 TUN |
//!
//! # argv 而非 shell 字符串
//!
//! 与 `custody/sysproxy.rs` 同一条纪律：命令一律以 `Vec<String>` 形式交给
//! `Command::args`，绝不拼 `sh -c`。这里的参数虽然大多来自 `IpAddr::to_string()`
//! 看着无害，但网关地址是从 `route -n get default` 的输出里**解析**来的 ——
//! 一旦有人改成拼命令行，那就是一条从外部输出直达 shell 的路。argv 让这个
//! 问题在结构上不存在。

use std::net::IpAddr;

/// 一条待写入的路由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteEntry {
    /// 目的网段，`net/prefix` 或单个 host。
    pub dest: String,
    /// 下一跳。
    pub gateway: String,
}

/// 平台无关的路由动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteOp {
    Add,
    Delete,
}

impl RouteOp {
    /// `route`(8) 的动词。
    fn verb(self) -> &'static str {
        match self {
            RouteOp::Add => "add",
            RouteOp::Delete => "delete",
        }
    }
}

/// 构造 macOS `route` 命令的参数向量（不含程序名）。
///
/// `-q` 抑制正常输出但**不抑制退出码，也不抑制 stderr**（已实测：无权限时
/// 照样打印 "must be root to alter routing table" 并返回 77），错误仍能被
/// 发现，不会静默。`-n` 不做反解，避免路由表故障时命令自己卡在 DNS 上 ——
/// 而我们恰恰在**改**路由表，此时 DNS 大概率正不可用。
#[cfg(target_os = "macos")]
pub fn argv(op: RouteOp, e: &RouteEntry) -> Vec<String> {
    let mut v = vec!["-n".to_string(), "-q".to_string(), op.verb().to_string()];
    // IPv6 目的地必须显式声明地址族，否则 route 按 AF_INET 解释后直接报错。
    if e.dest.contains(':') {
        v.push("-inet6".into());
    }
    // `1.2.3.4` 无前缀即 host 路由；带 `/` 的按网段。显式给 -host/-net
    // 而不依赖 route 的启发式推断 —— man route(8) 白纸黑字：
    // 「128.32 is interpreted as -host 128.0.0.32」。少写一个修饰符，
    // 路由就可能安静地指到另一个地址上去。
    if e.dest.contains('/') {
        v.push("-net".into());
    } else {
        v.push("-host".into());
    }
    v.push(e.dest.clone());
    v.push(e.gateway.clone());
    v
}

/// TUN 接管所需的默认路由对。
///
/// 拆成 `0.0.0.0/1` 与 `128.0.0.0/1` 两条：它们合起来覆盖整个 IPv4 空间，
/// 但**前缀比 `default`(/0) 更长**，因此优先级更高，同时原来的
/// `default` 记录原封不动 —— 崩溃后系统立刻恢复正常上网。
///
/// 这也意味着「默认路由是不是我们的」**看 `default` 那一行是答不出来的**：
/// 判据只能是这两条 /1 的存在与它们的网关（`managed.rs` 的托管判据即此）。
pub fn tun_default_routes(tun_gateway: &str) -> Vec<RouteEntry> {
    vec![
        RouteEntry {
            dest: "0.0.0.0/1".into(),
            gateway: tun_gateway.into(),
        },
        RouteEntry {
            dest: "128.0.0.0/1".into(),
            gateway: tun_gateway.into(),
        },
    ]
}

/// 服务器 IP 的 bypass 路由：显式指向**物理**网关，绕开 TUN。
///
/// 这是环路防线（§8.3.1）落到路由表上的那一层。netstack 不会替我们避开
/// 环路（计划文档已实证），少一条这里的路由，对应那个出站就当场死循环。
pub fn bypass_routes(ips: &[IpAddr], phys_gateway: &str) -> Vec<RouteEntry> {
    ips.iter()
        .map(|ip| RouteEntry {
            dest: ip.to_string(),
            gateway: phys_gateway.into(),
        })
        .collect()
}

/// `route -n get <addr>` 输出里我们关心的三行。
///
/// 三个字段都是 `Option`：`route get` 在直连网段上**不打印 `gateway:`**
/// （已实测：`route -n get 10.0.0.5` 只有 destination/mask/interface）。
/// 把「没有网关」和「网关是空字符串」区分开，调用方才能报出有意义的错。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteQuery {
    pub destination: Option<String>,
    pub gateway: Option<String>,
    pub interface: Option<String>,
}

/// 解析 `route -n get <addr>` 的输出。
///
/// 该命令**不需要 root**（已实测，只有 add/delete/change 需要），因此
/// 「物理网关是哪个」与「某个地址当前归谁管」这两件事在无特权下就能问到。
///
/// 典型输出：
/// ```text
///    route to: default
/// destination: default
///        mask: default
///     gateway: 10.0.0.1
///   interface: en0
///       flags: <UP,GATEWAY,DONE,STATIC>
/// ```
/// 逐行按 `键: 值` 切，只认我们要的三个键；其余（`route to:`、`mask:`、
/// `flags:`、末尾的统计表）一律忽略。
pub fn parse_route_get(out: &str) -> RouteQuery {
    let mut q = RouteQuery::default();
    for line in out.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            // `route to:` 也以 `to` 结尾但键名不同，精确匹配即可区分。
            "destination" => q.destination = Some(value.to_string()),
            "gateway" => q.gateway = Some(value.to_string()),
            "interface" => q.interface = Some(value.to_string()),
            _ => {}
        }
    }
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn default_routes_use_two_halves_not_default_keyword() {
        // 改 `default` 会冲掉原网关记录，崩溃后无法恢复。必须用两条 /1。
        let r = tun_default_routes("198.18.0.1");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].dest, "0.0.0.0/1");
        assert_eq!(r[1].dest, "128.0.0.0/1");
        assert!(
            !r.iter()
                .any(|e| e.dest == "default" || e.dest == "0.0.0.0/0"),
            "绝不能动 default 路由"
        );
        assert!(r.iter().all(|e| e.gateway == "198.18.0.1"));
    }

    #[test]
    fn two_halves_cover_the_entire_ipv4_space() {
        // 两条 /1 的价值全在「合起来无缝覆盖」上：漏一个位就有流量绕过 TUN
        // 而我们毫无察觉。这里直接按位验算，而不是信注释。
        let r = tun_default_routes("198.18.0.1");
        let (a, b) = (&r[0].dest, &r[1].dest);
        let parse = |s: &str| -> (u32, u32) {
            let (net, bits) = s.split_once('/').unwrap();
            let n: std::net::Ipv4Addr = net.parse().unwrap();
            (u32::from(n), bits.parse().unwrap())
        };
        let (n0, p0) = parse(a);
        let (n1, p1) = parse(b);
        assert_eq!((p0, p1), (1, 1));
        // /1 的两半：0x00000000..=0x7fffffff 与 0x80000000..=0xffffffff
        assert_eq!(n0, 0x0000_0000);
        assert_eq!(n1, 0x8000_0000);
        // 前缀比 default(/0) 长，故优先级更高。
        assert!(p0 > 0 && p1 > 0, "前缀必须严格长于 /0 才能压过 default");
    }

    #[test]
    fn bypass_routes_point_at_physical_gateway() {
        let r = bypass_routes(&[ip("203.0.113.7")], "10.0.0.1");
        assert_eq!(
            r,
            vec![RouteEntry {
                dest: "203.0.113.7".into(),
                gateway: "10.0.0.1".into(),
            }]
        );
    }

    #[test]
    fn bypass_routes_never_point_at_the_tun() {
        // 指回 TUN 就等于没 bypass —— 环路照样成立，而且看上去「路由已写」。
        let ips = [ip("203.0.113.7"), ip("198.51.100.9")];
        let r = bypass_routes(&ips, "10.0.0.1");
        assert_eq!(r.len(), 2);
        assert!(r.iter().all(|e| e.gateway == "10.0.0.1"));
        assert!(r.iter().all(|e| !e.dest.contains('/')), "服务器 IP 是 host 路由");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn host_and_net_are_explicit_not_inferred() {
        // route(8) 的启发式会把 `128.32` 解释成 `128.0.0.32`。显式给出
        // -host / -net 才不会在某些 IP 上突然指错地方。
        let host = argv(
            RouteOp::Add,
            &RouteEntry {
                dest: "203.0.113.7".into(),
                gateway: "10.0.0.1".into(),
            },
        );
        assert_eq!(host, ["-n", "-q", "add", "-host", "203.0.113.7", "10.0.0.1"]);

        let net = argv(
            RouteOp::Add,
            &RouteEntry {
                dest: "0.0.0.0/1".into(),
                gateway: "198.18.0.1".into(),
            },
        );
        assert_eq!(net, ["-n", "-q", "add", "-net", "0.0.0.0/1", "198.18.0.1"]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn delete_mirrors_add() {
        let e = RouteEntry {
            dest: "203.0.113.7".into(),
            gateway: "10.0.0.1".into(),
        };
        let a = argv(RouteOp::Add, &e);
        let d = argv(RouteOp::Delete, &e);
        assert_eq!(a[2], "add");
        assert_eq!(d[2], "delete");
        // 除了动词，其余参数必须逐字一致，否则删不掉自己加的那条。
        assert_eq!(a[3..], d[3..]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ipv6_destination_declares_its_address_family() {
        // bypass 名单里的 IP 来自运行时 DNS 解析，AAAA 记录会带进 v6 地址。
        // 不给 -inet6，route 会按 AF_INET 解释这个目的地并直接失败。
        let v6 = argv(
            RouteOp::Add,
            &RouteEntry {
                dest: "2001:db8::1".into(),
                gateway: "fe80::1%en0".into(),
            },
        );
        assert_eq!(
            v6,
            ["-n", "-q", "add", "-inet6", "-host", "2001:db8::1", "fe80::1%en0"]
        );
        // v4 不能被误加地址族修饰符。
        let v4 = argv(
            RouteOp::Add,
            &RouteEntry {
                dest: "203.0.113.7".into(),
                gateway: "10.0.0.1".into(),
            },
        );
        assert!(!v4.iter().any(|a| a == "-inet6"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn argv_arity_is_fixed_so_nothing_can_smuggle_extra_arguments() {
        // 沿用 custody/sysproxy.rs 的纪律：走 argv 时元字符只是普通字节。
        // 网关是从 `route -n get default` 的输出里解析来的，一旦有人把这里
        // 改回拼 shell 命令行，这条会挂 —— 它就是为那一天准备的。
        let evil = r#"10.0.0.1"; rm -rf /tmp/x; echo ""#.to_string();
        let a = argv(
            RouteOp::Add,
            &RouteEntry {
                dest: "203.0.113.7".into(),
                gateway: evil.clone(),
            },
        );
        assert_eq!(a.len(), 6, "参数个数固定，元字符不得引入额外参数：{a:?}");
        assert_eq!(a[5], evil, "整体作为一个参数原样传递");
    }

    /// 真机上 `route -n get default` 的原样输出（本机实测抓取）。
    const REAL_DEFAULT: &str = "\
   route to: default
destination: default
       mask: default
    gateway: 10.0.0.1
  interface: en0
      flags: <UP,GATEWAY,DONE,STATIC,PRCLONING,GLOBAL>
 recvpipe  sendpipe  ssthresh  rtt,msec    rttvar  hopcount      mtu     expire
       0         0         0         0         0         0      1500         0
";

    #[test]
    fn parses_physical_gateway_out_of_route_get_default() {
        // 物理网关就是这么取的（`managed.rs::default_gateway`）。取错了，
        // 全部 bypass 路由都会指向一个不通的下一跳 —— 出站直接连不上。
        let q = parse_route_get(REAL_DEFAULT);
        assert_eq!(q.gateway.as_deref(), Some("10.0.0.1"));
        assert_eq!(q.interface.as_deref(), Some("en0"));
        assert_eq!(q.destination.as_deref(), Some("default"));
    }

    #[test]
    fn route_to_line_is_not_mistaken_for_destination() {
        // 首行是 `route to:`，与 `destination:` 值可能不同（见下面的 /1 例子）。
        // 前缀匹配会把它俩混起来，判据从此指着错的东西。
        let q = parse_route_get(REAL_DEFAULT);
        assert_eq!(
            q.destination.as_deref(),
            Some("default"),
            "必须取 destination 行而不是 route to 行"
        );
    }

    /// 本机实测：另一个 TUN 客户端（utun49）用 `0/1 + 128.0/1` 盖住了默认路由，
    /// 于是 fake-ip 段 `198.18.0.0/15` 里的地址全被它接走 —— 注意 `destination`
    /// 报的是 **`128.0.0.0`**（那条 /1），不是查询地址本身。
    const REAL_FAKEIP_TAKEN: &str = "\
   route to: 198.18.0.4
destination: 128.0.0.0
       mask: 128.0.0.0
    gateway: 172.18.0.1
  interface: utun49
      flags: <UP,GATEWAY,DONE,STATIC,PRCLONING>
";

    #[test]
    fn detects_a_foreign_tun_owning_the_fakeip_range() {
        // 两个 fake-ip 池共用 198.18.0.0/15 会互相吞对方的应答。
        // 判据是**接口名**：不是我们的 utun，这个段就已经被别人占了。
        let q = parse_route_get(REAL_FAKEIP_TAKEN);
        assert_eq!(q.interface.as_deref(), Some("utun49"));
        assert_eq!(q.gateway.as_deref(), Some("172.18.0.1"));
        assert_ne!(
            q.interface.as_deref(),
            Some("utun9"),
            "接口不是我们的，说明该段已被另一个 TUN 客户端接管"
        );
    }

    /// 直连网段：`route -n get` **不打印 gateway 行**（本机实测）。
    const REAL_ONLINK: &str = "\
   route to: 10.0.0.5
destination: 10.0.0.0
       mask: 255.255.0.0
  interface: en0
      flags: <UP,DONE,CLONING,STATIC>
";

    #[test]
    fn missing_gateway_line_is_none_not_empty_string() {
        // 直连网段没有下一跳。若把它当成空字符串网关，就会构造出
        // `route add -host <ip> ""` 这种命令 —— 失败信息还完全看不出所以然。
        let q = parse_route_get(REAL_ONLINK);
        assert_eq!(q.gateway, None, "没有 gateway 行时必须是 None");
        assert_eq!(q.interface.as_deref(), Some("en0"));
    }

    #[test]
    fn unparseable_output_yields_no_fields_rather_than_garbage() {
        // 宁可三个字段全 None 让调用方报错，也不要猜出一个假网关。
        let q = parse_route_get("route: writing to routing socket: not in table\n");
        assert_eq!(q, RouteQuery::default());
    }
}
