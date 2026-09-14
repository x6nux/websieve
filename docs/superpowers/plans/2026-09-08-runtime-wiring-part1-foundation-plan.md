# 运行时接入 config.yaml —— Part 1：地基 实现计划

> **执行方式**：使用 superpowers:subagent-driven-development——每个 Task 派一个
> 全新 subagent 实现，做完先过 spec 一致性审查，再过代码质量审查，有问题就
> 派修复 subagent 改完重新审查，全部通过才算这个 Task 完成，再进下一个。
> 所有 Rust 相关的 subagent 派工都要在指令里加一句
> `export PATH="$HOME/.cargo/bin:$PATH"`。

**目标**：本计划只做地基——`RuntimeState` 快照结构、启动流程从读 env 变量
改成读 `config.yaml`、出站的增量热更新（含承载 WebView 的动态建窗）。
代理组解析、命令接活（rule_test/outbound_enable/...）、流量统计接入、
Windows 系统代理这几块依赖本计划产出的 `RuntimeState`，留给后续的
Part 2/3/4 单独写计划（各自的计划会在这个 Part 1 完成、实际代码落地后
再写，因为它们要用到 `RuntimeState` 的确切字段/方法签名，而这些在写这份
计划的时候还只是设计，实现时大概率会有细节调整）。

**设计依据**：`docs/superpowers/specs/2026-09-08-runtime-config-wiring-design.md`
（§1 热替换结构、§2 启动流程、§2.1 承载 WebView 约束、§3 增量出站更新）。
下面每个 Task 会重复贴出相关的设计原文和已经确认的真实代码签名，不需要
另外去读设计文档，但**需要**去读引用到的现有源文件全文（签名可能在这份
计划写完之后又有细微变动，以实际文件为准）。

**技术栈**：Rust、`tokio`、`std::sync::RwLock`、Tauri 2 的
`WebviewWindowBuilder`/`AppHandle`。

**关键既有代码（写这份计划时已确认的真实签名，供各 Task 参考）**：

```rust
// src-tauri/src/outbound/mod.rs
pub struct OutboundManager { /* 私有字段：outbounds, core, env, tasks */ }
impl OutboundManager {
    pub fn new(
        outbounds: BTreeMap<String, Arc<OutboundInstance>>,
        core: Arc<TransportCore>,
        env: SessionEnv,
    ) -> Arc<Self>;
    pub fn get(&self, name: &str) -> Option<&Arc<OutboundInstance>>;
    pub fn status(&self, name: &str) -> Option<Status>;
    pub fn statuses(&self) -> BTreeMap<String, Status>;
    pub async fn start_all(self: &Arc<Self>);
    pub async fn set_enabled(self: &Arc<Self>, name: &str, on: bool) -> anyhow::Result<()>;
    pub async fn stop_all(&self);
}
pub fn try_plan_ports(base: u16, outbounds: &[(&str, usize)]) -> anyhow::Result<PortPlan>;

// src-tauri/src/outbound/instance.rs
pub struct OutboundCfg {
    pub name: String,
    pub server_pub: [u8; 32],
    pub client_priv: [u8; 32],
    pub mux_prefs: Vec<MuxId>,
    pub session_bases: Vec<Option<String>>,
}
pub struct OutboundInstance { /* ... */ }
impl OutboundInstance {
    pub fn new(cfg: OutboundCfg) -> Arc<Self>;
    pub fn cfg(&self) -> &OutboundCfg;
    pub fn dialer(&self) -> Option<Arc<StripeDialer>>;
    pub fn status(&self) -> Status;
    pub fn request_stop(&self);
}

// src-tauri/src/outbound/carrier.rs
pub enum CarrierMode { Shared, Isolated }
impl CarrierMode { pub fn parse(s: &str) -> anyhow::Result<Self>; }
pub struct CarrierPlan { /* ... */ }
impl CarrierPlan {
    pub fn build(mode: CarrierMode, host: &str, outbounds: &[(&str, &str)]) -> anyhow::Result<Self>;
    pub fn mode(&self) -> CarrierMode;
    pub fn host_name(&self) -> &str;
    pub fn base_for(&self, name: &str) -> Option<Option<String>>;
    pub fn window_label(&self, name: &str) -> Option<String>;
    pub fn windows(&self) -> Vec<(String, String)>;
}
// build() 在 outbounds 为空时报错——本计划的 Task 3 必须绕开这一点，
// 空列表时根本不调 CarrierPlan::build。

// crates/wsieve-route/src/engine.rs
pub struct RuleSet { /* ... */ }
impl RuleSet {
    pub fn build(
        lines: &[String],
        mode: Mode,
        global_outbound: &str,
        known_outbounds: &HashSet<String>,
    ) -> Result<Self, BuildError>;
}

// src-tauri/src/router.rs
pub struct Router { /* ... */ }
impl Router {
    pub fn new(
        rules: Arc<RuleSet>,
        geo: Arc<GeoDb>,
        outbounds: BTreeMap<String, Arc<OutboundInstance>>,
        resolver: Arc<dyn RoutingResolver>,
    ) -> Self;
    pub fn without_resolver(
        rules: Arc<RuleSet>,
        geo: Arc<GeoDb>,
        outbounds: BTreeMap<String, Arc<OutboundInstance>>,
    ) -> Self;
}

// crates/wsieve-config/src/model.rs（字段名，serde 走 kebab-case）
pub struct Config {
    pub mixed_port: u16,
    pub bind_address: String,
    pub allow_lan: bool,
    pub mode: String,               // "rule" / "global" / "direct"
    pub rule_preset: String,        // "custom" / "china"
    pub global_outbound: String,
    pub system_proxy: bool,
    pub proxies: Vec<Proxy>,
    pub proxy_groups: Vec<ProxyGroup>,
    pub rules: Vec<Spanned<String>>,   // .value 取字符串
    pub dns: Dns,
    pub tun: Tun,
    pub carrier: String,            // "shared" / "isolated"
    pub carrier_host: String,
    pub shard_base_port: u16,
    // 还有 log_level / geo_auto_update / geo_update_interval / geox_url，
    // 与本计划无关，不列出。
}
pub struct Proxy {
    pub name: String,
    pub kind: String,               // 目前只接受 "websieve"
    pub url: String,
    pub server_pub: String,         // 十六进制字符串，与 bootstrap.rs 的 hex32 同格式
    pub client_priv: String,
    pub extra_sessions: usize,      // 默认 3
    pub mux_prefs: Vec<u8>,         // 默认 [0,1,2,3,4]，与 MuxId::from_u8 配合
}

// src-tauri/src/commands/config.rs
pub fn config_path(app: &tauri::AppHandle) -> CmdResult<PathBuf>;
#[tauri::command] pub async fn config_save(app: tauri::AppHandle, ops: Vec<RuleOp>) -> CmdResult<()>;
#[tauri::command] pub async fn config_save_raw(app: tauri::AppHandle, text: String) -> CmdResult<()>;
#[tauri::command] pub async fn config_insert_proxy(app: tauri::AppHandle, lines: Vec<String>) -> CmdResult<()>;
#[tauri::command] pub async fn config_delete_proxy(app: tauri::AppHandle, name: String) -> CmdResult<()>;
```

