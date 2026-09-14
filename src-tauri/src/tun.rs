//! TUN 的 Tauri 侧编排：把 `wsieve-tun` 接到既有的路由层与出站层。
//!
//! **本模块不含任何转发逻辑、不选出站、不匹配规则。** netstack 交出
//! `TcpStream` 之后走的是与混合端口入站**完全相同**的下游路径（§4.2 纪律②）：
//! 同一个 `wsieve_inbound::Dispatch`，同一个 `router::Router::dispatch`。
//! 这条判据由 `tun_rs_contains_no_forwarding_or_routing_code` 机器检查 ——
//! 阶段 6 完成标准里写着「没有任何转发/出站/规则匹配代码」，靠眼睛看是守不住的。
//!
//! 本模块只做四件事：
//!   1. 把 `TunTarget` 翻译成 `AddrPort`（路由层的输入类型）—— 唯一的接缝
//!   2. 把 netstack 的 `TcpStream` 交给**既有的** dispatch
//!   3. 维护 bypass 名单与路由，随出站增删同步
//!   4. 给 `TunRoutes` 补一个 `ManagedSystemState` 的转发 impl（见下）
//!
//! # 为什么 `ManagedSystemState` 的 impl 在这里而不在 crate 里
//!
//! 那个 trait 定义在 `crate::custody`，而 `wsieve-tun` 在依赖图的**下游**
//! （`src-tauri` 依赖 crates，反向不成立）。crate 里照抄一份就成了两个同名
//! 不同源的 trait，`CustodyGuard` 永远收不进来，`custody/mod.rs` 预告的
//! `Vec<Box<dyn ManagedSystemState>>` 统一收拢当场破功。
//!
//! 于是 crate 那边给的是**同名同形的固有方法**，这里补一个转发 impl ——
//! 本地 trait + 外部类型，孤儿规则允许。纪律一字不改，trait 定义仍只有一处。
//!
//! # 启动顺序在哪里
//!
//! 钉死的六步（§8.3.2）由 `wsieve_tun::startup::bring_up` 编排，其中
//! 「起转发器」与「写 hosts」两步归 `shard_setup::plan`。因此 bypass 路由
//! 这一步必须**插进 plan 内部**（解析之后、起转发器之前），走的是 plan 的
//! `on_upstream_resolved` 钩子。本模块的 [`bypass_hook`] 就是那个钩子。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use futures::StreamExt;
use wsieve_proto::addr::{AddrPort, TargetAddr};
use wsieve_tun::bypass::BypassSet;
use wsieve_tun::fakeip::FakeIpPool;
use wsieve_tun::inbound::{TunInbound, TunTarget};
use wsieve_tun::managed::TunRoutes;

/// `TunRoutes` 的托管转发。**只有转发，没有第二套纪律。**
///
/// 三个方法逐字转发到 crate 的固有方法上。写成 `TunRoutes::apply(self)` 而
/// 不是 `self.apply()`：固有方法在方法解析里优先于 trait 方法，后者看着
/// 像递归其实不是 —— 这种「读起来是一回事、编译出来是另一回事」的写法
/// 迟早会被人「修好」成真的递归。显式限定让它没有歧义。
impl crate::custody::ManagedSystemState for TunRoutes {
    fn name(&self) -> &'static str {
        "TUN 路由"
    }
    fn apply(&self) -> anyhow::Result<()> {
        TunRoutes::apply(self)
    }
    fn revert(&self) -> anyhow::Result<()> {
        TunRoutes::revert(self)
    }
    fn clear_stale(&self) -> anyhow::Result<()> {
        TunRoutes::clear_stale(self)
    }
}

/// 把 TUN 的判定结果翻译成路由层的输入。
///
/// **这是 TUN 与既有各层唯一的接缝。** 翻译完之后，一条 TUN 连接与一条
/// SOCKS5 连接对下游而言毫无区别 —— 这正是「TUN 只是又一个入口」的实体。
///
/// 返回 `None` 的两种情况都**不进路由层**：它们是异常，不是目标。
pub fn to_addr_port(t: &TunTarget) -> Option<AddrPort> {
    match t {
        TunTarget::Domain(d, port) => Some(AddrPort {
            addr: TargetAddr::Domain(d.clone()),
            port: *port,
        }),
        TunTarget::Ip(sa) => Some(AddrPort {
            addr: match sa.ip() {
                IpAddr::V4(v) => TargetAddr::V4(v.octets()),
                IpAddr::V6(v) => TargetAddr::V6(v.octets()),
            },
            port: sa.port(),
        }),
        // 前者是环路漏了，后者是映射没了。两者都必须**报警后拒绝**，
        // 绝不能悄悄当成普通目标送进路由层。
        TunTarget::BypassLeak(_) | TunTarget::StaleFakeIp(_) => None,
    }
}

