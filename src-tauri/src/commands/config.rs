//! 配置读写命令（设计文档 §11.2 / §5.4 / §5.6）。
//!
//! ## 私钥在这条边界上的处置
//!
//! `config_get` **脱敏** `client-priv`，`config_get_raw` **不脱敏**。
//!
//! 理由是绝大多数 UI 交互（看规则、切模式、看流量）都不需要私钥。默认不让它
//! 出现在 IPC 载荷里，就少一个把它泄漏进日志 / 崩溃报告 / 截图 / devtools 的
//! 机会。需要导出或核对时走 raw 通道 —— 那是用户的一次显式动作，且 §5.4 要求
//! 在那个入口给出「该文件含私钥」的警告。
//!
//! 这不是把风险消掉，是把它收进一条命令里：**`config_get_raw` 的返回值必须被
//! 当作机密对待**（不进日志、不进错误上报）。它之所以还能存在，唯一的依托是
//! control capability 与 transport capability 的命令集合交集为空 ——
//! 见 `tests/capability_isolation.rs`。往 `transport.json` 里加这一条权限，
//! 就等于把私钥交给那台随时可能被攻破的服务器。
//!
//! ## 写回为什么不走 serde
//!
//! §5.1 选择 YAML 的唯一理由是能写注释。走序列化写回会把用户手写的缩进、
//! 前导注释、行尾注释全部归一化掉 —— UI 上点一次开关就全没了，选 YAML 的
//! 理由随之被摧毁。因此结构化写回一律走 `wsieve_config::edit` 的行级定点改写，
//! 非目标行一个字节都不碰。该 crate 的 `serde-saphyr` 关掉了 `serialize`
//! feature，`serde_saphyr::to_string` 编译期就不存在 —— 这条纪律是编译期事实。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::Manager;

use super::{CmdError, CmdResult};
use wsieve_route::Rule;

/// 私钥在结构化读取里的占位。UI 看到这个值就知道「这里有一把私钥，
/// 但我手上没有它」—— 与直接删掉字段不同，后者会让 UI 以为没配私钥。
pub const REDACTED: &str = "***";

/// 首次启动时自动写出的默认配置。
///
/// 空 `proxies` / 空 `rules` 是完全合法的状态 —— `Config::validate()` 对此
/// 不报错，UI 也早就为它准备好了「还没有配置任何出站」「还没有规则」这类
/// 引导文案（见 `TrafficView.svelte` / `RulesView.svelte` 的空状态）。
/// 因此默认配置**刻意不写任何规则**（比如不写 `MATCH,DIRECT` 兜底）——
/// 加一条默认放行规则会在用户还没添加任何服务器时就悄悄把流量放出去，
/// 与「没有出站可去，代理会拒绝连接而不是偷偷直连」这条产品前提正相反。
/// 其余字段全部省略，交给 `Config` 的 `#[serde(default)]` 补 ——
/// 单一事实源在 `model.rs` 的 `Default for Config`，这里不重复一份。
const DEFAULT_CONFIG_YAML: &str = "\
# websieve 首次运行时自动生成的配置文件。
# 目前还没有任何出站服务器和规则 —— 代理会拒绝连接而不是偷偷直连。
# 可以在控制窗口的「出站」页添加服务器、「规则」页添加规则，
# 也可以直接编辑这个文件：它支持注释，UI 保存时只改动被改动的那几行。

mixed-port: 25500
proxies: []
rules: []
";

