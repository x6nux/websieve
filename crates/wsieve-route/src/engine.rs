//! 两阶段路由判决引擎（Task 10 实现）。

use crate::rule::Rule;

/// 当前连接的判决结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 转发给指定出站
    Outbound(String),
    /// 直连
    Direct,
    /// 拒绝
    Reject,
}

/// evaluate() 的返回值——可能是终态，也可能需要调用方补充 DNS 信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 已可判决
    Decided(Decision),
    /// 需要对 domain 做 DNS 解析后再调一轮
    NeedResolve(String),
}

/// 已加载的规则集，供 evaluate() 使用（Task 9 实现）。
// Task 9 会填充构造逻辑并引用 rules，届时移除此 allow。
#[allow(dead_code)]
pub struct RuleSet {
    pub(crate) rules: Vec<Rule>,
}
