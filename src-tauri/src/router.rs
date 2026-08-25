//! 入口判决与分派：把阶段 1 的规则判决接进入口层与出站层之间。
//!
//! 这是设计文档 §6.4「出站不可用时拒绝而非静默回退」的落地点。整条路径上
//! **只有这里**同时看得见「规则要求走哪个出站」与「那个出站现在通不通」，
//! 也就只有这里能把两者悄悄错开。因此本模块刻意不提供任何「换一个出站试试」
//! 的分支：
//!
//! - 出站不存在 → `NotFound`
//! - 出站存在但不可用 → `NotConnected`，错误里点名是哪个出站
//! - 正在启动中 → **短暂排队等待**，超时仍不通就转为拒绝
//!
//! 等待不是回退：它等的是**同一个**出站，超时后给出的仍是拒绝。而回退是
//! 换一条路把数据发出去 —— 用户以为在走日本节点、实际裸奔或走了别的落地，
//! 那不是可用性折衷，是隐私事故。
//!
//! **两阶段判决**（§4.2 纪律① / §6.2）：`RuleSet::evaluate` 是同步的，
//! 需要 DNS 时返回 `Verdict::NeedResolve` 把需求抛回给调用方 —— 也就是这里。
//! 本模块把两阶段协议整个委托给 `RoutingResolver`，见 `decide()` 的注释。

//! **本模块尚未接线**：唯一的调用方是 Task 13 的出站管理器（它把 `dispatch`
//! 包成 `wsieve_inbound::Dispatch` 交给混合端口入口），在那之前编译器看不到
//! 任何使用点。因此这里整模块 `allow(dead_code)` —— **接线完成后必须删掉
//! 这一行**，否则它会长期掩盖真正的死代码。模块内的实现与测试都是真的。
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io;
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::DuplexStream;
use tokio::net::TcpStream;
use wsieve_geo::GeoDb;
use wsieve_proto::addr::{AddrPort, TargetAddr};
use wsieve_route::{Decision, RuleHit, RuleSet, Verdict};

use crate::outbound::instance::{OutboundInstance, Status};

/// 「正在启动中」时允许排队等待的默认上限。
///
/// 取值权衡：太短则应用刚启动的那几秒里所有请求都被拒（用户看到的是「刚打开
/// 就上不了网」）；太长则一个真的连不上的出站会把每条连接都吊住那么久，
/// 浏览器侧表现为整页卡死而非快速失败。
///
/// `ponytail:` 3s 是拍脑袋的初值，未实测。升级路径：按出站近期握手耗时的
/// 分位数自适应，或做成配置项。
pub const DEFAULT_START_WAIT: Duration = Duration::from_secs(3);

/// 直连的连接超时。
///
/// 没有它，一个被丢包黑洞吃掉的目标会让这条连接挂到系统 TCP 超时（Linux 上
/// 默认 130s 左右），期间入口侧的客户端只能干等。
///
/// `ponytail:` 10s 拍脑袋，未实测。
pub const DIRECT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 判决路径需要的全部解析能力。
///
/// 形状与阶段 3 的 `wsieve_dns::RoutingResolver` 逐字对齐（含 `?Sized` 友好的
/// boxed future 写法），阶段 3 接入时把本 trait 换成那个即可，`decide()` 的
/// 调用点一个字都不用改。
///
/// **返回值里没有 `Result`**：调用方因此无法表达「DNS 失败就阻断连接」。
/// 纪律被编码进类型，而不是写在注释里等人遵守 —— 解析失败应当让 IP 类规则
/// 视为不匹配、流程继续（设计文档 §6.2），绝不是把连接掐掉。
pub trait RoutingResolver: Send + Sync {
    /// 解析域名。**永不失败**：超时、NXDOMAIN、上游不可达一律返回空 Vec。
    fn resolve<'a>(
        &'a self,
        domain: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<IpAddr>> + Send + 'a>>;
}