/// fake-ip 池的构造：**服务器域名自动并入 filter**（§7.2 纪律①）。
///
/// 调用方只需把出站的服务器域名递进来，不需要记得往 `fake-ip-filter` 里
/// 补一笔。记不住是必然的，而漏掉的后果是转发器连向虚空且全程无日志。
pub fn build_pool(config_filter: Vec<String>, server_domains: &[String]) -> Arc<FakeIpPool> {
    Arc::new(FakeIpPool::new(config_filter).with_server_domains(server_domains))
}

/// 出站解析出真实 IP 时同步 bypass 名单与系统路由。
///
/// 名单与路由**必须一起动**：只更名单则系统路由还是旧的，转发器的包照样
/// 进 TUN；只写路由则判定层认不出那个 IP，`classify` 会把它当普通目标。
/// 两道防线缺任何一道，环路就成立。
///
/// # 反向操作（出站下线）在哪里
///
/// 手工验证清单 M9「出站下线时 bypass 路由同步摘除」需要一个**多出站的
/// 生命周期**，而当前的启动路径只描述一个出站（`main.rs` 的 env 配置，
/// `commands::control::outbound_enable` 尚是占位）。因此这里**不预先写**
/// 一个没有调用方的 `drop_outbound` —— 那是死代码。
///
/// 摘除的全部逻辑已经在 crate 里就位并有测试：`BypassSet::remove` 只报
/// 引用计数归零的 IP（共享同一台服务器的另一个出站不会被误伤），后接一次
/// `TunRoutes::sync_bypass(&bypass.snapshot())` 即可。多出站管理器落地时
/// 照这两行接上，判据本身一个字都不用改。
pub fn sync_outbound(
    bypass: &BypassSet,
    routes: &TunRoutes,
    outbound_id: &str,
    ips: Vec<IpAddr>,
) -> anyhow::Result<()> {
    bypass.insert(outbound_id, ips);
    routes
        .sync_bypass(&bypass.snapshot())
        .map_err(|e| anyhow::anyhow!("同步出站「{outbound_id}」的 bypass 路由失败：{e:#}"))
}

/// 造一个供 `shard_setup::plan` 使用的 bypass 钩子。
///
/// **它必须在「解析之后、起转发器之前」被调用**（§8.3.2 的第 2 步）。
/// 钩子失败时 `plan` 会降级并把错误带回来，调用方据此**拒绝拉起 TUN** ——
/// 带着空 bypass 拉起 TUN 是确定性的环路。
pub fn bypass_hook(
    bypass: BypassSet,
    routes: Arc<TunRoutes>,
    outbound_id: String,
) -> impl Fn(SocketAddr) -> anyhow::Result<()> + Send + Sync + 'static {
    move |upstream: SocketAddr| sync_outbound(&bypass, &routes, &outbound_id, vec![upstream.ip()])
}

