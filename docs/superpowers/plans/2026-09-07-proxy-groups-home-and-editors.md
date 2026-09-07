# 代理组、首页与可视化编辑器 — 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: 用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务执行本计划。步骤用复选框（`- [ ]`）追踪。

**Goal:** 在控制窗口里加入代理组（select/auto/load-balance）机制、一个整合式首页、
以及节点/规则的可视化新增-编辑-删除表单，全部落点为「配置 schema + 纯逻辑 +
落盘持久化 + UI 呈现」——不接入尚未消费 config.yaml 的 main.rs 运行时。

**Architecture:** 后端在 `wsieve-config`（schema/校验/定点改写原语）与
`wsieve-route`（组解析纯逻辑）两个既有 crate 上做加法，不改动 `Mode`/
`engine.rs` 的既有语义。前端在既有 `App.svelte` 状态与 `config-map.js` 编辑器
之上加新视图/新表单，复用已经做好的 `routingPreset`/`saveRoutingPreset`、
`Segmented`、`EmptyState` 等构件。

**Tech Stack:** Rust（`wsieve-config` / `wsieve-route` / `wsieve-app` 三个 crate）、
Svelte 5 runes、vitest、cargo test。

**依据:** `docs/superpowers/specs/2026-09-07-proxy-groups-and-home-view-design.md`

---

## 前置阅读（实现者必读）

- **本计划建立在本会话之前已完成的工作之上**：`config.yaml` 首次缺失时自动
  创建默认配置（`ensure_config_exists`）、默认端口 `25500`、`mode`/`rule-preset`
  分流预设（`全局直连/全局代理/中国大陆/规则`，`RulesView.svelte` 顶部的
  segmented control，`App.svelte` 的 `routingPreset`/`saveRoutingPreset`）。
  这些已经在 `feat/routing-and-control-ui` 分支上，本计划直接复用，不重做。
- **诚实边界，逐任务都要记住**：`main.rs` 目前完全不读 `config.yaml`，本计划
  新增的一切在写完之后**都还不会影响真实代理流量**——这是设计已经定好的
  范围边界，不是本计划的疏漏。落盘、纯逻辑、UI 呈现今天就是真实、可测的。
- **`crates/wsieve-config/src/edit.rs` 的既有纪律必须原样延续**：任何写回都要
  「读得回来、其余字节不变、验不过就整体回滚」。写新函数前**先读一遍该文件
  开头到 `split_item`/`find_comment_start`/`check_index`/`split_eol`/
  `indent_width`/`find_dash` 这几个私有辅助函数**——新函数要复用它们，不要
  重新发明。

---

## 文件结构

```
crates/wsieve-config/
  src/model.rs          修改：新增 ProxyGroup 结构体、Config.proxy_groups 字段
  src/lib.rs             修改：validate() 新增 proxy-groups 校验
  src/edit.rs             修改：新增 insert_rule_line / append_proxy_block /
                          delete_proxy_block 三个定点改写原语

crates/wsieve-route/
  src/group.rs            新建：auto_pick / load_balance_pick 两个纯函数 +
                          LbStrategy（resolve_group/select_pick/ResolveCtx
                          推迟——理由见 Task 3 开头，wsieve-route 不依赖
                          wsieve-config，且今天没有调用点）
  src/lib.rs              修改：导出 group 模块

src-tauri/src/commands/
  config.rs               修改：RuleOp::InsertRule、ConfigView.rules_key_line、
                          apply_rule_ops 补 Rule::parse 校验与 Insert 分支、
                          新命令 config_insert_proxy / config_delete_proxy
  mod.rs                  修改（如需要）：新命令导出
src-tauri/src/main.rs     修改：generate_handler! 里注册两个新命令
src-tauri/capabilities/control.json  修改：两个新命令加进权限列表

ui/src/lib/
  config-map.js           修改：新增 setGroupSelected / defaultInsertAnchor /
                          ruleTypeToFormType
  config-map.test.js      修改：对应测试
  ipc.js                  修改：新增 configInsertProxy / configDeleteProxy 绑定

ui/src/views/
  HomeView.svelte         新建：首页四张卡片
  HomeView.test.js        新建
  RuleForm.svelte         新建：规则新增/编辑共用表单
  RuleForm.test.js        新建
  ProxyForm.svelte        新建：节点新增表单（含私钥掩码输入）
  ProxyForm.test.js       新建
  RulesView.svelte        修改：接入新增/编辑/删除按钮与 RuleForm
  RulesView.test.js       修改：对应测试
  OutboundsView.svelte    修改：接入新增/删除按钮与 ProxyForm
  OutboundsView.test.js   修改：对应测试

ui/src/App.svelte         修改：view 默认值改 'home'、Segmented 选项加首页、
                          HomeView/RuleForm/ProxyForm 相关状态与保存函数、
                          proxy-groups 相关 $derived
ui/src/App.test.js        修改：对应测试
```

---

## Part A — 配置 schema 与组解析纯逻辑

### Task 1: `ProxyGroup` 模型

**Files:**
- Modify: `crates/wsieve-config/src/model.rs`

- [ ] **Step 1: 加 `ProxyGroup` 结构体与 `Config.proxy_groups` 字段**

在 `model.rs` 里 `Proxy` 结构体定义之后（约第 103 行 `default_mux_prefs` 函数
之后）插入：

```rust
/// 代理组（设计文档「代理组与首页视图」§1）。
///
/// 只用 `name` 做唯一标识，不单独设 `id`——`selected` 字段写在组自己的
/// YAML 块里，不是外部按键索引的状态，改名不会打断任何引用（见设计文档
/// §1「只用 name」一节的完整论证）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ProxyGroup {
    pub name: String,
    pub kind: String,
    pub proxies: Vec<String>,
    /// 仅 `select` 类型使用；其余类型省略时为空字符串。
    #[serde(default)]
    pub selected: String,
    /// 仅 `load-balance` 类型使用；其余类型省略时为空字符串。
    #[serde(default)]
    pub strategy: String,
}
```

然后在 `Config` 结构体的「── 出站 ──」段（`pub proxies: Vec<Proxy>,` 那一行）
之后加一行：

```rust
    pub proxy_groups: Vec<ProxyGroup>,
```

最后在 `impl Default for Config` 的 `Self { ... }` 里，`proxies: Vec::new(),`
那一行之后加：

```rust
            proxy_groups: Vec::new(),
```

- [ ] **Step 2: 编译确认**

Run: `cargo build -p wsieve-config`
Expected: 编译通过（`Config` 派生的 `Deserialize`/`Serialize` 会自动处理新字段，
`#[serde(default)]` 在 struct 级别已经覆盖，省略 `proxy-groups` 键时得到空数组）。

- [ ] **Step 3: 写默认值测试**

在 `crates/wsieve-config/src/lib.rs` 的 `#[cfg(test)] mod tests` 里找到
`omitted_fields_get_defaults` 测试（`assert_eq!(c.rule_preset, "custom");` 那一行
附近），在它后面加一行：

```rust
        assert!(c.proxy_groups.is_empty(), "省略 proxy-groups 时应为空数组");
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p wsieve-config omitted_fields_get_defaults`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-config/src/model.rs crates/wsieve-config/src/lib.rs
git commit -m "feat(config): ProxyGroup 模型，只用 name 做唯一标识"
```

---

### Task 2: `proxy-groups` 的 schema 校验

**Files:**
- Modify: `crates/wsieve-config/src/lib.rs`

- [ ] **Step 1: 写失败的测试**

在 `#[cfg(test)] mod tests` 里，紧接着 `validate_rejects_bogus_carrier_log_level_and_rule_preset`
测试之后加一组新测试：

```rust
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p wsieve-config validate_rejects_a_duplicate_group_name`
Expected: 编译失败——`ConfigError::DuplicateProxyGroupName` 等变体还不存在。

- [ ] **Step 3: 在 `ConfigError` 里加新变体**

找到 `crates/wsieve-config/src/lib.rs` 里 `ConfigError` 枚举定义（`DuplicateProxyName`
所在的那个枚举），在它旁边加四个新变体：

```rust
    #[error("代理组名 {0:?} 重复——组名必须唯一，UI 靠它定位该改写哪个组块")]
    DuplicateProxyGroupName(String),
    #[error("代理组名 {0:?} 与一个出站名重复——组名与出站名共享同一个命名空间，规则引用时才不会混淆")]
    GroupNameCollidesWithOutbound(String),
    #[error("代理组 {group:?} 的成员 {member:?} 不是一个已存在的出站名（也不允许引用另一个组）")]
    UnknownGroupMember { group: String, member: String },
    #[error("代理组 {group:?} 的 selected 是 {selected:?}，但它不在自己的 proxies 成员列表里")]
    SelectedNotAMember { group: String, selected: String },
```

- [ ] **Step 4: 在 `validate()` 里实现校验逻辑**

紧接着 `check_enum("rule-preset", ...)` 那一段（Task 之前已加）之后，`carrier`
校验之前插入：

```rust
        {
            let mut seen_groups: HashSet<&str> = HashSet::new();
            let outbound_names = self.outbound_names();
            for g in &self.proxy_groups {
                if !seen_groups.insert(g.name.as_str()) {
                    return Err(ConfigError::DuplicateProxyGroupName(g.name.clone()));
                }
                if outbound_names.contains(g.name.as_str()) {
                    return Err(ConfigError::GroupNameCollidesWithOutbound(g.name.clone()));
                }
                check_enum(
                    "proxy-groups[].kind",
                    &g.kind,
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
                if g.kind == "select" && !g.proxies.iter().any(|m| m == &g.selected) {
                    return Err(ConfigError::SelectedNotAMember {
                        group: g.name.clone(),
                        selected: g.selected.clone(),
                    });
                }
                if g.kind == "load-balance" {
                    check_enum(
                        "proxy-groups[].strategy",
                        &g.strategy,
                        &["consistent-hash", "round-robin"],
                        "consistent-hash / round-robin",
                    )?;
                }
            }
            // 组名互相之间也不能重复（第二个组名与第一个组名相同的场景已被
            // seen_groups 挡住；这里再补一条组名之间不能与任一其他组名相同——
            // 与上面的 seen_groups 是同一次遍历完成的，无需额外循环）。
        }
```

