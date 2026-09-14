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
    /// 供解析器用于校验 no-resolve 不被误加到非 IP 规则上。
    fn is_domain_kind(self) -> bool {
        matches!(
            self,
            RuleKind::Domain | RuleKind::DomainSuffix | RuleKind::DomainKeyword | RuleKind::GeoSite
        )
    }
}

/// 用户在配置里写的那个词，原样还回去。
///
/// 告警文案要用它，而 `{kind:?}` 给的是 Rust 的驼峰变体名（`GeoSite`）——
/// 用户配置里写的是 `GEOSITE`，让他拿着一个文件里搜不到的词去找那一行，
/// 是在给诊断信息故意打折。
impl std::fmt::Display for RuleKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RuleKind::Domain => "DOMAIN",
            RuleKind::DomainSuffix => "DOMAIN-SUFFIX",
            RuleKind::DomainKeyword => "DOMAIN-KEYWORD",
            RuleKind::GeoSite => "GEOSITE",
            RuleKind::IpCidr => "IP-CIDR",
            RuleKind::GeoIp => "GEOIP",
            RuleKind::DstPort => "DST-PORT",
            RuleKind::Match => "MATCH",
        })
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
    /// I1：域名类和 GEO 类规则的匹配值不能为空
    #[error("{0} 规则的匹配值不能为空——空字符串会匹配所有请求，请填写具体的域名或分类名")]
    EmptyValue(String),
    /// I3：出站名不能为空
    #[error("出站名不能为空——请填写出站节点的名称，例如：MATCH,DIRECT 或 MATCH,我的节点")]
    EmptyTarget,
    /// N2：no-resolve 只对 IP 类规则（IP-CIDR、GEOIP）有效
    #[error("{0} 是域名类规则，no-resolve 对它无效——该标志只用于 IP-CIDR 和 GEOIP 规则，请删除 no-resolve")]
    NoResolveOnDomainRule(String),
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
                // I2：u16::parse 放过了 0，但端口 0 永远不会匹配真实连接，
                //     而且本模块自己的错误提示就写着「合法范围 1-65535」，
                //     必须手动拦截，否则代码与文档自相矛盾。
                let port = value_str
                    .parse::<u16>()
                    .map_err(|_| RuleError::BadPort(value_str.to_string()))?;
                if port == 0 {
                    return Err(RuleError::BadPort(value_str.to_string()));
                }
                RuleValue::Port(port)
            }
            RuleKind::Match => RuleValue::None,
            // I1：域名与 GEO 类别：规范化前先拒空值——
            //     空字符串经 contains("") 会命中所有请求，等同于通配符，
            //     用户通常并不知道自己写了一条「匹配所有」的规则。
            _ => {
                if value_str.is_empty() {
                    return Err(RuleError::EmptyValue(format!("{kind:?}")));
                }
                RuleValue::Text(value_str.trim_end_matches('.').to_ascii_lowercase())
            }
        };

        // I3：出站名不能为空——空名会通过 known_outbounds 查找时给出
        //     「引用了不存在的出站：」（名字那里什么都没有）的误导性错误，
        //     在解析期就拦掉能给出更清晰的信息。
        if target_str.is_empty() {
            return Err(RuleError::EmptyTarget);
        }

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

        // N2：no-resolve 只对 IP 类规则有意义，§96 的注释已说明这一点。
        //     在域名类规则上静默接受会让用户以为打开了某个开关，实则什么都没发生。
        if no_resolve && kind.is_domain_kind() {
            return Err(RuleError::NoResolveOnDomainRule(format!("{kind:?}")));
        }

        Ok(Rule {
            kind,
            value,
            target,
            no_resolve,
        })
    }

    /// IP 类规则是否匹配给定地址（供引擎调用）。
    pub(crate) fn matches_ip(&self, ip: IpAddr) -> bool {
        match &self.value {
            RuleValue::Cidr(net) => net.contains(&ip),
            _ => false,
        }
    }
}