---

### Task 1：`RuntimeState` 快照结构 + 纯函数出站 diff 逻辑

**Files:**
- Create: `src-tauri/src/runtime_state.rs`
- Modify: `src-tauri/src/main.rs`（只加一行 `mod runtime_state;`）

**背景**（设计文档 §1、§3 原文）：

> 新增一个快照结构体，把"一份配置对应的完整运行时状态"打包成一个整体：
> `RuntimeState { rule_set, outbound_manager, router, groups }`。用
> `RwLock<Arc<RuntimeState>>` 托管在 Tauri 状态里……不引入 `arc-swap`
> （workspace 里没有这个依赖），标准库的 `RwLock<Arc<T>>` 已经够用。
>
> 真正需要"增量"的是出站……方案是每次保存后对比新旧 `proxies:` 列表：
> 没变的出站原样搬（`Arc` 克隆，不重启）；新增的构建新实例；删除的
> `stop_one` 后丢弃；字段被改动的按"先停旧的、再当新增处理"走。

本 Task **只写纯逻辑，不碰 `main.rs` 的启动流程、不碰 Tauri 状态托管**——
这两块分别是 Task 2/3 的范围。这样切的理由：diff 逻辑是可以完全脱离
Tauri/真实网络单测的纯函数，先在隔离环境里把正确性钉死，比一上来就往
`main.rs` 那个大函数里塞代码更容易审查、更容易复用。

- [ ] **Step 1：`RuntimeState` 结构体**

`src-tauri/src/runtime_state.rs`：

```rust
//! 运行时状态快照（设计文档「运行时接入 config.yaml」§1）。
//!
//! 一次配置保存对应一份完整快照，整体构建、整体替换——不给
//! `RuleSet`/`OutboundManager`/`Router` 各开一把锁分别热替换，是因为
//! `Router`/`OutboundManager` 各自维护一份出站表，必须来自同一批
//! `Arc<OutboundInstance>` 才不会错位；分开加锁会有"两把锁各自被刷新，
//! 中间那一刻两者不一致"的窗口，合成一个快照结构体从根上排除这个可能。

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::outbound::instance::OutboundInstance;
use crate::outbound::OutboundManager;
use crate::router::Router;

pub struct RuntimeState {
    pub rule_set: Arc<wsieve_route::RuleSet>,
    pub outbound_manager: Arc<OutboundManager>,
    pub router: Arc<Router>,
    // GroupTable 是 Part 3（代理组接入）的产出，本计划不实现，先占位成
    // 一个空结构体，避免 Part 3 落地时要改这里的字段名/调用点。
    pub groups: Arc<GroupTable>,
}

/// 代理组表——本计划只放占位结构，真正的构建逻辑属于 Part 3。
#[derive(Default)]
pub struct GroupTable;
```

（`GroupTable` 现在是空的占位类型——Part 3 会给它加字段与构建函数。放在
这里而不是等 Part 3 再引入，是为了让 `RuntimeState` 的字段集合从一开始
就是最终形态，后续计划不需要改这个结构体本身，只需要充实 `GroupTable`。）

- [ ] **Step 2：先写失败的测试——出站 diff 逻辑**

在 `runtime_state.rs` 末尾加：

