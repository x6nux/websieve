//! 两阶段判决引擎（设计文档 §4.2 纪律① / §6.2）。

use std::collections::HashSet;
use std::net::IpAddr;

use wsieve_geo::GeoDb;
use wsieve_proto::addr::{AddrPort, TargetAddr};

use crate::rule::{Mode, Rule, RuleError, RuleKind, RuleValue, Target};

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

impl From<&Target> for Decision {
    fn from(t: &Target) -> Self {
        match t {
            Target::Outbound(n) => Decision::Outbound(n.clone()),
            Target::Direct => Decision::Direct,
            Target::Reject => Decision::Reject,
        }
    }
}

/// evaluate() 的返回值——可能是终态，也可能需要调用方补充 DNS 信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 已可判决
    Decided(Decision),
    /// 扫到一条 IP 类规则、目标是域名、且未带 no-resolve。
    /// 调用方解析后带 Some(&ips) 重新调用一次；解析失败传 Some(&[])。
    NeedResolve { domain: String },
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("第 {line} 行：{source}")]
    Rule {
        line: usize,
        #[source]
        source: RuleError,
    },
    #[error("第 {line} 行引用了不存在的出站：{name}")]
    UnknownOutbound { line: usize, name: String },
    #[error(
        "规则列表缺少 MATCH 兜底。websieve 不设隐式默认 —— \
         隐式直连等于静默裸奔，隐式拒绝等于莫名断网，两者都会让你在不知情下承担后果。\
         请显式添加一行，例如：MATCH,DIRECT"
    )]
    MissingMatch,
    #[error("第 {line} 行位于 MATCH 之后，永远不会被执行。请移到 MATCH 之前或删除")]
    RuleAfterMatch { line: usize },
    #[error("global-outbound 指向不存在的出站：{0}")]
    UnknownGlobalOutbound(String),
}

/// 已加载的规则集，供 evaluate() 使用。
///
/// `Debug` 是必需的，不是装饰：测试里对 `Result<RuleSet, _>` 调
/// `.unwrap_err()` 要求 `T: Debug`，少了它 Task 9 的 5 个测试全部编译失败。
#[derive(Debug)]
pub struct RuleSet {
    rules: Vec<Rule>,
    mode: Mode,
    global: Decision,
    /// MATCH 的目标。加载期已保证存在。
    fallback: Decision,
}

