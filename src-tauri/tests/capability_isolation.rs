//! 双 capability 隔离（设计文档 §11.1 / §13）。
//!
//! 这是**安全测试**，不是形式检查。`main` 窗口加载的是远端服务器的页面
//! （`WebviewUrl::External`，见 src-tauri/src/main.rs），那台服务器一旦被
//! 攻破，其页面 JS 就能调用它被授权的每一个 Tauri 命令 —— 而配置里存着
//! `client-priv` 私钥。两个 capability 的命令集合必须无交集。
//!
//! 三条刻意的设计：
//!   1. 读**真实的** capabilities/*.json，不硬编码权限列表 —— 硬编码会随
//!      时间腐烂：有人往 transport.json 加权限，测试还在查旧清单。
//!   2. permission → command 的展开读 gen/schemas/acl-manifests.json，
//!      同样不自己维护映射表。
//!   3. 断言的是**命令名**集合而非 permission 名集合 —— 两个不同名字的
//!      permission 完全可以指向同一个命令。

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Deserialize;

// ── capability 文件的最小 schema（只解析我们要断言的字段）──────────

#[derive(Debug, Deserialize)]
struct CapabilityFile {
    identifier: String,
    #[serde(default)]
    windows: Vec<String>,
    #[serde(default)]
    permissions: Vec<PermissionEntry>,
    #[serde(default)]
    remote: Option<Remote>,
    #[serde(default = "default_true")]
    local: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct Remote {
    #[serde(default)]
    urls: Vec<String>,
}

/// permission 条目可以是裸字符串，也可以是带 scope 的对象。
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PermissionEntry {
    Simple(String),
    Scoped { identifier: String },
}

impl PermissionEntry {
    fn id(&self) -> &str {
        match self {
            Self::Simple(s) => s,
            Self::Scoped { identifier } => identifier,
        }
    }
}

// ── ACL 清单的最小 schema ───────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(default)]
    default_permission: Option<PermissionSet>,
    #[serde(default)]
    permissions: BTreeMap<String, Permission>,
    #[serde(default)]
    permission_sets: BTreeMap<String, PermissionSet>,
}

#[derive(Debug, Deserialize)]
struct PermissionSet {
    #[serde(default)]
    permissions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Permission {
    #[serde(default)]
    commands: Commands,
}

#[derive(Debug, Default, Deserialize)]
struct Commands {
    #[serde(default)]
    allow: Vec<String>,
}

/// 应用自身命令在 acl-manifests.json 里的键。
const APP_ACL_KEY: &str = "__app-acl__";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_capability(name: &str) -> CapabilityFile {
    let p = root().join("capabilities").join(name);
    let text =
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("读取 {} 失败：{e}", p.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("解析 {} 失败：{e}", p.display()))
}

fn read_manifests() -> BTreeMap<String, Manifest> {
    let p = root().join("gen/schemas/acl-manifests.json");
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| {
        panic!(
            "读取 {} 失败：{e}。这个文件由 tauri-build 生成，先跑一次 `cargo build`",
            p.display()
        )
    });
    serde_json::from_str(&text).expect("解析 acl-manifests.json")
}

/// 把 capability 的 permission 列表递归展开为**命令名**集合。
fn expand_commands(
    cap: &CapabilityFile,
    manifests: &BTreeMap<String, Manifest>,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for entry in &cap.permissions {
        expand_one(entry.id(), manifests, &mut out, &mut Vec::new());
    }
    out
}

fn expand_one(
    id: &str,
    manifests: &BTreeMap<String, Manifest>,
    out: &mut BTreeSet<String>,
    stack: &mut Vec<String>,
) {
    // 权限集互相引用成环时，递归会栈溢出。显式报错而不是崩。
    if stack.iter().any(|s| s == id) {
        panic!("permission 引用成环：{stack:?} -> {id}");
    }
    stack.push(id.to_string());

    // "plugin:name"，或裸 "name"（应用自身的命令）
    let (plugin, name) = match id.rsplit_once(':') {
        Some((p, n)) => (p.to_string(), n.to_string()),
        None => (APP_ACL_KEY.to_string(), id.to_string()),
    };
    let manifest = manifests
        .get(&plugin)
        .unwrap_or_else(|| panic!("acl-manifests.json 里没有插件 {plugin}（来自 {id}）"));

    // 1) default 集合
    if name == "default" {
        let set = manifest
            .default_permission
            .as_ref()
            .unwrap_or_else(|| panic!("{plugin} 没有 default 权限集"));
        for child in &set.permissions {
            let child = qualify(&plugin, child);
            expand_one(&child, manifests, out, stack);
        }
        stack.pop();
        return;
    }
    // 2) 具名权限集
    if let Some(set) = manifest.permission_sets.get(&name) {
        for child in &set.permissions {
            let child = qualify(&plugin, child);
            expand_one(&child, manifests, out, stack);
        }
        stack.pop();
        return;
    }
    // 3) 叶子权限 → 命令
    let perm = manifest
        .permissions
        .get(&name)
        .unwrap_or_else(|| panic!("插件 {plugin} 里没有权限 {name}（来自 {id}）"));
    for cmd in &perm.commands.allow {
        // 核心插件的命令挂在自己的命名空间下，应用命令是裸名。
        // 不加前缀的话，core:event 的 "listen" 会和应用自己的 "listen" 混为一谈。
        out.insert(if plugin == APP_ACL_KEY {
            cmd.clone()
        } else {
            format!("{plugin}|{cmd}")
        });
    }
    stack.pop();
}