```rust
/// 一次配置更新里，出站集合要如何从旧的过渡到新的。
///
/// 用 `Arc<OutboundInstance>` 而非 `OutboundCfg` 表示"复用"，是因为复用
/// 的重点就是**不重新构造实例**——上层拿到这个结构后，`reused` 里的每一项
/// 直接原样放进新 `OutboundManager`，`added` 里的每一项才需要真的
/// `OutboundInstance::new(cfg)`。
pub struct OutboundDiff {
    /// 名字与关键字段都未变的出站：原样复用的 `Arc`。
    pub reused: BTreeMap<String, Arc<OutboundInstance>>,
    /// 需要新建的出站配置（新增的 + 字段被改动、按"先停旧的再当新增"处理的）。
    pub added: Vec<crate::outbound::instance::OutboundCfg>,
    /// 需要停止并丢弃的旧出站名（删除的 + 字段被改动的旧版本）。
    pub removed: Vec<String>,
}

/// 纯函数：给定旧的出站实例表与新的 `Proxy` 列表，算出增量。
///
/// "未变"的判定标准是 `OutboundCfg` 的全部字段相等（`name`/`server_pub`/
/// `client_priv`/`mux_prefs`/`session_bases`）——`session_bases` 由承载
/// 计划算出、不来自 `Proxy` 本身，所以调用方要在算出新的 `session_bases`
/// 之后才能调这个函数；本函数只管"给定两份完整 `OutboundCfg`，谁跟谁一样"，
/// 不负责计算 `session_bases`。
pub fn diff_outbounds(
    old: &BTreeMap<String, Arc<OutboundInstance>>,
    new_cfgs: &[crate::outbound::instance::OutboundCfg],
) -> OutboundDiff {
    todo!("Task 1 Step 3 实现")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbound::instance::OutboundCfg;
    use wsieve_proto::hello::MuxId;

    fn cfg(name: &str) -> OutboundCfg {
        OutboundCfg {
            name: name.into(),
            server_pub: [1u8; 32],
            client_priv: [2u8; 32],
            mux_prefs: vec![MuxId::Yamux],
            session_bases: vec![None],
        }
    }

    fn instance_of(c: OutboundCfg) -> (String, Arc<OutboundInstance>) {
        (c.name.clone(), OutboundInstance::new(c))
    }

    #[test]
    fn unchanged_outbound_is_reused_not_rebuilt() {
        let (name, inst) = instance_of(cfg("A"));
        let old = BTreeMap::from([(name.clone(), inst.clone())]);
        let diff = diff_outbounds(&old, &[cfg("A")]);
        assert!(Arc::ptr_eq(diff.reused.get("A").unwrap(), &inst),
            "字段完全没变时必须原样复用同一个 Arc，不能悄悄换成一个新实例");
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn brand_new_outbound_is_added() {
        let old = BTreeMap::new();
        let diff = diff_outbounds(&old, &[cfg("A")]);
        assert!(diff.reused.is_empty());
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].name, "A");
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn removed_outbound_is_listed_for_teardown() {
        let (name, inst) = instance_of(cfg("A"));
        let old = BTreeMap::from([(name, inst)]);
        let diff = diff_outbounds(&old, &[]);
        assert!(diff.reused.is_empty());
        assert!(diff.added.is_empty());
        assert_eq!(diff.removed, vec!["A".to_string()]);
    }

    #[test]
    fn changed_field_is_treated_as_remove_then_add() {
        let (name, inst) = instance_of(cfg("A"));
        let old = BTreeMap::from([(name, inst)]);
        let mut changed = cfg("A");
        changed.server_pub = [9u8; 32]; // 换了公钥——身份变了
        let diff = diff_outbounds(&old, &[changed]);
        assert!(diff.reused.is_empty(), "字段变了不该被当成复用");
        assert_eq!(diff.removed, vec!["A".to_string()]);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].server_pub, [9u8; 32]);
    }

    #[test]
    fn mixed_scenario_reuse_add_remove_together() {
        let (a_name, a_inst) = instance_of(cfg("A"));
        let (b_name, b_inst) = instance_of(cfg("B"));
        let old = BTreeMap::from([(a_name, a_inst.clone()), (b_name, b_inst)]);
        // A 不变，B 删掉，C 新增
        let diff = diff_outbounds(&old, &[cfg("A"), cfg("C")]);
        assert!(Arc::ptr_eq(diff.reused.get("A").unwrap(), &a_inst));
        assert_eq!(diff.reused.len(), 1);
        assert_eq!(diff.removed, vec!["B".to_string()]);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].name, "C");
    }
}
```

- [ ] **Step 2b：跑测试确认失败**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test --manifest-path src-tauri/Cargo.toml runtime_state
```
Expected: FAIL（`todo!()` 会 panic）。

- [ ] **Step 3：实现 `diff_outbounds`**

```rust
pub fn diff_outbounds(
    old: &BTreeMap<String, Arc<OutboundInstance>>,
    new_cfgs: &[crate::outbound::instance::OutboundCfg],
) -> OutboundDiff {
    let mut reused = BTreeMap::new();
    let mut added = Vec::new();
    let mut seen_names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for c in new_cfgs {
        seen_names.insert(c.name.clone());
        match old.get(&c.name) {
            Some(inst) if cfg_equal(inst.cfg(), c) => {
                reused.insert(c.name.clone(), inst.clone());
            }
            _ => added.push(c.clone()),
        }
    }

    let removed = old
        .keys()
        .filter(|name| {
            // 不在新列表里 → 真删除；在新列表里但没进 reused → 字段变了，
            // 旧的这份也要走停机流程（新的那份已经进了 added）。
            !seen_names.contains(*name) || !reused.contains_key(*name)
        })
        .cloned()
        .collect();

    OutboundDiff { reused, added, removed }
}