/// 阶段 2 的解析器占位实现：不做任何 DNS 查询，恒返回空。
///
/// **这不是 mock，也不是临时凑合**。按设计文档 §6.2，解析失败本就该传空切片
/// 让流程继续，因此它的行为语义完全正确：「所有 IP 类规则对域名目标都不匹配」。
/// 阶段 2 尚未接入 DNS 解析器（那是阶段 3 的内容），此时选择「不解析」而不是
/// 「猜一个结果」，是唯一诚实的做法 —— 它既不会泄漏 DNS 查询，也不会让判决
/// 建立在编造的 IP 上。
///
/// 覆盖面的缺失是真实的，且**必须让用户看得见**：`Router::new` 在规则表含
/// IP 类规则时会告警一次，说明这些规则对域名目标暂不生效。
#[derive(Debug, Default)]
pub struct NoResolver;

impl RoutingResolver for NoResolver {
    fn resolve<'a>(
        &'a self,
        _domain: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<IpAddr>> + Send + 'a>> {
        Box::pin(async { Vec::new() })
    }
}

/// 一次判决的完整记录。
///
/// 字段构成「交给阶段 4/5 的数据契约」的前三样（第四样 `bytes` 只有连接
/// 结束时才知道，由 `dispatch` 的调用方在收尾时补）。这些在判决路径上本就
/// 全部已知，顺手带出去接近零成本；事后补采集则要把判决再跑一遍。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routed {
    pub decision: Decision,
    /// 做出判决的那条规则。`None` = 没有规则参与（mode 短路或扫穿兜底）。
    pub rule: Option<RuleHit>,
    /// 是否真的发生了 DNS 查询。多数流量在域名类规则处就命中，此值为 false
    /// —— 这正是两阶段协议「不需要时零 DNS 泄漏」的可观测证据。
    pub resolved: bool,
    /// 解析得到的 IP。
    ///
    /// **只用于判决与展示，绝不改写传给出站的地址**（设计文档 §6.3）：
    /// 判决走代理时仍把**域名**递给出站，由服务端做远程解析 —— 服务端离
    /// 目标更近，且不受本地污染影响。
    pub ips: Vec<IpAddr>,
}

/// 分派器：持规则、GEO、出站表与解析器。
pub struct Router {
    rules: Arc<RuleSet>,
    geo: Arc<GeoDb>,
    /// 出站名 → 实例。BTreeMap 让诊断输出的顺序稳定。
    outbounds: BTreeMap<String, Arc<OutboundInstance>>,
    resolver: Arc<dyn RoutingResolver>,
    start_wait: Duration,
    /// 解析次数计数，用来在测试里锁住「最多解析一次」。
    resolver_calls: AtomicUsize,
}

impl Router {
    pub fn new(
        rules: Arc<RuleSet>,
        geo: Arc<GeoDb>,
        outbounds: BTreeMap<String, Arc<OutboundInstance>>,
        resolver: Arc<dyn RoutingResolver>,
    ) -> Self {
        Self {
            rules,
            geo,
            outbounds,
            resolver,
            start_wait: DEFAULT_START_WAIT,
            resolver_calls: AtomicUsize::new(0),
        }
    }

    /// 用阶段 2 的占位解析器（`NoResolver`）构造，并如实告警覆盖面。
    ///
    /// 「有 IP 类规则却没有解析器」不是错误 —— 判决照常给出，语义也正确
    /// （解析失败视为不匹配，§6.2）。但它**必须被说出来**：不告警的话，
    /// 用户会以为自己写的 `GEOIP,CN,DIRECT` 正在生效，而对域名目标它一条
    /// 都不会命中。静默的覆盖面缺失比报错更危险。
    pub fn without_resolver(
        rules: Arc<RuleSet>,
        geo: Arc<GeoDb>,
        outbounds: BTreeMap<String, Arc<OutboundInstance>>,
    ) -> Self {
        let n = rules.resolving_rule_count();
        if n > 0 {
            tracing::warn!(
                "尚未接入 DNS 解析器：{n} 条 IP 类规则（GEOIP / IP-CIDR）对**域名**目标不会命中，\
                 这类流量会落到后续规则或 MATCH 兜底。对 IP 目标的规则不受影响。"
            );
        }
        Self::new(rules, geo, outbounds, Arc::new(NoResolver))
    }

