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
//! 需要 DNS 时返回 `Verdict::NeedResolve` 把需求抛回给调用方。那个「接住
//! NeedResolve、解析、再来一轮」的循环**不在本模块**，而是整个委托给
//! `wsieve_dns::decide()` —— 见 `Router::decide` 的注释。

use std::collections::BTreeMap;
use std::io;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::DuplexStream;
use tokio::net::TcpStream;
use wsieve_dns::RoutingResolver;
use wsieve_geo::GeoDb;
use wsieve_proto::addr::{AddrPort, TargetAddr};
use wsieve_route::{Decision, RuleSet};

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

/// 一次判决的完整记录，直接复用 `wsieve_dns` 的那一个。
///
/// 阶段 2 曾在本模块自带一份同形状的 `Routed`，因为当时 `wsieve_dns::decide()`
/// 还不存在（两者并行开发）。阶段 3 接入后不再保留第二份：同一个数据契约
/// 存两处，迟早有一处先长出字段而另一处不知道。
pub use wsieve_dns::Outcome;

/// 不做任何 DNS 查询、恒返回空的解析器。
///
/// **这不是 mock，也不是临时凑合**。按设计文档 §6.2，解析失败本就该传空切片
/// 让流程继续，因此它的行为语义完全正确：「所有 IP 类规则对域名目标都不匹配」。
///
/// 阶段 3 之后它仍有真实职责：`main.rs` 的启动配置是**环境变量**式的
/// （见 `bootstrap::AppConfig`），里面没有 `dns.nameserver` 字段可读。
/// 在配置文件接入之前（阶段 4），给 `DnsResolver::new` 硬编一个上游等于
/// 替用户决定他的 DNS 走谁 —— 那比不解析更糟。此时选择「不解析」而不是
/// 「猜一个结果」，是唯一诚实的做法：既不泄漏 DNS 查询，也不让判决建立在
/// 编造的 IP 上。
///
/// 覆盖面的缺失是真实的，且**必须让用户看得见**：`Router::without_resolver`
/// 在规则表含 IP 类规则时会告警一次，说明这些规则对域名目标暂不生效。
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