/// TUN 入站主循环。
///
/// `dispatch` 就是混合端口入口用的那一个 —— **同一个类型、同一个闭包**，
/// 不是同名的另一个。它内部是 `router::Router::dispatch`，规则匹配与出站
/// 选择全在那里；本函数一行都没有。
pub async fn run(stack: wsieve_tun::device::NetStack, inbound: Arc<TunInbound>, dispatch: wsieve_inbound::Dispatch) {
    let mut tcp = stack.tcp;
    let if_name = stack.if_name;
    tracing::info!("TUN 入站循环就绪：{if_name}");
    while let Some((stream, _local, remote)) = tcp.next().await {
        let t = inbound.classify(remote);
        let inbound_dbg = format!("{t:?}");
        match to_addr_port(&t) {
            Some(target) => {
                let d = dispatch.clone();
                tokio::spawn(async move {
                    let mut stream = stream;
                    let shown = target.display();
                    match d(target).await {
                        Ok(mut down) => {
                            // 这一行与 `wsieve-socks5/src/lib.rs:113`、
                            // `wsieve-inbound/src/http.rs:297` 逐字同形：每个入口
                            // 在下游连通之后都要把两端对接起来。它不是一套新的
                            // 转发逻辑，是同一条收尾的第三个调用点。
                            if let Err(e) = tokio::io::copy_bidirectional(&mut stream, &mut down).await {
                                tracing::debug!("TUN 连接 {shown} 搬运结束：{e}");
                            }
                        }
                        // 拒绝要留痕。TUN 没有 SOCKS5 那样的回复码可用，
                        // 唯一能告诉用户「为什么打不开」的地方就是日志。
                        Err(e) => tracing::debug!("TUN 连接 {shown} 被拒：{e}"),
                    }
                });
            }
            None => {
                match t {
                    TunTarget::BypassLeak(sa) => {
                        // bypass 路由本该拦住它。走到这里说明路由表被改了 ——
                        // 必须报警，否则表现为「代理莫名其妙卡死」（§8.3.1）。
                        tracing::error!(
                            "环路风险：到服务器 {sa} 的连接被 TUN 捕获，说明 bypass 路由未生效。\
                             请检查是否有其他 VPN / 网络工具改写了路由表"
                        );
                    }
                    TunTarget::StaleFakeIp(sa) => {
                        // 198.18/15 在公网不可达，连出去只会挂到超时。
                        tracing::warn!("拒绝陈旧 fake-ip {sa}：映射已失效，请让客户端重新查询 DNS");
                    }
                    // `to_addr_port` 对 Domain / Ip 一定返回 Some，走不到这里。
                    // 不用 unreachable!()：入口层 panic 会带走整个进程，而这
                    // 只是一条连接。照实记一条日志然后丢弃。
                    other => tracing::error!("TUN 判定 {other:?} 未被翻译也未被处理，丢弃该连接"),
                }
                drop(stream);
                tracing::debug!("TUN 丢弃连接：{inbound_dbg}");
            }
        }
    }
    // 循环退出 = TUN 入口彻底失守。绝不静默。
    tracing::error!("TUN 入站循环结束（{if_name}）：协议栈已停，TUN 流量从此无人处理");
}