    /// 改「正在启动中」的排队上限（测试与将来的配置项用）。
    pub fn with_start_wait(mut self, d: Duration) -> Self {
        self.start_wait = d;
        self
    }

    /// 已解析次数（诊断/测试）。
    pub fn resolver_calls(&self) -> usize {
        self.resolver_calls.load(Ordering::Relaxed)
    }

    /// 出站表（管理器要用同一份实例去跑会话循环）。
    pub fn outbounds(&self) -> &BTreeMap<String, Arc<OutboundInstance>> {
        &self.outbounds
    }

    /// 走完两阶段协议，返回判决与它的出处。
    ///
    /// - 第一轮 `evaluate(target, None, ..)`；命中即返回，**不发任何 DNS**
    /// - 抛出 `NeedResolve` 才解析，然后 `evaluate(target, Some(&ips), ..)`
    /// - 解析失败/超时得到空切片，该 IP 规则视为不匹配，流程继续
    ///
    /// 第二轮**永不**再抛 `NeedResolve` —— 那是 `wsieve-route` 的协议承诺。
    /// 走到那里说明引擎违约，静默兜底只会让 bug 藏进生产环境。
    pub async fn decide(&self, target: &AddrPort) -> Routed {
        let first = self.rules.evaluate_explained(target, None, &self.geo);
        match first.verdict {
            Verdict::Decided(decision) => Routed {
                decision,
                rule: first.hit,
                resolved: false,
                ips: Vec::new(),
            },
            Verdict::NeedResolve { domain } => {
                self.resolver_calls.fetch_add(1, Ordering::Relaxed);
                let ips = self.resolver.resolve(&domain).await;
                let second = self.rules.evaluate_explained(target, Some(&ips), &self.geo);
                match second.verdict {
                    Verdict::Decided(decision) => Routed {
                        decision,
                        rule: second.hit,
                        resolved: true,
                        ips,
                    },
                    Verdict::NeedResolve { domain } => unreachable!(
                        "第二轮不该再请求解析（domain={domain}）——\
                         这是 wsieve-route::evaluate 的协议违约，见设计文档 §4.2 纪律①"
                    ),
                }
            }
        }
    }

    /// 判决并按判决建立连接。返回的是「已经连上目标」的双向管道。
    pub async fn dispatch(&self, target: AddrPort) -> io::Result<DuplexStream> {
        self.dispatch_reported(target).await.map(|(s, _)| s)
    }

    /// 同 `dispatch`，额外带回判决记录。
    ///
    /// 入口层拿它去喂事件聚合器（阶段 4/5 的流量视图）。分成两个方法而不是
    /// 让 `dispatch` 直接返回元组：`wsieve_inbound::Dispatch` 的签名只要流，
    /// 多出来的那一半会逼每个调用点写 `.0`。
    pub async fn dispatch_reported(
        &self,
        target: AddrPort,
    ) -> io::Result<(DuplexStream, Routed)> {
        let routed = self.decide(&target).await;
        let stream = match &routed.decision {
            Decision::Reject => Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("{} 被规则拒绝", target.display()),
            )),
            Decision::Direct => direct_connect(&target).await,
            Decision::Outbound(name) => self.via_outbound(name, &target).await,
        }?;
        Ok((stream, routed))
    }

    /// §6.4 的落地点。这个函数里**没有**、也绝不能有「换一个出站」的分支。
    async fn via_outbound(&self, name: &str, target: &AddrPort) -> io::Result<DuplexStream> {
        let Some(inst) = self.outbounds.get(name) else {
            // 加载期已校验过规则里引用的出站都存在（`RuleSet::build` 的
            // UnknownOutbound），走到这里说明出站在运行时被删了。
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("出站「{name}」不存在"),
            ));
        };

        // 正在启动中 → 短暂排队等待。等的是**同一个**出站，超时后仍是拒绝
        // —— 这与「换条路发出去」有本质区别。
        if matches!(inst.status(), Status::Connecting) {
            let _ = tokio::time::timeout(self.start_wait, inst.wait_connected()).await;
        }

        let Some(dialer) = inst.dialer() else {
            // 绝不回退直连 —— 用户以为在走代理、实际裸奔，
            // 那不是可用性折衷，是隐私事故（设计文档 §6.4）。
            //
            // 错误必须点名是哪个出站：用户看到的是「上不了网」，
            // 不点名的话他无从知道该去重连哪一个。
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                format!("出站「{name}」当前不可用（{}）", describe(&inst.status())),
            ));
        };

        // 目标**原样**递给出站：域名就是域名，绝不用本地解析到的 IP 替换
        // （设计文档 §6.3）。服务端离目标更近，且不受本地 DNS 污染影响。
        let stream = dialer
            .connect(target)
            .await
            .map_err(|e| io::Error::other(format!("出站「{name}」开流失败: {e}")))?;
        Ok(splice(stream))
    }
}