/// 分派器：持规则、GEO、出站表与解析器。
pub struct Router {
    rules: Arc<RuleSet>,
    geo: Arc<GeoDb>,
    /// 出站名 → 实例。BTreeMap 让诊断输出的顺序稳定。
    outbounds: BTreeMap<String, Arc<OutboundInstance>>,
    resolver: Arc<dyn RoutingResolver>,
    /// 静态解析表。判决那一半已经叠在 `resolver` 里（`HostsResolver`），
    /// 这份是给**转发**用的：判决完成后把域名目标换成 IP 字面量。
    ///
    /// 两处必须是同一张表。各持一份的话，用户改了 hosts 后可能出现
    /// 「规则按新 IP 判、连接却发去旧 IP」这种只在改配置那一瞬间存在的错位。
    hosts: Arc<wsieve_dns::Hosts>,
    start_wait: Duration,
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
            hosts: Arc::new(wsieve_dns::Hosts::default()),
            start_wait: DEFAULT_START_WAIT,
        }
    }

    /// 装上静态解析表，供**转发**改写用。
    ///
    /// 判决那一半不在这里——它靠把 `HostsResolver` 传给 `new` 的 `resolver`
    /// 参数完成。做成两步而不是一个参数，是因为这两半的接线点本就不同：
    /// 判决那半要包在解析器链里，转发这半只是查表。硬塞进一个构造参数会让
    /// 调用方以为「传了就两边都生效」，而实际上漏掉哪一半都不会报错。
    /// 唯一的生产接线点是 `runtime_state::build_router`，它两半一起装。
    pub fn with_hosts(mut self, hosts: Arc<wsieve_dns::Hosts>) -> Self {
        self.hosts = hosts;
        self
    }

    /// 用 `NoResolver` 构造，并如实告警覆盖面。
    ///
    /// 「有 IP 类规则却没有解析器」不是错误 —— 判决照常给出，语义也正确
    /// （解析失败视为不匹配，§6.2）。但它**必须被说出来**：不告警的话，
    /// 用户会以为自己写的 `GEOIP,CN,DIRECT` 正在生效，而对域名目标它一条
    /// 都不会命中。静默的覆盖面缺失比报错更危险。
    ///
    /// 这条路径在阶段 3 之后仍然存在，理由见 `NoResolver` 的注释：
    /// 启动配置目前是环境变量式的，没有 `dns.nameserver` 可读，硬编一个
    /// 上游等于替用户决定他的 DNS 走谁。等阶段 4 接入配置文件后，这里改成
    /// `Self::new(.., Arc::new(DnsResolver::new(&cfg.dns.nameserver, ..)?))`。
    /// 逐项 `allow(dead_code)`：上面那段「等阶段 4 接入配置文件」已经发生了
    /// ——生产路径现在走的是真实 `DnsResolver`，只剩 `runtime_state` 的测试
    /// 辅助 `snapshot` 还在用它。保留而不删：它承载的「没有解析器时必须把
    /// 覆盖面缺失说出来」那条纪律仍然有效，将来任何一条重新走无解析器的
    /// 路径都该从这里进。整模块 allow 会连真正的死代码一起盖住，所以逐项标。
    #[allow(dead_code)]
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

    /// 换一份规则表，其余（GeoDb / 出站表 / 解析器 / 启动等待）原样沿用。
    ///
    /// 配置保存后重建运行时快照用（「运行时接入 config.yaml」§3）。出站表
    /// 照抄的是**同一批 `Arc<OutboundInstance>`**，不是重新构造——重造实例
    /// 会把所有已经连上的出站一起断掉，而用户只是改了一条规则。这正是 §3
    /// 「没变的出站不重启、不断连接」那条产品决策落到代码上的样子。
    ///
    /// 不打 `without_resolver` 那条覆盖面告警：解析器没换，重复告警只是
    /// 每次保存都往日志里刷一遍同一句话。
    pub fn with_rules(&self, rules: Arc<RuleSet>) -> Self {
        Self {
            rules,
            geo: self.geo.clone(),
            outbounds: self.outbounds.clone(),
            resolver: self.resolver.clone(),
            hosts: self.hosts.clone(),
            start_wait: self.start_wait,
        }
    }

    /// 改「正在启动中」的排队上限。
    ///
    /// `#[cfg(test)]`：当前生产路径一律用 `DEFAULT_START_WAIT`。等它变成
    /// 配置项时（阶段 4）再放开 —— 在那之前留一个公开的可变旋钮，只会让
    /// 「这个值到底从哪来的」多一个要排查的地方。
    #[cfg(test)]
    fn with_start_wait(mut self, d: Duration) -> Self {
        self.start_wait = d;
        self
    }

    /// 走完两阶段协议，返回判决与它的出处。
    ///
    /// **循环本身不在这里，整个委托给 `wsieve_dns::decide()`。**
    ///
    /// 阶段 2 曾在本模块自带一份同样的循环 —— 当时 `wsieve_dns` 还不存在，
    /// 两者并行开发。阶段 3 接入后立刻删掉那一份：两阶段协议要证明的三件事
    /// （最多解析一次、第二轮永不再抛 `NeedResolve`、解析失败不阻断连接）
    /// 若各证一遍，迟早有一份先被改动而另一份不知道，届时「判决为什么不一样」
    /// 会成为一个没人查得动的问题。UI 的规则试算（§11.2）走的也是同一个
    /// `decide()`，试算与真实判决因此不可能各说各话。
    pub async fn decide(&self, target: &AddrPort) -> Outcome {
        wsieve_dns::decide(&self.rules, target, &self.geo, self.resolver.as_ref()).await
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
    ) -> io::Result<(DuplexStream, Outcome)> {
        let routed = self.decide(&target).await;
        // **判决之后**才改写。反过来的话目标已是 IP，DOMAIN-SUFFIX /
        // DOMAIN-KEYWORD 这类规则会集体不命中，而配置两边看着都对。
        //
        // 拒绝分支的错误信息仍用原始 `target`：用户写的是域名，报「1.2.3.4
        // 被规则拒绝」他根本对不上是哪个请求。
        let dialed = self.apply_hosts(&target);
        // 建连耗时按判决分开记：卡在这里说明是「开隧道」慢，卡在别处说明是
        // 隧道建好之后的数据搬运慢。两者的排查方向完全不同。
        let t0 = std::time::Instant::now();
        let stream = match &routed.decision {
            Decision::Reject => Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("{} 被规则拒绝", target.display()),
            )),
            Decision::Direct => direct_connect(&dialed).await,
            Decision::Outbound(name) => self.via_outbound(name, &dialed).await,
        };
        let took = t0.elapsed();
        match &stream {
            Ok(_) if took > std::time::Duration::from_secs(1) => tracing::warn!(
                target = %target.display(), decision = ?routed.decision, ?took,
                "建连异常慢"
            ),
            Ok(_) => tracing::debug!(
                target = %target.display(), decision = ?routed.decision, ?took, "建连"
            ),
            Err(e) => tracing::warn!(
                target = %target.display(), decision = ?routed.decision, ?took,
                error = %e, "建连失败"
            ),
        }
        Ok((stream?, routed))
    }

    /// 命中 hosts 的域名目标换成 IP 字面量；其余原样返回。
    ///
    /// 端口不动——hosts 是「这个名字对应哪个地址」，与端口无关。
    fn apply_hosts(&self, target: &AddrPort) -> AddrPort {
        let TargetAddr::Domain(d) = &target.addr else {
            // 目标本来就是 IP：hosts 无从插手。按表里的某条把它换掉就是
            // 改掉调用方明确指定的地址（§6.4 的静默改道）。
            return target.clone();
        };
        let Some(ip) = self.hosts.lookup(d) else {
            return target.clone();
        };
        tracing::debug!("hosts 命中：{d} → {ip}");
        AddrPort {
            addr: match ip {
                std::net::IpAddr::V4(v4) => TargetAddr::V4(v4.octets()),
                std::net::IpAddr::V6(v6) => TargetAddr::V6(v6.octets()),
            },
            port: target.port,
        }
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
    use std::sync::atomic::{AtomicUsize, Ordering};
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
            mux_prefs: vec![wsieve_proto::hello::MuxId::Wsmux],
            session_bases: vec![None],
            server_origin: format!("https://{name}.example"),
            ip_strategy: wsieve_proto::hello::IpStrategy::Auto,
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

    fn hosts_of(pairs: &[(&str, &str)]) -> Arc<wsieve_dns::Hosts> {
        let (h, rejected) = wsieve_dns::Hosts::build(pairs.iter().copied());
        assert!(rejected.is_empty(), "测试数据本身写错了：{rejected:?}");
        Arc::new(h)
    }

    /// 命中 hosts 的域名目标在**建连前**被换成 IP 字面量；端口不动。
    #[test]
    fn apply_hosts_rewrites_a_hit_and_keeps_the_port() {
        let r = test_router(&["MATCH,DIRECT"]).with_hosts(hosts_of(&[("pinned.example", "1.2.3.4")]));
        let got = r.apply_hosts(&domain("pinned.example", 8443));
        assert_eq!(got.addr, TargetAddr::V4([1, 2, 3, 4]));
        assert_eq!(got.port, 8443, "hosts 管的是名字对应哪个地址，与端口无关");
    }

    #[test]
    fn apply_hosts_handles_ipv6_values() {
        let r = test_router(&["MATCH,DIRECT"]).with_hosts(hosts_of(&[("v6.example", "2001:db8::1")]));
        let got = r.apply_hosts(&domain("v6.example", 443));
        let expected: std::net::Ipv6Addr = "2001:db8::1".parse().unwrap();
        assert_eq!(got.addr, TargetAddr::V6(expected.octets()));
    }

    #[test]
    fn apply_hosts_leaves_a_miss_untouched() {
        let r = test_router(&["MATCH,DIRECT"]).with_hosts(hosts_of(&[("pinned.example", "1.2.3.4")]));
        let t = domain("other.example", 443);
        assert_eq!(r.apply_hosts(&t).addr, t.addr);
    }

    /// 目标本来就是 IP 时 hosts 不插手——按表里的某条把它换掉就是改掉
    /// 调用方明确指定的地址（§6.4 的静默改道）。
    #[test]
    fn apply_hosts_never_touches_a_literal_ip_target() {
        let r = test_router(&["MATCH,DIRECT"]).with_hosts(hosts_of(&[("1.2.3.4", "9.9.9.9")]));
        let t = AddrPort {
            addr: TargetAddr::V4([1, 2, 3, 4]),
            port: 443,
        };
        assert_eq!(r.apply_hosts(&t).addr, TargetAddr::V4([1, 2, 3, 4]));
    }

    /// **判决必须在改写之前**。这条是整个 hosts 设计的支点：反过来的话
    /// 目标已是 IP，DOMAIN 类规则会集体不命中，而两边配置各自看着都对。
    ///
    /// 这里直接对 `decide()` 断言——它拿的是原始域名目标，与 hosts 无关。
    #[tokio::test]
    async fn domain_rules_still_match_when_hosts_would_rewrite_the_target() {
        let r = test_router(&["DOMAIN-SUFFIX,example.com,REJECT", "MATCH,DIRECT"])
            .with_hosts(hosts_of(&[("www.example.com", "1.2.3.4")]));
        let t = domain("www.example.com", 443);
        assert!(
            matches!(r.decide(&t).await.decision, Decision::Reject),
            "判决要看域名；若先改写成 1.2.3.4，这条 DOMAIN-SUFFIX 规则就不命中了"
        );
    }

    /// 没配 hosts 时 `apply_hosts` 是纯粹的恒等——空表不该有任何行为。
    #[test]
    fn an_empty_hosts_table_is_the_identity() {
        let r = test_router(&["MATCH,DIRECT"]);
        let t = domain("anything.example", 80);
        assert_eq!(r.apply_hosts(&t).addr, t.addr);
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
    /// 注入式解析器，用来验证两阶段协议闭环。
    ///
    /// 它**自己**记账被调了几次。计数原先是 `Router` 的一个 `#[cfg(test)]`
    /// 字段，但两阶段循环搬去 `wsieve_dns::decide()` 之后，`Router` 已经不再
    /// 经手那一步 —— 计数留在它身上就成了「数一个自己没做的动作」。放在
    /// 解析器这一侧数，量的才是真正发生过的调用，与 `wsieve-dns` 的
    /// `CountingResolver` 同理。
    struct FixedResolver {
        ips: Vec<IpAddr>,
        calls: AtomicUsize,
    }

    impl FixedResolver {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }

    impl RoutingResolver for FixedResolver {
        fn resolve<'a>(
            &'a self,
            _domain: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<IpAddr>> + Send + 'a>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let ips = self.ips.clone();
            Box::pin(async move { ips })
        }
    }

    /// 建一个带计数解析器的 router，并把解析器另拿一份出来供断言。
    fn test_router_with_resolver(lines: &[&str], ips: &[&str]) -> (Router, Arc<FixedResolver>) {
        let o = inst("日本节点");
        let mut m = BTreeMap::new();
        m.insert("日本节点".to_string(), o);
        let resolver = Arc::new(FixedResolver {
            ips: ips.iter().map(|s| s.parse().unwrap()).collect(),
            calls: AtomicUsize::new(0),
        });
        let router = Router::new(
            ruleset(lines, &["日本节点"]),
            geo_stub(),
            m,
            resolver.clone(),
        );
        (router, resolver)
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
        let (r, res) =
            test_router_with_resolver(&["GEOIP,CN,DIRECT", "MATCH,日本节点"], &["1.2.3.4"]);
        let d = r.decide(&domain("a.com", 443)).await;
        assert!(
            matches!(&d.decision, Decision::Outbound(n) if n == "日本节点"),
            "{:?}",
            d.decision
        );
        assert_eq!(res.calls(), 1, "只该解析一次");
        assert!(d.resolved, "确实解析过");
        assert_eq!(d.ips, vec!["1.2.3.4".parse::<IpAddr>().unwrap()]);
    }

    #[tokio::test]
    async fn a_domain_rule_hit_never_triggers_dns() {
        // 两阶段协议的收益就在这里：多数流量在域名类规则处命中，
        // 一个 DNS 包都不发。这是「不需要时零 DNS 泄漏」的可观测证据。
        let (r, res) = test_router_with_resolver(
            &["DOMAIN-SUFFIX,a.com,日本节点", "GEOIP,CN,DIRECT", "MATCH,DIRECT"],
            &["1.2.3.4"],
        );
        let d = r.decide(&domain("www.a.com", 443)).await;
        assert!(matches!(&d.decision, Decision::Outbound(n) if n == "日本节点"));
        assert_eq!(res.calls(), 0, "域名类规则命中时不该有任何解析");
        assert!(!d.resolved);
    }

    #[tokio::test]
    async fn a_failing_resolver_does_not_block_the_connection() {
        // 解析失败传空切片让流程继续（§6.2）。若改成阻断，一次 DNS 抖动
        // 就会让本可直连的流量整片失败。
        let (r, _) = test_router_with_resolver(&["GEOIP,CN,DIRECT", "MATCH,日本节点"], &[]);
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