fn cfg_equal(
    a: &crate::outbound::instance::OutboundCfg,
    b: &crate::outbound::instance::OutboundCfg,
) -> bool {
    a.name == b.name
        && a.server_pub == b.server_pub
        && a.client_priv == b.client_priv
        && a.mux_prefs == b.mux_prefs
        && a.session_bases == b.session_bases
}
```

（`OutboundCfg` 需要 `PartialEq`/`Clone` 才能这样比较——检查
`src-tauri/src/outbound/instance.rs` 里 `OutboundCfg` 现在的 derive 列表，
若没有 `PartialEq`，在那个文件里给它加上，不要在这个新文件里重新实现
一遍比较逻辑。`MuxId`（`mux_prefs` 的元素类型）需要确认它本身是否已经
`derive(PartialEq)`——多半已经有，因为它在别处也被拿来比较过，读
`wsieve-proto` 确认。）

- [ ] **Step 4：跑测试确认通过**

```bash
cargo test --manifest-path src-tauri/Cargo.toml runtime_state
```
Expected: 全部 PASS。

- [ ] **Step 5：跑全量 Rust 测试确认无回归**

```bash
cargo test -p wsieve-config
cargo test -p wsieve-route
cargo test --manifest-path src-tauri/Cargo.toml
```
Expected: 全部 PASS（新增的 `mod runtime_state;` 只是加了新代码，不该
影响任何既有测试；若 `OutboundCfg` 加 `PartialEq` 触发了别处的重复 derive
或冲突，按实际编译报错处理，不要绕过）。

- [ ] **Step 6：提交**

```bash
git add src-tauri/src/runtime_state.rs src-tauri/src/main.rs \
        src-tauri/src/outbound/instance.rs
git commit -m "feat(runtime): 新增 RuntimeState 快照结构与出站增量 diff 纯函数"
```

---

### Task 2：把承载窗口的建窗逻辑抽成可复用函数

**Files:**
- Modify: `src-tauri/src/main.rs`
- Modify: `src-tauri/src/outbound/carrier.rs`（如果测试更适合放在这里）

**背景**（设计文档 §2.1 原文）：

> 把 `main.rs` 现在建承载窗口那段代码（`for (label, url) in
> carrier.windows() { WebviewWindowBuilder::new(...).build()?; }`）抽成
> 一个可以在启动之后再调用的独立函数（接收 `&AppHandle`）。

这是一个**纯重构**——本 Task 结束时，应用的实际行为（建哪些窗口、什么
参数）与重构前必须完全一致，只是把内联代码挪成一个具名函数，为 Task 3/4
的"启动之后再建新窗口"铺路。

- [ ] **Step 1：读现状**

读 `src-tauri/src/main.rs` 里 `tauri::Builder::default().setup(move |app| {
... })` 闭包内建承载窗口那一段（大致在 `for (label, url) in
carrier.windows() { tauri::webview::WebviewWindowBuilder::new(...)
.title("websieve").inner_size(480.0, 320.0).visible(show_window)
.background_throttling(...).initialization_script(bootstrap::loader_js())
.build()?; }` 附近，具体行号以实际文件为准，可能比写这份计划时的行号
有偏移）。记下这段代码用到的每一个外部变量（`show_window`、
`bootstrap::loader_js()` 等）。

- [ ] **Step 2：抽成函数**

在 `src-tauri/src/outbound/carrier.rs`（这个函数是"按 `CarrierPlan` 建窗"，
放在 `carrier` 模块里比放在 `main.rs` 里更合适，`main.rs` 应该只剩下
"调用它"这一行）末尾加：

```rust
/// 按 `carrier.windows()` 给出的每一个 `(标签, URL)` 建一个隐藏（或按
/// `show_window` 显示）的承载 WebView。
///
/// 可以在 `.setup()` 里调（进程启动时），也可以在启动之后调（运行时
/// 新增出站需要一个之前没有过的窗口时）——两处用的是同一份逻辑，参数
/// 完全一致，不允许出现"启动时建的窗口"和"运行时建的窗口"配置不一致
/// 这种分叉。
///
/// **幂等**：若某个标签对应的窗口已经存在，跳过，不重复建（不返回错误——
/// 调用方在"增量出站更新"场景下，`carrier.windows()` 里混着新旧标签是
/// 正常情况，不该因为窗口已存在就整体失败）。
pub fn spawn_carrier_windows(
    app: &tauri::AppHandle,
    plan: &CarrierPlan,
    show_window: bool,
) -> anyhow::Result<()> {
    for (label, url) in plan.windows() {
        if app.get_webview_window(&label).is_some() {
            continue;
        }
        tauri::webview::WebviewWindowBuilder::new(
            app,
            label,
            tauri::WebviewUrl::External(url.parse()?),
        )
        .title("websieve")
        .inner_size(480.0, 320.0)
        .visible(show_window)
        .background_throttling(tauri_utils::config::BackgroundThrottlingPolicy::Disabled)
        .initialization_script(crate::bootstrap::loader_js())
        .build()?;
    }
    Ok(())
}
```

（`app: &tauri::AppHandle` 而非 `&tauri::App`——`.setup()` 闭包里拿到的是
`&tauri::App`，`App` 实现了 `Manager` trait、有 `.handle()` 方法能拿到
`AppHandle`，两者都满足 `WebviewWindowBuilder::new` 的 trait bound，选
`AppHandle` 是因为运行时（Task 4 触发重建时）手上拿到的就是 `AppHandle`
而不是 `App`——`App` 只在 `.setup()` 那一刻存在。检查
`WebviewWindowBuilder::new` 的实际 trait bound 是不是 `Manager<R>` 而非
具体类型，确认 `&AppHandle` 确实满足；若签名对不上，调整为实际能编译
通过的类型，不要死抠这里给出的字面签名。）

`main.rs` 的 `.setup()` 闭包里，把原来那段内联 `for` 循环替换成：

```rust
outbound::carrier::spawn_carrier_windows(&app.handle().clone(), &carrier, show_window)?;
```

- [ ] **Step 3：验证行为不变**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --manifest-path src-tauri/Cargo.toml
```
Expected: 编译通过。这一步没有自动化测试能验证"真机行为跟重构前一致"
（建窗需要真的起一个 Tauri 应用），**这是本 Task 唯一允许"没有新增自动化
测试、只靠人工代码审查确认逻辑等价"的一步**——审查时逐字对比重构前后
两段代码，确认外部变量（`show_window`、`bootstrap::loader_js()` 等）
全部原样传入，没有遗漏或改值。