/// 把状态说成人话，进错误信息。
fn describe(s: &Status) -> String {
    match s {
        Status::Stopped => "已停止".into(),
        Status::Connecting => "正在连接".into(),
        Status::Connected { sessions } => format!("已连接 {sessions} 条会话"),
        Status::Retrying { after } => format!("{after:?} 后重试"),
        Status::Failed { reason } => format!("失败：{reason}"),
    }
}

/// 直连：本地自己发起 TCP。
///
/// 域名在这里**必须**本地解析 —— 直连没有第二个解析点。这与「走代理时把
/// 域名递给服务端」不矛盾：那条路上有服务端替我们解析，这条路上没有。
async fn direct_connect(target: &AddrPort) -> io::Result<DuplexStream> {
    let display = target.display();
    let connect = async {
        match &target.addr {
            TargetAddr::Domain(d) => TcpStream::connect((d.as_str(), target.port)).await,
            TargetAddr::V4(o) => {
                TcpStream::connect((std::net::Ipv4Addr::from(*o), target.port)).await
            }
            TargetAddr::V6(a) => {
                TcpStream::connect((std::net::Ipv6Addr::from(*a), target.port)).await
            }
        }
    };
    let tcp = match tokio::time::timeout(DIRECT_CONNECT_TIMEOUT, connect).await {
        Ok(r) => r.map_err(|e| io::Error::new(e.kind(), format!("直连 {display} 失败: {e}")))?,
        Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("直连 {display} 超时（{DIRECT_CONNECT_TIMEOUT:?}）"),
            ))
        }
    };
    let _ = tcp.set_nodelay(true);
    Ok(splice(tcp))
}

