//! Clash 风格 YAML 配置的读写。
//!
//! 读走 serde-saphyr 的反序列化；写**不走** serde 序列化，而是按行号
//! 定点改写（见 edit.rs 与设计文档 §5.6）—— 否则用户手写的规则注释
//! 会在 UI 点一次开关之后全部消失。

pub mod edit;
pub mod model;

pub use model::{Config, Dns, DnsCache, GeoxUrl, Proxy, Tun};

use std::collections::HashSet;
use std::path::Path;

use serde_saphyr::{MessageFormatter, UserMessageFormatter};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("读取 {path} 失败：{source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("第 {line} 行第 {column} 列 YAML 语法错误：{message}")]
    Syntax {
        line: u64,
        column: u64,
        message: String,
    },
    #[error(
        "不支持的出站类型 {kind}（节点「{name}」）。\
         websieve 只支持自有协议，type 必须是 websieve。\
         若这是从 Clash 配置粘贴来的，其中的 ss / vmess / trojan 等节点无法使用"
    )]
    UnsupportedProxyType { name: String, kind: String },
    #[error("出站名重复：{0}。规则用名字引用出站，重名会产生歧义")]
    DuplicateProxyName(String),
    #[error("代理组名 {0} 重复——组名必须唯一，UI 靠它定位该改写哪个组块")]
    DuplicateProxyGroupName(String),
    #[error("代理组名 {0} 与一个出站名重复——组名与出站名共享同一个命名空间，规则引用时才不会混淆")]
    GroupNameCollidesWithOutbound(String),
    #[error("代理组 {group:?} 的成员 {member:?} 不是一个已存在的出站名（也不允许引用另一个组）")]
    UnknownGroupMember { group: String, member: String },
    #[error("代理组 {group:?} 的 selected 是 {selected:?}，但它不在自己的 proxies 成员列表里——UI 只会从 proxies 里列选项，selected 落在列表外就会显示成一个选不中的空选项")]
    SelectedNotAMember { group: String, selected: String },
    #[error("{field} 的值 {value:?} 不合法，应为 {legal} 之一")]
    BadEnumField {
        field: &'static str,
        value: String,
        legal: &'static str,
    },
}

/// 枚举字段的合法取值。
///
/// 判定沿用下游 `Mode::from_str` 的宽松度（`trim` + ASCII 转小写），
/// 这样 `validate()` 不会比真正的消费方更严 —— 拒掉一个下游明明收得下的值，
/// 与放过一个下游收不下的值同样是错的，只是方向相反。
fn check_enum(
    field: &'static str,
    value: &str,
    legal: &'static [&'static str],
    legal_text: &'static str,
) -> Result<(), ConfigError> {
    let normalized = value.trim().to_ascii_lowercase();
    if legal.contains(&normalized.as_str()) {
        Ok(())
    } else {
        Err(ConfigError::BadEnumField {
            field,
            value: value.to_string(),
            legal: legal_text,
        })
    }
}

/// 从字符串读取配置。
///
/// 出错时给出**行号**——这是 UI 能把光标定到错处的唯一依据。行号取自
/// serde-saphyr 的结构化 `Error::location()`，不是从错误文本里刮出来的：
/// 刮文本会随上游措辞变化而静默失效，留下一个恒为 0 的假行号。
pub fn load_str(s: &str) -> Result<Config, ConfigError> {
    serde_saphyr::from_str::<Config>(s).map_err(|e| {
        // format_message 给出**不含**位置后缀、也不含 ASCII 代码片段的裸消息；
        // 位置由我们自己拼进中文模板，免得出现中英夹杂的两套坐标。
        // 用 User 版而非 Default 版：这条消息直达终端用户，不该带内部细节。
        let message = UserMessageFormatter.format_message(&e).into_owned();
        // location() 在极少数无位置信息的错误上返回 None（如从 reader 读取时的
        // 某些 IO 场景），退化为 0 —— UI 据此决定是否高亮某一行。
        let (line, column) = e.location().map_or((0, 0), |l| (l.line(), l.column()));
        ConfigError::Syntax {
            line,
            column,
            message,
        }
    })
}