/// fake-ip 段归属的探测地址。取池的第一个可分配地址而不是网关 ——
/// 网关是我们自己要配上去的，探测它等于问「我自己在不在」。
pub const RANGE_PROBE: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 4);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::custody::ManagedSystemState;
    use std::sync::Mutex;
    use wsieve_tun::routes::RouteEntry;

    /// 记录动作的路由后端。**不执行** `route`(8) —— 它要 root，单测里真跑
    /// 一条就会在开发机上留下黑洞路由（同 `custody/hosts.rs` 的测试绝不
    /// 指向真实 /etc/hosts）。
    #[derive(Default)]
    struct RecordingBackend {
        table: Mutex<Vec<RouteEntry>>,
        calls: Mutex<Vec<String>>,
    }

    impl wsieve_tun::managed::RouteBackend for RecordingBackend {
        fn add(&self, e: &RouteEntry) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(format!("add {}", e.dest));
            self.table.lock().unwrap().push(e.clone());
            Ok(())
        }
        fn delete(&self, e: &RouteEntry) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(format!("delete {}", e.dest));
            self.table.lock().unwrap().retain(|x| x != e);
            Ok(())
        }
        fn list_managed(&self) -> anyhow::Result<Vec<RouteEntry>> {
            Ok(self.table.lock().unwrap().clone())
        }
    }

    fn routes() -> (Arc<RecordingBackend>, TunRoutes) {
        let b = Arc::new(RecordingBackend::default());
        let r = TunRoutes::new(b.clone(), "198.18.0.1", "10.0.0.1");
        (b, r)
    }

    #[test]
    fn domain_target_keeps_the_domain_so_domain_rules_can_match() {
        // 反查回来的域名必须原样进路由层，否则 GEOSITE / DOMAIN-SUFFIX
        // 全部失效 —— 而 fake-ip 存在的**全部理由**就是让它们能生效。
        let t = TunTarget::Domain("example.com".into(), 443);
        let a = to_addr_port(&t).unwrap();
        assert_eq!(a.addr, TargetAddr::Domain("example.com".into()));
        assert_eq!(a.port, 443);
    }

    #[test]
    fn ip_targets_translate_for_both_families_with_the_port_intact() {
        let v4 = to_addr_port(&TunTarget::Ip("1.2.3.4:8080".parse().unwrap())).unwrap();
        assert_eq!(v4.addr, TargetAddr::V4([1, 2, 3, 4]));
        assert_eq!(v4.port, 8080);

        let v6 = to_addr_port(&TunTarget::Ip("[2001:db8::1]:443".parse().unwrap())).unwrap();
        let mut want = [0u8; 16];
        want[0] = 0x20;
        want[1] = 0x01;
        want[2] = 0x0d;
        want[3] = 0xb8;
        want[15] = 1;
        assert_eq!(v6.addr, TargetAddr::V6(want));
        assert_eq!(v6.port, 443);
    }

    /// **两类异常绝不能进路由层。**
    ///
    /// `BypassLeak` 一旦被翻译成 `AddrPort`，路由层就会命中 `MATCH` 把它
    /// 送去代理 —— 环路当场闭合。这条断言是 `to_addr_port` 存在返回值为
    /// `None` 的那一支的全部理由。
    #[test]
    fn neither_anomaly_is_ever_translated_into_a_routable_target() {
        let leak = TunTarget::BypassLeak("203.0.113.7:443".parse().unwrap());
        let stale = TunTarget::StaleFakeIp("198.18.9.9:443".parse().unwrap());
        assert!(to_addr_port(&leak).is_none(), "服务器 IP 进了路由层就是环路");
        assert!(to_addr_port(&stale).is_none(), "陈旧 fake-ip 连出去只会超时");
        // 与 crate 侧的异常判据保持一致 —— 两处若分家，就会出现
        // 「翻译成 None 却不报警」或者反过来的组合。
        assert!(leak.is_anomaly() && stale.is_anomaly());
    }

    /// 托管转发 impl 的三个方法真的落到 crate 的固有方法上。
    ///
    /// 写成 `self.apply()` 时它会解析到固有方法（固有优先于 trait），
    /// 看着像递归其实不是；而一旦有人把固有方法改名，`self.apply()` 就会
    /// **真的**变成无限递归。这条测试盯住的是「转发确实发生了」。
    #[test]
    fn the_forwarding_impl_actually_reaches_the_crate_methods() {
        let (b, r) = routes();
        r.sync_bypass(&["203.0.113.7".parse().unwrap()]).unwrap();

        ManagedSystemState::apply(&r).unwrap();
        let dests: Vec<String> = b.table.lock().unwrap().iter().map(|e| e.dest.clone()).collect();
        assert!(dests.contains(&"0.0.0.0/1".to_string()), "{dests:?}");
        assert!(dests.contains(&"128.0.0.0/1".to_string()), "{dests:?}");
        assert!(dests.contains(&"203.0.113.7".to_string()), "{dests:?}");

        ManagedSystemState::revert(&r).unwrap();
        assert!(b.table.lock().unwrap().is_empty(), "revert 必须清空");

        // clear_stale 与 revert 同一动作，且幂等。
        ManagedSystemState::clear_stale(&r).unwrap();
        assert_eq!(ManagedSystemState::name(&r), "TUN 路由");
    }

    /// `CustodyGuard` 能收下 `TunRoutes` —— 这正是不在 crate 里重定义 trait
    /// 的目的。收不下就说明又冒出了第二个同名 trait。
    #[test]
    fn custody_guard_accepts_tun_routes_and_reverts_on_drop() {
        let (b, r) = routes();
        {
            let g = crate::custody::CustodyGuard::acquire(r).unwrap();
            assert!(!b.table.lock().unwrap().is_empty(), "acquire 应当写入路由");
            // 借出来的还是同一个对象 —— 后续 wiring 要靠它同步 bypass。
            assert!(g.get().bypass_snapshot().is_empty());
        }
        assert!(b.table.lock().unwrap().is_empty(), "drop 必须摘除全部路由");
    }

    /// `acquire` 是**先 clear_stale 再 apply**：上一次崩溃的残骸先清掉，
    /// 本次写的不会被紧接着的清理抹掉。
    #[test]
    fn acquire_clears_the_previous_crashs_leftovers_first() {
        let b = Arc::new(RecordingBackend::default());
        // 上次崩溃留下的：指向一个已经不存在的 utun 的半个 IPv4 空间。
        wsieve_tun::managed::RouteBackend::add(
            &*b,
            &RouteEntry {
                dest: "0.0.0.0/1".into(),
                gateway: "198.18.0.1".into(),
            },
        )
        .unwrap();
        b.calls.lock().unwrap().clear();

        let r = TunRoutes::new(b.clone(), "198.18.0.1", "10.0.0.1");
        let g = crate::custody::CustodyGuard::acquire(r).unwrap();

        let calls = b.calls.lock().unwrap().clone();
        let first_add = calls.iter().position(|c| c.starts_with("add")).unwrap();
        let first_delete = calls.iter().position(|c| c.starts_with("delete")).unwrap();
        assert!(first_delete < first_add, "清残留必须早于写入：{calls:?}");
        assert_eq!(
            b.table.lock().unwrap().iter().filter(|e| e.dest == "0.0.0.0/1").count(),
            1,
            "清完再写才不会攒出两份"
        );
        drop(g);
    }

    /// 出站上下线时名单与路由**一起动**。
    ///
    /// 下线走的是「`BypassSet::remove` + 一次 `sync_bypass`」那两行 ——
    /// 也就是多出站管理器落地时该照抄的东西（见 `sync_outbound` 的文档）。
    #[test]
    fn syncing_an_outbound_moves_the_list_and_the_routes_together() {
        let (b, r) = routes();
        let bypass = BypassSet::new();

        sync_outbound(&bypass, &r, "jp", vec!["203.0.113.7".parse().unwrap()]).unwrap();
        assert!(bypass.contains(&"203.0.113.7".parse().unwrap()), "名单没收");
        assert!(
            b.table.lock().unwrap().iter().any(|e| e.dest == "203.0.113.7"),
            "路由没写"
        );

        bypass.remove("jp");
        r.sync_bypass(&bypass.snapshot()).unwrap();
        assert!(!bypass.contains(&"203.0.113.7".parse().unwrap()), "名单没摘");
        assert!(
            !b.table.lock().unwrap().iter().any(|e| e.dest == "203.0.113.7"),
            "路由没删"
        );
    }

    /// 共享同一台服务器的两个出站，摘一个时路由必须留着 ——
    /// 删掉它，剩下那个出站当场进环路。这正是手工验证清单 M9 在无 root
    /// 环境下能验到的那一半（真机上路由表的形态仍须手工确认）。
    #[test]
    fn dropping_one_of_two_outbounds_sharing_a_host_keeps_the_route() {
        let (b, r) = routes();
        let bypass = BypassSet::new();
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        sync_outbound(&bypass, &r, "jp", vec![ip]).unwrap();
        sync_outbound(&bypass, &r, "sg", vec![ip]).unwrap();

        bypass.remove("jp");
        r.sync_bypass(&bypass.snapshot()).unwrap();
        assert!(
            b.table.lock().unwrap().iter().any(|e| e.dest == "203.0.113.7"),
            "sg 还在线，这条路由删掉它就进环路"
        );

        bypass.remove("sg");
        r.sync_bypass(&bypass.snapshot()).unwrap();
        assert!(!b.table.lock().unwrap().iter().any(|e| e.dest == "203.0.113.7"));
    }

    /// 钩子把 `plan` 解析出的地址原样变成一条 bypass 路由。
    #[test]
    fn the_bypass_hook_turns_a_resolved_upstream_into_a_route() {
        let (b, r) = routes();
        let bypass = BypassSet::new();
        let hook = bypass_hook(bypass.clone(), Arc::new(r), "jp".into());

        hook("203.0.113.7:443".parse().unwrap()).unwrap();
        assert!(
            b.table.lock().unwrap().iter().any(|e| e.dest == "203.0.113.7"),
            "钩子没写 bypass 路由 —— 转发器会在 TUN 覆盖下裸奔"
        );
        // 端口不进路由：路由是**主机**路由，带端口就成了另一个地址。
        assert!(!b.table.lock().unwrap().iter().any(|e| e.dest.contains(':')));
    }

    /// **钩子失败必须报出来。** 静默吞掉等于带着空 bypass 拉起 TUN。
    #[test]
    fn a_failing_route_backend_makes_the_hook_report_not_swallow() {
        struct Failing;
        impl wsieve_tun::managed::RouteBackend for Failing {
            fn add(&self, _: &RouteEntry) -> anyhow::Result<()> {
                anyhow::bail!("must be root to alter routing table")
            }
            fn delete(&self, _: &RouteEntry) -> anyhow::Result<()> {
                Ok(())
            }
            fn list_managed(&self) -> anyhow::Result<Vec<RouteEntry>> {
                Ok(vec![])
            }
        }
        let r = Arc::new(TunRoutes::new(Arc::new(Failing), "198.18.0.1", "10.0.0.1"));
        let hook = bypass_hook(BypassSet::new(), r, "jp".into());
        let e = hook("203.0.113.7:443".parse().unwrap()).unwrap_err();
        let m = format!("{e:#}");
        assert!(m.contains("jp"), "错误要点名是哪个出站：{m}");
        assert!(m.contains("bypass"), "错误要点名失败的是什么：{m}");
    }

    /// 服务器域名自动进 filter，且不误伤其余域名。
    #[test]
    fn build_pool_filters_server_domains_without_touching_the_rest() {
        let pool = build_pool(vec!["*.local".into()], &["srv.example.com".into()]);
        assert!(pool.is_filtered("srv.example.com"), "服务器域名必须进 filter");
        assert!(pool.is_filtered("api.srv.example.com"), "子域也要");
        assert!(pool.is_filtered("box.local"), "配置里的 filter 不能被覆盖掉");
        assert!(!pool.is_filtered("www.example.com"));
        assert!(pool.allocate("www.example.com").is_ok());
    }

    /// 探测地址必须落在**可分配范围**内。
    ///
    /// 取 TUN 网关（`198.18.0.1`）去探测是个很自然的错误：那个地址是我们
    /// 自己要配上去的，问它等于问「我自己在不在」，第二次启动必然自我误判。
    #[test]
    fn the_range_probe_is_allocatable_and_is_not_the_gateway() {
        assert!(FakeIpPool::in_range(RANGE_PROBE));
        assert!(FakeIpPool::in_segment(RANGE_PROBE));
        assert_ne!(
            RANGE_PROBE.to_string(),
            wsieve_tun::device::TUN_ADDR,
            "拿网关探段归属会让第二次启动自我误判成冲突"
        );
    }

    /// **阶段 6 完成标准的可检查判据。**
    ///
    /// 「`src-tauri/src/tun.rs` 里没有任何转发/出站/规则匹配代码」这条标准
    /// 靠人眼是守不住的 —— 半年后有人为了「顺手」在这里加一句选出站，评审
    /// 未必看得出来。这里把它变成编译进测试的机器检查。
    ///
    /// 只扫 `mod tests` 之前的部分：测试自己要提到这些名字才能断言。
    #[test]
    fn tun_rs_contains_no_forwarding_or_routing_code() {
        let src = include_str!("tun.rs");
        let body = src
            .split("#[cfg(test)]")
            .next()
            .expect("split 至少给一段");
        for forbidden in [
            // 规则匹配
            "RuleSet",
            "check_geo",
            "GeoDb",
            "Decision",
            "Outcome",
            // 出站选择与建连
            "via_outbound",
            "OutboundInstance",
            "OutboundManager",
            "dialer(",
            "TcpStream::connect",
            "direct_connect",
        ] {
            assert!(
                !body.contains(forbidden),
                "`{forbidden}` 出现在 tun.rs 里 —— TUN 是入口，规则与出站归 router.rs。\
                 阶段 6 完成标准明确要求这里没有这类代码"
            );
        }
        // 反向护栏：本文件确实是那个编排层，而不是被谁改成了空壳 ——
        // 一个空文件也能通过上面全部断言。
        assert!(body.contains("fn to_addr_port"), "接缝函数不见了");
        assert!(body.contains("copy_bidirectional"), "入站循环不见了");
    }
}