- [ ] **Step 4：跑全量测试确认无回归**

```bash
cargo test -p wsieve-config
cargo test -p wsieve-route
cargo test --manifest-path src-tauri/Cargo.toml
```
Expected: 全部 PASS（纯重构，不该有任何测试因此变红）。

- [ ] **Step 5：提交**

```bash
git add src-tauri/src/main.rs src-tauri/src/outbound/carrier.rs
git commit -m "refactor(runtime): 承载窗口建窗逻辑抽成可复用函数，为运行时动态建窗铺路"
```

---

### Task 3：启动流程改造——从读 env 变量到读 `config.yaml`

**Files:**
- Modify: `src-tauri/src/main.rs`
- Modify: `src-tauri/src/bootstrap.rs`（`load_cfg` 的调用方式可能要变，
  具体看 Step 1 的调研结果）

**背景**（设计文档 §2、§2.1 原文，已在计划开头的"关键既有代码"里给出
`Config` 的字段）：

> 用已有的"读取或首次生成默认 `config.yaml`"逻辑加载 `Config`……用
> `Config.proxies` 构建初始 `OutboundManager`（可以是空的）……
> `WSIEVE_SERVER_PUB`/`WSIEVE_CLIENT_PRIV`/`WSIEVE_OUTBOUND_NAME` 这三个
> env 变量的读取整体删除。
>
> 启动时若 `Config.proxies` 为空，完全跳过承载相关的一切（不算
> `CarrierPlan`、不建任何承载窗口）——控制窗口与混合端口入口照常起。

这是本计划里改动面最大的一个 Task，直接改写 `main.rs` 现有的启动序列。
**这个 Task 的实现者必须先完整读一遍当前的 `src-tauri/src/main.rs`
全文**（不只是本计划引用的片段）——这份计划写成时看到的行号/变量名，
到实现时可能因为 Task 1/2 的改动已经不完全对得上，必须以实际文件为准。

- [ ] **Step 1：调研——config_get 背后"读取或首次生成默认配置"的确切函数**

`src-tauri/src/commands/config.rs` 里 `config_get`/`config_get_raw` 都调用
了 `ensure_config_exists(&p)`（在 `config_path(&app)?` 算出的路径上）。
读这个函数的实现，确认它的签名是不是纯粹"给一个 `&Path`，若不存在就写
默认配置"，不依赖 `AppHandle`（如果依赖，说明它内部还做了别的与本 Task
无关的事，需要另外抽一个不依赖 `AppHandle` 的版本，或者直接在 `main.rs`
里传入由 `app.path().app_config_dir()` 算出的路径）。确认之后，`main.rs`
的启动路径要能：
1. 算出 `config.yaml` 的路径（`app_config_dir()`，与 `config_path` 用的
   是同一个目录，二者必须一致，否则控制台编辑的和运行时读到的是两份
   不同的文件）。
2. 调 `ensure_config_exists`（或它的等价物）。
3. 用 `wsieve_config::load_str`（或 `commands/config.rs` 里已经在用的
   同名读取函数——检查 `read_view`/`read_text` 背后具体调了什么）解析出
   `Config`。

- [ ] **Step 2：写一个 Rust 侧的启动构建函数，脱离 `main()` 单测**

新增（可以放进 `runtime_state.rs`，也可以新开一个模块，取决于实现时
`main.rs` 现有代码的组织方式更适合哪种切法——由实现者判断）一个函数：