impl RuleSet {
    /// `lines` 是配置里 `rules:` 数组的原始字符串，逐行对应。
    /// 行号按数组下标 +1 报告，注释与空行也占号 —— 与用户在编辑器里看到的一致。
    pub fn build(
        lines: &[String],
        mode: Mode,
        global_outbound: &str,
        known_outbounds: &HashSet<String>,
    ) -> Result<Self, BuildError> {
        let mut rules = Vec::new();
        let mut fallback: Option<Decision> = None;

        for (i, raw) in lines.iter().enumerate() {
            let line = i + 1;
            let Some(rule) =
                Rule::parse_line(raw).map_err(|source| BuildError::Rule { line, source })?
            else {
                continue;
            };

            if fallback.is_some() {
                return Err(BuildError::RuleAfterMatch { line });
            }

            if let Target::Outbound(name) = &rule.target {
                if !known_outbounds.contains(name) {
                    return Err(BuildError::UnknownOutbound {
                        line,
                        name: name.clone(),
                    });
                }
            }

            if rule.kind == RuleKind::Match {
                fallback = Some(Decision::from(&rule.target));
            }
            rules.push(rule);
        }

        let fallback = fallback.ok_or(BuildError::MissingMatch)?;

        let global = if global_outbound.trim().is_empty() {
            fallback.clone()
        } else {
            let name = global_outbound.trim();
            if !known_outbounds.contains(name) {
                return Err(BuildError::UnknownGlobalOutbound(name.to_string()));
            }
            Decision::Outbound(name.to_string())
        };

        Ok(Self {
            rules,
            mode,
            global,
            fallback,
        })
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn global_target(&self) -> &Decision {
        &self.global
    }

    /// 两阶段求值。协议见设计文档 §4.2 纪律①。
    ///
    /// - 第一轮传 `resolved: None`。多数流量在域名类规则处命中，**不触发解析**
    /// - 返回 `NeedResolve` 时，调用方解析后传 `Some(&ips)` 再调一轮；
    ///   解析失败或超时传 `Some(&[])`
    /// - 第二轮**永不**再返回 `NeedResolve`
    ///
    /// 第二轮从头重扫而非断点续扫：规则只有几十条，开销可忽略，
    /// 换来的是函数完全幂等、无需维护游标状态。若断点续扫，`resolved`
    /// 只对触发点之后的规则可见，同一域名在更靠前的另一条 IP 规则上
    /// 会得到不同判决，幂等性直接破。
    pub fn evaluate(
        &self,
        target: &AddrPort,
        resolved: Option<&[IpAddr]>,
        geo: &GeoDb,
    ) -> Verdict {
        // mode 短路
        match self.mode {
            Mode::Direct => return Verdict::Decided(Decision::Direct),
            Mode::Global => return Verdict::Decided(self.global.clone()),
            Mode::Rule => {}
        }

        // 目标地址的两种形态，预先取出，避免每条规则重复 match
        let (domain, target_ip) = match &target.addr {
            TargetAddr::Domain(d) => (Some(d.trim_end_matches('.').to_ascii_lowercase()), None),
            TargetAddr::V4(o) => (None, Some(IpAddr::from(*o))),
            TargetAddr::V6(a) => (None, Some(IpAddr::from(*a))),
        };

        for rule in &self.rules {
            if rule.kind == RuleKind::Match {
                return Verdict::Decided(Decision::from(&rule.target));
            }

            // 每个 kind 独占一个 arm，穷举性由编译器静态保证，无需 unreachable!()
            let hit = match rule.kind {
                RuleKind::DstPort => matches!(&rule.value, RuleValue::Port(p) if *p == target.port),

                // ── 域名类：目标是 IP 就跳过 ──
                RuleKind::Domain => {
                    let Some(d) = domain.as_deref() else { continue };
                    let RuleValue::Text(v) = &rule.value else { continue };
                    d == v
                }
                RuleKind::DomainSuffix => {
                    let Some(d) = domain.as_deref() else { continue };
                    let RuleValue::Text(v) = &rule.value else { continue };
                    suffix_matches(d, v)
                }
                RuleKind::DomainKeyword => {
                    let Some(d) = domain.as_deref() else { continue };
                    let RuleValue::Text(v) = &rule.value else { continue };
                    d.contains(v.as_str())
                }
                // GEO 不可用时视为不匹配，绝不阻断连接（设计文档 §12）
                RuleKind::GeoSite => {
                    let Some(d) = domain.as_deref() else { continue };
                    let RuleValue::Text(v) = &rule.value else { continue };
                    geo.site_matches(v, d).unwrap_or(false)
                }

                // ── IP 类：目标是域名则需要解析 ──
                RuleKind::IpCidr => {
                    let ips: &[IpAddr] = if let Some(ip) = &target_ip {
                        std::slice::from_ref(ip)
                    } else {
                        if rule.no_resolve {
                            continue;
                        }
                        match resolved {
                            None => {
                                return Verdict::NeedResolve {
                                    domain: domain.clone().unwrap_or_default(),
                                };
                            }
                            Some(ips) => ips,
                        }
                    };
                    ips.iter().any(|ip| rule.matches_ip(*ip))
                }
                RuleKind::GeoIp => {
                    let ips: &[IpAddr] = if let Some(ip) = &target_ip {
                        std::slice::from_ref(ip)
                    } else {
                        if rule.no_resolve {
                            continue;
                        }
                        match resolved {
                            None => {
                                return Verdict::NeedResolve {
                                    domain: domain.clone().unwrap_or_default(),
                                };
                            }
                            Some(ips) => ips,
                        }
                    };
                    let RuleValue::Text(code) = &rule.value else { continue };
                    ips.iter().any(|ip| geo.ip_matches(code, *ip).unwrap_or(false))
                }

                RuleKind::Match => {
                    // 循环开头已提前 return，此处永远不到；
                    // 但写出来比 unreachable!() 更能让读者明白控制流。
                    continue;
                }
            };

            if hit {
                return Verdict::Decided(Decision::from(&rule.target));
            }
        }

        // build() 已保证 MATCH 存在，正常走不到这里；保底仍用 fallback
        Verdict::Decided(self.fallback.clone())
    }
}

/// 后缀匹配，边界必须落在标签分隔点上。
/// `example.com` 匹配 `example.com` 与 `a.example.com`，但不匹配 `notexample.com`。
fn suffix_matches(domain: &str, suffix: &str) -> bool {
    if domain == suffix {
        return true;
    }
    domain
        .len()
        .checked_sub(suffix.len())
        .filter(|&i| i > 0)
        .is_some_and(|i| domain.as_bytes()[i - 1] == b'.' && &domain[i..] == suffix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn outbounds(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn missing_match_rule_is_a_load_error() {
        let e = RuleSet::build(
            &lines(&["DOMAIN,a.com,PROXY"]),
            Mode::Rule,
            "",
            &outbounds(&["PROXY"]),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("MATCH"), "错误要点名 MATCH：{e}");
    }

    #[test]
    fn match_rule_makes_it_load() {
        let rs = RuleSet::build(
            &lines(&["DOMAIN,a.com,PROXY", "MATCH,PROXY"]),
            Mode::Rule,
            "",
            &outbounds(&["PROXY"]),
        )
        .unwrap();
        assert_eq!(rs.len(), 2);
    }

    #[test]
    fn rule_referencing_unknown_outbound_is_reported_with_line_number() {
        let e = RuleSet::build(
            &lines(&["DOMAIN,a.com,PROXY", "DOMAIN,b.com,GHOST", "MATCH,PROXY"]),
            Mode::Rule,
            "",
            &outbounds(&["PROXY"]),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("GHOST"), "要点名不存在的出站：{e}");
        assert!(e.contains('2'), "要给出行号（1-based）：{e}");
    }

    #[test]
    fn rules_after_match_are_rejected() {
        // MATCH 之后的规则永远不可达，静默忽略会让用户困惑
        let e = RuleSet::build(
            &lines(&["MATCH,PROXY", "DOMAIN,a.com,PROXY"]),
            Mode::Rule,
            "",
            &outbounds(&["PROXY"]),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("MATCH"), "{e}");
    }

    #[test]
    fn comments_and_blanks_do_not_break_line_numbers() {
        let e = RuleSet::build(
            &lines(&["# 注释", "", "DOMAIN,a.com,GHOST", "MATCH,PROXY"]),
            Mode::Rule,
            "",
            &outbounds(&["PROXY"]),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains('3'), "行号应是 3（含注释与空行）：{e}");
    }

    #[test]
    fn global_outbound_must_exist_when_set() {
        let e = RuleSet::build(
            &lines(&["MATCH,PROXY"]),
            Mode::Global,
            "GHOST",
            &outbounds(&["PROXY"]),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("GHOST"), "{e}");
    }

    #[test]
    fn empty_global_outbound_falls_back_to_match_target() {
        let rs = RuleSet::build(
            &lines(&["MATCH,PROXY"]),
            Mode::Global,
            "",
            &outbounds(&["PROXY"]),
        )
        .unwrap();
        assert_eq!(rs.global_target(), &Decision::Outbound("PROXY".into()));
    }
}
