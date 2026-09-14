//! 两阶段求值的驱动方：路由引擎与解析器之间那道缝。
//!
//! 引擎是同步纯函数，解析是 async —— 设计文档 §4.2 纪律①用「把解析需求
//! 作为返回值抛出」把二者解开。本模块就是那个「接住 NeedResolve、解析、
//! 再来一轮」的调用方，也是整个项目里**唯一**允许调用解析器的判决入口。
//!
//! 集中成一个函数而非散在各入口，为的是让三件事只需要证明一次：
//! 最多解析一次、第二轮永不再抛 NeedResolve、解析失败不阻断连接。

use std::net::IpAddr;

use wsieve_geo::GeoDb;
use wsieve_proto::addr::AddrPort;
use wsieve_route::{Decision, RuleHit, RuleSet, Verdict};

use crate::inject::RoutingResolver;

/// 一次判决的完整记录。UI 的 `rule_test`（设计文档 §11.2）需要知道
/// 「到底解析了没、解析出了什么」，所以这些不能只留在日志里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub decision: Decision,
    /// 做出判决的那条规则。`None` = 没有规则参与（mode 短路，或规则表
    /// 扫穿后落到兜底）—— 两者都不是「某条规则命中」，不能假装是。
    ///
    /// 这一项**只在判决当下拿得到**：事后想知道「是哪条规则把它送去了这个
    /// 出站」，要么做不到，要么得把整轮判决重跑一遍。阶段 4/5 的流量视图
    /// （桑基图中层显示规则原文）与 §11.2 的规则试算都要它，因此本函数走
    /// `evaluate_explained` 而非 `evaluate`：两者是同一套逻辑，后者只是把
    /// `hit` 丢掉。判决路径上顺手带出这一个字段接近零成本。
    pub rule: Option<RuleHit>,
    /// 是否真的发生了 DNS 查询。多数流量在域名类规则处就命中，此值为 false ——
    /// 这正是两阶段协议「不需要时零 DNS 泄漏」的可观测证据。
    pub resolved: bool,
    /// 解析得到的 IP。`resolved == true` 且此表为空，即解析失败或超时。
    ///
    /// **不要拿它替换 `AddrPort`**，理由见 `decide` 的文档注释。
    pub ips: Vec<IpAddr>,
}

/// 走完两阶段协议，返回最终判决**及其出处**。
///
/// - 第一轮 `evaluate_explained(target, None, ..)`；命中即返回，**不发任何 DNS**
/// - 抛出 `NeedResolve` 才解析，然后 `evaluate_explained(target, Some(&ips), ..)`
/// - 解析失败/超时得到空切片，该 IP 规则视为不匹配，流程继续（纪律③）
///
/// **为什么第二轮不会再抛 `NeedResolve`（这不是信任，是可核查的结构事实）：**
/// `evaluate` 里 `NeedResolve` 只有两个构造点（IP-CIDR 与 GEOIP 各一个），
/// 两处都嵌在 `match resolved { None => ... }` 这一个 arm 里。第二轮我们传
/// 的是 `Some(&ips)`，那条 arm 结构上不可达 —— 与 `ips` 是空还是满无关，
/// 空切片走的是 `Some(ips)` 分支，只是匹配不上而已。因此循环最多转一圈，
/// 不存在「解析了还是要解析」的可能。下面的 `unreachable!` 守的正是这条
/// 结构不变量：它只在有人改坏 `evaluate` 时才会响。
///
/// **注意判决结果的使用边界（设计文档 §6.3）**：解析出的 IP 只用于「判决」，
/// **绝不改写传给出站的地址**。判决走代理时仍把**域名**递给出站，由服务端
/// 做远程解析 —— 服务端离目标更近，且不受本地污染影响。`Outcome::ips` 存在
/// 只是为了给 UI 展示与直连路径复用，调用方不得拿它替换 `AddrPort`。
///
/// **`?Sized` 不是装饰**：加上它，`decide` 才能接受 `&dyn RoutingResolver`。
/// 阶段 2 的出站管理器会把解析器装进 `Arc<dyn RoutingResolver>`，以便在
/// 「DNS 已配置」与「尚未就绪」之间切换。现在加一个 `?Sized`，比将来改签名便宜。
pub async fn decide<R: RoutingResolver + ?Sized>(
    rules: &RuleSet,
    target: &AddrPort,
    geo: &GeoDb,
    resolver: &R,
) -> Outcome {
    let first = rules.evaluate_explained(target, None, geo);
    match first.verdict {
        Verdict::Decided(decision) => Outcome {
            decision,
            rule: first.hit,
            resolved: false,
            ips: Vec::new(),
        },
        Verdict::NeedResolve { domain } => {
            let ips = resolver.resolve(&domain).await;
            let second = rules.evaluate_explained(target, Some(&ips), geo);
            match second.verdict {
                Verdict::Decided(decision) => Outcome {
                    decision,
                    rule: second.hit,
                    resolved: true,
                    ips,
                },
                // 两阶段协议的死线。走到这里说明引擎违约了，静默兜底只会让
                // bug 藏进生产环境 —— 宁可在测试里炸掉。
                Verdict::NeedResolve { domain } => unreachable!(
                    "第二轮不该再请求解析（domain={domain}）——\
                     这是 wsieve-route::evaluate 的协议违约，见设计文档 §4.2 纪律①"
                ),
            }
        }
    }
}