**注意成员校验发生在 `seen_groups` 只累积了「已经看过的组名」时**——
`外层组` 引用 `内层组` 这个测试之所以能通过，是因为 `outbound_names()`
只包含 `proxies:` 里的出站名，从不包含任何组名，所以引用一个组名必然落进
`UnknownGroupMember`，不需要额外判断「这个名字是不是另一个组」。

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test -p wsieve-config validate_rejects validate_accepts_a_well_formed_select_group`
Expected: 全部 PASS（8 个新测试）

- [ ] **Step 6: 提交**

```bash
git add crates/wsieve-config/src/lib.rs
git commit -m "feat(config): proxy-groups 的 schema 校验（重名/成员存在性/select 与 load-balance 各自的约束）"
```

---

### Task 3: `wsieve-route/src/group.rs` — `auto_pick` 与 `load_balance_pick`

**Files:**
- Create: `crates/wsieve-route/src/group.rs`
- Modify: `crates/wsieve-route/src/lib.rs`

**规划阶段发现的一处调整，先说清楚**：设计文档 §2 原本还想要一个
`resolve_group(name, groups: &[ProxyGroup], ctx) -> String`，把「规则判决出的
名字是不是某个组、是的话展开成哪个物理出站」也做成 `wsieve-route` 里的纯函数。
但核实 `Cargo.toml` 后发现 `wsieve-route` **不依赖** `wsieve-config`
（`RuleSet::build` 自己也只吃 `&[String]` / `&HashSet<String>` 这类原始类型，
从不直接认识 `wsieve_config::Config`，是同一条解耦纪律）——`resolve_group`
要接 `&[ProxyGroup]` 就得新增一条 `wsieve-route → wsieve-config` 的依赖边，
而这层「组名展开」本质上是**胶水代码**（既要懂规则判决、又要懂配置 schema），
且今天没有任何调用点（main.rs 不消费 config.yaml）。本计划把它**推迟**——
不是砍掉，是等真正接线那天，按那时的调用方所在的 crate（大概率是
`wsieve-app`）来写，避免为一个零调用点的函数决定它该长在哪个 crate。
`select_pick` 同理：它就是 `|selected| selected`，不值得单独包一层，
调用方直接读 `selected` 字段即可。

本 Task 只落地两个真正有逻辑、且本身可以完全脱离 `wsieve-config` 类型、
纯靠原始类型（`Vec<String>` 等）表达的函数。

- [ ] **Step 1: 写失败的测试**

`crates/wsieve-route/src/group.rs`：

```rust
//! 代理组的挑选逻辑（设计文档「代理组与首页视图」§2）。
//!
//! **不依赖 `wsieve-config`**——与 `RuleSet::build` 只吃 `&[String]` /
//! `&HashSet<String>` 是同一条解耦纪律：本 crate 只认识「规则语法」与
//! 「一组带延迟的候选名字」，不认识 YAML schema。调用方（未来真正接线时）
//! 自己把 `ProxyGroup` 拆成这里要的原始类型。
//!
//! **暂时没有运行时调用点**——main.rs 还不消费 config.yaml，与
//! `rule::CHINA_PRESET_RULES` 同一处境。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LbStrategy {
    ConsistentHash,
    RoundRobin,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(name: &str, latency: Option<u64>) -> (String, Option<u64>) {
        (name.to_string(), latency)
    }

    #[test]
    fn auto_pick_chooses_the_lowest_latency() {
        let members = [m("a", Some(80)), m("b", Some(30)), m("c", Some(120))];
        assert_eq!(auto_pick(&members), "b");
    }

    #[test]
    fn auto_pick_ignores_unmeasured_members() {
        let members = [m("a", None), m("b", Some(30)), m("c", None)];
        assert_eq!(auto_pick(&members), "b");
    }

    #[test]
    fn auto_pick_falls_back_to_the_first_member_when_nothing_is_measured() {
        // 全 None 时给一个可预测的默认值，而不是让调用方处理「挑不出」。
        let members = [m("a", None), m("b", None)];
        assert_eq!(auto_pick(&members), "a");
    }

    #[test]
    fn auto_pick_breaks_ties_by_the_first_minimum() {
        let members = [m("a", Some(50)), m("b", Some(50))];
        assert_eq!(auto_pick(&members), "a");
    }

    #[test]
    fn load_balance_consistent_hash_is_stable_for_the_same_key() {
        let members = ["a".to_string(), "b".to_string(), "c".to_string()];
        let mut rr = 0usize;
        let first = load_balance_pick(&members, LbStrategy::ConsistentHash, "example.com", &mut rr);
        let second = load_balance_pick(&members, LbStrategy::ConsistentHash, "example.com", &mut rr);
        assert_eq!(first, second, "同一个目标 host 每次都该落到同一个成员上");
    }

    #[test]
    fn load_balance_consistent_hash_does_not_touch_the_round_robin_counter() {
        let members = ["a".to_string(), "b".to_string()];
        let mut rr = 0usize;
        load_balance_pick(&members, LbStrategy::ConsistentHash, "x.com", &mut rr);
        load_balance_pick(&members, LbStrategy::ConsistentHash, "y.com", &mut rr);
        assert_eq!(rr, 0, "consistent-hash 不消耗 round-robin 状态");
    }

    #[test]
    fn load_balance_round_robin_cycles_through_every_member() {
        let members = ["a".to_string(), "b".to_string(), "c".to_string()];
        let mut rr = 0usize;
        let picks: Vec<String> = (0..5)
            .map(|_| load_balance_pick(&members, LbStrategy::RoundRobin, "irrelevant", &mut rr))
            .collect();
        assert_eq!(picks, vec!["a", "b", "c", "a", "b"], "轮询应严格按顺序回绕");
    }

    #[test]
    fn load_balance_round_robin_starts_fresh_from_whatever_counter_it_is_given() {
        let members = ["a".to_string(), "b".to_string()];
        let mut rr = 3usize; // 调用方可能带着上一次的状态进来
        let pick = load_balance_pick(&members, LbStrategy::RoundRobin, "x", &mut rr);
        assert_eq!(pick, "b"); // 3 % 2 == 1
        assert_eq!(rr, 4);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p wsieve-route group::`
Expected: 编译失败——`auto_pick`/`load_balance_pick` 还不存在。

- [ ] **Step 3: 写实现**

在测试模块之前（`#[cfg(test)]` 之前）插入：

```rust
/// auto：挑延迟最小的非 `None` 成员。全 `None`（一个都没测过延迟）时
/// 退回第一个成员——给一个可预测的默认值，而不是让调用方处理「挑不出」。
///
/// `members` 为空时 panic：一个零成员的组本该在配置校验阶段就被拒绝
/// （`wsieve-config` 的 `validate()`），这里把「非空」当成调用方保证的前提，
/// 而不是再报一次错——两层各管一段，不重复。
pub fn auto_pick(members: &[(String, Option<u64>)]) -> &str {
    members
        .iter()
        .filter(|(_, lat)| lat.is_some())
        .min_by_key(|(_, lat)| lat.unwrap())
        .or_else(|| members.first())
        .map(|(name, _)| name.as_str())
        .expect("空成员列表——上游 validate() 应已挡住零成员的组")
}

/// load-balance：
///   `ConsistentHash` 对 `key`（目标 host）取哈希取模，同一 host 稳定落在
///   同一个成员上，不消耗 `rr_counter`。
///   `RoundRobin` 用调用方传入的可变计数器递增取模，`rr_counter` 的初值
///   由调用方决定（可以是每次从 0 开始，也可以是上一次留下的状态）。
pub fn load_balance_pick(
    members: &[String],
    strategy: LbStrategy,
    key: &str,
    rr_counter: &mut usize,
) -> String {
    assert!(!members.is_empty(), "空成员列表——上游 validate() 应已挡住零成员的组");
    match strategy {
        LbStrategy::ConsistentHash => {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            key.hash(&mut hasher);
            let idx = (hasher.finish() as usize) % members.len();
            members[idx].clone()
        }
        LbStrategy::RoundRobin => {
            let idx = *rr_counter % members.len();
            *rr_counter += 1;
            members[idx].clone()
        }
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p wsieve-route group::`
Expected: 8 个测试全部 PASS

- [ ] **Step 5: 导出模块**

`crates/wsieve-route/src/lib.rs`：找到 `pub mod rule;` 那一行，之后加一行：

```rust
pub mod group;
```

再找到 `pub use rule::{...}` 那一行，之后加一行：

```rust
pub use group::LbStrategy;
```

- [ ] **Step 6: 编译确认**

Run: `cargo build -p wsieve-route`
Expected: 编译通过

- [ ] **Step 7: 提交**

```bash
git add crates/wsieve-route/src/group.rs crates/wsieve-route/src/lib.rs
git commit -m "feat(route): 代理组的 auto/load-balance 挑选逻辑（纯函数，暂无运行时调用点）"
```

---

## Part B — 规则语法校验加固 + 插入原语

### Task 4: `apply_rule_ops` 补一次真正的规则语法校验

**Files:**
- Modify: `src-tauri/src/commands/config.rs`

**这是一个需要补的真实缺口，不是「复用现成校验」**——规划阶段核对过：
`apply_rule_ops`（`ReplaceRule`/`DeleteRule` 的实现）今天完全不校验规则语法，
只跑 `wsieve_config::load_str(&out)?.validate()?`，而 `wsieve-config` 按设计
只把规则当不透明字符串。也就是说今天往 `ReplaceRule` 的 `value` 里塞一段
语法错误的文本会被原样写进文件、不报任何错。`wsieve-app` 的 `Cargo.toml`
已经依赖 `wsieve-route`（main.rs 用它构建启动时的 RuleSet），只是
`commands/config.rs` 还没 `use` 过它。

- [ ] **Step 1: 写失败的测试**

在 `src-tauri/src/commands/config.rs` 的 `#[cfg(test)] mod tests` 里，找到
`structured_save_preserves_every_comment` 测试附近，加一组新测试：

```rust
    #[test]
    fn structured_save_rejects_a_syntactically_invalid_rule_value() {
        let d = tmpdir("bad-rule-syntax");
        let p = d.join("config.yaml");
        save_text(&p, SAMPLE).unwrap();

        let before = std::fs::read_to_string(&p).unwrap();
        let ops = vec![RuleOp::ReplaceRule {
            line: 13, // SAMPLE 里 "GEOSITE,cn,DIRECT" 那一行
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
    }

    #[test]
    fn structured_save_still_accepts_a_syntactically_valid_rule_value() {
        // 补校验不能误伤合法值——这条守住「加固」没有变成「更严格到拒绝正常输入」。
        let d = tmpdir("good-rule-syntax");
        let p = d.join("config.yaml");
        save_text(&p, SAMPLE).unwrap();

        let ops = vec![RuleOp::ReplaceRule {
            line: 13,
            expect: "GEOSITE,cn,DIRECT".to_string(),
            value: "GEOSITE,private,DIRECT".to_string(),
        }];
        apply_rule_ops(&p, ops).unwrap();
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --bin wsieve-app structured_save_rejects_a_syntactically_invalid_rule_value`
Expected: FAIL——`apply_rule_ops` 目前照单全收，`before == after` 的断言会通过
但 `matches!(err, CmdError::ConfigInvalid { .. })` 会因为 `unwrap_err()` 在一个
`Ok(())` 上 panic 而失败。

- [ ] **Step 3: 加校验**

在 `apply_rule_ops` 函数顶部加一个 `use` （文件顶部已有的 `use super::{...}`
那一行下面）：

```rust
use wsieve_route::Rule;
```

然后在 `apply_rule_ops` 里，「并发校验」那个 `for op in &ops` 循环**之后**、
`let mut out = text;` 那一行**之前**插入一段新的循环，对将要写入的每个值
跑语法校验：

```rust
    // 语法校验：DeleteRule 不产生新内容，不需要过这一关。
    for op in &ops {
        if let RuleOp::ReplaceRule { value, .. } | RuleOp::InsertRule { value, .. } = op {
            if let Err(e) = Rule::parse(value) {
                return Err(CmdError::ConfigInvalid {
                    message: format!("{value:?} 不是一条合法规则：{e}"),
                });
            }
        }
    }
```

（此刻 `RuleOp::InsertRule` 变体还不存在，编译会报错——留到 Task 5 补上变体
后再解决，这是本 Task 与下一个 Task 之间刻意的临时不一致，Step 4 会先注释掉
`InsertRule` 分支跑通本 Task，Task 5 再解开。）

**更正**：为了让本 Task 独立可编译可测，上面那段先只写 `ReplaceRule`：

```rust
    for op in &ops {
        if let RuleOp::ReplaceRule { value, .. } = op {
            if let Err(e) = Rule::parse(value) {
                return Err(CmdError::ConfigInvalid {
                    message: format!("{value:?} 不是一条合法规则：{e}"),
                });
            }
        }
    }
```

Task 5 会把这里的 `if let` 换成 `match`，同时覆盖 `InsertRule`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --bin wsieve-app structured_save_rejects_a_syntactically_invalid_rule_value structured_save_still_accepts_a_syntactically_valid_rule_value`
Expected: 2 个测试 PASS

- [ ] **Step 5: 跑一遍全量 config 测试确认没有回归**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --bin wsieve-app commands::config::`
Expected: 全部 PASS（不应该有任何既有测试因为这条新校验被误伤——SAMPLE 与
既有测试用的值都是合法规则语法）

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/commands/config.rs
git commit -m "fix(ipc): config_save 补一次真正的规则语法校验（此前语法错误会被原样写进文件）"
```

---

### Task 5: `insert_rule_line` 原语 + `RuleOp::InsertRule` + `ConfigView.rules_key_line`

**Files:**
- Modify: `crates/wsieve-config/src/edit.rs`
- Modify: `src-tauri/src/commands/config.rs`

**先处理一个必须解决的边界情况**：默认生成的配置（本会话之前的
`ensure_config_exists`）长这样——`rules: []`，空的**流式**序列。用户点击
「添加第一条规则」时，没有任何已有规则的行号可以当插入锚点。因此
`insert_rule_line` 的锚点**既可以是一条已有规则的行，也可以是 `rules:` 键
本身所在的行**——后者处理「一条规则都没有」与「把空流式序列展开成块式」
两种情况。为了让前端总能拿到一个可用的锚点，`ConfigView` 新增
`rules_key_line: u64` 字段。

- [ ] **Step 1: 写 `insert_rule_line` 的失败测试**

在 `crates/wsieve-config/src/edit.rs` 的 `#[cfg(test)] mod tests` 里找到
`SRC` 常量附近，加一组新测试：

```rust
    #[test]
    fn insert_after_an_existing_rule() {
        let src = "rules:\n  - MATCH,DIRECT\n";
        let out = insert_rule_line(src, 2, "GEOSITE,cn,DIRECT").unwrap();
        assert_eq!(out, "rules:\n  - MATCH,DIRECT\n  - GEOSITE,cn,DIRECT\n");
    }

    #[test]
    fn insert_as_the_first_item_of_a_block_form_list() {
        let src = "rules:\n  - MATCH,DIRECT\n";
        let out = insert_rule_line(src, 1, "GEOSITE,cn,DIRECT").unwrap();
        assert_eq!(out, "rules:\n  - GEOSITE,cn,DIRECT\n  - MATCH,DIRECT\n");
    }

    #[test]
    fn insert_expands_an_empty_flow_sequence() {
        let src = "mixed-port: 7890\nrules: []\n";
        let out = insert_rule_line(src, 2, "MATCH,DIRECT").unwrap();
        assert_eq!(out, "mixed-port: 7890\nrules:\n  - MATCH,DIRECT\n");
    }

    #[test]
    fn insert_preserves_untouched_lines_and_comments() {
        let src = "rules:\n  # 兜底\n  - MATCH,DIRECT\n";
        let out = insert_rule_line(src, 3, "GEOSITE,cn,日本节点").unwrap();
        assert_eq!(
            out,
            "rules:\n  # 兜底\n  - MATCH,DIRECT\n  - GEOSITE,cn,日本节点\n"
        );
    }

    #[test]
    fn insert_refuses_a_non_empty_flow_sequence() {
        // 与 replace_rule_line 对这类文件的已知限制一致：不猜测怎么改。
        let src = "rules: [MATCH,DIRECT]\n";
        assert!(matches!(
            insert_rule_line(src, 1, "GEOSITE,cn,DIRECT"),
            Err(EditError::NotASequenceItem(1))
        ));
    }

    #[test]
    fn insert_refuses_an_anchor_that_is_neither_a_rule_nor_the_rules_key() {
        let src = "mixed-port: 7890\nrules:\n  - MATCH,DIRECT\n";
        assert!(matches!(
            insert_rule_line(src, 1, "GEOSITE,cn,DIRECT"),
            Err(EditError::NotASequenceItem(1))
        ));
    }

    #[test]
    fn insert_rejects_an_empty_value() {
        let src = "rules:\n  - MATCH,DIRECT\n";
        assert!(matches!(
            insert_rule_line(src, 1, "   "),
            Err(EditError::EmptyValue)
        ));
    }

    #[test]
    fn insert_handles_a_file_with_no_trailing_newline() {
        // 锚点行若恰好是文件最后一行且没有换行符，插入后原行与新行不能糊在一起。
        let src = "rules:\n  - MATCH,DIRECT";
        let out = insert_rule_line(src, 2, "GEOSITE,cn,DIRECT").unwrap();
        assert_eq!(out, "rules:\n  - MATCH,DIRECT\n  - GEOSITE,cn,DIRECT\n");
    }

    #[test]
    fn insert_out_of_range_anchor_is_an_error_not_a_panic() {
        let src = "rules:\n  - MATCH,DIRECT\n";
        assert!(matches!(
            insert_rule_line(src, 99, "GEOSITE,cn,DIRECT"),
            Err(EditError::LineOutOfRange(99, 2))
        ));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p wsieve-config insert_`
Expected: 编译失败——`insert_rule_line` 还不存在。

- [ ] **Step 3: 写实现**

在 `delete_rule_line` 函数结束（`}` 之后、`/// 一行序列项拆成三段` 那条文档
注释之前）插入：

```rust
/// 在 `anchor_line` 之后插入一条新规则。`anchor_line` 可以是：
///   - 某条已有规则所在行——新规则插在它之后，缩进与它一致
///   - `rules:` 键本身所在行（块式，键后即换行）——新规则成为第一条
///   - `rules: []`（空流式序列）所在行——就地展开成块式，新规则成为第一条
/// 除此之外一律报 `NotASequenceItem`，包括非空流式序列（`rules: [A, B]`）——
/// 与 `replace_rule_line` 对这类文件的已知限制一致，不猜测怎么改。
///
/// 与 `replace_rule_line` 同一套「验不过就整体回滚」的纪律：改完立刻读回，
/// 确认新插入的那一行确实是一条值为 `value` 的规则，不是就整体报错。
pub fn insert_rule_line(src: &str, anchor_line: u64, value: &str) -> Result<String, EditError> {
    check_new_value(value)?;

    let lines: Vec<&str> = split_keep_ends(src);
    let idx = check_index(anchor_line, lines.len())?;
    let (body, orig_eol) = split_eol(lines[idx]);
    // 新行自己的换行符：锚点行若有换行符就沿用，没有（文件不以换行结尾）就补一个。
    let sep = if orig_eol.is_empty() { "\n" } else { orig_eol };

    // 情形一：锚点是一条已有规则——插在它之后，缩进与它一致。
    if split_item(body, anchor_line).is_ok() {
        let indent = &body[..indent_width(body)];
        let new_line = format!("{indent}- {value}{sep}");
        let out = splice_after(&lines, idx, orig_eol, &new_line);
        verify_written(src, &out, anchor_line + 1, value)?;
        return Ok(out);
    }

    let trimmed = body.trim_end();

    // 情形二：块式 rules: 键——新规则成为第一条。
    if trimmed == "rules:" {
        let new_line = format!("  - {value}{sep}");
        let out = splice_after(&lines, idx, orig_eol, &new_line);
        verify_written(src, &out, anchor_line + 1, value)?;
        return Ok(out);
    }

    // 情形三：空流式序列——就地展开成块式。
    if trimmed == "rules: []" {
        let replacement = format!("rules:{sep}  - {value}{sep}");
        let mut out = String::with_capacity(src.len() + replacement.len());
        for (i, l) in lines.iter().enumerate() {
            if i == idx {
                out.push_str(&replacement);
            } else {
                out.push_str(l);
            }
        }
        verify_written(src, &out, anchor_line + 1, value)?;
        return Ok(out);
    }

    // 非空流式序列、或压根不是 rules 相关的行——都不猜测，直接拒绝。
    Err(EditError::NotASequenceItem(anchor_line))
}

/// 在第 `at`（0-based）行之后插入 `new_line`；若该行原本没有换行符
/// （文件不以换行结尾），先补一个，避免原内容与新行糊成一行。
fn splice_after(lines: &[&str], at: usize, orig_eol: &str, new_line: &str) -> String {
    let mut out = String::with_capacity(
        lines.iter().map(|l| l.len()).sum::<usize>() + new_line.len() + 1,
    );
    for (i, l) in lines.iter().enumerate() {
        out.push_str(l);
        if i == at {
            if orig_eol.is_empty() {
                out.push('\n');
            }
            out.push_str(new_line);
        }
    }
    out
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p wsieve-config insert_`
Expected: 9 个测试全部 PASS

- [ ] **Step 5: 提交 wsieve-config 这一半**

```bash
git add crates/wsieve-config/src/edit.rs
git commit -m "feat(config): insert_rule_line —— 含空流式序列展开与无尾换行两个边界情况"
```

- [ ] **Step 6: `ConfigView` 加 `rules_key_line`，`RuleOp` 加 `InsertRule`**

`src-tauri/src/commands/config.rs`：在 `ConfigView` 结构体的 `rules` 字段
之后加一个新字段：

```rust
    /// `rules:` 键本身所在的 1-based 行号。规则列表为空时，UI 没有任何
    /// 已有规则的行号可以当插入锚点，只能靠这个字段——见 `RuleOp::InsertRule`。
    pub rules_key_line: u64,
    /// `rules_key_line` 那一行的原样文本（`"rules:"` 或 `"rules: []"`）。
    /// `RuleOp::InsertRule` 拿 `rules_key_line` 当 anchor 时，`anchor_expect`
    /// 必须填这一行**当前实际的**文本——前端拿不到这份文本就只能猜，
    /// 猜错了并发校验会拒绝一次本该成功的插入。单独给一个字段，
    /// 不指望前端凭空知道空列表在文件里到底写的是哪一种空写法。
    pub rules_key_text: String,
```

`RuleOp` 枚举加一个变体：

```rust
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
```

`impl RuleOp` 的 `line()`/`expect()` 补上新分支：

```rust
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
```

- [ ] **Step 7: 编译，确认能定位出哪些地方要跟着改**

Run: `cargo build --manifest-path src-tauri/Cargo.toml`
Expected: 编译失败，报错指向 `build_view`（缺 `rules_key_line` 初始化）、
`apply_rule_ops` 的并发校验循环（`snapshot.rules.iter().find(...)` 对
`InsertRule` 的 anchor 不一定能找到）、Task 4 里那个 `if let RuleOp::ReplaceRule`
（该换成 `match` 覆盖 `InsertRule`）、以及改写循环的 `match op`（缺
`InsertRule` 分支）。这是刻意的——按报错逐一修，比预先猜全更不容易漏。

- [ ] **Step 8: 补 `build_view` 的 `rules_key_line`**

找到 `build_view` 函数（在 `fn build_view(text: &str) -> CmdResult<ConfigView>`），
补一个小函数并接入：

```rust
/// `rules:` 键本身所在的 1-based 行号。扫的是顶层（零缩进）的 `rules:` 键，
/// 不认识 `rules:` 出现在别处（比如某个字符串值里恰好含这几个字符）的情况——
/// 那种输入本就不是合法配置，`load_str` 会先一步拒绝。
fn find_rules_key_line(text: &str) -> Option<u64> {
    text.lines()
        .enumerate()
        .find(|(_, l)| *l == "rules:" || l.trim_end() == "rules: []" || l.starts_with("rules:"))
        .map(|(i, _)| i as u64 + 1)
}
```

在 `build_view` 里，`Ok(ConfigView { config, rules })` 那一行改成：

```rust
    let rules_key_line = find_rules_key_line(text).ok_or_else(|| CmdError::ConfigInvalid {
        message: "配置里找不到顶层的 rules: 键——这不应该发生，Config::default() 与\
                  DEFAULT_CONFIG_YAML 都会写这个键".to_string(),
    })?;
    let rules_key_text = text
        .lines()
        .nth(rules_key_line as usize - 1)
        .unwrap_or("rules:")
        .to_string();

    Ok(ConfigView { config, rules, rules_key_line, rules_key_text })
```

- [ ] **Step 9: 补并发校验循环对 `InsertRule` 的处理**

`apply_rule_ops` 里「并发校验」那个循环，目前只认「anchor 必须是一条已有
规则」。`InsertRule` 的 anchor 也可能是 `rules_key_line`（此时不是规则，是
`rules:` 这一行本身）。把循环体改成：

```rust
    for op in &ops {
        let actual = if let Some(r) = snapshot.rules.iter().find(|r| r.defined.line() == op.line()) {
            r.value.clone()
        } else if matches!(op, RuleOp::InsertRule { .. }) {
            // anchor 不是一条已有规则——按 InsertRule 的约定，它应该是
            // rules_key_line，直接比对那一行的原始文本。
            text.lines().nth(op.line() as usize - 1).map(str::to_string).ok_or_else(|| {
                CmdError::ConfigInvalid {
                    message: format!("第 {} 行不存在", op.line()),
                }
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
                     配置在此期间被改过，已放弃本次保存",
                    op.line(),
                    actual,
                    op.expect()
                ),
            });
        }
    }
```

- [ ] **Step 10: 补改写循环与 Task 4 的语法校验循环**

改写循环（`let mut out = text; for op in &ops { out = match op { ... } }`）
加一个分支：

```rust
            RuleOp::InsertRule { anchor, value, .. } => {
                wsieve_config::edit::insert_rule_line(&out, *anchor, value)?
            }
```

**重要**：`InsertRule` 与 `ReplaceRule`/`DeleteRule` 混在同一批操作里按行号
从大到小施加时，插入操作会让它**之后**的行号全部下移——但既有排序纪律
（「从大到小」）本身保证了插入永远排在受影响的更小行号操作之前处理完，
所以不需要额外调整；这与 `DeleteRule` 让后续行号上移是同一个已经解决的问题
（那一段既有注释已经讲过）。

Task 4 的语法校验循环，把 `if let RuleOp::ReplaceRule { value, .. } = op` 换成：

```rust
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
```

- [ ] **Step 11: 编译确认**

Run: `cargo build --manifest-path src-tauri/Cargo.toml`
Expected: 编译通过

- [ ] **Step 12: 写 `InsertRule` 命令层的测试**

在 `src-tauri/src/commands/config.rs` 的测试模块里加：

```rust
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
    }
```

（`read_view` 是模块内已有的私有测试辅助路径——直接用 `super::read_view`，
若测试模块里此前没有直接调用过它，确认它对 `#[cfg(test)] mod tests` 可见；
它是 `fn read_view(p: &Path) -> CmdResult<ConfigView>`，模块私有函数，
测试模块用 `use super::*;` 已经能访问到。）

- [ ] **Step 13: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --bin wsieve-app commands::config::`
Expected: 全部 PASS，含新增的 4 条与 Task 4 的 2 条

- [ ] **Step 14: 跑全量既有测试确认无回归**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --bin wsieve-app commands`
Expected: 全部 PASS

- [ ] **Step 15: 提交**

```bash
git add src-tauri/src/commands/config.rs
git commit -m "feat(ipc): RuleOp::InsertRule —— 支持插入新规则，含空 rules: [] 场景"
```

---

## Part C — 节点块编辑原语与命令

### Task 6: `append_proxy_block` 与 `delete_proxy_block`

**Files:**
- Modify: `crates/wsieve-config/src/edit.rs`

`proxies:` 的每一项是**多行结构化块**，不像规则是单行 `Spanned<String>`——
插入/删除需要定位「一个块的起止行」而不是「一行」。默认配置的 `proxies: []`
（空流式）与规则的 `rules: []` 是同一个边界情况，处理方式对称。

- [ ] **Step 1: 写失败的测试**

在 `crates/wsieve-config/src/edit.rs` 的测试模块里加：

```rust
    #[test]
    fn append_proxy_expands_an_empty_flow_sequence() {
        let src = "mixed-port: 25500\nproxies: []\nrules: []\n";
        let lines = vec![
            "- name: \"日本节点\"".to_string(),
            "  type: websieve".to_string(),
            "  url: https://example.com/".to_string(),
            "  server-pub: \"aa\"".to_string(),
            "  client-priv: \"bb\"".to_string(),
        ];
        let out = append_proxy_block(src, &lines).unwrap();
        assert_eq!(
            out,
            "mixed-port: 25500\nproxies:\n  - name: \"日本节点\"\n    type: websieve\n    url: https://example.com/\n    server-pub: \"aa\"\n    client-priv: \"bb\"\nrules: []\n"
        );
    }

    #[test]
    fn append_proxy_after_an_existing_block() {
        let src = "proxies:\n  - name: \"日本节点\"\n    type: websieve\nrules: []\n";
        let lines = vec!["- name: \"香港节点\"".to_string(), "  type: websieve".to_string()];
        let out = append_proxy_block(src, &lines).unwrap();
        assert_eq!(
            out,
            "proxies:\n  - name: \"日本节点\"\n    type: websieve\n  - name: \"香港节点\"\n    type: websieve\nrules: []\n"
        );
    }

    #[test]
    fn append_proxy_when_the_list_runs_to_end_of_file() {
        let src = "proxies:\n  - name: \"日本节点\"\n    type: websieve\n";
        let lines = vec!["- name: \"香港节点\"".to_string(), "  type: websieve".to_string()];
        let out = append_proxy_block(src, &lines).unwrap();
        assert_eq!(
            out,
            "proxies:\n  - name: \"日本节点\"\n    type: websieve\n  - name: \"香港节点\"\n    type: websieve\n"
        );
    }

    #[test]
    fn append_proxy_untouched_bytes_stay_untouched() {
        let src = "mixed-port: 25500\nproxies:\n  - name: \"日本节点\"\n    type: websieve\nrules:\n  - MATCH,日本节点\n";
        let lines = vec!["- name: \"香港节点\"".to_string(), "  type: websieve".to_string()];
        let out = append_proxy_block(src, &lines).unwrap();
        assert!(out.starts_with("mixed-port: 25500\nproxies:\n  - name: \"日本节点\"\n    type: websieve\n"));
        assert!(out.ends_with("rules:\n  - MATCH,日本节点\n"));
    }

    #[test]
    fn append_proxy_rejects_empty_lines() {
        let src = "proxies: []\n";
        assert!(matches!(append_proxy_block(src, &[]), Err(EditError::EmptyValue)));
    }

    #[test]
    fn append_proxy_missing_key_is_an_error() {
        let src = "mixed-port: 25500\nrules: []\n";
        let lines = vec!["- name: \"x\"".to_string()];
        assert!(matches!(
            append_proxy_block(src, &lines),
            Err(EditError::NotASequenceItem(0))
        ));
    }

    #[test]
    fn delete_proxy_removes_the_named_block_and_nothing_else() {
        let src = "proxies:\n  - name: \"日本节点\"\n    type: websieve\n  - name: \"香港节点\"\n    type: websieve\nrules: []\n";
        let out = delete_proxy_block(src, "日本节点").unwrap();
        assert_eq!(out, "proxies:\n  - name: \"香港节点\"\n    type: websieve\nrules: []\n");
    }

    #[test]
    fn delete_proxy_that_is_the_last_item_before_eof() {
        let src = "proxies:\n  - name: \"日本节点\"\n    type: websieve\n";
        let out = delete_proxy_block(src, "日本节点").unwrap();
        assert_eq!(out, "proxies:\n");
    }

    #[test]
    fn delete_proxy_matches_an_unquoted_name_too() {
        let src = "proxies:\n  - name: 日本节点\n    type: websieve\nrules: []\n";
        let out = delete_proxy_block(src, "日本节点").unwrap();
        assert_eq!(out, "proxies:\nrules: []\n");
    }

    #[test]
    fn delete_proxy_unknown_name_is_an_error_not_a_silent_noop() {
        let src = "proxies:\n  - name: \"日本节点\"\n    type: websieve\nrules: []\n";
        assert!(matches!(
            delete_proxy_block(src, "幽灵节点"),
            Err(EditError::NotASequenceItem(_))
        ));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p wsieve-config append_proxy delete_proxy`
Expected: 编译失败——两个函数还不存在。

- [ ] **Step 3: 写实现**

在 `insert_rule_line` 与 `splice_after` 之后插入：

```rust
/// 在 `proxies:` 列表末尾追加一个新的服务器块。`lines` 是调用方已经按
/// 固定缩进格式化好的若干行（每行是 `- name: ...` 这一级的内容，本函数
/// 统一在每行前面补两格缩进），不接受任意文本——UI 端拼好结构，这里只
/// 负责找到插入点。`proxies: []`（空流式）会被就地展开成块式。
///
/// `proxies:` 键不存在时报错，不猜测——本项目的默认配置与
/// `Config::default()` 都会写这个键，不存在意味着文件被手动删过这个键。
pub fn append_proxy_block(src: &str, lines: &[String]) -> Result<String, EditError> {
    if lines.is_empty() {
        return Err(EditError::EmptyValue);
    }

    let all: Vec<&str> = split_keep_ends(src);
    let key_idx = all
        .iter()
        .position(|l| {
            let (body, _) = split_eol(l);
            body == "proxies:" || body.trim_end() == "proxies: []"
        })
        .ok_or(EditError::NotASequenceItem(0))?;

    let (key_body, key_eol) = split_eol(all[key_idx]);
    let sep = if key_eol.is_empty() { "\n" } else { key_eol };

    let mut block = String::new();
    for l in lines {
        block.push_str("  ");
        block.push_str(l);
        block.push_str(sep);
    }

    if key_body.trim_end() == "proxies: []" {
        let expanded = format!("proxies:{sep}{block}");
        let mut out = String::with_capacity(src.len() + expanded.len());
        for (i, l) in all.iter().enumerate() {
            if i == key_idx {
                out.push_str(&expanded);
            } else {
                out.push_str(l);
            }
        }
        return Ok(out);
    }

    // 块式：找列表结束的位置——遇到缩进为 0 的非空行（下一个顶层键）
    // 或文件结束为止。空行仍算列表内的间隔，不当作结束标志。
    let mut end = all.len();
    for i in (key_idx + 1)..all.len() {
        let (body, _) = split_eol(all[i]);
        if body.trim().is_empty() {
            continue;
        }
        if indent_width(body) == 0 {
            end = i;
            break;
        }
    }

    let mut out = String::with_capacity(src.len() + block.len());
    for l in &all[..end] {
        out.push_str(l);
    }
    if end == all.len() {
        if let Some(last) = all.last() {
            let (_, last_eol) = split_eol(last);
            if last_eol.is_empty() {
                out.push_str(sep);
            }
        }
    }
    out.push_str(&block);
    for l in &all[end..] {
        out.push_str(l);
    }
    Ok(out)
}

/// 按 `name` 定位并删除对应的服务器块，从它的 `- name: ...` 行到下一个
/// 同级 `- name:`（或列表结束）为止，整段删掉，其余字节不动。
///
/// 认 `- name: "日本节点"` 与 `- name: 日本节点` 两种写法（带引号与不带）。
/// 找不到匹配的名字、或 `proxies:` 键本身不存在/是空列表，都报错——
/// 删除一个不存在的东西不该悄悄什么都不做。
pub fn delete_proxy_block(src: &str, name: &str) -> Result<String, EditError> {
    let all: Vec<&str> = split_keep_ends(src);
    let key_idx = all
        .iter()
        .position(|l| {
            let (body, _) = split_eol(l);
            body == "proxies:" || body.trim_end() == "proxies: []"
        })
        .ok_or(EditError::NotASequenceItem(0))?;

    let (key_body, _) = split_eol(all[key_idx]);
    if key_body.trim_end() == "proxies: []" {
        return Err(EditError::NotASequenceItem(key_idx as u64 + 1));
    }

    let mut start: Option<usize> = None;
    let mut end = all.len();
    let mut i = key_idx + 1;
    while i < all.len() {
        let (body, _) = split_eol(all[i]);
        if body.trim().is_empty() {
            i += 1;
            continue;
        }
        if indent_width(body) == 0 {
            end = i;
            break;
        }
        let trimmed = body.trim_start();
        if let Some(rest) = trimmed.strip_prefix("- ") {
            if start.is_some() {
                end = i;
                break;
            }
            if item_name_matches(rest, name) {
                start = Some(i);
            }
        }
        i += 1;
    }
    let start = start.ok_or(EditError::NotASequenceItem(key_idx as u64 + 1))?;

    let mut out = String::with_capacity(src.len());
    for (idx, l) in all.iter().enumerate() {
        if idx < start || idx >= end {
            out.push_str(l);
        }
    }
    Ok(out)
}

/// 判断 `- ` 之后的这一行内容（形如 `name: "xxx"` 或 `name: xxx`）
/// 是否是 `target` 这个名字——认引号也认不带引号两种写法。
fn item_name_matches(rest: &str, target: &str) -> bool {
    let Some(value) = rest.strip_prefix("name:").map(str::trim) else {
        return false;
    };
    value.trim_matches('"') == target
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p wsieve-config append_proxy delete_proxy`
Expected: 10 个测试全部 PASS

- [ ] **Step 5: 跑一遍 edit.rs 全量测试确认没有回归**

Run: `cargo test -p wsieve-config`
Expected: 全部 PASS

- [ ] **Step 6: 提交**

```bash
git add crates/wsieve-config/src/edit.rs
git commit -m "feat(config): append_proxy_block / delete_proxy_block —— 节点块的定点插入与删除"
```

---

### Task 7: 两个新命令 `config_insert_proxy` / `config_delete_proxy`

**Files:**
- Modify: `src-tauri/src/commands/config.rs`
- Modify: `src-tauri/src/main.rs`
- Modify: `src-tauri/capabilities/control.json`

- [ ] **Step 1: 写命令层测试**

在 `src-tauri/src/commands/config.rs` 的测试模块里加：

```rust
    #[test]
    fn insert_proxy_appends_a_new_server_block() {
        let d = tmpdir("insert-proxy");
        let p = d.join("config.yaml");
        std::fs::write(&p, "mixed-port: 25500\nproxies: []\nrules: []\n").unwrap();

        insert_proxy_block(
            &p,
            vec![
                "- name: \"日本节点\"".to_string(),
                "  type: websieve".to_string(),
                "  url: https://example.com/".to_string(),
                "  server-pub: \"aa\"".to_string(),
                "  client-priv: \"bb\"".to_string(),
            ],
        )
        .unwrap();

        let view = read_view(&p).unwrap();
        let names: Vec<_> = view.config["proxies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["日本节点"]);
    }

    #[test]
    fn insert_proxy_rejects_a_name_that_already_exists() {
        let d = tmpdir("insert-proxy-dup");
        let p = d.join("config.yaml");
        save_text(&p, SAMPLE).unwrap(); // SAMPLE 已含 "日本节点"

        let err = insert_proxy_block(
            &p,
            vec![
                "- name: \"日本节点\"".to_string(),
                "  type: websieve".to_string(),
                "  url: https://example.com/".to_string(),
                "  server-pub: \"cc\"".to_string(),
                "  client-priv: \"dd\"".to_string(),
            ],
        )
        .unwrap_err();
        assert!(matches!(err, CmdError::ConfigInvalid { .. }));
    }

    #[test]
    fn delete_proxy_by_name_removes_it() {
        let d = tmpdir("delete-proxy");
        let p = d.join("config.yaml");
        save_text(&p, SAMPLE).unwrap();

        delete_proxy_block_cmd(&p, "日本节点").unwrap();

        let view = read_view(&p).unwrap();
        assert_eq!(view.config["proxies"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn delete_proxy_unknown_name_is_named_in_the_error() {
        let d = tmpdir("delete-proxy-unknown");
        let p = d.join("config.yaml");
        save_text(&p, SAMPLE).unwrap();

        let err = delete_proxy_block_cmd(&p, "幽灵节点").unwrap_err();
        assert!(matches!(err, CmdError::ConfigInvalid { .. }));
    }
```

（`insert_proxy_block`/`delete_proxy_block_cmd` 是本 Step 要写的、与
`AppHandle` 无关的纯路径函数——与 `apply_rule_ops`/`save_text` 同一层，
`#[tauri::command]` 的薄封装另起。函数名特意不叫 `insert_proxy`/`delete_proxy`，
避免与 `wsieve_config::edit::append_proxy_block`/`delete_proxy_block`
在同一文件里 `use` 之后撞名。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --bin wsieve-app insert_proxy delete_proxy`
Expected: 编译失败——两个函数还不存在。

- [ ] **Step 3: 写实现**

在 `apply_rule_ops` 函数之后、`write_0600` 函数之前插入：

```rust
/// 新增一个出站服务器块。`lines` 由调用方（`config_insert_proxy` 命令）
/// 按固定格式拼好——见该命令的文档注释。
///
/// 名字冲突（与已有出站名或已有组名重复）在这里挡，而不是等
/// `Config::validate()` 去挡：`validate()` 的报错信息是给「文件已经写完」
/// 之后的场景设计的，这里能在写之前就说清楚是哪个名字冲突，用户体验更直接，
/// 也避免了「先写坏、再报错、再要求用户手动改回去」这种更差的路径。
fn insert_proxy_block(p: &Path, lines: Vec<String>) -> CmdResult<()> {
    let name = extract_name_field(&lines).ok_or_else(|| CmdError::ConfigInvalid {
        message: "新节点的第一行必须是 `- name: \"...\"`".to_string(),
    })?;

    let text = read_text(p)?;
    let snapshot = wsieve_config::load_str(&text)?;
    if snapshot.outbound_names().contains(name.as_str())
        || snapshot.proxy_groups.iter().any(|g| g.name == name)
    {
        return Err(CmdError::ConfigInvalid {
            message: format!("名字 {name:?} 已经被一个出站或代理组占用，换一个名字"),
        });
    }

    let out = wsieve_config::edit::append_proxy_block(&text, &lines)?;
    wsieve_config::load_str(&out)?.validate()?;
    write_0600(p, &out)
}

/// 从新节点的行数组里取出 `name` 字段的值（认引号也认不带引号）。
fn extract_name_field(lines: &[String]) -> Option<String> {
    let first = lines.first()?;
    let rest = first.trim_start().strip_prefix("- name:")?;
    Some(rest.trim().trim_matches('"').to_string())
}

/// 按名字删除一个出站服务器块。
fn delete_proxy_block_cmd(p: &Path, name: &str) -> CmdResult<()> {
    let text = read_text(p)?;
    let out = wsieve_config::edit::delete_proxy_block(&text, name)?;
    wsieve_config::load_str(&out)?.validate()?;
    write_0600(p, &out)
}

/// 新增一个出站服务器（结构化，UI 表单驱动）。
///
/// `lines` 由前端按固定顺序拼好：`- name: "..."` / `type: websieve` /
/// `url: ...` / `server-pub: "..."` / `client-priv: "..."`，可选再加
/// `extra-sessions: N` / `mux-prefs: [...]`。**不接受任意文本**——本命令
/// 只负责把这几行原样插进 `proxies:` 列表，不解释、不校验字段语义之外的
/// 格式（那是 `Config::validate()` 与 YAML 解析本身的职责）。
///
/// 私钥经过这条 IPC 边界是真实存在的事——`client-priv` 出现在 `lines`
/// 里，随命令参数一起序列化。这与 `config_save_raw` 已经承担的风险同类，
/// UI 侧的处置义务见设计文档 §6.2。
#[tauri::command]
pub async fn config_insert_proxy(app: tauri::AppHandle, lines: Vec<String>) -> CmdResult<()> {
    insert_proxy_block(&config_path(&app)?, lines)
}

#[tauri::command]
pub async fn config_delete_proxy(app: tauri::AppHandle, name: String) -> CmdResult<()> {
    if name.trim().is_empty() {
        return Err(CmdError::ConfigInvalid {
            message: "出站名不能为空".to_string(),
        });
    }
    delete_proxy_block_cmd(&config_path(&app)?, &name)
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --bin wsieve-app insert_proxy delete_proxy`
Expected: 4 个测试全部 PASS

- [ ] **Step 5: 注册命令**

`src-tauri/src/main.rs`：在 `generate_handler!` 列表里 `commands::config::config_save_raw,`
那一行之后加两行：

```rust
            commands::config::config_insert_proxy,
            commands::config::config_delete_proxy,
```

- [ ] **Step 6: 加 capability 权限**

`src-tauri/capabilities/control.json`：`permissions` 数组里
`"allow-config-save-raw",` 那一行之后加两行：

```json
    "allow-config-insert-proxy",
    "allow-config-delete-proxy",
```

- [ ] **Step 7: 编译并跑一遍 capability 隔离测试**

Run: `cargo build --manifest-path src-tauri/Cargo.toml && cargo test --manifest-path src-tauri/Cargo.toml capability_isolation`
Expected: 编译通过，`tests/capability_isolation.rs` 全部 PASS（两个新命令只在
control.json 里，不在 transport.json 里，交集依然为空）。

- [ ] **Step 8: 提交**

```bash
git add src-tauri/src/commands/config.rs src-tauri/src/main.rs src-tauri/capabilities/control.json
git commit -m "feat(ipc): config_insert_proxy / config_delete_proxy 两个新命令"
```

---

## Part D — 首页

### Task 8: `config-map.js` 的 `setGroupSelected`

**Files:**
- Modify: `ui/src/lib/config-map.js`
- Modify: `ui/src/lib/config-map.test.js`

- [ ] **Step 1: 写失败的测试**

在 `ui/src/lib/config-map.test.js` 里加一组新测试（跟在 `setScalar` 那组
`describe` 之后）：

```js
describe('setGroupSelected', () => {
  const src = [
    'proxy-groups:',
    '  - name: 节点选择',
    '    kind: select',
    '    proxies: [日本节点, 香港节点]',
    '    selected: 日本节点',
    '  - name: 自动选优',
    '    kind: auto',
    '    proxies: [日本节点, 香港节点]',
    'rules: []',
    '',
  ].join('\n');

  it('只改目标组的 selected，不动别的组', () => {
    const out = setGroupSelected(src, '节点选择', '香港节点');
    expect(out).toContain('    selected: 香港节点');
    expect(out).toContain('  - name: 自动选优');
    const autoBlockLines = out
      .split('\n')
      .slice(out.split('\n').indexOf('  - name: 自动选优'));
    expect(autoBlockLines.some((l) => l.includes('selected:'))).toBe(false);
  });

  it('保留行尾注释', () => {
    const withComment = src.replace(
      '    selected: 日本节点',
      '    selected: 日本节点  # 默认走这个',
    );
    const out = setGroupSelected(withComment, '节点选择', '香港节点');
    expect(out).toContain('    selected: 香港节点  # 默认走这个');
  });

  it('其余组与其余内容一字节不变', () => {
    const out = setGroupSelected(src, '节点选择', '香港节点');
    expect(out).toContain('  - name: 自动选优\n    kind: auto\n    proxies: [日本节点, 香港节点]\nrules: []');
  });

  it('找不到匹配的组名时如实报错', () => {
    expect(() => setGroupSelected(src, '不存在的组', '日本节点')).toThrow(/不存在的组/);
  });

  it('组存在但没有 selected 行（比如 auto 类型）时如实报错，不静默无操作', () => {
    expect(() => setGroupSelected(src, '自动选优', '日本节点')).toThrow(/selected/);
  });

  it('没有 proxy-groups 键时如实报错', () => {
    expect(() => setGroupSelected('rules: []\n', '节点选择', '日本节点')).toThrow(/proxy-groups/);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd ui && npx vitest run config-map`
Expected: FAIL——`setGroupSelected` 还不存在。

- [ ] **Step 3: 写实现**

在 `ui/src/lib/config-map.js` 的 `setScalar` 函数之后插入：

```js
/**
 * 改写某个代理组的 `selected:` 字段——与 `setScalar` 的区别是 `selected:`
 * 这个键名可能在好几个组块里各出现一次，纯字符串匹配会串到别的组头上，
 * 必须先按 `groupName` 定位到具体是哪个组块，再只在那个范围内查找替换。
 *
 * 逐行文本操作，不重新序列化整份 YAML，其余组的内容与全部注释原样保留——
 * 与 `setScalar` 同一条纪律。
 */
export function setGroupSelected(text, groupName, member) {
  const lines = String(text).split('\n');
  const keyIdx = lines.findIndex((l) => l.replace(/\r$/, '') === 'proxy-groups:');
  if (keyIdx < 0) {
    throw new Error(`配置里没有 proxy-groups 键，找不到组 ${groupName}`);
  }

  let start = -1;
  let end = lines.length;
  for (let i = keyIdx + 1; i < lines.length; i++) {
    const line = lines[i].replace(/\r$/, '');
    if (line.trim() === '') continue;
    const indent = line.length - line.trimStart().length;
    if (indent === 0) {
      end = i;
      break;
    }
    const trimmed = line.trimStart();
    if (trimmed.startsWith('- ')) {
      if (start >= 0) {
        end = i;
        break;
      }
      const rest = trimmed.slice(2);
      if (rest.startsWith('name:')) {
        const name = rest.slice('name:'.length).trim().replace(/^"|"$/g, '');
        if (name === groupName) start = i;
      }
    }
  }
  if (start < 0) {
    throw new Error(`找不到名为 ${groupName} 的代理组`);
  }

  const head = 'selected:';
  for (let i = start; i < end; i++) {
    const raw = lines[i];
    const cr = raw.endsWith('\r') ? '\r' : '';
    const line = cr ? raw.slice(0, -1) : raw;
    const trimmed = line.trimStart();
    if (!trimmed.startsWith(head)) continue;
    const indentStr = line.slice(0, line.length - trimmed.length);
    let rest = trimmed.slice(head.length);
    const c = commentStart(rest);
    if (c < 0) {
      lines[i] = `${indentStr}${head} ${member}${cr}`;
    } else {
      const gap = /\s*$/.exec(rest.slice(0, c))[0];
      lines[i] = `${indentStr}${head} ${member}${gap}${rest.slice(c)}${cr}`;
    }
    return lines.join('\n');
  }
  throw new Error(`代理组 ${groupName} 没有 selected 字段（不是 select 类型？）`);
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd ui && npx vitest run config-map`
Expected: 全部 PASS（含既有的 `setScalar` 测试，无回归）

- [ ] **Step 5: 提交**

```bash
git add ui/src/lib/config-map.js ui/src/lib/config-map.test.js
git commit -m "feat(ui): setGroupSelected —— 定位到具体组块再改写，不串行"
```

---

### Task 9: `HomeView.svelte`（四张卡片）

**Files:**
- Create: `ui/src/views/HomeView.svelte`
- Create: `ui/src/views/HomeView.test.js`

四张卡片：节点选择、系统代理/虚拟网卡、分流模式、流量统计。视觉延续既有
设计令牌（borders-only、密度高），不引入圆角阴影卡片皮肤。

- [ ] **Step 1: 写失败的测试**

`ui/src/views/HomeView.test.js`：

```js
import { describe, it, expect, vi } from 'vitest';
import { render, screen, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import HomeView from './HomeView.svelte';

const colorOf = () => '#5b8ff9';

const base = () => ({
  groups: [],
  colorOf,
  allowLan: false,
  systemProxy: false,
  tunEnabled: false,
  preset: 'custom',
  spark: [10, 20, 5, 40, 15],
  downRate: 1024,
  upRate: 512,
  onselectmember: vi.fn(),
  onsystemproxychange: vi.fn(),
  ontunchange: vi.fn(),
  onpresetchange: vi.fn(),
});

describe('首页 —— 节点选择卡片', () => {
  it('零个 select 组时给出引导，不假装有一个组', () => {
    render(HomeView, base());
    expect(screen.getByText(/还没有配置代理组/)).toBeInTheDocument();
  });

  it('有一个 select 组时渲染成员下拉，当前选中项正确', () => {
    render(HomeView, {
      ...base(),
      groups: [{ name: '节点选择', kind: 'select', proxies: ['日本节点', '香港节点'], selected: '日本节点' }],
    });
    const select = screen.getByLabelText(/节点/);
    expect(select.value).toBe('日本节点');
  });

  it('切换成员触发 onselectmember，带上组名与新成员', async () => {
    const u = userEvent.setup();
    const p = {
      ...base(),
      groups: [{ name: '节点选择', kind: 'select', proxies: ['日本节点', '香港节点'], selected: '日本节点' }],
    };
    render(HomeView, p);
    await u.selectOptions(screen.getByLabelText(/节点/), '香港节点');
    expect(p.onselectmember).toHaveBeenCalledWith('节点选择', '香港节点');
  });

  it('多个 select 组时先选组、再选成员', async () => {
    const u = userEvent.setup();
    render(HomeView, {
      ...base(),
      groups: [
        { name: '节点选择', kind: 'select', proxies: ['日本节点'], selected: '日本节点' },
        { name: '备用组', kind: 'select', proxies: ['香港节点'], selected: '香港节点' },
      ],
    });
    const groupSelect = screen.getByLabelText(/代理组/);
    expect(groupSelect).toBeInTheDocument();
    await u.selectOptions(groupSelect, '备用组');
    expect(screen.getByLabelText(/节点/).value).toBe('香港节点');
  });

  it('auto / load-balance 类型的组不出现在节点选择卡片里', () => {
    render(HomeView, {
      ...base(),
      groups: [{ name: '自动选优', kind: 'auto', proxies: ['日本节点'] }],
    });
    expect(screen.getByText(/还没有配置代理组/)).toBeInTheDocument();
  });
});

describe('首页 —— 系统代理 / 虚拟网卡卡片', () => {
  it('两个开关反映当前状态', () => {
    render(HomeView, { ...base(), systemProxy: true, tunEnabled: false });
    expect(screen.getByRole('switch', { name: /系统代理/ })).toHaveAttribute('aria-checked', 'true');
    expect(screen.getByRole('switch', { name: /虚拟网卡|TUN/ })).toHaveAttribute('aria-checked', 'false');
  });

  it('切换系统代理触发 onsystemproxychange', async () => {
    const u = userEvent.setup();
    const p = base();
    render(HomeView, p);
    await u.click(screen.getByRole('switch', { name: /系统代理/ }));
    expect(p.onsystemproxychange).toHaveBeenCalledWith(true);
  });

  it('切换虚拟网卡触发 ontunchange', async () => {
    const u = userEvent.setup();
    const p = base();
    render(HomeView, p);
    await u.click(screen.getByRole('switch', { name: /虚拟网卡|TUN/ }));
    expect(p.ontunchange).toHaveBeenCalledWith(true);
  });
});

describe('首页 —— 分流模式卡片', () => {
  it('渲染与规则视图同一组预设选项，当前值正确', () => {
    render(HomeView, { ...base(), preset: 'china' });
    const group = screen.getByRole('radiogroup', { name: /分流预设/ });
    expect(within(group).getByRole('radio', { name: '中国大陆' })).toHaveAttribute('aria-checked', 'true');
  });

  it('切换预设触发 onpresetchange', async () => {
    const u = userEvent.setup();
    const p = base();
    render(HomeView, p);
    await u.click(screen.getByRole('radio', { name: '全局直连' }));
    expect(p.onpresetchange).toHaveBeenCalledWith('direct');
  });
});

describe('首页 —— 流量统计卡片', () => {
  it('显示上下行速率', () => {
    render(HomeView, { ...base(), downRate: 2048, upRate: 1024 });
    expect(screen.getByText(/2\.00 KB\/s|2 KB\/s/)).toBeInTheDocument();
  });

  it('sparkline 柱状条数与传入的采样点数一致', () => {
    const { container } = render(HomeView, { ...base(), spark: [1, 2, 3, 4, 5, 6] });
    expect(container.querySelectorAll('.spark i')).toHaveLength(6);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd ui && npx vitest run HomeView`
Expected: FAIL——`./HomeView.svelte` 还不存在。

- [ ] **Step 3: 写实现**

`ui/src/views/HomeView.svelte`：

```svelte
<script>
  /**
   * 首页（设计文档「代理组与首页视图」§3）。
   *
   * 四张卡片：节点选择、系统代理/虚拟网卡、分流模式、流量统计。
   * 视觉上延续既有设计令牌（borders-only、密度高、IBM Plex）——不引入
   * 圆角阴影卡片皮肤，参考 Clash Verge 的是「首页该放什么」这条功能分区，
   * 不是它的视觉风格（后者与本项目「密集像交易台，克制像 Proxyman」的
   * 定位正相反）。
   *
   * 「节点选择卡片」只列 kind === 'select' 的组——auto/load-balance
   * 没有用户手动可切的「当前值」，不该出现在这张卡片里。
   */
  import Segmented from '../lib/Segmented.svelte';
  import Switch from '../lib/Switch.svelte';
  import EmptyState from './EmptyState.svelte';
  import { bytes } from '../lib/format.js';

  let {
    /** proxy-groups 的全量列表（已脱敏 config 的一部分） */
    groups = [],
    colorOf,
    allowLan = false,
    systemProxy = false,
    tunEnabled = false,
    /** 与 RulesView 顶部同一份状态：direct / global / china / custom */
    preset = 'custom',
    /** 状态条已有的 sparkline 采样点，这里放大复用，不重新聚合 */
    spark = [],
    downRate = 0,
    upRate = 0,
    onselectmember = () => {},
    onsystemproxychange = () => {},
    ontunchange = () => {},
    onpresetchange = () => {},
  } = $props();

  const PRESET_OPTIONS = [
    { value: 'direct', label: '全局直连' },
    { value: 'global', label: '全局代理' },
    { value: 'china', label: '中国大陆' },
    { value: 'custom', label: '规则' },
  ];

  const selectGroups = $derived(groups.filter((g) => g.kind === 'select'));
  let activeGroupName = $state('');
  const activeGroup = $derived(
    selectGroups.find((g) => g.name === activeGroupName) ?? selectGroups[0] ?? null,
  );

  function pickGroup(name) {
    activeGroupName = name;
  }
  function pickMember(member) {
    if (activeGroup) onselectmember(activeGroup.name, member);
  }
</script>

<div class="home">
  <section class="card" aria-label="节点选择">
    <h2>节点选择</h2>
    {#if !selectGroups.length}
      <EmptyState
        title="还没有配置代理组。"
        hint="代理组让规则指向一个可切换的组而不是固定节点。在 config.yaml 的 proxy-groups 段加一个 kind: select 的组，填好 proxies 成员列表，这里就会出现对应的选择器。" />
    {:else}
      {#if selectGroups.length > 1}
        <label class="row">
          <span class="lbl">代理组</span>
          <select
            aria-label="代理组"
            value={activeGroup?.name}
            onchange={(e) => pickGroup(e.currentTarget.value)}>
            {#each selectGroups as g (g.name)}
              <option value={g.name}>{g.name}</option>
            {/each}
          </select>
        </label>
      {/if}
      {#if activeGroup}
        <label class="row">
          <span class="lbl">节点</span>
          <select
            aria-label="节点"
            value={activeGroup.selected}
            onchange={(e) => pickMember(e.currentTarget.value)}>
            {#each activeGroup.proxies as name (name)}
              <option value={name}>{name}</option>
            {/each}
          </select>
        </label>
      {/if}
    {/if}
  </section>

  <section class="card" aria-label="系统代理与虚拟网卡">
    <h2>系统代理 / 虚拟网卡</h2>
    <div class="row">
      <span class="lbl">系统代理</span>
      <Switch checked={systemProxy} label="系统代理" onchange={onsystemproxychange} />
    </div>
    <div class="row">
      <span class="lbl">虚拟网卡（TUN）</span>
      <Switch checked={tunEnabled} label="虚拟网卡（TUN）" onchange={ontunchange} />
    </div>
  </section>

  <section class="card" aria-label="分流模式">
    <h2>分流模式</h2>
    <Segmented label="分流预设" options={PRESET_OPTIONS} value={preset} onchange={onpresetchange} />
  </section>

  <section class="card" aria-label="流量统计">
    <h2>流量统计</h2>
    <div class="spark" aria-hidden="true">
      {#each spark as v, i (i)}
        <i style:height="{Math.max(1, Math.min(40, Math.log10(v + 1) * 6))}px"></i>
      {/each}
    </div>
    <p class="rates mono">
      <span>↓ {bytes(downRate)}/s</span>
      <span>↑ {bytes(upRate)}/s</span>
    </p>
  </section>
</div>

<style>
  .home {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: var(--space-3);
    padding: var(--space-4);
    background: var(--surface-1);
  }

  .card {
    border: 1px solid var(--border);
    border-radius: var(--radius);
    padding: var(--space-4);
    background: var(--surface-0);
  }
  .card h2 {
    margin: 0 0 var(--space-3);
    font-size: var(--fs-13);
    font-weight: var(--fw-semibold);
    color: var(--text-2);
  }

  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
    padding: 6px 0;
  }
  .lbl {
    font-size: var(--fs-12);
    color: var(--text-3);
  }

  select {
    background: var(--surface-2);
    color: var(--text-1);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius);
    padding: 4px 8px;
    font-family: inherit;
    font-size: var(--fs-12);
  }

  .spark {
    display: flex;
    align-items: flex-end;
    gap: 2px;
    height: 44px;
  }
  .spark i {
    display: block;
    width: 4px;
    background: var(--outbound-1);
    opacity: 0.7;
    border-radius: 1px;
  }
  .rates {
    display: flex;
    gap: var(--space-3);
    margin: var(--space-2) 0 0;
    font-size: var(--fs-12);
    color: var(--text-2);
  }
</style>
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd ui && npx vitest run HomeView`
Expected: 全部 PASS

- [ ] **Step 5: 提交**

```bash
git add ui/src/views/HomeView.svelte ui/src/views/HomeView.test.js
git commit -m "feat(ui): 首页四张卡片（节点选择/系统代理·TUN/分流模式/流量统计）"
```

---

### Task 10: `App.svelte` 接入首页

**Files:**
- Modify: `ui/src/App.svelte`
- Modify: `ui/src/App.test.js`

- [ ] **Step 1: 写失败的测试**

在 `ui/src/App.test.js` 里找一个已有的、渲染 `App` 并检查默认视图的测试
（若没有现成的，就近找一个已经在 mock `invoke`/`listen` 的测试作为参照写法），
加一条新测试：

```js
it('默认打开首页而不是流量视图', async () => {
  render(App);
  await waitFor(() => {
    expect(screen.getByRole('radiogroup', { name: /视图/ })).toBeInTheDocument();
  });
  const nav = screen.getByRole('radiogroup', { name: /视图/ });
  expect(within(nav).getByRole('radio', { name: '首页' })).toHaveAttribute('aria-checked', 'true');
});
```

（若文件顶部尚未 `import { within } from '@testing-library/svelte'`，加上这个
具名导入；若已有类似的 mock `invoke` 设置，复用该文件里现成的 `beforeEach`/
mock 工厂，不重新搭一套。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cd ui && npx vitest run App.test`
Expected: FAIL——`view` 的默认值目前是 `'traffic'`，`首页` 这个选项也还不存在。

- [ ] **Step 3: 接入**

`ui/src/App.svelte`：

1. 顶部 `import` 区加：
```js
  import HomeView from './views/HomeView.svelte';
```

2. 找到 `let view = $state('traffic');`（或类似的视图状态初始化），改成：
```js
  let view = $state('home');
```

3. 找到顶部 `Segmented` 的 `options` 数组（`{ value: 'traffic', label: '流量' }`
那一组），在最前面加一项：
```js
        { value: 'home', label: '首页' },
```

4. 找到 `{#if view === 'traffic'} <TrafficView .../> {:else if ...}` 这段，
在它之前加一个新分支：
```svelte
    {#if view === 'home'}
      <HomeView
        groups={config?.['proxy-groups'] ?? []}
        {colorOf}
        allowLan={config?.['allow-lan'] ?? false}
        systemProxy={config?.['system-proxy'] ?? false}
        tunEnabled={config?.tun?.enable ?? false}
        preset={routingPreset}
        {spark}
        downRate={status.downRate}
        upRate={status.upRate}
        onselectmember={saveGroupSelection}
        onsystemproxychange={(v) => saveSystemProxyOrTun('system-proxy', v)}
        ontunchange={(v) => saveSystemProxyOrTun('tun.enable', v)}
        onpresetchange={saveRoutingPreset} />
    {:else if view === 'traffic'}
```

- [ ] **Step 4: 补 `saveGroupSelection` 与 `saveSystemProxyOrTun` 两个处理函数**

在 `saveRoutingPreset` 函数（本会话之前已写好）之后加：

```js
  /**
   * 首页系统代理/TUN 快捷开关的保存路径。与 saveSettings/saveRoutingPreset
   * 同构——取原文 → setScalar 改一行 → config_save_raw 整份写回 → 重新加载。
   * 只有一个字段变化时不复用 saveSettings（它一次性收 4 个字段的 draft），
   * 避免首页的一次点击意外把设置覆盖层里可能还没提交的其他草稿也带上。
   */
  async function saveSystemProxyOrTun(key, value) {
    const raw = await call(configGetRaw);
    if (!raw.ok) {
      pushAlert(`切换失败（读不到配置原文）${whereOf(raw.error)}：${raw.error.message}`);
      return;
    }
    const text = setScalar(raw.value, key, value);
    const w = await call(configSaveRaw, text);
    if (!w.ok) {
      pushAlert(`切换失败${whereOf(w.error)}：${w.error.message}`);
      return;
    }
    await loadConfig();
  }

  /**
   * 首页「节点选择」卡片切换成员——走 setGroupSelected 而非 setScalar，
   * 见该函数存在的理由（同名 selected: 字段在好几个组块里各出现一次）。
   */
  async function saveGroupSelection(groupName, member) {
    const raw = await call(configGetRaw);
    if (!raw.ok) {
      pushAlert(`切换节点失败（读不到配置原文）${whereOf(raw.error)}：${raw.error.message}`);
      return;
    }
    let text;
    try {
      text = setGroupSelected(raw.value, groupName, member);
    } catch (e) {
      pushAlert(`切换节点失败：${e.message}`);
      return;
    }
    const w = await call(configSaveRaw, text);
    if (!w.ok) {
      pushAlert(`切换节点失败${whereOf(w.error)}：${w.error.message}`);
      return;
    }
    await loadConfig();
  }
```

回到 Step 3 的模板，`onsystemproxychange`/`ontunchange` 已直接写成最终形式，
无需再改。

最后在文件顶部 `import { setScalar, ... } from './lib/config-map.js';` 那一行
（若 `setGroupSelected` 还没被导入）里加上它：

```js
  import { setScalar, setGroupSelected, /* 原有的其余具名导入保留 */ } from './lib/config-map.js';
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cd ui && npx vitest run App.test`
Expected: 全部 PASS，无回归

- [ ] **Step 6: 跑全量前端测试**

Run: `cd ui && npx vitest run`
Expected: 全部 PASS

- [ ] **Step 7: 提交**

```bash
git add ui/src/App.svelte ui/src/App.test.js
git commit -m "feat(ui): 首页接入 App.svelte，成为默认视图"
```

---

## Part E — 可视化规则编辑器

### Task 11: `defaultInsertAnchor` 纯逻辑 + `RuleForm.svelte`

**Files:**
- Modify: `ui/src/lib/config-map.js`
- Modify: `ui/src/lib/config-map.test.js`
- Create: `ui/src/views/RuleForm.svelte`
- Create: `ui/src/views/RuleForm.test.js`

规则语法 `TYPE,VALUE,TARGET[,no-resolve]`，`MATCH` 例外为 `TYPE,TARGET`
（`crates/wsieve-route/src/rule.rs` 头部文档注释）。校验交给后端的
`Rule::parse`（Task 4 已经补上这个真实缺口），本表单只做「别提交空字段」
这一层最基本的把关。

- [ ] **Step 1: `defaultInsertAnchor` 的失败测试**

在 `ui/src/lib/config-map.test.js` 加：

```js
describe('defaultInsertAnchor', () => {
  it('规则列表为空时，锚点是 rules 键本身', () => {
    const a = defaultInsertAnchor([], 9, 'rules: []');
    expect(a).toEqual({ anchor: 9, anchorExpect: 'rules: []' });
  });

  it('末条不是 MATCH 时，新规则接在最后一条规则之后', () => {
    const rules = [
      { line: 10, raw: 'DOMAIN,a.com,proxyA' },
      { line: 11, raw: 'DOMAIN,b.com,proxyB', type: 'domain' },
    ];
    const a = defaultInsertAnchor(rules, 9, 'rules:');
    expect(a).toEqual({ anchor: 11, anchorExpect: 'DOMAIN,b.com,proxyB' });
  });

  it('末条是 MATCH 且前面还有别的规则时，新规则接在 MATCH 前一条之后', () => {
    const rules = [
      { line: 10, raw: 'DOMAIN,a.com,proxyA', type: 'domain' },
      { line: 11, raw: 'MATCH,proxyB', type: 'match' },
    ];
    const a = defaultInsertAnchor(rules, 9, 'rules:');
    expect(a).toEqual({ anchor: 10, anchorExpect: 'DOMAIN,a.com,proxyA' });
  });

  it('只有一条 MATCH 兜底时，新规则的锚点回退到 rules 键（成为新的第一条）', () => {
    const rules = [{ line: 10, raw: 'MATCH,proxyB', type: 'match' }];
    const a = defaultInsertAnchor(rules, 9, 'rules:');
    expect(a).toEqual({ anchor: 9, anchorExpect: 'rules:' });
  });
});

describe('ruleTypeToFormType', () => {
  it('把 parseRuleLine 产出的展示用短写映射回 RuleForm 认的规则类型', () => {
    expect(ruleTypeToFormType('domain')).toBe('DOMAIN');
    expect(ruleTypeToFormType('suffix')).toBe('DOMAIN-SUFFIX');
    expect(ruleTypeToFormType('keyword')).toBe('DOMAIN-KEYWORD');
    expect(ruleTypeToFormType('ip-cidr')).toBe('IP-CIDR');
    expect(ruleTypeToFormType('geosite')).toBe('GEOSITE');
    expect(ruleTypeToFormType('geoip')).toBe('GEOIP');
    expect(ruleTypeToFormType('match')).toBe('MATCH');
    expect(ruleTypeToFormType('final')).toBe('MATCH');
  });

  it('认不出的短写（解析失败的 "?"）保守地落到 DOMAIN，而不是抛异常打断编辑', () => {
    expect(ruleTypeToFormType('?')).toBe('DOMAIN');
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd ui && npx vitest run config-map`
Expected: FAIL——`defaultInsertAnchor` 还不存在。

- [ ] **Step 3: 写实现**

在 `ui/src/lib/config-map.js` 的 `setGroupSelected` 之后加：

```js
/**
 * 新增规则时默认的插入锚点——插在最后一条 MATCH 之前（若存在），否则接在
 * 末尾。理由：路由引擎的 `RuleSet::build` 要求 MATCH 必须是最后一条
 * （`RuleAfterMatch` 校验），插在它之后会被直接拒绝；`insert_rule_line`
 * 语义是「插在 anchor 行之后」，所以要选 MATCH **前一条**规则的行号当锚点，
 * 而不是 MATCH 自己的行号。
 *
 * 用户随后仍可以用既有的拖拽/Alt+↑↓ 把新规则挪到别的位置——这只是一个
 * 省得每次都要手动拖到底的默认值，不是强制位置。
 */
export function defaultInsertAnchor(rules, rulesKeyLine, rulesKeyText) {
  if (!rules.length) {
    return { anchor: rulesKeyLine, anchorExpect: rulesKeyText };
  }
  const lastIdx = rules.length - 1;
  const last = rules[lastIdx];
  if (last.type !== 'match') {
    return { anchor: last.line, anchorExpect: last.raw };
  }
  if (lastIdx === 0) {
    return { anchor: rulesKeyLine, anchorExpect: rulesKeyText };
  }
  const prev = rules[lastIdx - 1];
  return { anchor: prev.line, anchorExpect: prev.raw };
}

/**
 * `parseRuleLine` 产出的展示用短写（`domain` / `suffix` / `keyword` /
 * `ip-cidr` / `geosite` / `geoip` / `match` / `final` / `?`）→
 * `RuleForm` 的类型下拉认的大写规则类型。编辑一条已有规则时用来把
 * `rules[].type` 转回表单的初值。
 *
 * `?`（解析失败）没有对应的表单类型——保守落到 `DOMAIN`，让用户能打开
 * 表单把这条改成合法值，而不是抛异常拦住整个编辑入口。
 */
export function ruleTypeToFormType(t) {
  const MAP = {
    domain: 'DOMAIN',
    suffix: 'DOMAIN-SUFFIX',
    keyword: 'DOMAIN-KEYWORD',
    'ip-cidr': 'IP-CIDR',
    geosite: 'GEOSITE',
    geoip: 'GEOIP',
    match: 'MATCH',
    final: 'MATCH',
  };
  return MAP[t] ?? 'DOMAIN';
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd ui && npx vitest run config-map`
Expected: 全部 PASS

- [ ] **Step 5: 提交**

```bash
git add ui/src/lib/config-map.js ui/src/lib/config-map.test.js
git commit -m "feat(ui): defaultInsertAnchor / ruleTypeToFormType"
```

- [ ] **Step 6: `RuleForm.test.js` 的失败测试**

`ui/src/views/RuleForm.test.js`：

```js
import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import RuleForm from './RuleForm.svelte';

const base = () => ({
  open: true,
  mode: 'add',
  initial: null,
  outboundNames: ['日本节点', '香港节点'],
  groupNames: ['节点选择'],
  serverError: null,
  onsubmit: vi.fn(),
  onclose: vi.fn(),
});

describe('规则表单 —— 新增', () => {
  it('类型下拉有全部 7 种规则类型', () => {
    render(RuleForm, base());
    const opts = screen.getByLabelText('类型').querySelectorAll('option');
    const values = [...opts].map((o) => o.value);
    expect(values).toEqual([
      'DOMAIN', 'DOMAIN-SUFFIX', 'DOMAIN-KEYWORD', 'IP-CIDR', 'GEOSITE', 'GEOIP', 'MATCH',
    ]);
  });

  it('出站下拉包含出站名、组名与内置的 DIRECT/REJECT', () => {
    render(RuleForm, base());
    const opts = [...screen.getByLabelText('出站').querySelectorAll('option')].map((o) => o.value);
    expect(opts).toEqual(expect.arrayContaining(['日本节点', '香港节点', '节点选择', 'DIRECT', 'REJECT']));
  });

  it('类型为 MATCH 时不显示匹配值输入框', async () => {
    const u = userEvent.setup();
    render(RuleForm, base());
    await u.selectOptions(screen.getByLabelText('类型'), 'MATCH');
    expect(screen.queryByLabelText('匹配值')).not.toBeInTheDocument();
  });

  it('类型不是 IP-CIDR/GEOIP 时 no-resolve 复选框被禁用', () => {
    render(RuleForm, base());
    expect(screen.getByLabelText(/no-resolve/)).toBeDisabled();
  });

  it('切到 GEOIP 后 no-resolve 复选框可勾选', async () => {
    const u = userEvent.setup();
    render(RuleForm, base());
    await u.selectOptions(screen.getByLabelText('类型'), 'GEOIP');
    expect(screen.getByLabelText(/no-resolve/)).toBeEnabled();
  });

  it('提交时把字段拼成规则文本：TYPE,VALUE,TARGET', async () => {
    const u = userEvent.setup();
    const p = base();
    render(RuleForm, p);
    await u.selectOptions(screen.getByLabelText('类型'), 'DOMAIN-SUFFIX');
    await u.type(screen.getByLabelText('匹配值'), 'example.com');
    await u.selectOptions(screen.getByLabelText('出站'), '日本节点');
    await u.click(screen.getByRole('button', { name: '保存' }));
    expect(p.onsubmit).toHaveBeenCalledWith({ value: 'DOMAIN-SUFFIX,example.com,日本节点' });
  });

  it('MATCH 提交时拼成 TYPE,TARGET，没有中间的匹配值段', async () => {
    const u = userEvent.setup();
    const p = base();
    render(RuleForm, p);
    await u.selectOptions(screen.getByLabelText('类型'), 'MATCH');
    await u.selectOptions(screen.getByLabelText('出站'), 'DIRECT');
    await u.click(screen.getByRole('button', { name: '保存' }));
    expect(p.onsubmit).toHaveBeenCalledWith({ value: 'MATCH,DIRECT' });
  });

  it('勾选 no-resolve 后拼进第四段', async () => {
    const u = userEvent.setup();
    const p = base();
    render(RuleForm, p);
    await u.selectOptions(screen.getByLabelText('类型'), 'IP-CIDR');
    await u.type(screen.getByLabelText('匹配值'), '10.0.0.0/8');
    await u.selectOptions(screen.getByLabelText('出站'), 'DIRECT');
    await u.click(screen.getByLabelText(/no-resolve/));
    await u.click(screen.getByRole('button', { name: '保存' }));
    expect(p.onsubmit).toHaveBeenCalledWith({ value: 'IP-CIDR,10.0.0.0/8,DIRECT,no-resolve' });
  });

  it('匹配值为空时不提交，本地报错，不调用 onsubmit', async () => {
    const u = userEvent.setup();
    const p = base();
    render(RuleForm, p);
    await u.selectOptions(screen.getByLabelText('类型'), 'DOMAIN');
    await u.click(screen.getByRole('button', { name: '保存' }));
    expect(p.onsubmit).not.toHaveBeenCalled();
    expect(screen.getByRole('alert')).toHaveTextContent(/匹配值/);
  });

  it('后端校验失败时 serverError 原样显示，不是前端猜测出来的措辞', () => {
    render(RuleForm, { ...base(), serverError: '"DOMAIN,,DIRECT" 不是一条合法规则：匹配值不能为空' });
    expect(screen.getByRole('alert')).toHaveTextContent('不是一条合法规则');
  });

  it('取消按钮调用 onclose，不调用 onsubmit', async () => {
    const u = userEvent.setup();
    const p = base();
    render(RuleForm, p);
    await u.click(screen.getByRole('button', { name: '取消' }));
    expect(p.onclose).toHaveBeenCalled();
    expect(p.onsubmit).not.toHaveBeenCalled();
  });

  it('Escape 关闭', async () => {
    const u = userEvent.setup();
    const p = base();
    render(RuleForm, p);
    await u.keyboard('{Escape}');
    expect(p.onclose).toHaveBeenCalled();
  });
});

describe('规则表单 —— 编辑', () => {
  const editProps = () => ({
    ...base(),
    mode: 'edit',
    initial: { type: 'domain-suffix', value: 'old.com', target: '香港节点', noResolve: false },
  });

  it('字段预填自 initial', () => {
    render(RuleForm, editProps());
    expect(screen.getByLabelText('类型').value).toBe('DOMAIN-SUFFIX');
    expect(screen.getByLabelText('匹配值').value).toBe('old.com');
    expect(screen.getByLabelText('出站').value).toBe('香港节点');
  });

  it('标题与新增区分，按钮文案保持保存', () => {
    render(RuleForm, editProps());
    expect(screen.getByRole('heading')).toHaveTextContent('编辑规则');
  });
});
```

- [ ] **Step 7: 跑测试确认失败**

Run: `cd ui && npx vitest run RuleForm`
Expected: FAIL——`./RuleForm.svelte` 还不存在。

- [ ] **Step 8: 写实现**

`ui/src/views/RuleForm.svelte`：

```svelte
<script>
  /**
   * 规则新增/编辑表单（设计文档 §6.1）。
   *
   * 与 SettingsOverlay 同一套浮层骨架（scrim + 焦点陷阱 + Esc 关闭 + 焦点
   * 归还）——本项目没有把这套逻辑抽成共享组件，两处各自持有一份是刻意的
   * 现状，不是本次任务要收拾的技术债。
   *
   * **不在前端重新实现规则语法校验。** 这里只挡「必填字段是空的」——
   * 连按钮都点不出去的最基本情形。真正的语法校验（比如 IP-CIDR 的网段
   * 格式对不对）交给后端的 `Rule::parse`，失败原样通过 `serverError`
   * 显示，不猜测措辞。
   */
  import { tick, onDestroy, untrack } from 'svelte';

  const TYPES = ['DOMAIN', 'DOMAIN-SUFFIX', 'DOMAIN-KEYWORD', 'IP-CIDR', 'GEOSITE', 'GEOIP', 'MATCH'];
  const RESOLVABLE = new Set(['IP-CIDR', 'GEOIP']);
  const PLACEHOLDER = {
    DOMAIN: 'example.com',
    'DOMAIN-SUFFIX': 'example.com',
    'DOMAIN-KEYWORD': 'ads',
    'IP-CIDR': '10.0.0.0/8',
    GEOSITE: 'cn',
    GEOIP: 'CN',
  };

  let {
    open = false,
    /** 'add' | 'edit' */
    mode = 'add',
    /** 编辑时的初值：{ type, value, target, noResolve }（type 已是大写规则类型） */
    initial = null,
    outboundNames = [],
    groupNames = [],
    /** 上一次提交被后端拒绝时的原始错误文案 */
    serverError = null,
    onsubmit = () => {},
    onclose = () => {},
  } = $props();

  function blank() {
    return { type: 'DOMAIN', value: '', target: '', noResolve: false };
  }

  let dialog = $state(null);
  let draft = $state(initial ? { ...untrack(() => initial) } : blank());
  let localError = $state('');
  let restoreFocus = null;
  let wasOpen = false;

  function releaseFocus() {
    const el = restoreFocus;
    restoreFocus = null;
    if (el instanceof HTMLElement && document.contains(el)) el.focus();
  }

  $effect(() => {
    const isOpen = open;
    if (isOpen === wasOpen) return;
    wasOpen = isOpen;

    if (!isOpen) {
      releaseFocus();
      return;
    }

    draft = initial ? { ...initial } : blank();
    localError = '';
    restoreFocus = document.activeElement;

    tick().then(() => {
      dialog?.querySelector('select, input')?.focus();
    });
  });

  onDestroy(releaseFocus);

  const FOCUSABLE =
    'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])';

  function onKeydown(e) {
    if (e.key === 'Escape') {
      e.stopPropagation();
      onclose();
      return;
    }
    if (e.key !== 'Tab') return;
    const f = dialog?.querySelectorAll(FOCUSABLE);
    if (!f?.length) return;
    const first = f[0];
    const last = f[f.length - 1];
    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
  }

  const isMatch = $derived(draft.type === 'MATCH');
  const resolvable = $derived(RESOLVABLE.has(draft.type));

  $effect(() => {
    // 切到非 IP/GEOIP 类型时把 noResolve 一并清掉——不能留着一个用户看不见、
    // 但仍会被提交的隐藏勾选状态。
    if (!resolvable && draft.noResolve) draft.noResolve = false;
  });

  function composeValue() {
    // 先查匹配值、再查出站——两者都空时报「匹配值不能为空」，与用户在表单上
    // 从上到下遇到的第一个空字段一致，不会先报一个他还没扫到的字段。
    if (!isMatch && !draft.value.trim()) return { error: '匹配值不能为空。' };
    const target = draft.target.trim();
    if (!target) return { error: '出站不能为空。' };
    if (isMatch) return { value: `MATCH,${target}` };
    const tail = resolvable && draft.noResolve ? ',no-resolve' : '';
    return { value: `${draft.type},${draft.value.trim()},${target}${tail}` };
  }

  function submit() {
    const r = composeValue();
    if (r.error) {
      localError = r.error;
      return;
    }
    localError = '';
    onsubmit({ value: r.value });
  }
</script>

{#if open}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <div class="scrim" onclick={onclose} role="presentation"></div>

  <div
    class="panel"
    role="dialog"
    aria-modal="true"
    aria-label={mode === 'add' ? '新增规则' : '编辑规则'}
    bind:this={dialog}
    onkeydown={onKeydown}
    tabindex="-1">
    <header>
      <h2>{mode === 'add' ? '新增规则' : '编辑规则'}</h2>
      <button type="button" class="x" aria-label="关闭" onclick={onclose}>×</button>
    </header>

    <div class="body">
      <div class="field">
        <label for="rf-type">类型</label>
        <select id="rf-type" bind:value={draft.type}>
          {#each TYPES as t (t)}
            <option value={t}>{t}</option>
          {/each}
        </select>
      </div>

      {#if !isMatch}
        <div class="field">
          <label for="rf-value">匹配值</label>
          <input id="rf-value" type="text" class="mono" placeholder={PLACEHOLDER[draft.type]}
                 bind:value={draft.value} />
        </div>
      {/if}

      <div class="field">
        <label for="rf-target">出站</label>
        <select id="rf-target" bind:value={draft.target}>
          <option value="" disabled>选择出站或代理组…</option>
          <option value="DIRECT">DIRECT</option>
          <option value="REJECT">REJECT</option>
          {#each outboundNames as n (n)}
            <option value={n}>{n}</option>
          {/each}
          {#each groupNames as n (n)}
            <option value={n}>{n}</option>
          {/each}
        </select>
      </div>

      <div class="field row">
        <input id="rf-noresolve" type="checkbox" disabled={!resolvable} bind:checked={draft.noResolve} />
        <label for="rf-noresolve">no-resolve（仅 IP-CIDR / GEOIP 可用）</label>
      </div>

      {#if localError || serverError}
        <p class="err" role="alert">{localError || serverError}</p>
      {/if}
    </div>

    <footer>
      <button type="button" class="ghost" onclick={onclose}>取消</button>
      <button type="button" class="solid" onclick={submit}>保存</button>
    </footer>
  </div>
{/if}

<style>
  .scrim { position: fixed; inset: 0; background: rgba(0, 0, 0, .5); z-index: 10; }

  .panel {
    position: fixed;
    top: 50%; left: 50%;
    transform: translate(-50%, -50%);
    width: min(420px, calc(100vw - 48px));
    display: flex;
    flex-direction: column;
    background: var(--surface-1);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius);
    box-shadow: 0 0 0 1px rgba(0, 0, 0, .4), var(--shadow-overlay);
    z-index: 11;
  }

  header {
    display: flex;
    align-items: center;
    padding: 12px 16px;
    border-bottom: 1px solid var(--border);
    background: var(--surface-0);
  }
  h2 { margin: 0; font-size: var(--fs-14); font-weight: var(--fw-semibold); }

  .x {
    all: unset;
    margin-left: auto;
    padding: 0 6px;
    font-size: var(--fs-18);
    line-height: 1;
    color: var(--text-3);
    cursor: pointer;
  }
  .x:hover { color: var(--text-1); }
  .x:focus-visible { outline: 2px solid var(--outbound-1); outline-offset: 1px; }

  .body { padding: 12px 16px; }

  .field { margin-bottom: 10px; }
  .field.row { display: flex; align-items: center; gap: 8px; }
  .field.row label { margin: 0; }

  label { display: block; margin-bottom: 4px; font-size: var(--fs-12); color: var(--text-2); }

  input[type='text'], select {
    background: var(--surface-0);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius);
    padding: 6px 9px;
    color: var(--text-1);
    font-size: var(--fs-13);
    font-family: inherit;
    width: 100%;
  }

  .err {
    margin: 8px 0 0;
    padding: 8px 10px;
    border: 1px solid var(--state-fail);
    border-radius: var(--radius);
    color: var(--state-fail);
    font-size: var(--fs-12);
  }

  footer {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    padding: 12px 16px;
    border-top: 1px solid var(--border);
    background: var(--surface-0);
  }

  .ghost, .solid {
    border-radius: var(--radius);
    padding: 6px 14px;
    font-size: var(--fs-12);
    font-family: inherit;
    cursor: pointer;
  }
  .ghost { background: transparent; color: var(--text-2); border: 1px solid var(--border-strong); }
  .ghost:hover { color: var(--text-1); }
  .solid { background: var(--surface-2); color: var(--text-1); border: 1px solid var(--border-strong); }
  .solid:hover { border-color: rgba(255, 255, 255, .22); }
</style>
```

- [ ] **Step 9: 跑测试确认通过**

Run: `cd ui && npx vitest run RuleForm`
Expected: 全部 PASS

- [ ] **Step 10: 提交**

```bash
git add ui/src/views/RuleForm.svelte ui/src/views/RuleForm.test.js
git commit -m "feat(ui): 规则新增/编辑表单"
```

---

### Task 12: 把 `RuleForm` 接进 `RulesView` 与 `App.svelte`

**Files:**
- Modify: `ui/src/views/RulesView.svelte`
- Modify: `ui/src/views/RulesView.test.js`
- Modify: `ui/src/App.svelte`
- Modify: `ui/src/App.test.js`

- [ ] **Step 1: `RulesView` 的失败测试——工具栏与行内按钮**

在 `ui/src/views/RulesView.test.js` 里加：

```js
describe('规则的新增/编辑/删除入口', () => {
  const rules = [
    { id: 0, line: 10, raw: 'DOMAIN,a.com,日本节点', type: 'domain', value: 'a.com', target: '日本节点', hits: 0, enabled: true },
  ];

  it('非空列表时工具栏也有添加按钮，不是只有空状态才有', () => {
    render(RulesView, { rules, colorOf: () => '#fff', preset: 'custom', onadd: vi.fn() });
    expect(screen.getByRole('button', { name: /添加规则/ })).toBeInTheDocument();
  });

  it('每行都有编辑与删除按钮', () => {
    render(RulesView, { rules, colorOf: () => '#fff', preset: 'custom' });
    expect(screen.getByRole('button', { name: /编辑.*a\.com/ })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /删除.*a\.com/ })).toBeInTheDocument();
  });

  it('点编辑调用 onedit 并带上这条规则', async () => {
    const u = userEvent.setup();
    const onedit = vi.fn();
    render(RulesView, { rules, colorOf: () => '#fff', preset: 'custom', onedit });
    await u.click(screen.getByRole('button', { name: /编辑.*a\.com/ }));
    expect(onedit).toHaveBeenCalledWith(rules[0]);
  });

  it('点删除先要求二次确认，第二次点击才真的调用 ondelete', async () => {
    const u = userEvent.setup();
    const ondelete = vi.fn();
    render(RulesView, { rules, colorOf: () => '#fff', preset: 'custom', ondelete });
    const del = screen.getByRole('button', { name: /删除.*a\.com/ });
    await u.click(del);
    expect(ondelete).not.toHaveBeenCalled();
    await u.click(screen.getByRole('button', { name: /确认删除/ }));
    expect(ondelete).toHaveBeenCalledWith(rules[0]);
  });

  it('china/direct/global 预设下不显示添加按钮——那三条不是可编辑内容', () => {
    render(RulesView, { rules: [], colorOf: () => '#fff', preset: 'china', globalOutbound: '日本节点' });
    expect(screen.queryByRole('button', { name: /添加规则/ })).not.toBeInTheDocument();
  });
});
```

（若文件顶部尚未导入 `userEvent`，加上
`import userEvent from '@testing-library/user-event';`。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cd ui && npx vitest run RulesView`
Expected: FAIL——工具栏按钮、行内编辑/删除按钮都还不存在。

- [ ] **Step 3: 改 `RulesView.svelte`**

在 `let { ... } = $props();` 里加三个新 prop（紧跟在 `onadd` 之后）：

```js
    onedit = () => {},
    ondelete = () => {},
```

在 `<script>` 里、`chinaPresetRows` 之后加一个待确认删除的 id 追踪：

```js
  /** 待二次确认删除的规则 id；null 表示没有任何一行处在确认态 */
  let confirmingDelete = $state(null);

  function askDelete(id) {
    confirmingDelete = id;
  }
  function confirmDelete(r) {
    confirmingDelete = null;
    ondelete(r);
  }
  function cancelDelete() {
    confirmingDelete = null;
  }
```

找到 `{:else if !rules.length}`（空状态分支）与其后紧跟的 `{:else}`
（可编辑表格分支），只改**后者**——在可编辑表格分支的开头（`<table
class="rules-table">` 之前）加一个工具栏。**空状态分支不加**：
`EmptyState` 已经有自己的「添加第一条规则」action 按钮，两个「添加」
按钮同时出现在同一屏是重复而非补充。

```svelte
  {:else if !rules.length}
    <EmptyState
      title="还没有规则。"
      hint="规则决定流量往哪走，顺序即优先级 —— 首命中即返回。至少需要一条 MATCH 兜底，否则未命中的流量无处可去。"
      action="添加第一条规则"
      onaction={onadd} />
  {:else}
    <div class="toolbar">
      <button type="button" class="ghost" onclick={onadd}>+ 添加规则</button>
    </div>

    <!-- 排序结果的播报区。视觉上不可见，但对键盘路径是唯一的反馈通道。 -->
    <p class="sr-only" aria-live="polite" role="status">{announcement}</p>
```

（这两个分支本身已经存在于文件里——`{:else if !rules.length}` 那一段
原样不动；`{:else}` 那一段原本以 `<!-- 排序结果的播报区... -->` 开头，
这一步只是在它前面插入 `.toolbar`，播报区与后面的 `<table>` 都不动。）

表格结构加一列「操作」，在 `<thead>` 的最后一个 `<th>`（启用那列）之前插入：

```svelte
          <th scope="col"><span class="sr-only">操作</span></th>
```

在 `<tbody>` 的 `<tr>` 里、启用开关那个 `<td>` 之前插入：

```svelte
            <td class="ops">
              {#if confirmingDelete === r.id}
                <button type="button" class="mini danger" onclick={() => confirmDelete(r)}>确认删除</button>
                <button type="button" class="mini" aria-label="取消删除" onclick={cancelDelete}>取消</button>
              {:else}
                <button type="button" class="mini" aria-label={`编辑规则 ${ruleLabel(r)}`} onclick={() => onedit(r)}>编辑</button>
                <button type="button" class="mini danger" aria-label={`删除规则 ${ruleLabel(r)}`} onclick={() => askDelete(r.id)}>删除</button>
              {/if}
            </td>
```

在 `<style>` 末尾加：

```css
  .toolbar {
    display: flex;
    justify-content: flex-end;
    padding: 8px 16px;
    border-bottom: 1px solid var(--border);
    background: var(--surface-0);
  }

  .mini {
    background: transparent;
    color: var(--text-3);
    border: 1px solid var(--border);
    border-radius: 3px;
    padding: 2px 7px;
    font-size: var(--fs-11);
    font-family: inherit;
    cursor: pointer;
    margin-left: 4px;
  }
  .mini:hover { color: var(--text-1); border-color: var(--border-strong); }
  .mini.danger { color: var(--state-fail); border-color: rgba(255, 90, 90, .35); }
  .ops { text-align: right; white-space: nowrap; }
```

（`.toolbar` 复用了 `SettingsOverlay`/`RuleForm` 里出现过的 `.ghost` 按钮
类名与样式手感，但没有共享的样式表——每个组件的 `<style>` 是局部作用域，
`.ghost` 这个类名在别的文件里重复定义是既有惯例，不是重复劳动。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cd ui && npx vitest run RulesView`
Expected: 全部 PASS（含既有测试，无回归——之前 `toBeGreaterThanOrEqual(4)`
那条列数断言不是精确匹配，加一列不会破坏它）

- [ ] **Step 5: `App.svelte` 的失败测试——接线**

在 `ui/src/App.test.js` 加：

```js
it('规则「添加」打开新增表单，提交后调用 config_save 的 insert-rule 并重新加载配置', async () => {
  const u = userEvent.setup();
  render(App);
  await waitFor(() => screen.getByRole('radiogroup', { name: /视图/ }));
  await u.click(screen.getByRole('radio', { name: '规则' }));
  // 用 /添加/ 而非 /添加规则/：默认 mock 配置里规则是否为空未知，命中的可能是
  // 工具栏的「+ 添加规则」，也可能是空状态的「添加第一条规则」——两种文案都含
  // 「添加」，测试不该绑定某一种具体状态。
  await u.click(await screen.findByRole('button', { name: /添加/ }));
  await u.selectOptions(screen.getByLabelText('类型'), 'MATCH');
  await u.selectOptions(screen.getByLabelText('出站'), 'DIRECT');
  await u.click(screen.getByRole('button', { name: '保存' }));
  await waitFor(() => {
    expect(mockInvoke).toHaveBeenCalledWith(
      'config_save',
      expect.objectContaining({
        ops: [expect.objectContaining({ op: 'insert-rule', value: 'MATCH,DIRECT' })],
      }),
    );
  });
});
```

（`mockInvoke` 与 `waitFor` 沿用文件里已有的 mock 设施；若测试文件里
`invoke` 的 mock 变量名不同，改成与文件里实际一致的那个名字——不新起
一套 mock。）

- [ ] **Step 6: 跑测试确认失败**

Run: `cd ui && npx vitest run App.test`
Expected: FAIL——`RuleForm` 还没接进 `App.svelte`。

- [ ] **Step 7: 接线**

`ui/src/App.svelte`：

1. 顶部 `import` 加：
```js
  import RuleForm from './views/RuleForm.svelte';
```
并在 `config-map.js` 的具名导入列表里加 `defaultInsertAnchor` 与
`ruleTypeToFormType`。

2. `<RulesView>` 之外找到合适位置加新状态（跟 `settingsOpen` 挨着写）：
```js
  let ruleFormOpen = $state(false);
  let ruleFormMode = $state('add');
  let ruleFormInitial = $state(null);
  let ruleFormError = $state(null);
  /** 正在编辑的规则的行号/原文——ruleFormInitial 只有表单字段，不带这两样 */
  let editingRuleLine = $state(0);
  let editingRuleRaw = $state('');
  /** `rules:` 键本身的行号与原样文本，供新增规则时算默认插入锚点用 */
  let rulesKeyLine = $state(0);
  let rulesKeyText = $state('');
```

3. 找到 `loadConfig()`（本文件已有函数，不新建），在
`rules = (v.rules ?? []).map((r0, i) => parseRuleLine(r0, i, names));`
这一行之后加两行：

```js
    rulesKeyLine = v.rules_key_line ?? 0;
    rulesKeyText = v.rules_key_text ?? '';
```

（`ConfigView` 没有 `#[serde(rename_all = ...)]`，字段名原样序列化——
与既有的 `v.rules`/`v.config` 是同一种取法，不需要转 kebab-case。）

4. 在 `saveRoutingPreset` 之后加四个处理函数：

```js
  const outboundNames = $derived(outbounds.map((o) => o.name));
  const groupNames = $derived((config?.['proxy-groups'] ?? []).map((g) => g.name));

  function openAddRule() {
    ruleFormMode = 'add';
    ruleFormInitial = null;
    ruleFormError = null;
    ruleFormOpen = true;
  }

  function openEditRule(r) {
    ruleFormMode = 'edit';
    editingRuleLine = r.line;
    editingRuleRaw = r.raw;
    ruleFormInitial = {
      type: ruleTypeToFormType(r.type),
      value: r.value === '*' ? '' : r.value,
      target: r.target,
      noResolve: /,\s*no-resolve\s*$/i.test(r.raw ?? ''),
    };
    ruleFormError = null;
    ruleFormOpen = true;
  }

  function closeRuleForm() {
    ruleFormOpen = false;
  }

  /**
   * 规则表单提交——新增走 InsertRule，编辑走既有的 ReplaceRule。
   * 两者都是 `config_save` 的 ops，插入锚点由 `defaultInsertAnchor` 算，
   * 依据见该函数的文档注释：接在最后一条 MATCH 之前。编辑锚定的是
   * `openEditRule` 打开表单那一刻记下的 `editingRuleLine`/`editingRuleRaw`——
   * 不按字段内容重新在 `rules` 里查找，因为改了值之后内容本身就对不上了。
   */
  async function submitRuleForm({ value }) {
    const op =
      ruleFormMode === 'add'
        ? (() => {
            const a = defaultInsertAnchor(rules, rulesKeyLine, rulesKeyText);
            return { op: 'insert-rule', anchor: a.anchor, 'anchor-expect': a.anchorExpect, value };
          })()
        : { op: 'replace-rule', line: editingRuleLine, expect: editingRuleRaw, value };

    const r = await call(configSave, [op]);
    if (!r.ok) {
      ruleFormError = r.error.message;
      return;
    }
    ruleFormOpen = false;
    await loadConfig();
  }

  async function deleteRule(r) {
    const res = await call(configSave, [{ op: 'delete-rule', line: r.line, expect: r.raw }]);
    if (!res.ok) {
      saveError = res.error;
      return;
    }
    saveError = null;
    await loadConfig();
  }
```

5. 找到 `<RulesView>` 的调用处，加上新 props：

```svelte
      <RulesView
        {rules}
        {colorOf}
        preset={routingPreset}
        {globalOutbound}
        probe={probe}
        probeError={probeError}
        saveError={saveError}
        ontest={testRule}
        onreorder={reorder}
        ontoggle={toggleRule}
        onadd={openAddRule}
        onedit={openEditRule}
        ondelete={deleteRule}
        onpresetchange={saveRoutingPreset} />
      <RuleForm
        open={ruleFormOpen}
        mode={ruleFormMode}
        initial={ruleFormInitial}
        outboundNames={outboundNames}
        groupNames={groupNames}
        serverError={ruleFormError}
        onsubmit={submitRuleForm}
        onclose={closeRuleForm} />
```

（保留原本已有的那些 prop——上面只是把 `onadd`/新增的 `onedit`/`ondelete`
与紧跟着的 `<RuleForm>` 一并列出；原文件里 `<RulesView>` 具体是自闭合还是
多行、`probe`/`saveError` 具体怎么传，照原文件已有的写法保留，不要因为
这里列出来就整段替换掉原有格式。）

- [ ] **Step 8: 跑测试确认通过**

Run: `cd ui && npx vitest run App.test RulesView RuleForm`
Expected: 全部 PASS

- [ ] **Step 9: 跑全量前端测试**

Run: `cd ui && npx vitest run`
Expected: 全部 PASS

- [ ] **Step 10: 提交**

```bash
git add ui/src/views/RulesView.svelte ui/src/views/RulesView.test.js ui/src/App.svelte ui/src/App.test.js
git commit -m "feat(ui): 规则视图接入新增/编辑/删除，App.svelte 完成接线"
```

---

## Part F — 可视化节点编辑器

### Task 13: `ProxyForm.svelte`

**Files:**
- Create: `ui/src/views/ProxyForm.svelte`
- Create: `ui/src/views/ProxyForm.test.js`

设计文档 §6.2 的安全边界：`client-priv` 第一次需要用户手动输入到一个
`<input>` 里，输入框必须 `type="password"` 掩码，明文只活在这个组件的
局部 `$state`，提交或取消后立刻清空，不打进任何 `console`/错误上报。

- [ ] **Step 1: 失败的测试**

`ui/src/views/ProxyForm.test.js`：

```js
import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import ProxyForm from './ProxyForm.svelte';

const base = () => ({
  open: true,
  serverError: null,
  onsubmit: vi.fn(),
  onclose: vi.fn(),
});

describe('节点新增表单', () => {
  it('私钥输入框是 password 类型', () => {
    render(ProxyForm, base());
    expect(screen.getByLabelText('client-priv')).toHaveAttribute('type', 'password');
  });

  it('展示与 SettingsOverlay 导出提示同源的安全提示语', () => {
    render(ProxyForm, base());
    expect(screen.getByText(/私钥仅受文件系统权限保护/)).toBeInTheDocument();
  });

  it('必填字段为空时不提交，本地报错', async () => {
    const u = userEvent.setup();
    const p = base();
    render(ProxyForm, p);
    await u.click(screen.getByRole('button', { name: '保存' }));
    expect(p.onsubmit).not.toHaveBeenCalled();
    expect(screen.getByRole('alert')).toBeInTheDocument();
  });

  it('填完必填字段提交时拼出固定格式的行数组', async () => {
    const u = userEvent.setup();
    const p = base();
    render(ProxyForm, p);
    await u.type(screen.getByLabelText('名称'), '日本节点');
    await u.type(screen.getByLabelText('地址（url）'), 'https://example.com/');
    await u.type(screen.getByLabelText('server-pub'), 'aa==');
    await u.type(screen.getByLabelText('client-priv'), 'bb==');
    await u.click(screen.getByRole('button', { name: '保存' }));
    expect(p.onsubmit).toHaveBeenCalledWith([
      '- name: "日本节点"',
      '  type: websieve',
      '  url: https://example.com/',
      '  server-pub: "aa=="',
      '  client-priv: "bb=="',
    ]);
  });

  it('提交后立刻清空私钥输入框，不留在 DOM 里', async () => {
    const u = userEvent.setup();
    const p = base();
    render(ProxyForm, p);
    await u.type(screen.getByLabelText('名称'), 'x');
    await u.type(screen.getByLabelText('地址（url）'), 'https://x/');
    await u.type(screen.getByLabelText('server-pub'), 'aa');
    await u.type(screen.getByLabelText('client-priv'), 'bb');
    await u.click(screen.getByRole('button', { name: '保存' }));
    expect(screen.getByLabelText('client-priv').value).toBe('');
  });

  it('取消时也清空私钥输入框', async () => {
    const u = userEvent.setup();
    const p = base();
    render(ProxyForm, p);
    await u.type(screen.getByLabelText('client-priv'), 'bb');
    await u.click(screen.getByRole('button', { name: '取消' }));
    expect(screen.getByLabelText('client-priv').value).toBe('');
  });

  it('展开「高级」后可填 extra-sessions，留空则不出现在提交的行里', async () => {
    const u = userEvent.setup();
    const p = base();
    render(ProxyForm, p);
    await u.type(screen.getByLabelText('名称'), '日本节点');
    await u.type(screen.getByLabelText('地址（url）'), 'https://example.com/');
    await u.type(screen.getByLabelText('server-pub'), 'aa');
    await u.type(screen.getByLabelText('client-priv'), 'bb');
    await u.click(screen.getByRole('button', { name: '高级' }));
    await u.type(screen.getByLabelText('extra-sessions'), '4');
    await u.click(screen.getByRole('button', { name: '保存' }));
    expect(p.onsubmit).toHaveBeenCalledWith(
      expect.arrayContaining(['  extra-sessions: 4']),
    );
  });

  it('后端报错（如名字冲突）原样显示', () => {
    render(ProxyForm, { ...base(), serverError: '名字 "日本节点" 已经被一个出站或代理组占用，换一个名字' });
    expect(screen.getByRole('alert')).toHaveTextContent('已经被一个出站或代理组占用');
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd ui && npx vitest run ProxyForm`
Expected: FAIL——`./ProxyForm.svelte` 还不存在。

- [ ] **Step 3: 写实现**

`ui/src/views/ProxyForm.svelte`：

```svelte
<script>
  /**
   * 节点（出站服务器）新增表单（设计文档 §6.2）。
   *
   * v1 只做新增/删除，不做编辑已有节点的字段——`proxies` 里的项没有行号
   * 追踪（不像规则是 `Vec<Spanned<String>>`），要支持编辑得先给它上 span，
   * 是比这次大一截的改动。轮换私钥的路径现在是「删除重加」。
   *
   * ## 私钥这次真的要经过渲染层，必须显式承认
   *
   * `client-priv` 在这里第一次需要用户手动输入到一个 `<input>` 里。处置：
   * `type="password"` 掩码；明文只活在本组件的局部 `$state`（`draft`）；
   * 提交成功或取消**立刻清空**；不打进任何 `console`/`pushAlert`/错误上报。
   * 与 `SettingsOverlay` 导出配置时的既有警告同源，提交前展示同一句忠告。
   */
  import { tick, onDestroy, untrack } from 'svelte';

  let {
    open = false,
    serverError = null,
    onsubmit = () => {},
    onclose = () => {},
  } = $props();

  function blank() {
    return { name: '', url: '', serverPub: '', clientPriv: '', extraSessions: '', advanced: false };
  }

  let dialog = $state(null);
  let draft = $state(blank());
  let localError = $state('');
  let restoreFocus = null;
  let wasOpen = false;

  function releaseFocus() {
    const el = restoreFocus;
    restoreFocus = null;
    if (el instanceof HTMLElement && document.contains(el)) el.focus();
  }

  /** 私钥必须清干净——不只是重置整个 draft，是这一步存在的唯一理由被单独点名出来。 */
  function wipe() {
    draft = blank();
  }

  $effect(() => {
    const isOpen = open;
    if (isOpen === wasOpen) return;
    wasOpen = isOpen;

    if (!isOpen) {
      releaseFocus();
      wipe();
      return;
    }

    localError = '';
    restoreFocus = document.activeElement;
    tick().then(() => {
      dialog?.querySelector('input')?.focus();
    });
  });

  onDestroy(releaseFocus);

  const FOCUSABLE =
    'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])';

  function onKeydown(e) {
    if (e.key === 'Escape') {
      e.stopPropagation();
      handleClose();
      return;
    }
    if (e.key !== 'Tab') return;
    const f = dialog?.querySelectorAll(FOCUSABLE);
    if (!f?.length) return;
    const first = f[0];
    const last = f[f.length - 1];
    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
  }

  function handleClose() {
    wipe();
    onclose();
  }

  function submit() {
    const name = draft.name.trim();
    const url = draft.url.trim();
    const serverPub = draft.serverPub.trim();
    const clientPriv = draft.clientPriv;
    if (!name || !url || !serverPub || !clientPriv) {
      localError = '名称、地址、server-pub、client-priv 都是必填项。';
      return;
    }
    localError = '';
    const lines = [
      `- name: "${name}"`,
      '  type: websieve',
      `  url: ${url}`,
      `  server-pub: "${serverPub}"`,
      `  client-priv: "${clientPriv}"`,
    ];
    const es = draft.extraSessions.trim();
    if (es) lines.push(`  extra-sessions: ${es}`);
    onsubmit(lines);
    wipe();
  }
</script>

{#if open}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <div class="scrim" onclick={handleClose} role="presentation"></div>

  <div
    class="panel"
    role="dialog"
    aria-modal="true"
    aria-label="新增服务器"
    bind:this={dialog}
    onkeydown={onKeydown}
    tabindex="-1">
    <header>
      <h2>新增服务器</h2>
      <button type="button" class="x" aria-label="关闭" onclick={handleClose}>×</button>
    </header>

    <div class="body">
      <div class="field">
        <label for="pf-name">名称</label>
        <input id="pf-name" type="text" bind:value={draft.name} />
      </div>
      <div class="field">
        <label for="pf-url">地址（url）</label>
        <input id="pf-url" type="text" class="mono" placeholder="https://example.com/" bind:value={draft.url} />
      </div>
      <div class="field">
        <label for="pf-pub">server-pub</label>
        <input id="pf-pub" type="text" class="mono" bind:value={draft.serverPub} />
      </div>
      <div class="field">
        <label for="pf-priv">client-priv</label>
        <input id="pf-priv" type="password" class="mono" bind:value={draft.clientPriv} />
      </div>

      <p class="hint warn">
        私钥仅受文件系统权限保护，确认来源可信后再提交。
      </p>

      <button type="button" class="ghost adv-toggle" onclick={() => (draft.advanced = !draft.advanced)}>
        高级
      </button>
      {#if draft.advanced}
        <div class="field">
          <label for="pf-sessions">extra-sessions</label>
          <input id="pf-sessions" type="number" min="0" class="mono" bind:value={draft.extraSessions} />
        </div>
      {/if}

      {#if localError || serverError}
        <p class="err" role="alert">{localError || serverError}</p>
      {/if}
    </div>

    <footer>
      <button type="button" class="ghost" onclick={handleClose}>取消</button>
      <button type="button" class="solid" onclick={submit}>保存</button>
    </footer>
  </div>
{/if}

<style>
  .scrim { position: fixed; inset: 0; background: rgba(0, 0, 0, .5); z-index: 10; }

  .panel {
    position: fixed;
    top: 50%; left: 50%;
    transform: translate(-50%, -50%);
    width: min(420px, calc(100vw - 48px));
    max-height: calc(100vh - 64px);
    display: flex;
    flex-direction: column;
    background: var(--surface-1);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius);
    box-shadow: 0 0 0 1px rgba(0, 0, 0, .4), var(--shadow-overlay);
    z-index: 11;
  }

  header {
    display: flex;
    align-items: center;
    padding: 12px 16px;
    border-bottom: 1px solid var(--border);
    background: var(--surface-0);
  }
  h2 { margin: 0; font-size: var(--fs-14); font-weight: var(--fw-semibold); }

  .x {
    all: unset;
    margin-left: auto;
    padding: 0 6px;
    font-size: var(--fs-18);
    line-height: 1;
    color: var(--text-3);
    cursor: pointer;
  }
  .x:hover { color: var(--text-1); }
  .x:focus-visible { outline: 2px solid var(--outbound-1); outline-offset: 1px; }

  .body { padding: 12px 16px; overflow-y: auto; }

  .field { margin-bottom: 10px; }
  label { display: block; margin-bottom: 4px; font-size: var(--fs-12); color: var(--text-2); }

  input[type='text'], input[type='password'], input[type='number'] {
    background: var(--surface-0);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius);
    padding: 6px 9px;
    color: var(--text-1);
    font-size: var(--fs-13);
    font-family: inherit;
    width: 100%;
  }

  .hint {
    margin: 6px 0 10px;
    font-size: var(--fs-11);
    color: var(--text-4);
    line-height: 1.65;
  }
  .hint.warn { color: var(--state-warn); }

  .adv-toggle { margin-bottom: 10px; }

  .err {
    margin: 8px 0 0;
    padding: 8px 10px;
    border: 1px solid var(--state-fail);
    border-radius: var(--radius);
    color: var(--state-fail);
    font-size: var(--fs-12);
  }

  footer {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    padding: 12px 16px;
    border-top: 1px solid var(--border);
    background: var(--surface-0);
  }

  .ghost, .solid {
    border-radius: var(--radius);
    padding: 6px 14px;
    font-size: var(--fs-12);
    font-family: inherit;
    cursor: pointer;
  }
  .ghost { background: transparent; color: var(--text-2); border: 1px solid var(--border-strong); }
  .ghost:hover { color: var(--text-1); }
  .solid { background: var(--surface-2); color: var(--text-1); border: 1px solid var(--border-strong); }
  .solid:hover { border-color: rgba(255, 255, 255, .22); }
</style>
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd ui && npx vitest run ProxyForm`
Expected: 全部 PASS

- [ ] **Step 5: 提交**

```bash
git add ui/src/views/ProxyForm.svelte ui/src/views/ProxyForm.test.js
git commit -m "feat(ui): 节点新增表单，私钥掩码输入 + 提交即清空"
```

---

### Task 14: 把 `ProxyForm` 接进 `OutboundsView` 与 `App.svelte`

**Files:**
- Modify: `ui/src/views/OutboundsView.svelte`
- Modify: `ui/src/views/OutboundsView.test.js`
- Modify: `ui/src/lib/ipc.js`
- Modify: `ui/src/lib/ipc.test.js`（若不存在则跳过——先确认文件是否存在）
- Modify: `ui/src/App.svelte`
- Modify: `ui/src/App.test.js`

- [ ] **Step 1: `ipc.js` 加两个新绑定**

在 `ui/src/lib/ipc.js` 的 `configSaveRaw` 之后加：

```js
export function configInsertProxy(lines) {
  return invoke('config_insert_proxy', { lines });
}

export function configDeleteProxy(name) {
  return invoke('config_delete_proxy', { name });
}
```

- [ ] **Step 2: `OutboundsView` 的失败测试——删除入口**

在 `ui/src/views/OutboundsView.test.js` 加：

```js
describe('节点的删除入口', () => {
  const outbounds = [{ id: '日本节点', name: '日本节点', state: 'live', latency: 40, sessions: 2, enabled: true }];

  it('每行有删除按钮，点击先要求二次确认', async () => {
    const u = userEvent.setup();
    const ondelete = vi.fn();
    render(OutboundsView, { outbounds, colorOf: () => '#fff', ondelete });
    await u.click(screen.getByRole('button', { name: /删除.*日本节点/ }));
    expect(ondelete).not.toHaveBeenCalled();
    await u.click(screen.getByRole('button', { name: /确认删除/ }));
    expect(ondelete).toHaveBeenCalledWith('日本节点');
  });
});
```

（若文件顶部尚未 `import userEvent from '@testing-library/user-event';`，加上。）

- [ ] **Step 3: 跑测试确认失败**

Run: `cd ui && npx vitest run OutboundsView`
Expected: FAIL——删除按钮还不存在。

- [ ] **Step 4: 改 `OutboundsView.svelte`**

`let { ... } = $props();` 里加：

```js
    ondelete = () => {},
```

`<script>` 里加确认态追踪（与 `RulesView` 的 `confirmingDelete` 同构）：

```js
  let confirmingDelete = $state(null);
  function askDelete(id) {
    confirmingDelete = id;
  }
  function confirmDelete(name) {
    confirmingDelete = null;
    ondelete(name);
  }
  function cancelDelete() {
    confirmingDelete = null;
  }
```

`<thead>` 的最后一个 `<th>`（启用那列）之前加：

```svelte
          <th scope="col"><span class="sr-only">删除</span></th>
```

`<tbody>` 的启用开关 `<td>` 之前加：

```svelte
            <td class="c">
              {#if confirmingDelete === o.id}
                <button type="button" class="mini danger" onclick={() => confirmDelete(o.name)}>确认删除</button>
                <button type="button" class="mini" aria-label="取消删除" onclick={cancelDelete}>取消</button>
              {:else}
                <button type="button" class="mini danger" aria-label={`删除节点 ${o.name}`} onclick={() => askDelete(o.id)}>删除</button>
              {/if}
            </td>
```

`<style>` 末尾加（与 `RulesView` 的 `.mini`/`.mini.danger` 同款，两处各自
定义、不共享，理由同 Task 12 Step 3 的说明）：

```css
  .mini {
    background: transparent;
    color: var(--text-3);
    border: 1px solid var(--border);
    border-radius: 3px;
    padding: 2px 7px;
    font-size: var(--fs-11);
    font-family: inherit;
    cursor: pointer;
    margin-left: 4px;
  }
  .mini:hover { color: var(--text-1); border-color: var(--border-strong); }
  .mini.danger { color: var(--state-fail); border-color: rgba(255, 90, 90, .35); }
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cd ui && npx vitest run OutboundsView`
Expected: 全部 PASS

- [ ] **Step 6: `App.svelte` 的失败测试——接线**

在 `ui/src/App.test.js` 加：

```js
it('出站「添加」打开新增表单，提交后调用 config_insert_proxy 并重新加载配置', async () => {
  const u = userEvent.setup();
  render(App);
  await waitFor(() => screen.getByRole('radiogroup', { name: /视图/ }));
  await u.click(screen.getByRole('radio', { name: '出站' }));
  await u.click(await screen.findByRole('button', { name: /添加/ }));
  await u.type(screen.getByLabelText('名称'), '测试节点');
  await u.type(screen.getByLabelText('地址（url）'), 'https://example.com/');
  await u.type(screen.getByLabelText('server-pub'), 'aa');
  await u.type(screen.getByLabelText('client-priv'), 'bb');
  await u.click(screen.getByRole('button', { name: '保存' }));
  await waitFor(() => {
    expect(mockInvoke).toHaveBeenCalledWith('config_insert_proxy', expect.objectContaining({ lines: expect.any(Array) }));
  });
});
```

- [ ] **Step 7: 跑测试确认失败**

Run: `cd ui && npx vitest run App.test`
Expected: FAIL——`ProxyForm` 还没接进 `App.svelte`，`OutboundsView` 也还没有添加按钮。

- [ ] **Step 8: 补 `OutboundsView` 的空列表之外的添加按钮**

现有的 `OutboundsView.svelte` 只在空状态的 `EmptyState` 里给了
`onaction={onadd}`（见组件当前实现）。非空列表时同样需要一个入口——在
`<section class="view" ...>` 内、`{#if !outbounds.length}` 判断**之前**加：

```svelte
  {#if outbounds.length}
    <div class="toolbar">
      <button type="button" class="ghost" onclick={onadd}>+ 添加服务器</button>
    </div>
  {/if}
```

`<style>` 里加（与 `RulesView` 的 `.toolbar` 同款）：

```css
  .toolbar {
    display: flex;
    justify-content: flex-end;
    padding: 8px 16px;
    border-bottom: 1px solid var(--border);
    background: var(--surface-0);
  }
```

- [ ] **Step 9: `App.svelte` 接线**

1. 顶部 `import` 加：
```js
  import ProxyForm from './views/ProxyForm.svelte';
```
并在 `ipc.js` 的具名导入列表里加 `configInsertProxy, configDeleteProxy`。

2. 新状态（跟 `ruleFormOpen` 那组挨着写）：
```js
  let proxyFormOpen = $state(false);
  let proxyFormError = $state(null);
```

3. 处理函数（跟规则表单那组挨着写）：

```js
  function openAddProxy() {
    proxyFormError = null;
    proxyFormOpen = true;
  }
  function closeProxyForm() {
    proxyFormOpen = false;
  }

  async function submitProxyForm(lines) {
    const r = await call(configInsertProxy, lines);
    if (!r.ok) {
      proxyFormError = r.error.message;
      return;
    }
    proxyFormOpen = false;
    await loadConfig();
  }

  async function deleteProxy(name) {
    const r = await call(configDeleteProxy, name);
    if (!r.ok) {
      toggleError = r.error;
      return;
    }
    toggleError = null;
    await loadConfig();
  }
```

4. `<OutboundsView>` 的调用处加 `onadd={openAddProxy}`、`ondelete={deleteProxy}`，
紧跟着加 `<ProxyForm>`：

```svelte
      <OutboundsView
        {outbounds}
        {colorOf}
        toggleError={toggleError}
        probeError={latencyError}
        ontoggle={toggleOutbound}
        onprobe={probeLatency}
        onadd={openAddProxy}
        ondelete={deleteProxy} />
      <ProxyForm
        open={proxyFormOpen}
        serverError={proxyFormError}
        onsubmit={submitProxyForm}
        onclose={closeProxyForm} />
```

（同 Task 12 Step 7 的说明：原文件里 `<OutboundsView>` 已有的 prop 照抄
保留，只是把新增的 `onadd`/`ondelete` 与紧跟着的 `<ProxyForm>` 并入。）

- [ ] **Step 10: 跑测试确认通过**

Run: `cd ui && npx vitest run App.test OutboundsView ProxyForm`
Expected: 全部 PASS

- [ ] **Step 11: 跑全量前端测试**

Run: `cd ui && npx vitest run`
Expected: 全部 PASS

- [ ] **Step 12: 提交**

```bash
git add ui/src/views/OutboundsView.svelte ui/src/views/OutboundsView.test.js \
        ui/src/lib/ipc.js ui/src/App.svelte ui/src/App.test.js
git commit -m "feat(ui): 出站视图接入新增/删除，App.svelte 完成接线"
```

---

## 收尾

### Task 15: 全量验证

**Files:** 无新文件——本任务只跑命令、走查、必要时回头修。

- [ ] **Step 1: 全量 Rust 测试**

Run:
```bash
cargo test -p wsieve-config
cargo test -p wsieve-route
cargo test --manifest-path src-tauri/Cargo.toml
```
Expected: 三条命令全部 PASS，包括 Part A–D 新增的全部测试
（schema 校验、`group.rs` 的 `auto_pick`/`load_balance_pick`、
`insert_rule_line`、`append_proxy_block`/`delete_proxy_block`、
新增的四个 IPC 命令、`capability_isolation`）。

- [ ] **Step 2: 全量前端测试**

Run: `cd ui && npx vitest run`
Expected: 全部 PASS——含本计划新增的 `HomeView`/`RuleForm`/`ProxyForm`
三个新组件、`config-map.js` 的 `setGroupSelected`/`defaultInsertAnchor`、
`RulesView`/`OutboundsView`/`App.svelte` 的接线测试，以及此前既有的全部
测试（无回归）。

- [ ] **Step 3: 编译真实应用**

Run: `cargo build --manifest-path src-tauri/Cargo.toml`
Expected: 编译通过，产出 `src-tauri/target/debug/wsieve-app.exe`。

- [ ] **Step 4: 用 `windows-control` MCP 工具做一次真机走查**

用本会话已经用过的方式启动（带 `WSIEVE_SERVER_PUB`/`WSIEVE_CLIENT_PRIV`/
`WSIEVE_OUTBOUND_NAME` 三个占位环境变量），依次截图/取 UI 元素确认：

1. 默认打开的是「首页」，顶部分段控件里「首页」被选中。
2. 首页四张卡片都渲染：没有代理组时「节点选择」卡片显示引导文案；
   系统代理/TUN 两个开关能点、点击后 `config.yaml` 里对应字段真的改了；
   分流模式与「规则」视图顶部的预设选择器保持同步（在一处切换，
   另一处再打开时显示同一个值）。
3. 「规则」视图：非空列表时工具栏出现「+ 添加规则」；点开新增表单，
   选类型/填值/选出站，保存后新规则出现在列表里、`config.yaml`
   对应位置真的多了一行、既有内容与注释未被破坏；对着一条已有规则点
   「编辑」，表单字段正确预填，改完保存后该行的值被替换；点「删除」
   走二次确认，确认后该行从文件与列表里消失。
4. 「出站」视图：点「+ 添加服务器」，填完名称/url/server-pub/client-priv，
   保存后 `proxies:` 列表里出现新块，字段格式与手写的一致；对着新增的
   节点点「删除」，走二次确认，确认后该块从文件里消失。
5. 用一份手写了 `proxy-groups`（一个 `select`、一个 `auto`）的
   `config.yaml` 重启应用，回到首页确认「节点选择」卡片正确列出那个
   `select` 组的成员，切换成员后 `config.yaml` 里对应组块的
   `selected:` 字段真的改了、且没有影响到另一个 `auto` 组的任何一行。

若走查中发现任何一步与预期不符，回到对应 Task 修 Bug（走正常的
「写失败测试复现 → 修 → 测试转绿」流程），不要为了让走查通过而跳过
后端校验或前端本地校验。

- [ ] **Step 5: 最终确认没有遗留的未提交改动**

Run: `git status`
Expected: working tree clean（Step 1–4 若有修复性改动，应已在各自的
修复步骤里提交）。




