//! TUN 启动编排：设计文档 §8.3.2 的**钉死顺序**。
//!
//! ```text
//! 查段归属 → 解析真实 IP → 写 bypass 路由 → 起转发器 → 写 hosts → 最后拉起 TUN
//! ```
//!
//! 任何一步顺序颠倒都**静默失败**：TUN 的 DNS 劫持若先生效，
//! `resolve_upstream` 拿到的就是 fake-ip，转发器连向虚空，而日志上
//! 什么异常都没有。
//!
//! 顺序纪律怎么变成可测的东西：把每一步记进一条时间线（`Step` 序列），
//! 编排函数只负责按序追加，测试直接断言序列。真实副作用由注入的
//! `TunStage` 实现承担，测试用假实现即可覆盖全部顺序分支 —— 不需要 root。
//!
//! # 为什么第一步是「查段归属」而不是「解析」
//!
//! 计划文档的原始顺序是五步，以解析打头。实测（2026-08-25，本机）发现
//! 前面还得再加一步：机器上跑着另一个 TUN 客户端时，`198.18.0.0/15`
//! 已经被它的 fake-ip 池占用（`utun49` 用 `0/1 + 128.0/1` 盖住默认路由）。
//! 两个池共用同一个段，分配出的假 IP 互相撞车，反查时各自认领对方的
//! 地址 —— 表现是随机的域名错连，无从排查。
//!
//! 这一步必须排在**最前面**，因为它是唯一一个「还没改动系统任何状态」
//! 的时刻。放在解析之后就意味着发现冲突时已经写过路由，得先回滚；
//! 而这一步本身只读（`route -n get` 不需要 root），代价接近零。

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

pub use crate::managed::FakeIpRangeOwner;

/// 编排的六个阶段。**顺序即语义**，`Ord` 由声明序给出。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    /// 0) 先问 `198.18.0.0/15` 归谁管。只读、无需 root、不改动任何状态 ——
    ///    因此是唯一一个「发现问题可以干净地掉头」的位置
    PrecheckFakeIpRange,
    /// 1) 用 bootstrap 解析器（系统 DNS，绕过一切劫持）解析服务器真实 IP
    ResolveUpstream,
    /// 2) 把真实 IP 写进 bypass 路由 —— 环路防线必须先于任何流量存在
    WriteBypassRoutes,
    /// 3) 起本地多端口转发器
    StartForwarder,
    /// 4) 写 hosts，域名指向转发器
    WriteHosts,
    /// 5) 最后才拉起 TUN（含 DNS 劫持）
    BringUpTun,
}

/// 编排各步骤的真实副作用。测试注入假实现，生产注入真家伙。
pub trait TunStage {
    /// `198.18.0.0/15` 当前归谁管。**只读**，不需要 root。
    ///
    /// 生产实现即 `managed::fakeip_range_owner`；判据是 `route -n get` 的
    /// `destination` 一行而非接口名 —— 详见 [`FakeIpRangeOwner`] 的文档，
    /// 只看接口会把干净机器误判成冲突。
    fn fakeip_range_owner(&self) -> anyhow::Result<FakeIpRangeOwner>;
    /// 解析服务器域名的真实 IP。**必须走 bootstrap 解析器**（§7.2 纪律①）。
    fn resolve_upstream(&self, host: &str) -> anyhow::Result<Vec<IpAddr>>;
    fn write_bypass_routes(&self, ips: &[IpAddr]) -> anyhow::Result<()>;
    fn start_forwarder(&self, ips: &[IpAddr]) -> anyhow::Result<()>;
    fn write_hosts(&self, host: &str) -> anyhow::Result<()>;
    fn bring_up_tun(&self) -> anyhow::Result<()>;
}

/// 编排记录仪：既是执行器也是证据。
#[derive(Default, Clone)]
pub struct Timeline(Arc<Mutex<Vec<Step>>>);

impl Timeline {
    pub fn record(&self, s: Step) {
        self.0.lock().expect("时间线锁中毒").push(s);
    }
    pub fn steps(&self) -> Vec<Step> {
        self.0.lock().expect("时间线锁中毒").clone()
    }
}