```rust
/// 从 `Config` 构建初始 `RuntimeState` 需要的各项材料。
///
/// 拆成"纯计算"与"有副作用（建 OutboundInstance、建窗口）"两半，前者可以
/// 脱离 Tauri 单测：给一个 `Config`，算出——
/// - 每个出站的 `OutboundCfg`（含 hex 解码 server_pub/client_priv、
///   mux_prefs 转换、session_bases 的计算——这部分要看 Task 1 之前
///   main.rs 里 `carrier.base_for`/`try_plan_ports` 那段现有逻辑怎么算的，
///   照搬过来，不要重新发明）；
/// - 出站为空时 `CarrierPlan` 是 `None`（不调 `CarrierPlan::build`）；
///   非空时正常调用。
pub struct StartupPlan {
    pub outbound_cfgs: Vec<crate::outbound::instance::OutboundCfg>,
    pub carrier: Option<crate::outbound::carrier::CarrierPlan>,
    pub rule_lines: Vec<String>,
    pub mode: wsieve_route::Mode,
    pub global_outbound: String,
}

pub fn build_startup_plan(config: &wsieve_config::Config) -> anyhow::Result<StartupPlan> {
    todo!("Task 3 Step 3 实现")
}
```

- [ ] **Step 3：先写失败的测试**

给 `build_startup_plan` 写测试（具体断言由实现者根据上面的字段设计，
至少要覆盖）：
- `proxies: []` 时 `outbound_cfgs` 为空、`carrier` 是 `None`。
- 有 1 个 proxy 时，`outbound_cfgs` 长度为 1，`server_pub`/`client_priv`
  被正确从十六进制字符串解码成 `[u8; 32]`（十六进制解析失败要报错，不能
  panic 或悄悄给一个全零数组——这与 `bootstrap.rs` 现有的 `hex32` helper
  是同一条纪律，直接复用那个函数，不要重新写一个解码逻辑）。
- 有 2 个 proxy 且 `carrier: "shared"` 时，`carrier` 是 `Some`，且两个
  出站的 `window_label` 相同（都是 `"main"`）。
- `carrier: "isolated"` 时，两个出站的 `window_label` 不同。
- `mode`/`global_outbound`/`rule_lines` 正确从 `Config` 对应字段搬过来
  （`rules: Vec<Spanned<String>>` 要取 `.value`，不是整个 `Spanned`）。

跑：
```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test --manifest-path src-tauri/Cargo.toml build_startup_plan
```
Expected: FAIL（`todo!()`）。

- [ ] **Step 4：实现 `build_startup_plan`**

由实现者对照 Step 1 的调研结果与 `main.rs` 现有的（Task 2 重构前）计算
`outbound_cfg`/`session_bases`/`carrier` 的那段逻辑，把"给单个 env 变量
出站算这些东西"的逻辑改写成"给 `Config.proxies` 这个列表算这些东西"，
每个出站独立算一遍 `session_bases`（现在的逻辑是单出站，`session_bases`
数组长度是 `extra_sessions + 1`，这部分不变，只是要对列表里每一项都
算一遍，而不是只对唯一那一个）。

**`carrier_host` 字段**：`Config.carrier_host` 对应现在 env 路径里
`CarrierPlan::build` 的 `host: &str` 参数（现在传的是空串`""`，表示"未
指定，取第一个"）——直接把 `Config.carrier_host` 传进去，空串时行为不变
（`CarrierPlan::build` 内部已经处理"空串=未指定"的语义）。

- [ ] **Step 5：跑测试确认通过**

```bash
cargo test --manifest-path src-tauri/Cargo.toml build_startup_plan
```
Expected: 全部 PASS。

- [ ] **Step 6：把 `build_startup_plan` 接进 `main()`，删除 env 变量启动路径**

这一步没有独立的自动化测试能覆盖（`main()` 本身不适合单测），验证手段是
Step 8 的手动真机验证 + Step 7 的编译通过 + 既有测试不回归。改动要点：

1. 删除 `bootstrap::load_cfg()` 里读 `WSIEVE_SERVER_PUB`/
   `WSIEVE_CLIENT_PRIV`/`WSIEVE_OUTBOUND_NAME` 的部分（其余 env 变量如
   `WSIEVE_SOCKS`/`WSIEVE_MUX_PREFS`/`WSIEVE_SHARD_BASE_PORT`/
   `WSIEVE_SHOW_WINDOW` 保留——设计文档 §2 明确这些不删）。检查
   `bootstrap::load_cfg` 返回的 `AppConfig` 结构体，把 `server_pub`/
   `client_priv` 这两个字段删掉（连带调用方的解构一起改），或者如果
   `AppConfig` 里其余字段还有用（`socks_listen`/`show_window`/
   `mux_prefs`/`shard_base_port`），保留结构体本身，只删这两个字段。
2. `main()` 里读 `Config`（Step 1 的方式）、调 `build_startup_plan`。
3. `outbound_cfg`（原来单数）改成 `outbound_cfgs`（复数，来自
   `StartupPlan`），后续所有"给单个出站算 session_bases/carrier"的代码
   改成对列表遍历（或者已经在 `build_startup_plan` 里算完了，`main()`
   只需要拿现成结果）。
