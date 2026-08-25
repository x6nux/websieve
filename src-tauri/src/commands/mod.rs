//! 控制窗口的命令面（设计文档 §11.2）。
//!
//! **这些命令只授权给 control 窗口。** transport capability 里绝不能出现
//! 它们中的任何一个 —— main 窗口加载的是远端服务器的页面，那台服务器一旦
//! 被攻破，其 JS 就能调用它被授权的一切，而配置里存着 client-priv 私钥。
//! 该纪律由 tests/capability_isolation.rs 守住，那是安全测试而非形式检查。
//!
//! 每加一个命令的固定动作（计划 Part C 的前言）：
//!   1. `permissions/wsieve/allow-<命令名>.toml` 写一份权限定义
//!   2. `capabilities/control.json` 的 permissions 里加上它
//!   3. `main.rs` 的 `generate_handler!` 里注册
//!   4. `ui/src/lib/ipc.js` 的调用清单里体现
//!   5. **重跑 `cargo test --test capability_isolation`** —— 每次都跑
//!
//! 子模块随各自的命令一起声明，而不是先写好一排指向空文件的 `pub mod`：
//! 计划 Task 10 Step 3 预期此处出现 `file not found for module` 编译错，
//! 但那与本仓库既定的「每一步都可构建、可测试」纪律（计划 Task 3 Step 5）
//! 冲突。留一个编译不过的提交，等于给二分查找埋一个假阳性。

pub mod config;
pub mod control;
pub mod probe;

/// 命令的统一错误类型。
///
/// 为什么不直接返回 String：Tauri 要求命令的错误类型实现 Serialize，
/// String 能满足但会把「什么原因失败」压成一句话，UI 无法分类处理
/// （譬如 §12 要求 YAML 语法错时把光标定到出错处 —— 那需要结构化的
/// line / column 字段，而不是让前端去正则刮一句中文）。
#[derive(Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CmdError {
    /// 配置文件语法错，带行列号（§12）。
    ///
    /// 为什么连 column 一起带：`wsieve-config` 的诊断本来就是行**与**列
    /// （它的 `unknown_keys_in_nested_mappings_are_caught_too` 断言了列号
    /// 精确指向拼错的那个键），在这一层砍掉列号等于把上游花力气做出来的
    /// 定位精度白白丢掉一半。0 表示上游没能给出位置。
    ConfigSyntax {
        message: String,
        line: u64,
        column: u64,
    },
    /// 配置语义错（不认识的出站类型、出站重名、枚举字段取值非法等）。
    /// 语法是对的，含义不对 —— 与 ConfigSyntax 分开是因为 UI 的处置不同：
    /// 前者定位光标，后者只需展示一句话。
    ConfigInvalid { message: String },
    /// 改写配置时被 `wsieve-config` 的 YAML 自校验拦下（写进去读回来不是原意）。
    /// 这不是 IO 失败，也不是用户的语法错 —— 是「这个值不能安全地写进 YAML」。
    ConfigNotWritable { message: String },
    /// IO 失败
    Io { message: String },
    /// 该功能所依赖的阶段尚未实现。
    ///
    /// 这是**真实且正确**的响应，不是占位：「还没接上」就是当前的事实。
    /// 返回编造的流量数字或假装连接成功才是伪实现。
    NotReady { message: String },
    /// 其他
    Other { message: String },
}

impl CmdError {
    pub fn io(e: impl std::fmt::Display) -> Self {
        Self::Io {
            message: e.to_string(),
        }
    }

    pub fn other(e: impl std::fmt::Display) -> Self {
        Self::Other {
            message: e.to_string(),
        }
    }

    /// `what` 是功能名，`depends_on` 是它在等哪一块。
    ///
    /// 强制传第二个参数而不是拼一句通用的「尚未实现」：用户看到
    /// 「rule_test 尚未接入」只会困惑，看到「在等路由引擎接进 src-tauri」
    /// 才知道这不是他配错了。
    pub fn not_ready(what: &str, depends_on: &str) -> Self {
        Self::NotReady {
            message: format!("{what} 尚未接入（{depends_on}）"),
        }
    }
}

