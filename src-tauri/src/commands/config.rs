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

/// 私钥在结构化读取里的占位。UI 看到这个值就知道「这里有一把私钥，
/// 但我手上没有它」—— 与直接删掉字段不同，后者会让 UI 以为没配私钥。
pub const REDACTED: &str = "***";

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
}

impl RuleOp {
    fn line(&self) -> u64 {
        match self {
            Self::ReplaceRule { line, .. } | Self::DeleteRule { line, .. } => *line,
        }
    }
    fn expect(&self) -> &str {
        match self {
            Self::ReplaceRule { expect, .. } | Self::DeleteRule { expect, .. } => expect,
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
    read_view(&config_path(&app)?)
}

/// 原文读 —— **含明文私钥**。
///
/// UI 侧必须在展示处给出 §5.4 要求的警告，且这个返回值不得进日志。
#[tauri::command]
pub async fn config_get_raw(app: tauri::AppHandle) -> CmdResult<String> {
    read_text(&config_path(&app)?)
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

/// 读 + 解析 + 脱敏。
fn read_view(p: &Path) -> CmdResult<ConfigView> {
    let text = read_text(p)?;
    build_view(&text)
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

    Ok(ConfigView { config, rules })
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
        let found = snapshot
            .rules
            .iter()
            .find(|r| r.defined.line() == op.line())
            .ok_or_else(|| CmdError::ConfigInvalid {
                message: format!("第 {} 行不是一条规则（配置已被改动？）", op.line()),
            })?;
        if found.value != op.expect() {
            return Err(CmdError::ConfigInvalid {
                message: format!(
                    "第 {} 行现在是 {:?}，而不是你看到的 {:?}。\
                     配置在此期间被改过，已放弃本次保存 —— 照旧行号改下去会改到别的规则头上",
                    op.line(),
                    found.value,
                    op.expect()
                ),
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