/// 结构化读的返回。
///
/// 为什么规则单独一列而不是留在 `config` 里：`Spanned<String>` 序列化时
/// **只吐出值**（serde-saphyr 的 `impl Serialize for Spanned` 就是
/// `self.value.serialize(..)`），行号会在 JSON 化的路上悄悄丢掉。而行号正是
/// `config_save` 定点改写的唯一定位依据 —— 丢了它，结构化保存就只剩
/// 「整份重写」一条路，注释也就保不住了。
///
/// 因此 `config` 里的 `rules` 键被**移除**，规则一律从这里读。一份数据只有
/// 一个出处，UI 不会拿到两份可能不一致的规则列表。
#[derive(Debug, Serialize)]
pub struct ConfigView {
    /// 全量配置的 JSON 投影，`client-priv` 已被替换为 [`REDACTED`]，
    /// 且不含 `rules` 键（见结构体注释）。
    pub config: serde_json::Value,
    /// 规则连同它在文件里的 1-based 行号。
    pub rules: Vec<RuleView>,
    /// `rules:` 键本身所在的 1-based 行号。规则列表为空时，UI 没有任何
    /// 已有规则的行号可以当插入锚点，只能靠这个字段——见 `RuleOp::InsertRule`。
    pub rules_key_line: u64,
    /// `rules_key_line` 那一行的原样文本（`"rules:"` 或 `"rules: []"`）。
    /// `RuleOp::InsertRule` 拿 `rules_key_line` 当 anchor 时，`anchor_expect`
    /// 必须填这一行**当前实际的**文本——前端拿不到这份文本就只能猜，
    /// 猜错了并发校验会拒绝一次本该成功的插入。单独给一个字段，
    /// 不指望前端凭空知道空列表在文件里到底写的是哪一种空写法。
    pub rules_key_text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuleView {
    pub value: String,
    /// 该规则在 config.yaml 里的行号（1-based）。`config_save` 的定点改写用它。
    pub line: u64,
}

/// 结构化写的一次操作。
///
/// 为什么带 `expect`：UI 手上的行号来自某一次 `config_get` 的快照。文件若在
/// 这中间被改过（用户开了原文编辑器、另一个窗口存过、外部工具改过），行号就
/// 是陈旧的 —— 照着它改会**改到另一条规则头上**，而且悄无声息。带上「我以为
/// 那一行是什么」再让服务端核对一次，就把这类失败从静默损坏变成一次明确的拒绝。
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RuleOp {
    /// 把第 `line` 行的规则值换成 `value`
    ReplaceRule {
        line: u64,
        /// 调用方看到的当前值（并发校验）
        expect: String,
        value: String,
    },
    /// 删除第 `line` 行的规则，连同紧贴其上方的注释块
    DeleteRule {
        line: u64,
        /// 调用方看到的当前值（并发校验）
        expect: String,
    },
    /// 在 anchor 行之后插入一条新规则。anchor 可以是某条已有规则的行，
    /// 也可以是 `rules_key_line`（此时新规则成为第一条）。
    InsertRule {
        anchor: u64,
        /// 并发校验：调用方看到 anchor 行当前的原始文本——
        /// 是某条规则时为它的值，是 rules_key_line 时为该行原样文本
        /// （`"rules:"` 或 `"rules: []"`）。
        anchor_expect: String,
        value: String,
    },
}

impl RuleOp {
    fn line(&self) -> u64 {
        match self {
            Self::ReplaceRule { line, .. } | Self::DeleteRule { line, .. } => *line,
            Self::InsertRule { anchor, .. } => *anchor,
        }
    }
    fn expect(&self) -> &str {
        match self {
            Self::ReplaceRule { expect, .. } | Self::DeleteRule { expect, .. } => expect,
            Self::InsertRule { anchor_expect, .. } => anchor_expect,
        }
    }
}

/// 配置文件路径。
///
/// 与 `stats.json` 同目录（见 `stats::path`）—— 那是应用的配置目录，
/// 由 Tauri 按平台惯例给出。
pub fn config_path(app: &tauri::AppHandle) -> CmdResult<PathBuf> {
    let dir = app.path().app_config_dir().map_err(CmdError::io)?;
    Ok(dir.join("config.yaml"))
}

// ── 命令 ────────────────────────────────────────────────────────

/// 结构化读。`client-priv` 被替换为 [`REDACTED`]。
#[tauri::command]
pub async fn config_get(app: tauri::AppHandle) -> CmdResult<ConfigView> {
    let p = config_path(&app)?;
    ensure_config_exists(&p)?;
    read_view(&p)
}

/// 原文读 —— **含明文私钥**。
///
/// UI 侧必须在展示处给出 §5.4 要求的警告，且这个返回值不得进日志。
#[tauri::command]
pub async fn config_get_raw(app: tauri::AppHandle) -> CmdResult<String> {
    let p = config_path(&app)?;
    ensure_config_exists(&p)?;
    read_text(&p)
}

/// 结构化写：按行号定点改写规则，**保留全部注释**。
///
/// 多条操作按行号**从大到小**依次施加。这不是风格选择：删除会让它下方的
/// 所有行号上移，先改小行号再改大行号的话，第二条操作会落到错误的行上 ——
/// 又一次静默损坏。从下往上改则每一条未处理的操作都还没被触碰过。
#[tauri::command]
pub async fn config_save(app: tauri::AppHandle, ops: Vec<RuleOp>) -> CmdResult<()> {
    apply_rule_ops(&config_path(&app)?, ops)
}

/// 原文写 —— 逃生舱（§5.6），一字不动地覆盖。
///
/// 写前先完整解析并校验一次：语法错或语义错的配置写进去会让下次启动失败，
/// 而那时用户可能已经关掉界面了。宁可在这里拒绝，并把行列号带回去。
#[tauri::command]
pub async fn config_save_raw(app: tauri::AppHandle, text: String) -> CmdResult<()> {
    save_text(&config_path(&app)?, &text)
}

// ── 实现（与 AppHandle 无关，因而可直接测）─────────────────────

fn read_text(p: &Path) -> CmdResult<String> {
    std::fs::read_to_string(p).map_err(|e| CmdError::Io {
        message: format!("读取 {} 失败：{e}", p.display()),
    })
}

/// 若配置文件不存在就地创建一份默认配置；若已存在（或检查本身失败）则
/// 原样放行，绝不覆盖用户已有的文件。
///
/// 只在错误种类确凿是 `NotFound` 时才动手创建 —— 权限不足、路径被占用之类
/// 的其他 I/O 错误如实上抛，不能把「这条路径读不了」误判成「这条路径该建
/// 默认文件」，那会在真正的故障上掩盖诊断信息。
fn ensure_config_exists(p: &Path) -> CmdResult<()> {
    match std::fs::metadata(p) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => write_0600(p, DEFAULT_CONFIG_YAML),
        Err(e) => Err(CmdError::Io {
            message: format!("检查 {} 是否存在失败：{e}", p.display()),
        }),
    }
}

