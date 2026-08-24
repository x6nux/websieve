//! Clash 规则语法：`TYPE,VALUE,TARGET[,no-resolve]`。
//!
//! 全部校验都在解析期完成 —— 判决路径上绝不再做文本解析。
//! 坏 CIDR、坏端口在加载时就报错并指出行号，而不是运行时静默不匹配。

use std::net::IpAddr;
use std::str::FromStr;

use ipnet::IpNet;

/// 代理模式：规则模式 / 全局代理 / 全局直连。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Rule,
    Global,
    Direct,
}

impl FromStr for Mode {
    type Err = RuleError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "rule" => Ok(Mode::Rule),
            "global" => Ok(Mode::Global),
            "direct" => Ok(Mode::Direct),
            other => Err(RuleError::UnknownMode(other.to_string())),
        }
    }
}

/// 规则类型。
///
/// IP-CIDR 与 IP-CIDR6 合并为同一个变体：ipnet::IpNet 自己区分 v4/v6，
/// 不需要在类型层面再区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    Domain,
    DomainSuffix,
    DomainKeyword,
    GeoSite,
    IpCidr,
    GeoIp,
    DstPort,
    Match,
}

impl RuleKind {
    /// 是否属于域名类规则（无需 IP 就能判定）。
    pub fn is_domain_kind(self) -> bool {
        matches!(
            self,
            RuleKind::Domain | RuleKind::DomainSuffix | RuleKind::DomainKeyword | RuleKind::GeoSite
        )
    }

    /// 是否属于 IP 类规则（域名目标需要先 DNS 解析）。
    pub fn is_ip_kind(self) -> bool {
        matches!(self, RuleKind::IpCidr | RuleKind::GeoIp)
    }
}

/// 规则命中后的目标出站。
///
/// 出站名保持用户原样（大小写与内部空格均不修改），因为出站名是
/// 节点组的引用键——规范化会让规则找不到节点。
/// 只有 DIRECT / REJECT 这两个内置值做大小写不敏感识别。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// 转发给用户命名的出站
    Outbound(String),
    /// 内置：直连
    Direct,
    /// 内置：拒绝/丢弃
    Reject,
}

/// 已在解析期校验并规范化的规则值。
#[derive(Debug, Clone)]
pub enum RuleValue {
    /// 域名类与 GEO 类：已转小写、已去尾点，匹配时无需再处理。
    Text(String),
    /// CIDR 类：已解析为 IpNet，包含 v4/v6 信息，can call `.contains(&IpAddr)`。
    Cidr(IpNet),
    /// 端口类：已校验范围（1-65535）。
    Port(u16),
    /// MATCH 规则无值。
    None,
}

/// 一条已解析的规则。
#[derive(Debug, Clone)]
pub struct Rule {
    pub kind: RuleKind,
    pub value: RuleValue,
    pub target: Target,
    /// 是否带有 `no-resolve` 标志（IP 类规则专用）。
    pub no_resolve: bool,
}

/// 规则解析错误，始终携带冒犯的原始文本。
#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    #[error("未知规则类型：{0}")]
    UnknownKind(String),
    #[error("未知模式：{0}（应为 rule / global / direct）")]
    UnknownMode(String),
    #[error("字段不足：{0}")]
    TooFewFields(String),
    #[error("非法 CIDR：{0}")]
    BadCidr(String),
    #[error("非法端口：{0}（合法范围 1-65535）")]
    BadPort(String),
    #[error("未知的第四段参数：{0}（只支持 no-resolve）")]
    UnknownFlag(String),
}