4. `carrier.windows()` 建窗那段（Task 2 已经抽成
   `spawn_carrier_windows`）只在 `carrier: Some(plan)` 时调用；`None`
   时跳过，不建任何承载窗口，也不报错。
5. `run_stack` 函数签名要从"单个出站"改造成"一批出站"——具体怎么改
   （是整体传 `Vec<OutboundCfg>` 还是别的形式）由实现者对照 `run_stack`
   现有实现判断，本计划不预先规定内部实现细节，但必须满足：
   - `proxies: []` 时 `run_stack` 能正常跑起来（`OutboundManager`/
     `Router` 内部出站表为空，`start_all()` 是空操作，不报错）。
   - 有出站时，行为与改造前（单出站）等价，只是从"恒定一个"变成"可以
     是任意多个"。
6. 控制窗口（`control::open`）与混合端口入口的建立不依赖出站是否存在，
   确认这两处代码路径本来就没有隐含"至少一个出站"的假设（读代码确认，
   不要凭感觉）。

- [ ] **Step 7：编译 + 跑全量测试确认无回归**

```bash
cargo build --manifest-path src-tauri/Cargo.toml
cargo test -p wsieve-config
cargo test -p wsieve-route
cargo test --manifest-path src-tauri/Cargo.toml
```
Expected: 编译通过，全部测试 PASS。

- [ ] **Step 8：手动验证（不使用 windows-control MCP 工具，只用 bash 检查
  进程/日志）**

```bash
# 用一份 proxies: [] 的 config.yaml 启动，不再需要任何 WSIEVE_* 环境变量
./src-tauri/target/debug/wsieve-app.exe &
sleep 3
tasklist //FI "IMAGENAME eq wsieve-app.exe"   # 进程应该还活着，没有因为零出站而退出
```
Expected: 进程正常启动并保持运行（不再需要 `WSIEVE_SERVER_PUB` 等三个
环境变量），说明零出站启动路径确实可用。这一步验证完毕后
`taskkill //F //IM wsieve-app.exe` 关掉测试用的进程，不要留着占端口。

- [ ] **Step 9：提交**

```bash
git add src-tauri/src/main.rs src-tauri/src/bootstrap.rs src-tauri/src/runtime_state.rs
git commit -m "feat(runtime): 启动流程改为读 config.yaml，支持零出站启动，删除 env 变量出站引导"
```

---

### Task 4：把 `RuntimeState` 托管进 Tauri 状态，`config_save` 系列命令触发重建

**Files:**
- Modify: `src-tauri/src/main.rs`
- Modify: `src-tauri/src/commands/config.rs`
- Modify: `src-tauri/src/runtime_state.rs`

**背景**（设计文档 §1、§3、数据流示意原文）：

> 用 `RwLock<Arc<RuntimeState>>` 托管在 Tauri 状态里……每次配置保存后，
> 后台整体构建一份新的 `RuntimeState`，写锁替换指针……
>
> `config_save`（行级定点改写）→ 写盘前校验 → 写盘成功 → 新增步骤：从
> 磁盘重新读取 `Config`，与当前 `RuntimeState` 对比 → 出站增删改走
> `diff_outbounds`，rules/mode 整体重建 `RuleSet` → 打包成新
> `RuntimeState` → 写锁替换……

这个 Task 把 Task 1（diff 逻辑）、Task 2（动态建窗）、Task 3（启动时
构建初始状态）三者串起来，接进真正的保存流程。

- [ ] **Step 1：`main()` 里托管 `RwLock<Arc<RuntimeState>>`**

Task 3 结束时，`main()` 已经能从 `Config` 构建出初始的
`RuleSet`/`OutboundManager`/`Router`。把这三者打包成 `RuntimeState`
（`groups` 字段先用 `Arc::new(GroupTable::default())` 占位），
`app.manage(std::sync::RwLock::new(Arc::new(initial_state)))`——**只调用
一次**，在 `.setup()` 闭包里，不要在后续任何地方重复 `.manage()`（这是
设计文档 §1 特意点名要避开的坑：`CurrentCore` 现在的写法就是反复
`.manage()`，导致后续读到的永远是第一代——新代码不能重蹈覆辙）。

- [ ] **Step 2：写一个"重建并替换"的共享函数**

```rust
// src-tauri/src/runtime_state.rs

/// 从磁盘重新读取配置、按 §3 的增量规则重建 `RuntimeState`、原子替换
/// 托管状态里的那份。
///
/// **失败时保留旧快照，不替换**（设计文档"错误处理"一节）——写盘前的
/// `validate()` 已经挡住了绝大多数非法配置，这里的失败应该只发生在
/// "写盘和重读之间文件被外部改坏"这种边缘情况，此时让运行时继续用旧的
/// 那份、只记一条错误日志，比强行换成一个不完整的半成品安全。
pub async fn rebuild_and_swap(app: &tauri::AppHandle) -> anyhow::Result<()> {
    todo!("Task 4 Step 3 实现")
}
```

- [ ] **Step 3：先写失败的测试，再实现**