/// 把任意双向流包成入口层要的 `DuplexStream`：开一条本地 duplex，
/// 后台任务做双向 copy。
fn splice<S>(mut upstream: S) -> DuplexStream
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (local, mut remote_end) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        // 搬运结束（正常收尾或出错）都只是这一条连接的事，不影响别人。
        // 但不静默丢弃：出错要留下痕迹，否则「偶尔断一下」永远查不出原因。
        if let Err(e) = tokio::io::copy_bidirectional(&mut remote_end, &mut upstream).await {
            tracing::debug!("连接搬运结束: {e}");
        }
    });
    local
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use wsieve_route::Mode;

    use crate::outbound::instance::OutboundCfg;

    fn geo_stub() -> Arc<GeoDb> {
        // 指向不存在的文件：GEO 查询一律失败，验证「GEO 不可用时规则跳过
        // 而非阻断」（设计文档 §12）。
        Arc::new(GeoDb::new(
            PathBuf::from("/nonexistent/geoip.dat"),
            PathBuf::from("/nonexistent/geosite.dat"),
        ))
    }

    fn domain(d: &str, port: u16) -> AddrPort {
        AddrPort {
            addr: TargetAddr::Domain(d.into()),
            port,
        }
    }

    fn ruleset(lines: &[&str], known: &[&str]) -> Arc<RuleSet> {
        let known: HashSet<String> = known.iter().map(|s| s.to_string()).collect();
        Arc::new(
            RuleSet::build(
                &lines.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                Mode::Rule,
                "",
                &known,
            )
            .unwrap(),
        )
    }

    fn inst(name: &str) -> Arc<OutboundInstance> {
        OutboundInstance::new(OutboundCfg {
            name: name.into(),
            server_pub: [1u8; 32],
            client_priv: [2u8; 32],
            mux_prefs: vec![wsieve_proto::hello::MuxId::Yamux],
            session_bases: vec![None],
        })
    }

    /// 只有规则、没有出站的 router（规则里也不能引用出站）。
    fn test_router(lines: &[&str]) -> Router {
        Router::new(
            ruleset(lines, &[]),
            geo_stub(),
            BTreeMap::new(),
            Arc::new(NoResolver),
        )
    }

    /// 规则指向一个**存在但停着**的出站。
    fn test_router_with_stopped_outbound(lines: &[&str]) -> Router {
        let o = inst("日本节点");
        assert_eq!(o.status(), Status::Stopped);
        assert!(o.dialer().is_none());
        let mut m = BTreeMap::new();
        m.insert("日本节点".to_string(), o);
        Router::new(
            ruleset(lines, &["日本节点"]),
            geo_stub(),
            m,
            Arc::new(NoResolver),
        )
    }

    /// 规则指向一个**卡在 Connecting** 的出站（永远连不上）。
    fn test_router_with_starting_outbound(lines: &[&str], wait: Duration) -> Router {
        let o = inst("日本节点");
        o.force_status_for_test(Status::Connecting);
        let mut m = BTreeMap::new();
        m.insert("日本节点".to_string(), o);
        Router::new(
            ruleset(lines, &["日本节点"]),
            geo_stub(),
            m,
            Arc::new(NoResolver),
        )
        .with_start_wait(wait)
    }

    /// 注入式解析器，用来验证两阶段协议闭环。
    struct FixedResolver(Vec<IpAddr>);

    impl RoutingResolver for FixedResolver {
        fn resolve<'a>(
            &'a self,
            _domain: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<IpAddr>> + Send + 'a>> {
            let ips = self.0.clone();
            Box::pin(async move { ips })
        }
    }

    fn test_router_with_resolver(lines: &[&str], ips: &[&str]) -> Router {
        let o = inst("日本节点");
        let mut m = BTreeMap::new();
        m.insert("日本节点".to_string(), o);
        Router::new(
            ruleset(lines, &["日本节点"]),
            geo_stub(),
            m,
            Arc::new(FixedResolver(
                ips.iter().map(|s| s.parse().unwrap()).collect(),
            )),
        )
    }

    #[tokio::test]
    async fn decision_reject_fails_immediately() {
        let r = test_router(&["DOMAIN,blocked.com,REJECT", "MATCH,DIRECT"]);
        let e = r.dispatch(domain("blocked.com", 443)).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused);
        assert!(e.to_string().contains("blocked.com"), "{e}");
    }

    #[tokio::test]
    async fn unavailable_outbound_refuses_never_falls_back() {
        // §6.4 的隐私防线：出站不可用时拒绝，绝不静默走直连。
        let r = test_router_with_stopped_outbound(&["MATCH,日本节点"]);
        let e = r.dispatch(domain("a.com", 443)).await.unwrap_err();
        assert_eq!(
            e.kind(),
            io::ErrorKind::NotConnected,
            "必须是「拒绝」，而不是任何形式的成功：{e}"
        );
        assert!(
            e.to_string().contains("日本节点"),
            "错误要点名是哪个出站出了问题：{e}"
        );
    }

    #[tokio::test]
    async fn an_unavailable_outbound_never_becomes_a_direct_connection() {
        // 上一条锁的是错误类型，这条锁的是**没有连接被建立**。
        // 若哪天有人在 via_outbound 里补一个「不通就直连」的好心分支，
        // 上一条测试可能还过（错误类型对不上会挂），但这条一定挂 ——
        // 它断言的是「目标服务器一个字节都没收到」。
        use tokio::io::AsyncReadExt as _;
        use tokio::net::TcpListener;

        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let hit = Arc::new(AtomicUsize::new(0));
        let h2 = hit.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                h2.fetch_add(1, Ordering::SeqCst);
                let mut b = [0u8; 1];
                let _ = s.read(&mut b).await;
            }
        });

        let r = test_router_with_stopped_outbound(&["MATCH,日本节点"]);
        let target = AddrPort {
            addr: TargetAddr::V4([127, 0, 0, 1]),
            port,
        };
        assert!(r.dispatch(target).await.is_err());
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            hit.load(Ordering::SeqCst),
            0,
            "出站不可用时绝不能有任何字节到达目标 —— 那就是静默裸奔"
        );
    }

    #[tokio::test]
    async fn starting_outbound_waits_then_times_out() {
        // 「正在启动中」短暂排队等待，超时转为拒绝 —— 等待不是回退。
        let r = test_router_with_starting_outbound(&["MATCH,日本节点"], Duration::from_millis(50));
        let t0 = std::time::Instant::now();
        let e = r.dispatch(domain("a.com", 443)).await.unwrap_err();
        assert!(t0.elapsed() >= Duration::from_millis(50), "要真的等过");
        assert_eq!(e.kind(), io::ErrorKind::NotConnected);
        assert!(e.to_string().contains("日本节点"), "{e}");
    }

    #[tokio::test]
    async fn a_connecting_outbound_that_comes_up_is_served_not_refused() {
        // 等待要真的有用：出站在等待窗口内连上，这条连接就该放行。
        // 否则「等待」只是拖时间，不如直接拒。
        let o = inst("日本节点");
        o.force_status_for_test(Status::Connecting);
        let mut m = BTreeMap::new();
        m.insert("日本节点".to_string(), o.clone());
        let r = Router::new(
            ruleset(&["MATCH,日本节点"], &["日本节点"]),
            geo_stub(),
            m,
            Arc::new(NoResolver),
        )
        .with_start_wait(Duration::from_secs(2));

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            // 只推状态、不给 dialer：等待被唤醒后仍拿不到拨号器。
            o.force_status_for_test(Status::Connected { sessions: 1 });
            o.notify_connected_for_test();
        });

        let t0 = std::time::Instant::now();
        let e = r.dispatch(domain("a.com", 443)).await.unwrap_err();
        // 等待确实被提前唤醒了（远早于 2s 上限），但因为没有真的拨号器，
        // 结果仍是拒绝 —— 状态好看不等于能用，`dialer()` 才是判据。
        assert!(t0.elapsed() < Duration::from_millis(1500), "应被提前唤醒");
        assert_eq!(e.kind(), io::ErrorKind::NotConnected);
    }

    #[tokio::test]
    async fn a_rule_naming_a_deleted_outbound_is_not_found_not_rerouted() {
        // 出站被运行时删掉时，同样是拒绝。「找不到就随便挑一个」是最容易
        // 被好心写出来的回退。
        let r = Router::new(
            ruleset(&["MATCH,日本节点"], &["日本节点"]),
            geo_stub(),
            BTreeMap::new(), // 出站表空 —— 模拟运行时被删
            Arc::new(NoResolver),
        );
        let e = r.dispatch(domain("a.com", 443)).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(e.to_string().contains("日本节点"), "{e}");
    }

    #[tokio::test]
    async fn need_resolve_triggers_second_pass() {
        // 两阶段协议在这里闭环。
        let r = test_router_with_resolver(&["GEOIP,CN,DIRECT", "MATCH,日本节点"], &["1.2.3.4"]);
        let d = r.decide(&domain("a.com", 443)).await;
        assert!(
            matches!(&d.decision, Decision::Outbound(n) if n == "日本节点"),
            "{:?}",
            d.decision
        );
        assert_eq!(r.resolver_calls(), 1, "只该解析一次");
        assert!(d.resolved, "确实解析过");
        assert_eq!(d.ips, vec!["1.2.3.4".parse::<IpAddr>().unwrap()]);
    }

    #[tokio::test]
    async fn a_domain_rule_hit_never_triggers_dns() {
        // 两阶段协议的收益就在这里：多数流量在域名类规则处命中，
        // 一个 DNS 包都不发。这是「不需要时零 DNS 泄漏」的可观测证据。
        let r = test_router_with_resolver(
            &["DOMAIN-SUFFIX,a.com,日本节点", "GEOIP,CN,DIRECT", "MATCH,DIRECT"],
            &["1.2.3.4"],
        );
        let d = r.decide(&domain("www.a.com", 443)).await;
        assert!(matches!(&d.decision, Decision::Outbound(n) if n == "日本节点"));
        assert_eq!(r.resolver_calls(), 0, "域名类规则命中时不该有任何解析");
        assert!(!d.resolved);
    }

    #[tokio::test]
    async fn a_failing_resolver_does_not_block_the_connection() {
        // 解析失败传空切片让流程继续（§6.2）。若改成阻断，一次 DNS 抖动
        // 就会让本可直连的流量整片失败。
        let r = test_router_with_resolver(&["GEOIP,CN,DIRECT", "MATCH,日本节点"], &[]);
        let d = r.decide(&domain("a.com", 443)).await;
        assert!(matches!(&d.decision, Decision::Outbound(n) if n == "日本节点"));
        assert!(d.resolved, "问过了");
        assert!(d.ips.is_empty(), "但什么都没问到");
    }

    #[tokio::test]
    async fn a_decision_carries_the_rule_and_target_for_the_ui() {
        // 交给阶段 4/5 的数据契约：target / rule / outbound 三样必须在
        // 判决时一并采到。事后补采集要么做不到，要么要把判决重跑一遍。
        let r = test_router_with_stopped_outbound(&[
            "DOMAIN-SUFFIX,google.com,日本节点",
            "MATCH,DIRECT",
        ]);
        let d = r.decide(&domain("www.google.com", 443)).await;
        assert!(matches!(&d.decision, Decision::Outbound(n) if n == "日本节点"));
        let hit = d.rule.expect("规则出处必须带出来");
        assert_eq!(hit.text, "DOMAIN-SUFFIX,google.com,日本节点");
        assert_eq!(hit.line, 1);
    }

    #[tokio::test]
    async fn direct_target_actually_reaches_the_peer() {
        // 直连分支不是摆设：字节要真的到对端并原样回来。
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::TcpListener;

        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = [0u8; 5];
            s.read_exact(&mut buf).await.unwrap();
            s.write_all(&buf).await.unwrap();
        });

        let r = test_router(&["MATCH,DIRECT"]);
        let mut stream = r
            .dispatch(AddrPort {
                addr: TargetAddr::V4([127, 0, 0, 1]),
                port,
            })
            .await
            .expect("直连应当成功");
        stream.write_all(b"hello").await.unwrap();
        let mut back = [0u8; 5];
        stream.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"hello");
    }

    #[tokio::test]
    async fn a_direct_connect_failure_is_reported_with_the_target() {
        // 连不上要说清是连谁没连上。只回一句「失败」的话，用户面对
        // 一堆并发连接根本无从定位。
        let r = test_router(&["MATCH,DIRECT"]);
        // 端口 1 上不会有服务在跑（且它 <1024，本进程也无从绑上去）。
        let e = r
            .dispatch(AddrPort {
                addr: TargetAddr::V4([127, 0, 0, 1]),
                port: 1,
            })
            .await
            .unwrap_err();
        assert!(e.to_string().contains("127.0.0.1:1"), "{e}");
    }
}