/// 段被别人认领时的错误信息。
///
/// # 为什么是「拒绝启动」而不是「换个段」或「照常启动」
///
/// - **照常启动**是最坏的：两个 fake-ip 池共用一个段，各自的反查表都会
///   认领对方分配出去的地址。表现是随机的域名错连 —— 用户访问 A 却到了 B，
///   而两边的日志都显示一切正常。这类故障没有任何可查的痕迹
/// - **自动换段**看着聪明，实则不可行：能用的保留段就那么几个
///   （`100.64/10` 是 CGNAT、`192.0.2/24` 只有 254 个地址），逐个探测再
///   决定意味着 fake-ip 段变成运行时才确定的值，而它被 `device.rs` 的接口
///   地址、`routes.rs` 的路由、`fakedns` 的污染判据三处同时依赖。为一个
///   「用户装了两个代理」的场景把段变成动态的，代价远超收益
/// - **拒绝启动**因此是对的：这是一台机器上不该出现两个 TUN 代理同时抢
///   同一个段的局面，唯一正确的处置是让用户挑一个
///
/// 关键在于**拒绝得有用**：错误信息必须点名是谁占了段（接口名）与冲突的
/// 是哪个段，用户才知道去关哪个软件。一句「fake-ip 段冲突」等于没说。
/// 而且这一步在任何系统改动之前，拒绝时机器状态与启动前一模一样。
fn claimed_range_error(destination: &str, interface: Option<&str>) -> anyhow::Error {
    let who = match interface {
        Some(i) => format!("接口 {i}"),
        // 接口名取不到就照实说，不编一个 —— 用户至少还能拿 destination
        // 去 `netstat -rn` 里自己查。
        None => "一个未知接口".to_string(),
    };
    anyhow::anyhow!(
        "fake-ip 段 198.18.0.0/15 已被{who}接管（命中路由 {destination}），\
         多半是机器上另有一个 TUN 代理在跑。\
         两个 fake-ip 池共用同一个段会互相认领对方分配的假 IP，\
         表现为随机的域名错连且无从排查，因此拒绝启动 TUN。\
         请关闭另一个代理的 TUN 模式后重试；\
         想继续用本程序可在设置里关掉 TUN，改用混合端口入口"
    )
}