impl Rule {
    /// 解析一行配置；注释行（`#` 开头）与空行返回 `Ok(None)`。
    ///
    /// 调用方按行迭代时用这个接口，可以跳过注释而不中止解析。
    pub fn parse_line(line: &str) -> Result<Option<Rule>, RuleError> {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            return Ok(None);
        }
        Rule::parse(t).map(Some)
    }

    /// 解析一条规则字符串。
    ///
    /// 格式：`TYPE,VALUE,TARGET[,no-resolve]`，MATCH 例外：`MATCH,TARGET`。
    /// 每段首尾空白被去除。错误返回值始终包含冒犯的原始文本。
    pub fn parse(s: &str) -> Result<Rule, RuleError> {
        // 按逗号拆分并对每段去除首尾空白
        let parts: Vec<&str> = s.split(',').map(str::trim).collect();

        if parts.len() < 2 {
            return Err(RuleError::TooFewFields(s.to_string()));
        }

        // 类型字段大小写不敏感
        let kind = match parts[0].to_ascii_uppercase().as_str() {
            "DOMAIN" => RuleKind::Domain,
            "DOMAIN-SUFFIX" => RuleKind::DomainSuffix,
            "DOMAIN-KEYWORD" => RuleKind::DomainKeyword,
            "GEOSITE" => RuleKind::GeoSite,
            // IP-CIDR6 与 IP-CIDR 合并：IpNet 自己区分 v4/v6
            "IP-CIDR" | "IP-CIDR6" => RuleKind::IpCidr,
            "GEOIP" => RuleKind::GeoIp,
            "DST-PORT" => RuleKind::DstPort,
            // FINAL 是 Clash 早期别名，与 MATCH 等价
            "MATCH" | "FINAL" => RuleKind::Match,
            other => return Err(RuleError::UnknownKind(other.to_string())),
        };

        // MATCH 是 2 段（TYPE,TARGET），其余是至少 3 段（TYPE,VALUE,TARGET[,FLAG]）
        let (value_str, target_str, rest) = if kind == RuleKind::Match {
            // MATCH 没有 value 段
            ("", parts[1], &parts[2..])
        } else {
            if parts.len() < 3 {
                return Err(RuleError::TooFewFields(s.to_string()));
            }
            (parts[1], parts[2], &parts[3..])
        };

        // 按类型解析并校验 value
        let value = match kind {
            RuleKind::IpCidr => {
                let net = value_str
                    .parse::<IpNet>()
                    .map_err(|_| RuleError::BadCidr(value_str.to_string()))?;
                RuleValue::Cidr(net)
            }
            RuleKind::DstPort => {
                // u16::parse 会拒绝 0 以外的非法值；70000 超出 u16 范围同样被拒
                let port = value_str
                    .parse::<u16>()
                    .map_err(|_| RuleError::BadPort(value_str.to_string()))?;
                RuleValue::Port(port)
            }
            RuleKind::Match => RuleValue::None,
            // 域名与 GEO 类别：统一规范化（小写 + 去尾点），匹配时不必再处理
            _ => RuleValue::Text(value_str.trim_end_matches('.').to_ascii_lowercase()),
        };

        // 目标出站解析：DIRECT / REJECT 大小写不敏感；其余保持原样
        let target = match target_str.to_ascii_uppercase().as_str() {
            "DIRECT" => Target::Direct,
            "REJECT" => Target::Reject,
            // 用户命名的出站：保持原始大小写与内部空格
            _ => Target::Outbound(target_str.to_string()),
        };

        // 可选的第四段及以后：目前只支持 no-resolve
        let mut no_resolve = false;
        for flag in rest {
            match flag.to_ascii_lowercase().as_str() {
                "no-resolve" => no_resolve = true,
                // 拆分后出现的空段（行尾多余逗号）直接忽略
                "" => {}
                other => return Err(RuleError::UnknownFlag(other.to_string())),
            }
        }

        Ok(Rule {
            kind,
            value,
            target,
            no_resolve,
        })
    }

    /// IP 类规则是否匹配给定地址（供引擎调用）。
    // Task 10 的 evaluate() 会调用此方法，届时移除此 allow。
    #[allow(dead_code)]
    pub(crate) fn matches_ip(&self, ip: IpAddr) -> bool {
        match &self.value {
            RuleValue::Cidr(net) => net.contains(&ip),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Rule {
        Rule::parse(s).unwrap_or_else(|e| panic!("解析 {s:?} 失败：{e}"))
    }

    #[test]
    fn parses_each_rule_type() {
        assert!(matches!(p("DOMAIN,a.com,PROXY").kind, RuleKind::Domain));
        assert!(matches!(p("DOMAIN-SUFFIX,a.com,PROXY").kind, RuleKind::DomainSuffix));
        assert!(matches!(p("DOMAIN-KEYWORD,goog,PROXY").kind, RuleKind::DomainKeyword));
        assert!(matches!(p("GEOSITE,cn,DIRECT").kind, RuleKind::GeoSite));
        assert!(matches!(p("IP-CIDR,10.0.0.0/8,DIRECT").kind, RuleKind::IpCidr));
        assert!(matches!(p("IP-CIDR6,fe80::/10,DIRECT").kind, RuleKind::IpCidr));
        assert!(matches!(p("GEOIP,CN,DIRECT").kind, RuleKind::GeoIp));
        assert!(matches!(p("DST-PORT,22,DIRECT").kind, RuleKind::DstPort));
        assert!(matches!(p("MATCH,PROXY").kind, RuleKind::Match));
    }

    #[test]
    fn builtin_targets_are_recognized() {
        assert!(matches!(p("MATCH,DIRECT").target, Target::Direct));
        assert!(matches!(p("MATCH,REJECT").target, Target::Reject));
        match p("MATCH,日本节点").target {
            Target::Outbound(n) => assert_eq!(n, "日本节点"),
            other => panic!("应是 Outbound，实为 {other:?}"),
        }
    }

    #[test]
    fn no_resolve_flag_is_parsed() {
        assert!(p("IP-CIDR,10.0.0.0/8,DIRECT,no-resolve").no_resolve);
        assert!(!p("IP-CIDR,10.0.0.0/8,DIRECT").no_resolve);
        // 大小写不敏感
        assert!(p("IP-CIDR,10.0.0.0/8,DIRECT,NO-RESOLVE").no_resolve);
    }

    #[test]
    fn type_and_builtin_target_are_case_insensitive() {
        assert!(matches!(p("domain-suffix,a.com,direct").kind, RuleKind::DomainSuffix));
        assert!(matches!(p("domain-suffix,a.com,direct").target, Target::Direct));
    }

    #[test]
    fn whitespace_around_fields_is_trimmed() {
        let r = p("  IP-CIDR , 10.0.0.0/8 , DIRECT , no-resolve ");
        assert!(r.no_resolve);
        assert!(matches!(r.target, Target::Direct));
    }

    #[test]
    fn outbound_name_keeps_original_case_and_spaces() {
        // 出站名是用户起的，不能规范化 —— 规范化会让规则引用不到节点
        match p("MATCH, My Node ").target {
            Target::Outbound(n) => assert_eq!(n, "My Node"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_type_is_rejected_with_the_offending_text() {
        let e = Rule::parse("NOT-A-TYPE,x,PROXY").unwrap_err().to_string();
        assert!(e.contains("NOT-A-TYPE"), "错误信息要含冒犯的类型名：{e}");
    }

    #[test]
    fn malformed_cidr_is_rejected_at_parse_time() {
        // 判决路径上不该再做文本解析 —— 坏 CIDR 必须在加载时就被挡住
        assert!(Rule::parse("IP-CIDR,not-a-cidr,DIRECT").is_err());
        assert!(Rule::parse("IP-CIDR,10.0.0.0/33,DIRECT").is_err());
    }

    #[test]
    fn malformed_port_is_rejected() {
        assert!(Rule::parse("DST-PORT,70000,DIRECT").is_err());
        assert!(Rule::parse("DST-PORT,abc,DIRECT").is_err());
    }

    #[test]
    fn too_few_fields_is_rejected() {
        assert!(Rule::parse("DOMAIN,a.com").is_err(), "缺 target");
        assert!(Rule::parse("MATCH").is_err(), "MATCH 也要 target");
    }

    #[test]
    fn comment_and_blank_lines_are_not_rules() {
        assert!(Rule::parse_line("# 这是注释").unwrap().is_none());
        assert!(Rule::parse_line("   ").unwrap().is_none());
        assert!(Rule::parse_line("DOMAIN,a.com,PROXY").unwrap().is_some());
    }
}