/// 「中国大陆」内置分流预设的前两条规则（产品级预设，非用户可编辑）。
///
/// 存在的理由：规则视图（阶段 5 UI）需要给用户一个「无视配置文件自带规则，
/// 直接用内置规则」的一键预设。这两行本身与配置文件无关——调用方在此基础上
/// 自己拼一条 `MATCH,<全局出站>` 收尾（目标出站是运行时数据，不适合定死
/// 在这个常量里）。
///
/// 本 crate 目前没有任何调用点消费它：把配置文件里的这个预设选项接进真实
/// 运行时（替换 main.rs 硬编码的单出站 RuleSet）是一块尚未开工的独立工程。
/// 这个常量先把「这两行规则语法是对的、真的解析得过」钉死——不然预设写错了
/// 字都不会有人发现，等真去接线时才炸。
pub const CHINA_PRESET_RULES: [&str; 2] = ["GEOSITE,cn,DIRECT", "GEOIP,CN,DIRECT"];

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Rule {
        Rule::parse(s).unwrap_or_else(|e| panic!("解析 {s:?} 失败：{e}"))
    }

    #[test]
    fn china_preset_rules_actually_parse() {
        // 唯一守住这份内置数据没打错字的地方——main.rs 还没有调用点会在
        // 启动时替我们发现。
        for line in CHINA_PRESET_RULES {
            let r = Rule::parse(line).unwrap_or_else(|e| panic!("内置规则 {line:?} 解析失败：{e}"));
            assert!(
                matches!(r.kind, RuleKind::GeoSite | RuleKind::GeoIp),
                "{line:?} 应该是 GEOSITE 或 GEOIP 规则，实为 {:?}",
                r.kind
            );
            assert!(matches!(r.target, Target::Direct), "{line:?} 应该直连");
        }
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

    // ── I1：空值被拒绝 ─────────────────────────────────────────────────────

    #[test]
    fn empty_value_is_rejected_for_domain_rules() {
        // 空 value 在 contains("") 时会命中所有请求，必须在解析期截断
        let e = Rule::parse("DOMAIN-KEYWORD,,REJECT").unwrap_err().to_string();
        assert!(e.contains("空"), "错误信息应说明值为空：{e}");
        assert!(Rule::parse("DOMAIN,,DIRECT").is_err(), "DOMAIN 空值");
        assert!(Rule::parse("DOMAIN-SUFFIX,,DIRECT").is_err(), "DOMAIN-SUFFIX 空值");
        assert!(Rule::parse("GEOSITE,,DIRECT").is_err(), "GEOSITE 空值");
        assert!(Rule::parse("GEOIP,,DIRECT").is_err(), "GEOIP 空值");
    }

    // ── I2：端口 0 被拒绝 ─────────────────────────────────────────────────

    #[test]
    fn port_zero_is_rejected() {
        // 端口 0 永不匹配真实连接，错误文案已声明合法范围为 1-65535
        let e = Rule::parse("DST-PORT,0,DIRECT").unwrap_err().to_string();
        assert!(e.contains("非法端口") || e.contains("0"), "错误信息要点名端口 0：{e}");
        // 65535 依然合法
        assert!(Rule::parse("DST-PORT,65535,DIRECT").is_ok());
        // 1 依然合法
        assert!(Rule::parse("DST-PORT,1,DIRECT").is_ok());
    }

    // ── I3：空出站名被拒绝 ────────────────────────────────────────────────

    #[test]
    fn empty_target_is_rejected() {
        // MATCH, 和 MATCH,   （纯空白）都应在解析期就报错
        let e = Rule::parse("MATCH,").unwrap_err().to_string();
        assert!(e.contains("出站名") || e.contains("不能为空"), "错误信息应说明出站名为空：{e}");

        let e2 = Rule::parse("MATCH,   ").unwrap_err().to_string();
        assert!(e2.contains("出站名") || e2.contains("不能为空"), "纯空白出站名应被拒绝：{e2}");

        // 非 MATCH 规则的空 target 同样应被拒绝
        assert!(Rule::parse("DOMAIN,a.com,").is_err(), "空 target 在 DOMAIN 规则上");
    }

    // ── N2：域名类规则上的 no-resolve 被拒绝 ───────────────────────────────

    #[test]
    fn no_resolve_is_rejected_on_domain_rules() {
        // no-resolve 只对 IP-CIDR / GEOIP 有意义，加在域名类规则上是无声 no-op
        let e = Rule::parse("DOMAIN,a.com,DIRECT,no-resolve").unwrap_err().to_string();
        assert!(
            e.contains("no-resolve") || e.contains("域名类"),
            "错误信息应指出 no-resolve 用错了位置：{e}"
        );
        assert!(Rule::parse("DOMAIN-SUFFIX,a.com,DIRECT,no-resolve").is_err());
        assert!(Rule::parse("DOMAIN-KEYWORD,google,DIRECT,no-resolve").is_err());
        assert!(Rule::parse("GEOSITE,cn,DIRECT,no-resolve").is_err());

        // IP 类规则上的 no-resolve 仍合法
        assert!(Rule::parse("IP-CIDR,10.0.0.0/8,DIRECT,no-resolve").is_ok());
        assert!(Rule::parse("GEOIP,CN,DIRECT,no-resolve").is_ok());
    }
}