/// `wsieve_config::ConfigError` → `CmdError` 的翻译。
///
/// 逐个变体显式映射而非兜底成 Other：Syntax 带的行列号是 UI 定位光标的
/// 唯一依据，压成一句话就等于让 §12 的「跳到出错行」永远做不出来。
impl From<wsieve_config::ConfigError> for CmdError {
    fn from(e: wsieve_config::ConfigError) -> Self {
        match &e {
            wsieve_config::ConfigError::Syntax {
                line,
                column,
                message,
            } => Self::ConfigSyntax {
                message: message.clone(),
                line: *line,
                column: *column,
            },
            wsieve_config::ConfigError::Io { .. } => Self::Io {
                message: e.to_string(),
            },
            // 语义错：类型不支持 / 出站重名 / 枚举字段非法
            wsieve_config::ConfigError::UnsupportedProxyType { .. }
            | wsieve_config::ConfigError::DuplicateProxyName(_)
            | wsieve_config::ConfigError::BadEnumField { .. } => Self::ConfigInvalid {
                message: e.to_string(),
            },
        }
    }
}

impl From<wsieve_config::edit::EditError> for CmdError {
    fn from(e: wsieve_config::edit::EditError) -> Self {
        Self::ConfigNotWritable {
            message: e.to_string(),
        }
    }
}

impl std::fmt::Display for CmdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfigSyntax {
                message,
                line,
                column,
            } if *line > 0 => write!(f, "配置第 {line} 行第 {column} 列：{message}"),
            Self::ConfigSyntax { message, .. } => write!(f, "配置语法错：{message}"),
            Self::ConfigInvalid { message }
            | Self::ConfigNotWritable { message }
            | Self::Io { message }
            | Self::NotReady { message }
            | Self::Other { message } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for CmdError {}

pub type CmdResult<T> = Result<T, CmdError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_error_keeps_the_line_and_column_from_the_config_crate() {
        // 行列号是 UI 定位光标的唯一依据 —— 翻译这一层把它压成字符串，
        // §12 的「跳到出错行」就永远做不出来。
        let e = wsieve_config::load_str("mixed_port: 9999\n").unwrap_err();
        match CmdError::from(e) {
            CmdError::ConfigSyntax {
                line,
                column,
                message,
            } => {
                assert_eq!(line, 1, "行号应原样带过来");
                assert!(column > 0, "列号应原样带过来，实为 {column}");
                assert!(message.contains("mixed_port"), "要点名是哪个键：{message}");
            }
            other => panic!("应是语法错，实为 {other:?}"),
        }
    }

    #[test]
    fn semantic_errors_are_not_reported_as_syntax_errors() {
        // 语义错没有「出错的那一行」可跳 —— 归成 ConfigSyntax 会让 UI 拿着
        // 一个恒为 0 的行号去定位光标。
        let cfg = "mode: bogus\nrules:\n  - MATCH,DIRECT\n";
        let e = wsieve_config::load_str(cfg).unwrap().validate().unwrap_err();
        match CmdError::from(e) {
            CmdError::ConfigInvalid { message } => {
                assert!(message.contains("mode"), "{message}");
                assert!(message.contains("bogus"), "{message}");
            }
            other => panic!("应是语义错，实为 {other:?}"),
        }
    }

    #[test]
    fn errors_serialize_with_a_discriminating_kind_tag() {
        // 前端靠 kind 分流（语法错定位光标、未就绪灰掉按钮），
        // 没有这个 tag 就只能去匹配中文文案 —— 改一个字就失效。
        let v = serde_json::to_value(CmdError::not_ready("rule_test", "阶段 5")).unwrap();
        assert_eq!(v["kind"], "not-ready");
        assert!(
            v["message"].as_str().unwrap().contains("阶段 5"),
            "要说清在等什么：{v}"
        );

        let v = serde_json::to_value(CmdError::io("磁盘满了")).unwrap();
        assert_eq!(v["kind"], "io");
    }

    #[test]
    fn display_without_a_location_does_not_print_line_zero() {
        // line=0 是「上游给不出位置」的编码。照着模板打出「配置第 0 行」
        // 会让用户去文件里找一个不存在的行。
        let e = CmdError::ConfigSyntax {
            message: "解析器没给位置".into(),
            line: 0,
            column: 0,
        };
        let text = e.to_string();
        assert!(!text.contains('0'), "不该打出第 0 行：{text}");
        assert!(text.contains("解析器没给位置"), "{text}");
    }

    #[test]
    fn edit_errors_land_in_their_own_variant() {
        // 定点改写被 YAML 自校验拦下，既不是 IO 错也不是用户的语法错。
        // 混进 Io 会让 UI 提示「检查磁盘权限」——方向完全错了。
        let e = wsieve_config::edit::replace_rule_line("rules:\n  - MATCH,DIRECT\n", 2, "")
            .unwrap_err();
        assert!(matches!(CmdError::from(e), CmdError::ConfigNotWritable { .. }));
    }
}