/// 权限集内部的引用可能是裸名（同插件内）或全限定名。
fn qualify(plugin: &str, child: &str) -> String {
    if child.contains(':') || plugin == APP_ACL_KEY {
        child.to_string()
    } else {
        format!("{plugin}:{child}")
    }
}

// ── 断言 ────────────────────────────────────────────────────────

/// 窗口面不重叠。
///
/// 这里比对的是 **glob 匹配**而不是字符串相等：`transport.json` 的
/// `windows` 里有 `wsieve-transport-*`（isolated 模式每出站一个窗口，
/// 见 outbound/carrier.rs）。字符串相等断言会漏掉「有人加一条
/// `control*` 之类的 glob 把控制窗口也罩进传输 capability」这种情况 ——
/// 而那正是最危险的改法。
#[test]
fn capability_files_exist_and_target_distinct_windows() {
    let t = read_capability("transport.json");
    let c = read_capability("control.json");
    assert_eq!(t.identifier, "transport");
    assert_eq!(c.identifier, "control");

    // 传输侧必须罩住主窗口与 isolated 的窗口前缀，否则 emitter 被 ACL 拒。
    assert!(
        t.windows.iter().any(|w| w == "main"),
        "shared 承载走 main 窗口，这条不能删"
    );
    assert!(
        t.windows.iter().any(|w| w == "wsieve-transport-*"),
        "isolated 每出站一个 wsieve-transport-{{名}} 窗口，没有这条 glob 就握不上手"
    );
    assert_eq!(c.windows, vec!["control".to_string()]);

    // 核心：任何一侧的 glob 都不得匹配到另一侧的窗口标签。
    let t_pat: Vec<glob::Pattern> = t
        .windows
        .iter()
        .map(|w| glob::Pattern::new(w).expect("transport 的 window 模式不合法"))
        .collect();
    let c_pat: Vec<glob::Pattern> = c
        .windows
        .iter()
        .map(|w| glob::Pattern::new(w).expect("control 的 window 模式不合法"))
        .collect();

    for label in &c.windows {
        assert!(
            !t_pat.iter().any(|p| p.matches(label)),
            "transport capability 的窗口模式匹配到了控制窗口 {label} —— \
             远端页面就能借传输 capability 之外的授权面动作"
        );
    }
    // 反向：控制侧不得罩住传输窗口。含中文出站名的真实标签也要试。
    for label in ["main", "wsieve-transport-日本节点"] {
        assert!(
            !c_pat.iter().any(|p| p.matches(label)),
            "control capability 的窗口模式匹配到了传输窗口 {label} —— \
             那台远端服务器的页面会拿到配置读写权"
        );
    }
}

#[test]
fn control_capability_has_no_remote_origin() {
    let c = read_capability("control.json");
    assert!(
        c.remote.is_none(),
        "control capability 绝不能有 remote 字段 —— 一旦有，远端页面就能借它读到含 client-priv 的配置"
    );
    assert!(c.local, "control 加载的是本地资产，必须 local: true");
}

#[test]
fn transport_capability_is_not_local_and_drops_http_wildcard() {
    let t = read_capability("transport.json");
    let remote = t
        .remote
        .expect("transport 必须有 remote —— 它加载的就是远端页面");

    assert!(
        !remote.urls.iter().any(|u| u == "http://**:*"),
        "http://**:* 必须被去掉（设计文档 §11.1 迁移注意）：\
         它意味着任意 http 站点都能调传输命令"
    );
    assert!(
        remote.urls.iter().any(|u| u == "http://127.0.0.1:*"),
        "scripts/e2e.sh 用 http://127.0.0.1:PORT，这一条不能删"
    );
    assert!(remote.urls.iter().any(|u| u == "https://**:*"));
    assert!(
        !t.local,
        "传输窗口永远导航到远端 origin，local: true 是纯多余的授权面"
    );
}

/// 本文件的核心断言。
#[test]
fn the_two_capabilities_share_no_command() {
    let manifests = read_manifests();
    let transport = expand_commands(&read_capability("transport.json"), &manifests);
    let control = expand_commands(&read_capability("control.json"), &manifests);

    // 空集合会让交集断言平凡地通过 —— 那是假绿灯。
    assert!(!transport.is_empty(), "transport 展开后不该为空");
    assert!(!control.is_empty(), "control 展开后不该为空");

    let shared: Vec<_> = transport.intersection(&control).cloned().collect();
    assert!(
        shared.is_empty(),
        "两个 capability 共享了命令：{shared:?}\n\
         transport = {transport:?}\n\
         control   = {control:?}"
    );
}

/// 收窄一层：传输窗口能调的命令**只能是**那三个。
///
/// 上面的交集断言只保证「不重叠」，不保证「不膨胀」—— 有人给 transport
/// 加一个全新的危险命令，只要 control 没有它，交集仍是空的。
#[test]
fn transport_can_only_reach_the_three_binary_channels() {
    let manifests = read_manifests();
    let transport = expand_commands(&read_capability("transport.json"), &manifests);

    let expected: BTreeSet<String> = ["wsieve_heartbeat", "wsieve_raw_post", "wsieve_raw_stream"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    assert_eq!(
        transport, expected,
        "远端页面能调的命令集合变了。若这是有意的，请先想清楚：\
         这台服务器被攻破时，新增的命令会造成什么后果"
    );
}