`rebuild_and_swap` 依赖 `AppHandle`（读配置路径、建承载窗口都需要），
不能像 Task 1/3 那样完全脱离 Tauri 单测。**这里允许测试覆盖面缩小到
"纯计算部分"**：把"从旧 `RuntimeState` + 新 `Config` 算出新
`RuntimeState` 该长什么样"这一段拆成一个不依赖 `AppHandle` 的子函数
（吃 `&RuntimeState` 旧快照 + `&Config` 新配置 + 一个"建新出站实例/建
承载窗口"的回调/trait，回调本身在真实调用时才接 Tauri，测试时传一个
记录调用次数的假实现），对这个子函数写单元测试覆盖"出站增删改是否真的
调用了正确次数的建/停"，具体怎么切这个边界由实现者判断（这是本计划
里唯一没有给出具体测试代码的一步，因为切法依赖 Task 3 实际落地后
`RuntimeState`/`StartupPlan` 的确切形状，写这份计划时无法预判）。至少
要保证：**这个子函数本身有单元测试**，`rebuild_and_swap` 这个"胶水层"
（读文件、拿锁、调子函数、换指针）允许只靠 Step 5 的手动验证 + 编译期
类型检查兜底，不强求覆盖它本身的每一行。

- [ ] **Step 4：接进四个命令**

`src-tauri/src/commands/config.rs` 的 `config_save`/`config_save_raw`/
`config_insert_proxy`/`config_delete_proxy` 四个 `#[tauri::command]`
函数，在各自现有的写盘逻辑成功之后，都要调
`crate::runtime_state::rebuild_and_swap(&app).await`。**重建失败不应该让
这次保存本身报错给用户**——保存到磁盘这个动作已经成功了（文件是对的），
重建运行时状态失败是另一个层面的问题（现有旧状态继续用），只记日志，
`CmdResult<()>` 仍然返回 `Ok(())`。这一点要在代码里用注释写清楚，否则
容易被后面的人"改成失败也要给用户报错"，那样会把"保存成功但运行时暂时
没跟上"这种可恢复状况，升级成"保存失败"这种更严重的误报。

- [ ] **Step 5：手动验证**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --manifest-path src-tauri/Cargo.toml
```
Expected: 编译通过。真机行为验证（新增/删除出站后运行时是否真的增/减
了对应的会话循环）留给 Part 1 全部完成后的一次性真机走查，不在这一个
Task 里单独做（避免同一件事被走查两次）。

- [ ] **Step 6：跑全量测试**

```bash
cargo test -p wsieve-config
cargo test -p wsieve-route
cargo test --manifest-path src-tauri/Cargo.toml
```
Expected: 全部 PASS。

- [ ] **Step 7：提交**

```bash
git add src-tauri/src/main.rs src-tauri/src/commands/config.rs src-tauri/src/runtime_state.rs
git commit -m "feat(runtime): config_save 系列命令写盘成功后触发 RuntimeState 增量重建"
```

---

## 收尾

### Task 5：Part 1 全量验证

**Files:** 无新文件。

- [ ] **Step 1：全量 Rust 测试**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test -p wsieve-config
cargo test -p wsieve-route
cargo test --manifest-path src-tauri/Cargo.toml
```
Expected: 全部 PASS。

- [ ] **Step 2：编译**

```bash
cargo build --manifest-path src-tauri/Cargo.toml
```
Expected: 成功，产出新的 `wsieve-app.exe`。若此刻有用户自己在跑的
`wsieve-app.exe` 进程占着文件锁，不要杀它，只确认编译（不含链接）本身
没有报错，链接推迟到那个进程结束后再验证一次。

- [ ] **Step 3：更新走查清单**

在 `docs/superpowers/plans/2026-09-07-live-walkthrough-checklist.md`
末尾补充几条新清单项：
- 用一份 `proxies: []` 的全新配置启动，进程不再需要任何 `WSIEVE_*`
  环境变量，控制窗口正常打开。
- 在「出站」视图添加第一个服务器后，等待几秒，确认应用没有崩溃/没有
  报错弹窗（这是"运行时第一次动态建承载窗口"这条路径第一次被真机走查
  到）。
- 删除唯一的出站后再添加回来，确认新添加的还能正常工作（复用同一个
  承载窗口的路径）。

- [ ] **Step 4：确认没有遗留的未提交改动**

```bash
git status
```
Expected: working tree clean（除已知的 `.shots/`、`scripts/*.ps1`、
`src-tauri/gen/schemas/windows-schema.json` 等既定不提交项）。

- [ ] **Step 5：给团队/用户的收尾说明**

Part 1 完成后，`main.rs` 已经不再需要 `WSIEVE_SERVER_PUB`/
`WSIEVE_CLIENT_PRIV`/`WSIEVE_OUTBOUND_NAME` 这三个环境变量——之后启动
真机走查时，直接用一份写好 `proxies:` 的 `config.yaml` 启动即可，不需要
再拼十六进制占位密钥。这一点要在完成汇报里向用户说清楚，因为它改变了
本 session 前面几轮真机走查时用的启动方式。

Part 2（代理组解析接入路由 + 出站延迟埋点）、Part 3（`NotReady` 命令
接活 + 首页连接总开关）、Part 4（流量统计 + 连接状态事件）、Part 5
（Windows 系统代理）留到 Part 1 实际完成、代码落地之后再各自写计划——
它们都要用到本计划产出的 `RuntimeState`/`rebuild_and_swap` 的确切签名，
现在预先写死细节风险是"写的时候是对的，Part 1 实现过程中细节一变就
过时"。
