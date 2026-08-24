//! 两阶段判决引擎（设计文档 §4.2 纪律① / §6.2）。

use std::collections::HashSet;

use crate::rule::{Mode, Rule, RuleError, RuleKind, Target};

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