/// 读 + 解析 + 脱敏。
fn read_view(p: &Path) -> CmdResult<ConfigView> {
    let text = read_text(p)?;
    build_view(&text)
}

/// `rules:` 键本身所在的 1-based 行号。扫的是顶层（零缩进）的 `rules:` 键，
/// 不认识 `rules:` 出现在别处（比如某个字符串值里恰好含这几个字符）的情况——
/// 那种输入本就不是合法配置，`load_str` 会先一步拒绝。
fn find_rules_key_line(text: &str) -> Option<u64> {
    text.lines()
        .enumerate()
        .find(|(_, l)| *l == "rules:" || l.trim_end() == "rules: []" || l.starts_with("rules:"))
        .map(|(i, _)| i as u64 + 1)
}

fn build_view(text: &str) -> CmdResult<ConfigView> {
    let cfg = wsieve_config::load_str(text)?;
    // 语义校验也在读的时候跑：文件可能是用户手改坏的。报出来而不是让 UI
    // 拿着一份跑不起来的配置显示「一切正常」（房规：错误绝不静默）。
    cfg.validate()?;

    let rules = cfg
        .rules
        .iter()
        .map(|r| RuleView {
            value: r.value.clone(),
            line: r.defined.line(),
        })
        .collect();

    // Config 的 Serialize 只用于 JSON 投影（model.rs 的模块注释说明了这一点），
    // 不可能误写成 YAML —— serialize feature 在 Cargo 层就关着。
    let mut config = serde_json::to_value(&cfg).map_err(CmdError::other)?;
    redact_private_keys(&mut config);
    // rules 从 config 里摘掉：那份拷贝没有行号，留着就是第二个出处。
    if let serde_json::Value::Object(map) = &mut config {
        map.remove("rules");
    }

    let rules_key_line = find_rules_key_line(text).ok_or_else(|| CmdError::ConfigInvalid {
        message: "配置里找不到顶层的 rules: 键——这不应该发生，Config::default() 与\
                  DEFAULT_CONFIG_YAML 都会写这个键"
            .to_string(),
    })?;
    let rules_key_text = text
        .lines()
        .nth(rules_key_line as usize - 1)
        .unwrap_or("rules:")
        .to_string();

    Ok(ConfigView {
        config,
        rules,
        rules_key_line,
        rules_key_text,
    })
}

fn save_text(p: &Path, text: &str) -> CmdResult<()> {
    let cfg = wsieve_config::load_str(text)?;
    cfg.validate()?;
    write_0600(p, text)
}

fn apply_rule_ops(p: &Path, ops: Vec<RuleOp>) -> CmdResult<()> {
    if ops.is_empty() {
        // 空操作不去碰文件。写一遍等价内容看着无害，但它会把文件的 mtime 推新，
        // 让「配置什么时候改过」这个问题失去答案。
        return Ok(());
    }

    let text = read_text(p)?;
    let mut ops = ops;
    // 从大到小施加（见 config_save 的注释）。
    ops.sort_by_key(|o| std::cmp::Reverse(o.line()));

    // 同一行两条操作的语义是不明确的（先删后改？改完再删？），当场拒绝。
    if let Some(w) = ops.windows(2).find(|w| w[0].line() == w[1].line()) {
        return Err(CmdError::ConfigInvalid {
            message: format!("第 {} 行上有多条操作，语义不明确，已拒绝", w[0].line()),
        });
    }

    // 并发校验：逐条核对「调用方以为那一行是什么」。全部核对通过之后才动手，
    // 不是边核对边改 —— 半途失败会留下一份改了一半的配置。
    let snapshot = wsieve_config::load_str(&text)?;
    for op in &ops {
        let actual = if let Some(r) = snapshot.rules.iter().find(|r| r.defined.line() == op.line()) {
            r.value.clone()
        } else if matches!(op, RuleOp::InsertRule { .. }) {
            // anchor 不是一条已有规则——按 InsertRule 的约定，它应该是
            // rules_key_line，直接比对那一行的原始文本。
            text.lines()
                .nth(op.line() as usize - 1)
                .map(str::to_string)
                .ok_or_else(|| CmdError::ConfigInvalid {
                    message: format!("第 {} 行不存在", op.line()),
                })?
        } else {
            return Err(CmdError::ConfigInvalid {
                message: format!("第 {} 行不是一条规则（配置已被改动？）", op.line()),
            });
        };
        if actual != op.expect() {
            return Err(CmdError::ConfigInvalid {
                message: format!(
                    "第 {} 行现在是 {:?}，而不是你看到的 {:?}。\
                     配置在此期间被改过，已放弃本次保存 —— 照旧行号改下去会改到别的规则头上",
                    op.line(),
                    actual,
                    op.expect()
                ),
            });
        }
    }

    // 语法校验：DeleteRule 不产生新内容，不需要过这一关。
    for op in &ops {
        let value = match op {
            RuleOp::ReplaceRule { value, .. } | RuleOp::InsertRule { value, .. } => value,
            RuleOp::DeleteRule { .. } => continue,
        };
        if let Err(e) = Rule::parse(value) {
            return Err(CmdError::ConfigInvalid {
                message: format!("{value:?} 不是一条合法规则：{e}"),
            });
        }
    }

    let mut out = text;
    for op in &ops {
        out = match op {
            RuleOp::ReplaceRule { line, value, .. } => {
                wsieve_config::edit::replace_rule_line(&out, *line, value)?
            }
            RuleOp::DeleteRule { line, .. } => {
                wsieve_config::edit::delete_rule_line(&out, *line)?
            }
            RuleOp::InsertRule { anchor, value, .. } => {
                wsieve_config::edit::insert_rule_line(&out, *anchor, value)?
            }
        };
    }

    // 改完整体再校验一次语义。定点改写只保证「YAML 读得回来且目标行是原意」，
    // 不认识规则语义 —— 而这里能拿到全量 Config，顺手把语义也过一遍。
    wsieve_config::load_str(&out)?.validate()?;

    write_0600(p, &out)
}