/// 按钉死顺序拉起 TUN。
///
/// 任一步失败即**中止**，绝不带着半套状态继续 —— 半套状态里最危险的是
/// 「hosts 写了但 bypass 没写」：那正是环路。
pub fn bring_up<S: TunStage>(
    stage: &S,
    host: &str,
    timeline: &Timeline,
) -> anyhow::Result<Vec<IpAddr>> {
    // 0) 段归属。放在最前是因为此刻还没动过系统任何状态，掉头是干净的。
    let owner = stage
        .fakeip_range_owner()
        .map_err(|e| anyhow::anyhow!("查询 fake-ip 段归属失败：{e:#}"))?;
    if let FakeIpRangeOwner::Claimed {
        destination,
        interface,
    } = &owner
    {
        // 注意：这里**不** record —— 时间线记的是「已完成的改动」，
        // 而这一步失败时什么都没改。断言空时间线才有意义。
        return Err(claimed_range_error(destination, interface.as_deref()));
    }
    timeline.record(Step::PrecheckFakeIpRange);

    // 1) 解析。此刻 TUN 尚未拉起、hosts 尚未写，系统解析器干净。
    let ips = stage.resolve_upstream(host).map_err(|e| {
        anyhow::anyhow!("解析服务器 {host} 失败（TUN 未拉起，此处不该受劫持影响）: {e}")
    })?;
    timeline.record(Step::ResolveUpstream);
    if ips.is_empty() {
        anyhow::bail!("服务器 {host} 解析不到地址：没有真实 IP 就没有 bypass，拉起 TUN 必成环路");
    }

    // 2) bypass 路由必须早于任何出网流量。
    stage.write_bypass_routes(&ips)?;
    timeline.record(Step::WriteBypassRoutes);

    // 3) 转发器。
    stage.start_forwarder(&ips)?;
    timeline.record(Step::StartForwarder);

    // 4) hosts。到这一步之后本机对该域名的解析就指向 127.0.0.1 了。
    stage.write_hosts(host)?;
    timeline.record(Step::WriteHosts);

    // 5) 最后才是 TUN。DNS 劫持从这一刻起生效。
    stage.bring_up_tun()?;
    timeline.record(Step::BringUpTun);

    Ok(ips)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// 假阶段：记录调用序，并模拟「TUN 一旦拉起，解析就返回 fake-ip」。
    struct Fake {
        tun_up: AtomicBool,
        hosts_written: AtomicBool,
        fail_at: Option<Step>,
        owner: FakeIpRangeOwner,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                tun_up: AtomicBool::new(false),
                hosts_written: AtomicBool::new(false),
                fail_at: None,
                owner: FakeIpRangeOwner::Unclaimed,
            }
        }
        fn failing_at(s: Step) -> Self {
            Self {
                fail_at: Some(s),
                ..Self::new()
            }
        }
        fn with_owner(owner: FakeIpRangeOwner) -> Self {
            Self {
                owner,
                ..Self::new()
            }
        }
    }

    impl TunStage for Fake {
        fn fakeip_range_owner(&self) -> anyhow::Result<FakeIpRangeOwner> {
            if self.fail_at == Some(Step::PrecheckFakeIpRange) {
                anyhow::bail!("查路由表失败");
            }
            Ok(self.owner.clone())
        }
        fn resolve_upstream(&self, _host: &str) -> anyhow::Result<Vec<IpAddr>> {
            if self.fail_at == Some(Step::ResolveUpstream) {
                anyhow::bail!("解析失败");
            }
            // 这就是陷阱 2 的实体：顺序错了，这里返回的是 fake-ip。
            if self.tun_up.load(Ordering::SeqCst) {
                return Ok(vec!["198.18.0.7".parse().unwrap()]);
            }
            if self.hosts_written.load(Ordering::SeqCst) {
                return Ok(vec!["127.0.0.1".parse().unwrap()]);
            }
            Ok(vec!["203.0.113.7".parse().unwrap()])
        }
        fn write_bypass_routes(&self, _ips: &[IpAddr]) -> anyhow::Result<()> {
            if self.fail_at == Some(Step::WriteBypassRoutes) {
                anyhow::bail!("写路由失败");
            }
            Ok(())
        }
        fn start_forwarder(&self, _ips: &[IpAddr]) -> anyhow::Result<()> {
            if self.fail_at == Some(Step::StartForwarder) {
                anyhow::bail!("转发器启动失败");
            }
            Ok(())
        }
        fn write_hosts(&self, _host: &str) -> anyhow::Result<()> {
            if self.fail_at == Some(Step::WriteHosts) {
                anyhow::bail!("写 hosts 失败");
            }
            self.hosts_written.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn bring_up_tun(&self) -> anyhow::Result<()> {
            if self.fail_at == Some(Step::BringUpTun) {
                anyhow::bail!("拉起 TUN 失败");
            }
            self.tun_up.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    /// **顺序纪律的主测试**（设计文档 §8.3.2）。
    #[test]
    fn startup_order_is_exactly_as_pinned() {
        let t = Timeline::default();
        let ips = bring_up(&Fake::new(), "srv.example.com", &t).unwrap();
        assert_eq!(
            t.steps(),
            vec![
                Step::PrecheckFakeIpRange,
                Step::ResolveUpstream,
                Step::WriteBypassRoutes,
                Step::StartForwarder,
                Step::WriteHosts,
                Step::BringUpTun,
            ]
        );
        // 顺序对 ⇒ 拿到的是真实 IP，不是 fake-ip、也不是 127.0.0.1。
        assert_eq!(ips, vec!["203.0.113.7".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn resolve_happens_before_tun_and_before_hosts() {
        // 冗余但值得：即使将来有人往 bring_up 里插新步骤，这条也会守住
        // 「解析必须最早」这个唯一真正致命的次序关系。
        let t = Timeline::default();
        bring_up(&Fake::new(), "srv.example.com", &t).unwrap();
        let s = t.steps();
        let pos = |x: Step| s.iter().position(|y| *y == x).unwrap();
        assert!(pos(Step::ResolveUpstream) < pos(Step::WriteHosts));
        assert!(pos(Step::ResolveUpstream) < pos(Step::BringUpTun));
        assert!(pos(Step::WriteBypassRoutes) < pos(Step::BringUpTun));
    }

    /// **反例：顺序颠倒会拿到 fake-ip。** 证明这条纪律不是形式主义。
    #[test]
    fn tun_first_would_poison_resolution() {
        let f = Fake::new();
        f.bring_up_tun().unwrap(); // 故意先拉 TUN
        let ips = f.resolve_upstream("srv.example.com").unwrap();
        assert_eq!(
            ips,
            vec!["198.18.0.7".parse::<IpAddr>().unwrap()],
            "TUN 先拉起时解析必然中毒 —— 这正是必须钉死顺序的原因"
        );
    }

    /// 反例二：hosts 先写会解析到环回，转发器转给自己。
    #[test]
    fn hosts_first_would_resolve_to_loopback() {
        let f = Fake::new();
        f.write_hosts("srv.example.com").unwrap();
        assert_eq!(
            f.resolve_upstream("srv.example.com").unwrap(),
            vec!["127.0.0.1".parse::<IpAddr>().unwrap()]
        );
    }

    /// 反例三：**中毒的解析结果会一路静默地穿过后面每一步。**
    ///
    /// 前两条只证明「解析会中毒」。这条把中毒结果接着往下走，证明**没有
    /// 任何一步会拦住它**：bypass 写的是 `198.18.0.7`（一条指向虚空的路由）、
    /// 转发器连向它、hosts 照写、TUN 照拉，全程零报错。
    /// 这就是「静默失败」四个字的实体 —— 也是为什么防线只能是顺序本身。
    #[test]
    fn a_poisoned_resolution_would_sail_through_every_later_step() {
        let f = Fake::new();
        f.bring_up_tun().unwrap(); // 模拟顺序被写反
        let ips = f.resolve_upstream("srv.example.com").unwrap();
        assert_eq!(ips, vec!["198.18.0.7".parse::<IpAddr>().unwrap()]);
        // 后面每一步都欣然接受这个假地址。
        f.write_bypass_routes(&ips).unwrap();
        f.start_forwarder(&ips).unwrap();
        f.write_hosts("srv.example.com").unwrap();
        assert!(
            crate::fakeip::FakeIpPool::in_segment(match ips[0] {
                IpAddr::V4(v) => v,
                _ => unreachable!(),
            }),
            "整条链路把一个 fake-ip 当成了服务器真实 IP，且无一处报错"
        );
    }

    #[test]
    fn failure_aborts_and_does_not_reach_later_steps() {
        for (fail, expect_len) in [
            (Step::PrecheckFakeIpRange, 0),
            (Step::ResolveUpstream, 1),
            (Step::WriteBypassRoutes, 2),
            (Step::StartForwarder, 3),
            (Step::WriteHosts, 4),
            (Step::BringUpTun, 5),
        ] {
            let t = Timeline::default();
            let r = bring_up(&Fake::failing_at(fail), "srv.example.com", &t);
            assert!(r.is_err(), "{fail:?} 失败必须报错");
            assert_eq!(t.steps().len(), expect_len, "{fail:?} 之后不应再执行任何步骤");
        }
    }

    #[test]
    fn empty_resolution_is_reported_not_silently_skipped() {
        // 沿用 shard.rs:214 的纪律：宁可拒绝启动，也不带着空 bypass 拉起 TUN。
        struct Empty;
        impl TunStage for Empty {
            fn fakeip_range_owner(&self) -> anyhow::Result<FakeIpRangeOwner> {
                Ok(FakeIpRangeOwner::Unclaimed)
            }
            fn resolve_upstream(&self, _: &str) -> anyhow::Result<Vec<IpAddr>> {
                Ok(vec![])
            }
            fn write_bypass_routes(&self, _: &[IpAddr]) -> anyhow::Result<()> {
                panic!("空解析结果绝不该走到写路由");
            }
            fn start_forwarder(&self, _: &[IpAddr]) -> anyhow::Result<()> {
                unreachable!()
            }
            fn write_hosts(&self, _: &str) -> anyhow::Result<()> {
                unreachable!()
            }
            fn bring_up_tun(&self) -> anyhow::Result<()> {
                unreachable!()
            }
        }
        let t = Timeline::default();
        let e = bring_up(&Empty, "srv.example.com", &t).unwrap_err();
        assert!(e.to_string().contains("环路"), "错误信息应点明后果: {e}");
    }

    /// **段被别的 TUN 认领 ⇒ 拒绝启动，且一步都不做。**
    ///
    /// 2026-08-25 实测场景：v2rayN 的 sing-box/xray 起了 `utun49`，
    /// 用 `0/1 + 128.0/1` 盖住默认路由，`198.18.0.0/15` 因此归它管。
    /// 此时启动我们自己的 fake-ip 池，两边分配的假 IP 会互相撞车。
    #[test]
    fn a_claimed_fake_ip_range_refuses_before_touching_anything() {
        let t = Timeline::default();
        let f = Fake::with_owner(FakeIpRangeOwner::Claimed {
            destination: "128.0.0.0".into(),
            interface: Some("utun49".into()),
        });
        let e = bring_up(&f, "srv.example.com", &t).unwrap_err();
        assert_eq!(t.steps(), vec![], "拒绝必须发生在任何系统改动之前");
        let m = e.to_string();
        assert!(m.contains("utun49"), "错误必须点名是谁占了段: {m}");
        assert!(m.contains("198.18.0.0/15"), "错误必须点名冲突的是哪个段: {m}");
    }

    /// 拒绝必须**可操作**：告诉用户怎么办，而不只是报告一个事实。
    ///
    /// 这是「拒绝启动」这个选择成立的前提。一句「fake-ip 段冲突」把用户
    /// 留在原地 —— 那样的话拒绝就只是把故障从「随机错连」换成「打不开」，
    /// 并没有更好。
    #[test]
    fn the_refusal_tells_the_user_what_to_do_about_it() {
        let e = claimed_range_error("128.0.0.0", Some("utun49"));
        let m = e.to_string();
        assert!(m.contains("关闭另一个代理"), "要给出第一条出路: {m}");
        assert!(m.contains("关掉 TUN"), "要给出第二条出路（降级用混合端口）: {m}");
    }

    /// 接口名取不到时照实说，不编一个也不静默。
    #[test]
    fn a_claim_without_an_interface_name_still_names_the_route() {
        let m = claimed_range_error("128.0.0.0", None).to_string();
        assert!(m.contains("未知接口"), "接口未知要照实说: {m}");
        assert!(
            m.contains("128.0.0.0"),
            "至少要给出命中的路由，用户才能自己 netstat 去查: {m}"
        );
    }

    /// 干净机器（命中 `default` 兜底）必须照常启动。
    ///
    /// 这条是对「拒绝启动」这个策略的护栏：判据一旦退化成看接口名，
    /// 干净机器上 `route -n get 198.18.0.4` 同样返回一个接口（`en0`，
    /// 因为命中 `default`），于是**每一台**干净机器都会被拒绝启动。
    /// 那种误报没有任何补救余地 —— 用户完全无从下手。
    #[test]
    fn an_unclaimed_range_starts_normally() {
        let t = Timeline::default();
        bring_up(
            &Fake::with_owner(FakeIpRangeOwner::Unclaimed),
            "srv.example.com",
            &t,
        )
        .unwrap();
        assert_eq!(t.steps().len(), 6);
    }

    /// 预检本身失败（`route` 跑不起来）⇒ 报错中止，**不当作「没冲突」**。
    ///
    /// 把「问不出来」当成「没人占」是最容易写出来的那一版，也是错的：
    /// 那等于在一台状况不明的机器上照常拉起 TUN。
    #[test]
    fn a_failed_precheck_aborts_rather_than_assuming_the_range_is_free() {
        let t = Timeline::default();
        let e = bring_up(
            &Fake::failing_at(Step::PrecheckFakeIpRange),
            "srv.example.com",
            &t,
        )
        .unwrap_err();
        assert_eq!(t.steps(), vec![]);
        assert!(
            e.to_string().contains("查询 fake-ip 段归属失败"),
            "错误要指明是哪一步出的问题: {e}"
        );
    }

    /// 时间线记的是**已完成的改动**，不是「试过的步骤」。
    ///
    /// 这条区分很实在：`revert` 要按时间线倒着回滚，把一个失败的步骤记进去
    /// 会让回滚去撤销一件根本没发生的事。
    #[test]
    fn the_timeline_records_completed_steps_only() {
        let t = Timeline::default();
        let _ = bring_up(&Fake::failing_at(Step::WriteHosts), "srv.example.com", &t);
        assert_eq!(
            t.steps(),
            vec![
                Step::PrecheckFakeIpRange,
                Step::ResolveUpstream,
                Step::WriteBypassRoutes,
                Step::StartForwarder,
            ],
            "失败的 WriteHosts 不该出现在时间线上"
        );
    }

    /// 声明序即优先序：`Step` 的 `Ord` 必须与钉死的顺序一致。
    ///
    /// 时间线断言靠的是相等比较，但「解析必须早于写 hosts」这类关系
    /// 用 `<` 表达更直接。声明序一旦被人调换，`Ord` 会跟着变而测试
    /// 却仍然绿 —— 除非这里钉住。
    #[test]
    fn step_ordering_matches_the_pinned_sequence() {
        let pinned = [
            Step::PrecheckFakeIpRange,
            Step::ResolveUpstream,
            Step::WriteBypassRoutes,
            Step::StartForwarder,
            Step::WriteHosts,
            Step::BringUpTun,
        ];
        for w in pinned.windows(2) {
            assert!(w[0] < w[1], "{:?} 必须排在 {:?} 之前", w[0], w[1]);
        }
    }
}