/// 从文件读取配置。
pub fn load_file(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.display().to_string(),
        source,
    })?;
    load_str(&text)
}

impl Config {
    /// 出站名集合，交给 `wsieve_route::RuleSet::build` 做规则引用校验。
    pub fn outbound_names(&self) -> HashSet<String> {
        self.proxies.iter().map(|p| p.name.clone()).collect()
    }

    /// 规则原文列表，交给 `wsieve_route::RuleSet::build` 解析。
    /// 本 crate 只把规则当字符串，不认识其语义。
    pub fn rule_lines(&self) -> Vec<String> {
        self.rules.iter().map(|r| r.value.clone()).collect()
    }

    /// 配置自身的校验。规则的校验在 wsieve-route 里做 ——
    /// 本 crate 刻意不认识规则语义。
    ///
    /// 枚举字段（mode / carrier / log-level）在这里就要拦下来。
    /// 它们最终确实会被下游的 `Mode::from_str` 之类挡住，所以不拦也漏不出去；
    /// 但一个叫 `validate` 的函数对配置里最基本的枚举字段放行，
    /// 会让调用方以为「过了 validate 就没问题」。要么名副其实，要么别叫这名字。
    pub fn validate(&self) -> Result<(), ConfigError> {
        // 合法值取自设计文档 §5.2 的 schema 注释；
        // log-level 的五档与 `tracing::Level::from_str` 一致（它同样忽略大小写），
        // 故这里放行的值到了 §5.2 那条 EnvFilter 上都收得下。
        check_enum("mode", &self.mode, &["rule", "global", "direct"], "rule / global / direct")?;
        check_enum(
            "rule-preset",
            &self.rule_preset,
            &["custom", "china"],
            "custom / china",
        )?;
        let mut seen_groups: HashSet<&str> = HashSet::new();
        let outbound_names = self.outbound_names();
        for g in &self.proxy_groups {
            // seen_groups 顺带把「组名互相之间不能重复」也一并挡住了：
            // 遍历到第二个同名组时 insert 会失败。
            if !seen_groups.insert(g.name.as_str()) {
                return Err(ConfigError::DuplicateProxyGroupName(g.name.clone()));
            }
            if outbound_names.contains(g.name.as_str()) {
                return Err(ConfigError::GroupNameCollidesWithOutbound(g.name.clone()));
            }
            // 归一化后再比较：check_enum 本身按 trim + 转小写判定合法性，
            // 若下面的 == 比较仍用原始 g.kind，像 "Select" 这种大小写变体会
            // 通过 check_enum 却在这里被当成"未知 kind"而跳过后续校验。
            let kind = g.kind.trim().to_ascii_lowercase();
            check_enum(
                "proxy-groups[].kind",
                &kind,
                &["select", "auto", "load-balance"],
                "select / auto / load-balance",
            )?;
            for m in &g.proxies {
                if !outbound_names.contains(m.as_str()) {
                    return Err(ConfigError::UnknownGroupMember {
                        group: g.name.clone(),
                        member: m.clone(),
                    });
                }
            }
            if kind == "select" && !g.proxies.iter().any(|m| m == &g.selected) {
                return Err(ConfigError::SelectedNotAMember {
                    group: g.name.clone(),
                    selected: g.selected.clone(),
                });
            }
            if kind == "load-balance" {
                check_enum(
                    "proxy-groups[].strategy",
                    &g.strategy,
                    &["consistent-hash", "round-robin"],
                    "consistent-hash / round-robin",
                )?;
            }
        }
        check_enum("carrier", &self.carrier, &["shared", "isolated"], "shared / isolated")?;
        check_enum(
            "log-level",
            &self.log_level,
            &["trace", "debug", "info", "warn", "error"],
            "trace / debug / info / warn / error",
        )?;

        let mut seen: HashSet<&str> = HashSet::new();
        for p in &self.proxies {
            if p.kind != "websieve" {
                return Err(ConfigError::UnsupportedProxyType {
                    name: p.name.clone(),
                    kind: p.kind.clone(),
                });
            }
            if !seen.insert(p.name.as_str()) {
                return Err(ConfigError::DuplicateProxyName(p.name.clone()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
mixed-port: 7890
proxies:
  - name: "日本节点"
    type: websieve
    url: https://example.com/
    server-pub: "aa"
    client-priv: "bb"
rules:
  - MATCH,日本节点
"#;

    #[test]
    fn parses_minimal_config() {
        let c = load_str(MINIMAL).unwrap();
        assert_eq!(c.mixed_port, 7890);
        assert_eq!(c.proxies.len(), 1);
        assert_eq!(c.proxies[0].name, "日本节点");
        assert_eq!(c.rules.len(), 1);
        assert_eq!(c.rules[0].value, "MATCH,日本节点");
    }

    #[test]
    fn omitted_fields_get_defaults() {
        let c = load_str(MINIMAL).unwrap();
        assert_eq!(c.mode, "rule");
        assert_eq!(c.rule_preset, "custom");
        assert!(c.proxy_groups.is_empty(), "省略 proxy-groups 时应为空数组");
        assert_eq!(c.shard_base_port, 18443);
        assert_eq!(c.dns.timeout_ms, 2000);
        assert_eq!(c.carrier, "shared");
    }

    #[test]
    fn proxy_level_defaults_apply() {
        // extra-sessions / mux-prefs 省略时要有值，否则条带数会是 0
        let c = load_str(MINIMAL).unwrap();
        assert_eq!(c.proxies[0].extra_sessions, 3);
        // MuxId 的线上标识，不是 0-based 序号——本 crate 依赖不到
        // `wsieve_proto::hello::MuxId`，「这五个值确实全部可转换」由
        // src-tauri 的 `runtime_state` 测试守着（那边两个 crate 都在）。
        assert_eq!(c.proxies[0].mux_prefs, vec![2, 1, 3, 4, 5]);
    }

    #[test]
    fn rules_carry_line_numbers() {
        // MINIMAL 首行是空行，故 rules: 在第 9 行、第一条规则在第 10 行。
        // 断言精确值而非 > 0：Task 14 的定点改写全靠这个行号，
        // 差一行就会改错别人的规则。
        let c = load_str(MINIMAL).unwrap();
        assert_eq!(c.rules[0].defined.line(), 10, "第一条规则应在第 10 行");
    }

    #[test]
    fn rule_line_numbers_survive_leading_comments() {
        // 注释与空行不能把行号算歪 —— 这正是定点改写要跨过的东西
        let cfg = "rules:\n  # 先走直连\n  - DOMAIN,a.com,DIRECT\n\n  # 兜底\n  - MATCH,DIRECT\n";
        let c = load_str(cfg).unwrap();
        assert_eq!(c.rules.len(), 2);
        assert_eq!(c.rules[0].defined.line(), 3);
        assert_eq!(c.rules[1].defined.line(), 6);
    }

    #[test]
    fn syntax_error_reports_a_line_number() {
        let bad = "mixed-port: 7890\n  bad-indent: true\n";
        let e = load_str(bad).unwrap_err();
        match e {
            ConfigError::Syntax { line, .. } => assert!(line > 0, "应给出行号"),
            other => panic!("应是语法错，实为 {other:?}"),
        }
    }

    #[test]
    fn syntax_error_line_points_at_the_actual_offender() {
        // 同一个错搬到不同行，行号必须跟着走。断言精确值而非 > 0：
        // 一个恒返回 1（或任何常数）的实现能过 `> 0`，却对 UI 毫无用处。
        for (line_no, pad) in [(1u64, ""), (3, "mode: rule\nallow-lan: true\n")] {
            let bad = format!("{pad}geo-update-interval: not-a-number\n");
            let e = load_str(&bad).unwrap_err();
            match e {
                ConfigError::Syntax { line, message, .. } => {
                    assert_eq!(line, line_no, "行号应随出错位置移动，实为第 {line} 行");
                    assert!(message.contains("u32"), "应说明为何不合法：{message}");
                }
                other => panic!("应是语法错，实为 {other:?}"),
            }
        }
    }

    #[test]
    fn missing_field_error_points_at_the_incomplete_mapping() {
        // 缺字段时，serde-saphyr 给的是**该映射最后一行**的位置（此处第 3 行的
        // `typ`），而非缺失字段本该出现的位置 —— 后者本来就不存在于文件里。
        // 记下这个语义：UI 高亮这一行是对的，但别指望它指向 `type` 该在的地方。
        let bad = "proxies:\n  - name: x\n    typ: websieve\nmode: rule\n";
        let e = load_str(bad).unwrap_err();
        match e {
            ConfigError::Syntax { line, message, .. } => {
                assert_eq!(line, 3, "应指向该序列项的末行，实为第 {line} 行");
                assert!(message.contains("type"), "应点名缺失的字段：{message}");
            }
            other => panic!("应是语法错，实为 {other:?}"),
        }
    }

    #[test]
    fn syntax_error_message_is_rendered_without_ascii_snippet() {
        // format_message 给的是裸消息。若误用 Display，错误里会混进
        // 多行 ASCII 代码片段和一套英文行列坐标，与中文模板打架。
        let bad = "proxies:\n\t- name: x\n";
        let e = load_str(bad).unwrap_err();
        let text = e.to_string();
        assert!(!text.contains("-->"), "不该带代码片段：{text}");
        assert!(!text.contains("at line"), "不该带第二套坐标：{text}");
        assert!(text.starts_with("第 2 行第 2 列"), "位置应在句首：{text}");
    }

    #[test]
    fn unsupported_proxy_type_is_named_explicitly() {
        let cfg = r#"
proxies:
  - name: "别人的节点"
    type: vmess
    url: https://x.com/
    server-pub: "aa"
    client-priv: "bb"
rules:
  - MATCH,别人的节点
"#;
        let c = load_str(cfg).unwrap();
        let e = c.validate().unwrap_err().to_string();
        assert!(e.contains("vmess"), "要点名不支持的类型：{e}");
        assert!(e.contains("别人的节点"), "要点名是哪个节点：{e}");
    }

    #[test]
    fn duplicate_proxy_names_are_rejected() {
        // 规则用名字引用出站，重名会让引用产生歧义
        let cfg = r#"
proxies:
  - name: "A"
    type: websieve
    url: https://x.com/
    server-pub: "aa"
    client-priv: "bb"
  - name: "A"
    type: websieve
    url: https://y.com/
    server-pub: "cc"
    client-priv: "dd"
rules:
  - MATCH,A
"#;
        let e = load_str(cfg).unwrap().validate().unwrap_err().to_string();
        assert!(e.contains('A'), "{e}");
    }

    #[test]
    fn a_misspelled_key_is_an_error_not_a_silent_default() {
        // 手写 YAML 最常见的错误就是键名拼错。没有 deny_unknown_fields 时，
        // `mixed_port`（下划线）会静默退回默认值 25500 —— 文件上白纸黑字写着
        // 9999，端口却没变，全程没有一条诊断。本 crate 花力气做行号诊断，
        // 结果对最高频的那个错误一言不发，那才是最坏的结果。
        let bad = "mixed_port: 9999\nrules:\n  - MATCH,DIRECT\n";
        match load_str(bad).unwrap_err() {
            ConfigError::Syntax { line, message, .. } => {
                assert_eq!(line, 1, "行号要指向拼错的那一行，实为第 {line} 行");
                assert!(message.contains("mixed_port"), "要点名是哪个键：{message}");
                // 上游把合法键名一并列出来了，正好能提示用户「你想写的是这个」
                assert!(message.contains("mixed-port"), "应提示正确的键名：{message}");
            }
            other => panic!("应是语法错，实为 {other:?}"),
        }
    }

    #[test]
    fn an_unknown_key_reports_the_line_it_sits_on() {
        // 断言行号会**移动**，而不是恒为 1 —— 一个恒返回常数的实现能过
        // 上一条测试，却对 UI 定位光标毫无用处。
        let bad = "mode: rule\nallow-lan: true\nrules:\n  - MATCH,DIRECT\nbogus-key: x\n";
        match load_str(bad).unwrap_err() {
            ConfigError::Syntax { line, message, .. } => {
                assert_eq!(line, 5, "行号应随出错位置移动，实为第 {line} 行");
                assert!(message.contains("bogus-key"), "{message}");
            }
            other => panic!("应是语法错，实为 {other:?}"),
        }
    }

    #[test]
    fn unknown_keys_in_nested_mappings_are_caught_too() {
        // 拼错的键藏在 dns / proxies 下面时同样要报。`timeout_ms` 静默退回
        // 2000 与 `mixed_port` 静默退回 25500 是同一个病，杀伤力也一样。
        for (src, key, line_no, col_no) in [
            ("dns:\n  enable: true\n  timeout_ms: 500\n", "timeout_ms", 3u64, 3u64),
            (
                "proxies:\n  - name: x\n    type: websieve\n    url: u\n    server-pub: a\n    client-priv: b\n    extra_sessions: 9\n",
                "extra_sessions",
                7,
                5,
            ),
        ] {
            match load_str(src).unwrap_err() {
                ConfigError::Syntax { line, column, message } => {
                    assert_eq!(line, line_no, "{key} 的行号应是 {line_no}，实为 {line}");
                    assert_eq!(column, col_no, "{key} 的列号应指向键本身，实为 {column}");
                    assert!(message.contains(key), "要点名是哪个键：{message}");
                }
                other => panic!("应是语法错，实为 {other:?}"),
            }
        }
    }

    #[test]
    fn validate_rejects_a_bogus_mode() {
        // `mode: bogus` 最终确实会被下游的 Mode::from_str 挡住，所以它漏不出去。
        // 但一个叫 validate 的函数对配置里最基本的枚举字段放行，会让调用方
        // 以为「过了 validate 就没问题」—— 那是它给出的一份虚假保证。
        let cfg = "mode: bogus\nrules:\n  - MATCH,DIRECT\n";
        let e = load_str(cfg).unwrap().validate().unwrap_err();
        assert!(matches!(e, ConfigError::BadEnumField { field: "mode", .. }), "{e:?}");
        let text = e.to_string();
        assert!(text.contains("mode"), "要点名是哪个字段：{text}");
        assert!(text.contains("bogus"), "要点名冒犯的值：{text}");
        assert!(text.contains("global"), "要列出合法取值：{text}");
    }

    #[test]
    fn validate_rejects_bogus_carrier_log_level_and_rule_preset() {
        for (cfg, field, bad, legal_hint) in [
            ("carrier: nope\n", "carrier", "nope", "isolated"),
            ("log-level: shout\n", "log-level", "shout", "debug"),
            ("rule-preset: nope\n", "rule-preset", "nope", "china"),
        ] {
            let e = load_str(cfg).unwrap().validate().unwrap_err();
            match &e {
                ConfigError::BadEnumField { field: f, value, .. } => {
                    assert_eq!(*f, field);
                    assert_eq!(value, bad);
                }
                other => panic!("应是枚举字段错，实为 {other:?}"),
            }
            assert!(e.to_string().contains(legal_hint), "要列出合法取值：{e}");
        }
    }

    #[test]
    fn validate_rejects_a_duplicate_group_name() {
        let cfg = "\
proxies:
  - name: \"日本节点\"
    type: websieve
    url: https://example.com/
    server-pub: \"aa\"
    client-priv: \"bb\"
proxy-groups:
  - name: 节点选择
    kind: select
    proxies: [日本节点]
    selected: 日本节点
  - name: 节点选择
    kind: select
    proxies: [日本节点]
    selected: 日本节点
rules:
  - MATCH,日本节点
";
        let e = load_str(cfg).unwrap().validate().unwrap_err();
        assert!(
            matches!(e, ConfigError::DuplicateProxyGroupName(ref n) if n == "节点选择"),
            "{e:?}"
        );
    }

    #[test]
    fn validate_rejects_a_group_name_that_collides_with_an_outbound() {
        let cfg = "\
proxies:
  - name: \"日本节点\"
    type: websieve
    url: https://example.com/
    server-pub: \"aa\"
    client-priv: \"bb\"
proxy-groups:
  - name: 日本节点
    kind: select
    proxies: [日本节点]
    selected: 日本节点
rules:
  - MATCH,日本节点
";
        let e = load_str(cfg).unwrap().validate().unwrap_err();
        assert!(
            matches!(e, ConfigError::GroupNameCollidesWithOutbound(ref n) if n == "日本节点"),
            "{e:?}"
        );
    }

    #[test]
    fn validate_rejects_an_unknown_group_kind() {
        let cfg = "\
proxies:
  - name: \"日本节点\"
    type: websieve
    url: https://example.com/
    server-pub: \"aa\"
    client-priv: \"bb\"
proxy-groups:
  - name: 我的组
    kind: bogus
    proxies: [日本节点]
rules:
  - MATCH,日本节点
";
        let e = load_str(cfg).unwrap().validate().unwrap_err();
        assert!(
            matches!(e, ConfigError::BadEnumField { field: "proxy-groups[].kind", .. }),
            "{e:?}"
        );
    }

    #[test]
    fn validate_rejects_a_group_member_that_is_not_a_known_outbound() {
        let cfg = "\
proxy-groups:
  - name: 我的组
    kind: select
    proxies: [幽灵节点]
    selected: 幽灵节点
rules:
  - MATCH,DIRECT
";
        let e = load_str(cfg).unwrap().validate().unwrap_err();
        assert!(
            matches!(e, ConfigError::UnknownGroupMember { group: ref g, member: ref m }
                if g == "我的组" && m == "幽灵节点"),
            "{e:?}"
        );
    }

    #[test]
    fn validate_rejects_a_group_member_that_is_itself_a_group() {
        // 禁止嵌套：成员必须是出站名，不能是另一个组的名字，避免解析时出现环。
        let cfg = "\
proxies:
  - name: \"日本节点\"
    type: websieve
    url: https://example.com/
    server-pub: \"aa\"
    client-priv: \"bb\"
proxy-groups:
  - name: 内层组
    kind: select
    proxies: [日本节点]
    selected: 日本节点
  - name: 外层组
    kind: select
    proxies: [内层组]
    selected: 内层组
rules:
  - MATCH,日本节点
";
        let e = load_str(cfg).unwrap().validate().unwrap_err();
        assert!(
            matches!(e, ConfigError::UnknownGroupMember { group: ref g, member: ref m }
                if g == "外层组" && m == "内层组"),
            "{e:?}"
        );
    }

    #[test]
    fn validate_rejects_a_select_group_whose_selected_is_not_a_member() {
        let cfg = "\
proxies:
  - name: \"日本节点\"
    type: websieve
    url: https://example.com/
    server-pub: \"aa\"
    client-priv: \"bb\"
  - name: \"香港节点\"
    type: websieve
    url: https://example.com/
    server-pub: \"aa\"
    client-priv: \"bb\"
proxy-groups:
  - name: 我的组
    kind: select
    proxies: [日本节点]
    selected: 香港节点
rules:
  - MATCH,日本节点
";
        let e = load_str(cfg).unwrap().validate().unwrap_err();
        assert!(
            matches!(e, ConfigError::SelectedNotAMember { group: ref g, .. } if g == "我的组"),
            "{e:?}"
        );
    }

    #[test]
    fn validate_rejects_a_select_group_whose_selected_is_not_a_member_even_with_mixed_case_kind() {
        // kind 写成 "Select"（大小写不同于 schema 里的 "select"）也必须走完
        // select 分支该有的校验，不能因为 check_enum 归一化后就漏判 == "select"
        // 而让下面这条本该失败的 selected 检查被静默跳过。
        let cfg = "\
proxies:
  - name: \"日本节点\"
    type: websieve
    url: https://example.com/
    server-pub: \"aa\"
    client-priv: \"bb\"
  - name: \"香港节点\"
    type: websieve
    url: https://example.com/
    server-pub: \"aa\"
    client-priv: \"bb\"
proxy-groups:
  - name: 我的组
    kind: Select
    proxies: [日本节点]
    selected: 香港节点
rules:
  - MATCH,日本节点
";
        let e = load_str(cfg).unwrap().validate().unwrap_err();
        assert!(
            matches!(e, ConfigError::SelectedNotAMember { group: ref g, .. } if g == "我的组"),
            "{e:?}"
        );
    }

    #[test]
    fn validate_rejects_a_load_balance_group_with_a_bogus_strategy() {
        let cfg = "\
proxies:
  - name: \"日本节点\"
    type: websieve
    url: https://example.com/
    server-pub: \"aa\"
    client-priv: \"bb\"
proxy-groups:
  - name: 我的组
    kind: load-balance
    proxies: [日本节点]
    strategy: bogus
rules:
  - MATCH,日本节点
";
        let e = load_str(cfg).unwrap().validate().unwrap_err();
        assert!(
            matches!(e, ConfigError::BadEnumField { field: "proxy-groups[].strategy", .. }),
            "{e:?}"
        );
    }

    #[test]
    fn validate_accepts_a_well_formed_select_group() {
        let cfg = "\
proxies:
  - name: \"日本节点\"
    type: websieve
    url: https://example.com/
    server-pub: \"aa\"
    client-priv: \"bb\"
  - name: \"香港节点\"
    type: websieve
    url: https://example.com/
    server-pub: \"aa\"
    client-priv: \"bb\"
proxy-groups:
  - name: 节点选择
    kind: select
    proxies: [日本节点, 香港节点]
    selected: 日本节点
  - name: 自动选优
    kind: auto
    proxies: [日本节点, 香港节点]
  - name: 均衡负载
    kind: load-balance
    proxies: [日本节点, 香港节点]
    strategy: consistent-hash
rules:
  - MATCH,日本节点
";
        load_str(cfg).unwrap().validate().unwrap();
    }

    #[test]
    fn validate_accepts_every_legal_enum_value() {
        // 校验不能比真正的消费方更严 —— 拒掉一个下游明明收得下的值，
        // 与放过一个下游收不下的值同样是错的，只是方向相反。
        for m in ["rule", "global", "direct"] {
            load_str(&format!("mode: {m}\n")).unwrap().validate().unwrap();
        }
        for c in ["shared", "isolated"] {
            load_str(&format!("carrier: {c}\n")).unwrap().validate().unwrap();
        }
        for l in ["trace", "debug", "info", "warn", "error"] {
            load_str(&format!("log-level: {l}\n")).unwrap().validate().unwrap();
        }
        for r in ["custom", "china"] {
            load_str(&format!("rule-preset: {r}\n")).unwrap().validate().unwrap();
        }
    }

    #[test]
    fn enum_validation_matches_the_downstream_leniency() {
        // 下游 `Mode::from_str` 做的是 `trim()` + ASCII 转小写，故 `RULE`、
        // ` rule ` 在那边都收得下。这里必须同样收下，否则同一份配置
        // 「校验不过但实际能跑」，两层又给出矛盾答案 —— 与 edit.rs 那处
        // 出站名之争同构，只是方向相反。
        for m in ["RULE", "Rule", " rule ", "\tGLOBAL\n"] {
            load_str(&format!("mode: \"{}\"\n", m.escape_debug()))
                .unwrap()
                .validate()
                .unwrap_or_else(|e| panic!("{m:?} 在下游合法，这里不该拒：{e}"));
        }
    }

    #[test]
    fn valid_config_passes_validation() {
        load_str(MINIMAL).unwrap().validate().unwrap();
    }

    #[test]
    fn outbound_names_are_exposed_for_rule_validation() {
        let c = load_str(MINIMAL).unwrap();
        let names = c.outbound_names();
        assert!(names.contains("日本节点"));
    }

    #[test]
    fn rule_lines_strips_spans_for_the_route_crate() {
        let c = load_str(MINIMAL).unwrap();
        assert_eq!(c.rule_lines(), vec!["MATCH,日本节点".to_string()]);
    }

    #[test]
    fn load_file_reads_from_disk() {
        let path = std::env::temp_dir().join("wsieve-config-load-file-test.yaml");
        std::fs::write(&path, MINIMAL).unwrap();
        let c = load_file(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(c.mixed_port, 7890);
        assert_eq!(c.rules[0].value, "MATCH,日本节点");
    }

    #[test]
    fn missing_file_reports_the_path() {
        let path = std::env::temp_dir().join("wsieve-config-definitely-absent.yaml");
        let e = load_file(&path).unwrap_err();
        match e {
            ConfigError::Io { path: p, .. } => {
                assert!(p.contains("wsieve-config-definitely-absent"), "{p}")
            }
            other => panic!("应是 IO 错，实为 {other:?}"),
        }
    }
}