/// 以 0600 创建并原子替换。
///
/// **必须以 0600 创建**，而不是先创建再 chmod —— 后者有一个竞态窗口，期间
/// 文件是 0644，同机其他用户能读到私钥（设计文档 §5.4 明确要求 0600）。
///
/// rename 而非原地写，理由与 `stats::write_atomically` 相同：原地写先截断，
/// 截断到 flush 之间被杀就留下半截配置，下次启动直接起不来。
/// 临时文件同目录，避免跨文件系统 rename 的 `EXDEV`。
///
/// 目标文件原有的权限位不被继承 —— rename 换掉的是整个 inode，新文件带的是
/// 我们创建时的 0600。因此这个函数还顺带把历史上被创建成 0644 的配置收紧。
fn write_0600(p: &Path, text: &str) -> CmdResult<()> {
    use std::io::Write;

    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(CmdError::io)?;
    }
    let tmp = p.with_extension("yaml.tmp");

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    // ponytail: Windows 上没有等价的「创建即限权」—— ACL 要另一套 API。
    // 上限：Windows 下配置文件继承目录 ACL（AppData 默认只有当前用户可读，
    // 实践上够用，但不是我们强制的，也没有测试守着）。
    // 升级路径：需要时用 windows-acl crate 显式设 DACL。
    let mut f = opts.open(&tmp).map_err(|e| CmdError::Io {
        message: format!("创建 {} 失败：{e}", tmp.display()),
    })?;
    f.write_all(text.as_bytes()).map_err(CmdError::io)?;
    f.sync_all().map_err(CmdError::io)?;
    drop(f);

    std::fs::rename(&tmp, p).map_err(|e| CmdError::Io {
        message: format!("替换 {} 失败：{e}", p.display()),
    })
}

/// 递归把任意深度的 `client-priv` 换成占位符。
///
/// 递归而非只看 `proxies[].client-priv` 这一条已知路径：配置格式为策略组等
/// 预留了嵌套（§2），只处理已知路径的话，将来新增一层嵌套就会**静默泄漏**
/// —— 而泄漏的是私钥，没有第二次机会。多脱敏几个无关字段的代价远小于此。
fn redact_private_keys(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                // 两种写法都认：YAML 里是 kebab-case，Rust 结构体字段是
                // snake_case，中间任何一层换了序列化配置都不该造成漏网。
                if k == "client-priv" || k == "client_priv" {
                    *val = serde_json::Value::String(REDACTED.to_string());
                } else {
                    redact_private_keys(val);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_private_keys),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"# 我手写的配置
mixed-port: 7890
proxies:
  - name: "日本节点"
    type: websieve
    url: https://example.com/
    server-pub: "cafe"
    client-priv: "deadbeefdeadbeef"
rules:
  # 内网直连
  - DOMAIN-SUFFIX,lan,DIRECT   # 行尾注释也要活下来
  - GEOSITE,cn,DIRECT
  - MATCH,日本节点
"#;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wsieve-cmd-config-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // ── 脱敏 ────────────────────────────────────────────────

    #[test]
    fn redacts_private_key_at_any_depth() {
        let mut v = serde_json::json!({
            "proxies": [
                { "name": "jp", "client-priv": "deadbeef", "server-pub": "cafe" },
                { "name": "us", "client_priv": "f00d" }
            ],
            "nested": { "deeper": { "client-priv": "secret" } }
        });
        redact_private_keys(&mut v);

        let s = serde_json::to_string(&v).unwrap();
        assert!(!s.contains("deadbeef"), "私钥泄漏了：{s}");
        assert!(!s.contains("f00d"), "下划线写法也要脱敏：{s}");
        assert!(!s.contains("secret"), "嵌套层里的也要脱敏：{s}");
        assert!(s.contains("cafe"), "公钥不该被动");
        assert!(s.contains("jp"), "其他字段不该被动");
    }

    #[test]
    fn redaction_leaves_a_visible_placeholder() {
        let mut v = serde_json::json!({ "client-priv": "x" });
        redact_private_keys(&mut v);
        assert_eq!(v["client-priv"], REDACTED, "不能直接删字段 —— UI 需要知道它存在");
    }

    // ── 首次启动：缺配置文件时自动补一份默认的 ──────────────

    #[test]
    fn missing_config_gets_a_usable_default_written() {
        let d = tmpdir("first-run");
        let p = d.join("config.yaml");
        assert!(!p.exists(), "前提：文件本不存在");

        ensure_config_exists(&p).unwrap();

        assert!(p.exists(), "该被创建出来了");
        let cfg = wsieve_config::load_str(&read_text(&p).unwrap()).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.mixed_port, 25500);
        assert!(cfg.proxies.is_empty(), "首次生成不该凭空造一个服务器");
        assert!(cfg.rules.is_empty(), "首次生成不该凭空造一条规则");

        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn an_existing_config_is_never_overwritten() {
        let d = tmpdir("first-run-existing");
        let p = d.join("config.yaml");
        std::fs::write(&p, SAMPLE).unwrap();

        ensure_config_exists(&p).unwrap();

        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            SAMPLE,
            "文件已存在时绝不能被默认配置覆盖 —— 那会丢掉用户的真实配置"
        );

        std::fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_auto_created_default_is_also_0600() {
        use std::os::unix::fs::PermissionsExt;

        let d = tmpdir("first-run-perms");
        let p = d.join("config.yaml");
        ensure_config_exists(&p).unwrap();

        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "自动生成的配置迟早会被写入私钥，权限位不能例外");

        std::fs::remove_dir_all(&d).unwrap();
    }

    // ── config_get 的边界语义 ───────────────────────────────

    #[test]
    fn structured_read_never_carries_the_private_key() {
        // 这是本文件存在的核心断言：结构化读的**整个载荷**里不得出现私钥。
        // 不只查 proxies[0].client-priv —— 序列化路径上任何一处把它复制到
        // 别的键下（譬如将来加一个 summary 字段）都要被这条抓住。
        let view = build_view(SAMPLE).unwrap();
        let payload = serde_json::to_string(&view).unwrap();
        assert!(
            !payload.contains("deadbeefdeadbeef"),
            "config_get 的载荷里出现了明文私钥：{payload}"
        );
        assert!(payload.contains(REDACTED), "应留下占位符：{payload}");
        assert!(payload.contains("cafe"), "公钥不该被脱敏：{payload}");
    }

    #[test]
    fn raw_read_does_carry_the_private_key_on_purpose() {
        // 逃生舱的语义就是「给我原文」。若哪天有人给 config_get_raw 加了脱敏，
        // §5.4 的「导出配置」就变成导出一份用不了的文件 —— 这条会当场变红。
        let d = tmpdir("raw-read");
        let p = d.join("config.yaml");
        std::fs::write(&p, SAMPLE).unwrap();
        let text = read_text(&p).unwrap();
        assert!(text.contains("deadbeefdeadbeef"), "原文读必须是原文");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn rules_come_back_with_the_line_numbers_that_edits_depend_on() {
        // Spanned<T> 序列化时只吐值，行号会在 JSON 化的路上丢掉。丢了它，
        // config_save 就没法定位，结构化保存只剩「整份重写」一条路 ——
        // 注释随之全没。
        let view = build_view(SAMPLE).unwrap();
        assert_eq!(view.rules.len(), 3);
        assert_eq!(view.rules[0].value, "DOMAIN-SUFFIX,lan,DIRECT");
        assert_eq!(view.rules[0].line, 11, "第一条规则在第 11 行");
        assert_eq!(view.rules[2].value, "MATCH,日本节点");
        assert_eq!(view.rules[2].line, 13);
    }

    #[test]
    fn the_config_projection_has_no_second_copy_of_the_rules() {
        // 一份数据两个出处，其中一个还缺行号 —— UI 迟早会读错那一个。
        let view = build_view(SAMPLE).unwrap();
        assert!(
            view.config.get("rules").is_none(),
            "config 里不该再有一份没有行号的规则：{}",
            view.config
        );
    }

    #[test]
    fn a_broken_config_reports_its_line_instead_of_pretending_to_be_fine() {
        match build_view("mixed_port: 9999\n").unwrap_err() {
            CmdError::ConfigSyntax { line, message, .. } => {
                assert_eq!(line, 1);
                assert!(message.contains("mixed_port"), "{message}");
            }
            other => panic!("应带行号报错，实为 {other:?}"),
        }
    }

    #[test]
    fn a_semantically_invalid_config_is_not_reported_as_healthy() {
        // 语法没问题、含义不对（mode 取值非法）。放过它等于让 UI 显示
        // 「一切正常」，而代理其实起不来。
        let bad = "mode: bogus\nrules:\n  - MATCH,DIRECT\n";
        assert!(matches!(
            build_view(bad).unwrap_err(),
            CmdError::ConfigInvalid { .. }
        ));
    }

    // ── 写：权限位 ──────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn written_config_is_0600_and_never_passes_through_0644() {
        use std::os::unix::fs::PermissionsExt;

        let d = tmpdir("mode");
        let p = d.join("config.yaml");
        save_text(&p, SAMPLE).unwrap();

        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "配置含私钥，必须 0600，实为 {mode:o}");

        // 临时文件也必须是 0600：它在 rename 之前就已经带着完整的私钥躺在
        // 磁盘上了。只管最终文件而放任临时文件是 0644，等于把窗口从「创建到
        // chmod」搬到「创建到 rename」，一点没少。
        // 这里靠「写一份、在 rename 之前观察」不好做，改为直接断言实现选择：
        // 复用同一条写路径写出临时文件名，检查它的权限位。
        let tmp = p.with_extension("yaml.tmp");
        write_0600(&tmp, SAMPLE).unwrap();
        let m = std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o777;
        assert_eq!(m, 0o600, "临时文件同样含私钥，实为 {m:o}");

        std::fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn writing_tightens_a_previously_world_readable_config() {
        // 历史上（或别的工具）创建成 0644 的配置，被我们写一次之后要变严。
        // rename 换掉整个 inode，所以旧权限位不会被继承 —— 这条守住那个事实。
        use std::os::unix::fs::PermissionsExt;

        let d = tmpdir("tighten");
        let p = d.join("config.yaml");
        std::fs::write(&p, SAMPLE).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();

        save_text(&p, SAMPLE).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "写过之后必须收紧，实为 {mode:o}");

        std::fs::remove_dir_all(&d).unwrap();
    }

    // ── 写：原文逃生舱 ──────────────────────────────────────

    #[test]
    fn raw_save_refuses_a_config_that_would_not_start_next_time() {
        let d = tmpdir("raw-refuse");
        let p = d.join("config.yaml");
        std::fs::write(&p, SAMPLE).unwrap();

        let e = save_text(&p, "mixed-port: 7890\n  bad-indent: true\n").unwrap_err();
        assert!(matches!(e, CmdError::ConfigSyntax { .. }), "{e:?}");

        // 被拒的写不能留下任何痕迹 —— 半坏的配置比拒绝更糟。
        assert_eq!(std::fs::read_to_string(&p).unwrap(), SAMPLE, "原文件不该被动");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn raw_save_writes_the_bytes_verbatim() {
        // 「一字不动」是逃生舱的全部意义。经过任何一次序列化往返都不算。
        let d = tmpdir("raw-verbatim");
        let p = d.join("config.yaml");
        save_text(&p, SAMPLE).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), SAMPLE);
        std::fs::remove_dir_all(&d).unwrap();
    }

    // ── 写：结构化定点改写 ─────────────────────────────────

    #[test]
    fn structured_save_preserves_every_comment() {
        // 这是整条写路径存在的理由。走 serde 序列化的话这条必红。
        let d = tmpdir("comments");
        let p = d.join("config.yaml");
        std::fs::write(&p, SAMPLE).unwrap();

        apply_rule_ops(
            &p,
            vec![RuleOp::ReplaceRule {
                line: 12,
                expect: "GEOSITE,cn,DIRECT".into(),
                value: "GEOSITE,cn,日本节点".into(),
            }],
        )
        .unwrap();

        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("# 我手写的配置"), "顶部注释没了：\n{after}");
        assert!(after.contains("# 内网直连"), "前导注释没了：\n{after}");
        assert!(after.contains("# 行尾注释也要活下来"), "行尾注释没了：\n{after}");
        assert!(after.contains("GEOSITE,cn,日本节点"), "改写没生效：\n{after}");
        assert!(!after.contains("GEOSITE,cn,DIRECT"), "旧值还在：\n{after}");

        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn structured_save_rejects_a_syntactically_invalid_rule_value() {
        let d = tmpdir("bad-rule-syntax");
        let p = d.join("config.yaml");
        std::fs::write(&p, SAMPLE).unwrap();

        let before = std::fs::read_to_string(&p).unwrap();
        let ops = vec![RuleOp::ReplaceRule {
            line: 12, // SAMPLE 里 "GEOSITE,cn,DIRECT" 那一行
            expect: "GEOSITE,cn,DIRECT".to_string(),
            value: "这不是一条合法规则".to_string(),
        }];
        let err = apply_rule_ops(&p, ops).unwrap_err();
        assert!(
            matches!(err, CmdError::ConfigInvalid { .. }),
            "语法错误的规则值应被拒绝，实为 {err:?}"
        );
        // 拒绝就不该有任何字节落盘——validate 失败必须在写文件之前发生
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn structured_save_rejects_a_value_with_the_right_shape_but_an_unknown_type() {
        // 上一条测试的值连逗号都没有，只够证明「字段数不对」这一最浅的一层
        // 被挡住了；这条换一个字段数正确、但 TYPE 段不认识的值，确认校验
        // 真的走到了 Rule::parse 的语义层，而不是只在数逗号。
        let d = tmpdir("bad-rule-type");
        let p = d.join("config.yaml");
        std::fs::write(&p, SAMPLE).unwrap();

        let ops = vec![RuleOp::ReplaceRule {
            line: 12,
            expect: "GEOSITE,cn,DIRECT".to_string(),
            value: "FOO,cn,DIRECT".to_string(),
        }];
        let err = apply_rule_ops(&p, ops).unwrap_err();
        assert!(
            matches!(err, CmdError::ConfigInvalid { .. }),
            "未知规则类型应被拒绝，实为 {err:?}"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn structured_save_still_accepts_a_syntactically_valid_rule_value() {
        // 补校验不能误伤合法值——这条守住「加固」没有变成「更严格到拒绝正常输入」。
        let d = tmpdir("good-rule-syntax");
        let p = d.join("config.yaml");
        std::fs::write(&p, SAMPLE).unwrap();

        let ops = vec![RuleOp::ReplaceRule {
            line: 12,
            expect: "GEOSITE,cn,DIRECT".to_string(),
            value: "GEOSITE,private,DIRECT".to_string(),
        }];
        apply_rule_ops(&p, ops).unwrap();
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn structured_save_never_touches_the_private_key_line() {
        // 规则改写不该有任何理由碰到 proxies 段。这条同时也是「非目标行
        // 原样透传」的一次实证 —— 私钥那一行的字节必须完全不变。
        let d = tmpdir("untouched-key");
        let p = d.join("config.yaml");
        std::fs::write(&p, SAMPLE).unwrap();

        apply_rule_ops(
            &p,
            vec![RuleOp::DeleteRule {
                line: 12,
                expect: "GEOSITE,cn,DIRECT".into(),
            }],
        )
        .unwrap();

        let after = std::fs::read_to_string(&p).unwrap();
        assert!(
            after.contains(r#"client-priv: "deadbeefdeadbeef""#),
            "私钥那一行被动过了：\n{after}"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_stale_line_number_is_refused_instead_of_editing_the_wrong_rule() {
        // UI 手上的行号来自某次快照。文件在这中间被改过时，照着旧行号改下去
        // 会改到另一条规则头上 —— 而且悄无声息。这是本命令最容易出的事故。
        let d = tmpdir("stale");
        let p = d.join("config.yaml");
        std::fs::write(&p, SAMPLE).unwrap();

        let e = apply_rule_ops(
            &p,
            vec![RuleOp::ReplaceRule {
                line: 12,
                expect: "GEOIP,CN,DIRECT".into(), // 调用方以为的旧值，其实不是
                value: "MATCH,DIRECT".into(),
            }],
        )
        .unwrap_err();

        match &e {
            CmdError::ConfigInvalid { message } => {
                assert!(message.contains("GEOSITE,cn,DIRECT"), "要说清实际是什么：{message}");
                assert!(message.contains("GEOIP,CN,DIRECT"), "要说清你以为是什么：{message}");
            }
            other => panic!("应被拒，实为 {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&p).unwrap(), SAMPLE, "文件不该被动");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn multiple_ops_apply_bottom_up_so_deletions_do_not_shift_later_targets() {
        // 从小行号往大行号改：删掉第 11 行之后，原第 12 行变成第 11 行，
        // 第二条操作就会打到错误的行上。乱序传入也必须得到正确结果 ——
        // 排序是实现的责任，不是调用方的。
        let d = tmpdir("bottom-up");
        let p = d.join("config.yaml");
        std::fs::write(&p, SAMPLE).unwrap();

        apply_rule_ops(
            &p,
            vec![
                RuleOp::ReplaceRule {
                    line: 12,
                    expect: "GEOSITE,cn,DIRECT".into(),
                    value: "GEOSITE,cn,日本节点".into(),
                },
                RuleOp::DeleteRule {
                    line: 11,
                    expect: "DOMAIN-SUFFIX,lan,DIRECT".into(),
                },
            ],
        )
        .unwrap();

        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("GEOSITE,cn,日本节点"), "第二条操作打歪了：\n{after}");
        assert!(!after.contains("DOMAIN-SUFFIX,lan"), "删除没生效：\n{after}");
        assert!(after.contains("MATCH,日本节点"), "兜底规则被误伤：\n{after}");

        // 改完仍是一份能读回来的合法配置
        let c = wsieve_config::load_str(&after).unwrap();
        assert_eq!(c.rules.len(), 2);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn two_ops_on_one_line_are_refused_rather_than_silently_ordered() {
        let d = tmpdir("dup-line");
        let p = d.join("config.yaml");
        std::fs::write(&p, SAMPLE).unwrap();

        let e = apply_rule_ops(
            &p,
            vec![
                RuleOp::DeleteRule {
                    line: 12,
                    expect: "GEOSITE,cn,DIRECT".into(),
                },
                RuleOp::ReplaceRule {
                    line: 12,
                    expect: "GEOSITE,cn,DIRECT".into(),
                    value: "GEOSITE,cn,日本节点".into(),
                },
            ],
        )
        .unwrap_err();
        assert!(matches!(e, CmdError::ConfigInvalid { .. }), "{e:?}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), SAMPLE, "文件不该被动");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_value_that_would_corrupt_the_file_is_refused_and_rolled_back() {
        // `MATCH,东京 #1` 写进去会被 YAML 当成行尾注释而静默截断。
        // wsieve-config 的写入自校验挡下它，这里确认这层错误不会被吞成 IO 错。
        let d = tmpdir("corrupting");
        let p = d.join("config.yaml");
        std::fs::write(&p, SAMPLE).unwrap();

        let e = apply_rule_ops(
            &p,
            vec![RuleOp::ReplaceRule {
                line: 13,
                expect: "MATCH,日本节点".into(),
                value: "MATCH,东京 #1".into(),
            }],
        )
        .unwrap_err();
        assert!(matches!(e, CmdError::ConfigNotWritable { .. }), "{e:?}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), SAMPLE, "必须整体回滚");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn an_empty_op_list_does_not_rewrite_the_file() {
        // 写一遍等价内容看着无害，但 mtime 被推新之后，「配置什么时候改过」
        // 就再也答不上来了。
        let d = tmpdir("noop");
        let p = d.join("config.yaml");
        std::fs::write(&p, SAMPLE).unwrap();
        let before = std::fs::metadata(&p).unwrap().modified().unwrap();

        apply_rule_ops(&p, vec![]).unwrap();

        let after = std::fs::metadata(&p).unwrap().modified().unwrap();
        assert_eq!(before, after, "空操作不该碰文件");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn insert_rule_appends_after_an_existing_rule() {
        let d = tmpdir("insert-after");
        let p = d.join("config.yaml");
        save_text(&p, SAMPLE).unwrap();

        let view = read_view(&p).unwrap();
        let last = view.rules.last().unwrap();
        apply_rule_ops(
            &p,
            vec![RuleOp::InsertRule {
                anchor: last.line,
                anchor_expect: last.value.clone(),
                value: "GEOSITE,private,DIRECT".to_string(),
            }],
        )
        .unwrap();

        let after = read_view(&p).unwrap();
        assert_eq!(after.rules.last().unwrap().value, "GEOSITE,private,DIRECT");
        assert_eq!(after.rules.len(), view.rules.len() + 1);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn insert_rule_into_an_empty_rules_list_uses_rules_key_line() {
        let d = tmpdir("insert-empty");
        let p = d.join("config.yaml");
        std::fs::write(&p, "mixed-port: 25500\nproxies: []\nrules: []\n").unwrap();

        let view = read_view(&p).unwrap();
        assert!(view.rules.is_empty());
        assert_eq!(view.rules_key_text, "rules: []");
        apply_rule_ops(
            &p,
            vec![RuleOp::InsertRule {
                anchor: view.rules_key_line,
                anchor_expect: view.rules_key_text.clone(),
                value: "MATCH,DIRECT".to_string(),
            }],
        )
        .unwrap();

        let after = read_view(&p).unwrap();
        assert_eq!(after.rules.len(), 1);
        assert_eq!(after.rules[0].value, "MATCH,DIRECT");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn insert_rule_rejects_syntactically_invalid_values() {
        let d = tmpdir("insert-bad-syntax");
        let p = d.join("config.yaml");
        save_text(&p, SAMPLE).unwrap();
        let view = read_view(&p).unwrap();
        let last = view.rules.last().unwrap();

        let err = apply_rule_ops(
            &p,
            vec![RuleOp::InsertRule {
                anchor: last.line,
                anchor_expect: last.value.clone(),
                value: "不合法".to_string(),
            }],
        )
        .unwrap_err();
        assert!(matches!(err, CmdError::ConfigInvalid { .. }));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn insert_rule_with_a_stale_anchor_is_refused() {
        let d = tmpdir("insert-stale");
        let p = d.join("config.yaml");
        save_text(&p, SAMPLE).unwrap();
        let view = read_view(&p).unwrap();
        let last = view.rules.last().unwrap();

        let err = apply_rule_ops(
            &p,
            vec![RuleOp::InsertRule {
                anchor: last.line,
                anchor_expect: "这不是当前的值".to_string(),
                value: "MATCH,DIRECT".to_string(),
            }],
        )
        .unwrap_err();
        assert!(matches!(err, CmdError::ConfigInvalid { .. }));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_line_that_is_not_a_rule_is_named_in_the_error() {
        // 譬如 UI 传了 proxies 段里的行号。含糊的「保存失败」会让用户无从下手。
        let d = tmpdir("not-a-rule");
        let p = d.join("config.yaml");
        std::fs::write(&p, SAMPLE).unwrap();

        let e = apply_rule_ops(
            &p,
            vec![RuleOp::ReplaceRule {
                line: 5,
                expect: "type: websieve".into(),
                value: "MATCH,DIRECT".into(),
            }],
        )
        .unwrap_err();
        match &e {
            CmdError::ConfigInvalid { message } => assert!(message.contains('5'), "{message}"),
            other => panic!("应被拒，实为 {other:?}"),
        }
        std::fs::remove_dir_all(&d).unwrap();
    }
}
