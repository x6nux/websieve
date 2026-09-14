# 代理组（select / auto / load-balance）与首页视图 — 设计

## 背景与动机

用户手动测试控制窗口时，参照本机已装的 Clash Verge 的首页布局，希望 websieve
也有一个「首页」——集中展示当前节点、系统代理/TUN 快捷开关、分流模式、流量
概览，而不必在三个独立视图之间来回切换。

Clash Verge 的「当前节点」卡片背后是 Clash 的**代理组**（proxy-group）机制：
规则不直接点名一个物理节点，而是点名一个组（例如「节点选择」），组内部再决定
实际走哪个成员。用户在讨论过程中明确要求把这个机制本身也做出来，并给了三种
组类型：手动选择（select）、自动选优（auto）、负载均衡（load-balance）。
一开始设计成组用 `id` 区分身份、`name` 仅做展示，后来简化掉了——见 §1
「只用 `name`」的说明。

讨论中一度扩展到「识别流量行为（下载/浏览）动态选策略」与「自动探测出口 IP
分组」——这两项已经在讨论中达成一致**不在本次范围内**：前者在当前的 SOCKS5/
CONNECT 隧道层面看不到 HTTP 语义，只能靠不可靠的连接数量启发式猜测；后者是一
个独立的、需要主动探测基础设施的子系统。负载均衡本轮只做两种标准、可测试的
策略：`consistent-hash`（按目标地址粘滞）与 `round-robin`（轮询）。

**与既有边界的关系**：本会话之前已经确认 `main.rs`（真正在跑的代理进程）目前
完全不读取 `config.yaml`，`RuleSet` 也不可热替换——这是一个更早、更大、独立
的工程缺口，不在本次范围内补上。因此本设计新增的一切（组的 schema、纯解析
逻辑、UI）今天**都不会影响真实流量**，落点是「配置 schema + 纯逻辑 + 落盘
持久化 + UI 呈现」，与本会话之前做的分流预设（全局直连/全局代理/中国大陆/
规则）完全同构、同一条诚实边界。

## 1. 配置 schema — `proxy-groups`

新增顶层数组，与现有 `proxies` / `rules` 平级：

```yaml
proxy-groups:
  - name: 节点选择
    kind: select
    proxies: [日本节点, 香港节点]
    selected: 日本节点
  - name: 自动选优
    kind: auto
    proxies: [日本节点, 香港节点, 新加坡节点]
  - name: 均衡负载
    kind: load-balance
    proxies: [日本节点, 香港节点]
    strategy: consistent-hash
```

字段：

| 字段 | 含义 | 校验 |
|---|---|---|
| `name` | 组的唯一标识，**同时是展示名，也是规则可以引用的名字**（与出站名共享同一命名空间） | 不得与任何出站名或其他组名重复 |
| `kind` | `select` / `auto` / `load-balance` | 枚举校验 |
| `proxies` | 成员列表，元素必须是已存在的**出站名**（不允许引用另一个组——禁止嵌套，避免解析时出现环） | 每个成员都在 `known_outbounds` 里 |
| `selected` | 仅 `select` 类型使用：当前选中的成员 | 必须是 `proxies` 里的一个 |
| `strategy` | 仅 `load-balance` 类型使用：`consistent-hash` / `round-robin` | 枚举校验 |

`Config` 加 `pub proxy_groups: Vec<ProxyGroup>`，默认空数组。`validate()` 新增
上述全部校验。**组名并入规则校验的已知出站集合**：`RuleSet::build` 的调用方
（未来接线时）把 `proxy_groups` 的 `name` 一并塞进 `known_outbounds`，`MATCH,
节点选择` 与 `MATCH,日本节点` 在语法层面完全一样。`wsieve-route` 的
`Mode`/`Rule`/`engine.rs` **不需要改一个字节**——组的存在对路由引擎而言只是
「多了几个合法的出站名」，组到物理节点的展开是另一层，见下节。

**只用 `name`，不单独设 `id`。** 最初设计里 `id` 是稳定机器键、`name` 只做
展示，理由是「改名不该打断内部引用」——但审视一遍后发现这个担心找错了对象：
`selected` 是**写在组自己的 YAML 块里**的字段，不是外部某处按键索引的一份
独立状态，UI 定位「该改哪个组块」时读的就是当下最新的配置，不存在「组改名后
某处还攥着旧名字」的场景。`id` 唯一的实际用途只是给 `{#each}` 一个稳定 key，
而 `name` 本来就唯一（已校验），拿它当 key 完全够用。去掉 `id` 少一个字段、
少一条校验、UI 与 config.yaml 里说的是同一个词——净收益，不是权衡。
**代价与出站今天的既有行为一致**：改组名会让引用它的规则跟着失效（旧名字
在文件里找不到对应的组了），这与今天改一个出站的名字会让引用它的规则失效
是同一件事，不是本设计新引入的脆弱点。

## 2. 组解析的纯逻辑 — 新模块 `wsieve-route/src/group.rs`

三个纯函数，穷举单测，**暂时没有运行时调用点**（与 `CHINA_PRESET_RULES` 同一
处境，main.rs 还不消费 config.yaml）：

```rust
/// select：直接取 selected。调用前 validate() 已保证它是合法成员，
/// 这里不再重复校验。
pub fn select_pick(group: &ProxyGroup) -> &str;

/// auto：挑延迟最小的非 None 成员。全 None（一个都没测过延迟）时
/// 退回第一个成员——给一个可预测的默认值，而不是让调用方处理「挑不出」。
pub fn auto_pick(members: &[(String, Option<u64>)]) -> &str;

/// load-balance：
///   consistent-hash 对 key（目标 host）取哈希取模，同一 host 稳定落在
///   同一个成员上；round-robin 用调用方传入的可变计数器递增取模。
pub fn load_balance_pick(
    members: &[String],
    strategy: LbStrategy,
    key: &str,
    rr_counter: &mut usize,
) -> String;
```

`Decision::Outbound(name)` 到「实际物理出站」的展开是另一个独立纯函数：

```rust
/// auto 用的延迟表、load-balance 用的目标 host 与 round-robin 计数器，
/// 全部集中到一个结构体里传递，避免 resolve_group 的参数表随组类型增多
/// 而不断变长。latencies/rr_counters 都按组名索引（组名唯一，见 §1）。
pub struct ResolveCtx<'a> {
    pub latencies: &'a HashMap<String, HashMap<String, Option<u64>>>, // group_name -> member -> latency
    pub target_host: &'a str,                                        // load-balance 的 consistent-hash key
    pub rr_counters: &'a mut HashMap<String, usize>,                  // group_name -> round-robin 计数器
}

/// name 若匹配某个组的 name，按组的 kind 展开成具体出站名；
/// 若匹配的是普通出站名，原样返回。不认识的名字在 RuleSet::build 阶段
/// 就已经被拒绝，这里不会遇到。
pub fn resolve_group(name: &str, groups: &[ProxyGroup], ctx: &mut ResolveCtx) -> String;
```

同样只做逻辑与测试，接线留给未来真正把 config.yaml 接进 main.rs 的那次工作。

## 3. 首页 — 新的默认视图

`ui/src/views/HomeView.svelte`，四张卡片。视觉上延续既有设计令牌（borders-only、
密度高、IBM Plex）——**不引入 Clash Verge 的圆角阴影卡片皮肤**，参考的是它的
「首页该放什么」这条功能分区，不是视觉风格（后者与 spec §11.4 documented 的
「密集像交易台，克制像 Proxyman」正相反）。

- **节点选择卡片**：列出 `kind: select` 的组。零个组时空状态引导「去
  config.yaml 加一个 `proxy-groups` 块」（与出站/规则视图的空状态同一套写法：
  引导语 + 可照做的具体步骤）。有组时下拉选当前组的成员，选择后落盘。
  多个 select 组时，卡片顶部再加一层「组」下拉先选组、再选成员——直接照抄
  截图里「代理组：节点选择」→「节点：[直连-香港]」那两层结构。
- **系统代理 / 虚拟网卡卡片**：两个快捷开关，直接改 `system-proxy` /
  `tun.enable`，走与 `saveSettings` 相同的 `setScalar` + `config_save_raw`
  模式。
- **分流模式卡片**：复用本会话已经做好的 `routingPreset` / `saveRoutingPreset`
  与 `PRESET_OPTIONS`（全局直连/全局代理/中国大陆/规则），从 `RulesView` 顶部
  的 segmented control 搬一份到首页，同一份状态与保存函数，不重复实现。
- **流量统计卡片**：复用状态条已有的 `spark`（最近 40 个采样点、log 压缩柱状）
  与 `status.downRate`/`upRate`，放大展示，不重新实现聚合逻辑。

`App.svelte` 的 `view` 状态默认值改为 `'home'`；顶部 `Segmented` 的选项数组
最前面加 `{ value: 'home', label: '首页' }`。

## 4. 「当前选中成员」的持久化 — 新的 config-map.js 编辑器

现有 `setScalar(text, key, value)` 只认「顶层、无缩进的 `key:` 行」，改不了
嵌套在某个 `proxy-groups` 列表项里的 `selected:`——同名字段可能在好几个组块
里各出现一次，纯字符串匹配会串到别的组头上。

新增 `setGroupSelected(text, groupName, member)`：

1. 定位 `proxy-groups:` 顶层键所在行
2. 在其后按缩进层级识别每个列表项的起止行（`- name: xxx` 开始，下一个同缩进
   的 `- name:` 或缩进回退到 `proxy-groups:` 同级为止）
3. 找到 `name` 字段等于 `groupName` 的那一项，只在**该项的行范围内**查找并
   替换 `selected:` 那一行
4. 找不到匹配的 `name`，或该项没有 `selected:` 行（比如误传了一个 `auto`
   类型组的名字），**如实报错**，不静默无操作、也不误伤到别的组

逐行文本操作，不重新序列化整份 YAML，注释照旧保留——延续 `setScalar` /
`config_save_raw` 已有的纪律（§5.6：非规则区手写注释会丢，这里额外保证「精确
定位到组」，不产生新的丢失面）。

## 5. 测试计划

- **`wsieve-config`**：`proxy-groups` 的 schema 校验——重复 `name`、非法
  `kind`、`selected` 不在自己的 `proxies` 里、`load-balance` 缺失或非法
  `strategy`、组名与出站名/其他组名撞车、成员引用不存在的出站名、成员引用
  另一个组（应拒绝，不允许嵌套）。
- **`wsieve-route/group.rs`**：`select_pick` 直接返回值；`auto_pick` 覆盖
  全 `None`、部分 `None`、并列最小值三种情况；`load_balance_pick` 的
  `consistent-hash` 覆盖同 key 多次调用结果一致、不同 key 分布到不同成员，
  `round-robin` 覆盖计数器正确递增取模、跨越成员数边界回绕；`resolve_group`
  覆盖组名/普通出站名两条路径。
- **`config-map.js`**：`setGroupSelected` 的往返测试——多个同名字段（不同组
  都有 `selected:`）不串行、目标组的注释保留、其余组的内容一字节不变、
  找不到 `name` 时如实报错而不是静默无操作。
- **`HomeView.svelte`**：四张卡片渲染；零个 select 组时的空状态与引导文案；
  多个 select 组时的两层下拉；切换成员触发正确的保存调用；系统代理/TUN 开关
  的即时反馈；分流模式卡片与 `RulesView` 顶部的状态保持一致（同一份
  `routingPreset`）。

## 6. 可视化节点 + 规则编辑器

今天新增节点/规则只有一条路：手改 `config.yaml`。这一节把「新增」这个动作
搬进 UI——**编辑已有规则**的后端能力（`ReplaceRule`）本就存在，只是没有表单；
**新增规则**与**新增节点**则连后端能力都还没有，需要新的定点改写原语。

### 6.1 规则编辑器

表单字段：`type`（下拉：DOMAIN / DOMAIN-SUFFIX / DOMAIN-KEYWORD / IP-CIDR /
GEOSITE / GEOIP / MATCH）、`value`（文本，随 `type` 变化 placeholder）、
`target`（下拉，选项来自当前已配置的出站名 **与** `proxy-groups` 的组名，
外加内置的 `DIRECT`/`REJECT`）、`no-resolve`（复选框，仅 IP-CIDR/GEOIP 时可勾）。

**校验交给后端，不在前端重复实现一份规则语法。** 提交时后端跑一次
`Rule::parse`，语法错误原样带回行文本一起显示——这与探针（signature ②）
「复用路由层纯函数，结果与真实判决永远一致」是同一条纪律：前端另起一份校验
逻辑，迟早会和后端的判定分叉。

**这是一个需要补的真实缺口，不是「复用现成校验」。** 核对过 `commands/
config.rs` 的现状：`apply_rule_ops`（`ReplaceRule`/`DeleteRule` 的实现）今天
**完全不校验规则语法**——它只跑 `wsieve_config::load_str(&out)?.validate()?`，
而 `wsieve-config` 按设计只把规则当不透明字符串（`rule_lines()` 的文档原话：
「本 crate 只把规则当字符串，不认识其语义」）。也就是说，今天往 `ReplaceRule`
的 `value` 里塞一段语法错误的文本，会被原样写进文件、不报任何错，只是碰巧
没人第一时间发现——因为 main.rs 目前也不从 config.yaml 读规则去真的构建
`RuleSet`。`wsieve-app` 的 Cargo.toml 已经依赖 `wsieve-route`（main.rs 用它
构建启动时的 RuleSet），只是 `commands/config.rs` 还没 `use` 过它。

因此本节要做的是：在 `apply_rule_ops` 里补一次 `wsieve_route::Rule::parse
(&value)`（**对 `ReplaceRule` 与新的 `InsertRule` 都适用**，不只是新表单
这一条路径——这顺带把既有的一个真实缺口堵上，是一次值得做的小范围加固，
不是范围蔓延）。`DeleteRule` 不需要，它不产生新内容。

**新增（Insert）**——`wsieve-config` 缺的原语：

```rust
/// 在 after_line 之后插入一条新规则；after_line 为 None 时插在文件最前。
/// 与 replace_rule_line / delete_rule_line 同一套并发校验：expect_after
/// 是调用方以为 after_line 那一行当前是什么，对不上就拒绝——语义与既有
/// 两个函数完全对称，不搞一套新花样。
pub fn insert_rule_line(
    src: &str,
    after_line: Option<u64>,
    expect_after: Option<&str>,
    value: &str,
) -> Result<String, EditError>;
```

`commands/config.rs` 的 `RuleOp` 加一个变体：

```rust
InsertRule { after_line: Option<u64>, expect_after: Option<String>, value: String },
```

**插入位置默认在最后一条 MATCH 之前**（若存在）——引擎的 `RuleAfterMatch`
校验要求 MATCH 必须是最后一条，插在它后面会直接被 `RuleSet::build` 拒绝。
前端算这个默认位置只需看已加载的 `rules` 数组最后一项是不是 `type === 'match'`，
不需要新的后端能力。插入之后用户仍可以用已有的拖拽/Alt+↑↓ 调整顺序。

**编辑与删除**：编辑复用既有 `ReplaceRule`，删除复用既有 `DeleteRule`——
`RulesView` 目前只暴露了启停开关，这次补上「编辑」（打开同一个表单，字段
预填）与「删除」按钮。

### 6.2 节点（出站）编辑器

**新增**——`proxies:` 里的每一项是没有行号追踪的结构化 YAML 块（不像规则是
`Spanned<String>` 单行），插入需要一个新的、块级别的定点改写原语：

```rust
/// 在 proxies: 列表末尾追加一个新的服务器块。lines 是调用方已经按固定缩进
/// 格式化好的若干行（name / type / url / server-pub / client-priv 等），
/// 不接受任意文本——UI 端拼好结构，这里只负责找到插入点。
/// proxies: 键不存在时（比如全新配置的 `proxies: []`）就地把 `[]` 换成
/// 一个新列表，其余文件内容不动。
pub fn append_proxy_block(src: &str, lines: &[String]) -> Result<String, EditError>;
```

表单字段：`name`（必填，需与现有出站名及组名都不重复）、`url`、`server-pub`、
`client-priv`、以及折叠在「高级」里的 `extra-sessions`/`mux-prefs`（默认值
与 `Proxy` 的 serde 默认一致，不填就不写这两行，让文件保持精简）。

**私钥经过渲染层的边界必须显式承认，不能假装没这回事。** `client-priv`
在这个表单里第一次需要用户**手动输入**到一个 `<input>` 里——这与
`config_get_raw` 导出时的既有警告是同一类风险，处置对齐：输入框用
`type="password"` 掩码、明文只活在这个表单组件的局部 `$state` 里、提交成功
或取消后立刻清空、不打进任何 `console`/`pushAlert`/错误上报。提交前展示与
`SettingsOverlay` 导出提示同源的一句话：「私钥仅受文件系统权限保护，确认
来源可信后再提交」。

**删除**：按 `name` 定位并删除对应块（同样是块级文本操作，不需要 Spanned
追踪——找到 `- name: "<target>"` 所在行到下一个同缩进 `- name:` 或列表结束
之间的行范围，整段删掉）。

**编辑已有节点的字段**（比如轮换私钥）：v1 **不做**——需要先给 `Proxy` 上
行号追踪（现在只有 `rules: Vec<Spanned<String>>` 有 span，`proxies` 没有），
是比「新增/删除」大一截的改动。v1 的路径是「删除重加」，与 spec 里其他地方
「宁可少做一步也不做半成品」的一贯取舍一致。

### 6.3 测试

- `wsieve-config::edit`：`insert_rule_line` 的往返测试（插在中间/开头/紧邻
  MATCH 之前、`expect_after` 并发校验、不改动其余行的注释与顺序）；
  `append_proxy_block` 的往返测试（`proxies: []` 场景、已有节点后追加、
  新块的字段顺序与格式一致、不改动其余内容）。
- `commands/config.rs`：`RuleOp::InsertRule` 的完整命令测试，与既有
  `ReplaceRule`/`DeleteRule` 测试同构。
- `RulesView.svelte`：新增/编辑表单的渲染与提交、`target` 下拉包含组名、
  校验错误原样来自后端而非前端猜测、插入位置默认在 MATCH 之前。
- `OutboundsView.svelte`：新增表单渲染、`client-priv` 输入框类型与清空时机、
  删除确认与实际调用、名字与出站/组名冲突时的报错。

## 明确不做的事

- 不做「识别流量行为（下载 vs 浏览）动态选负载均衡策略」——代理层看不到
  HTTP 语义，只能靠不可靠的连接数量启发式猜测，讨论中已达成一致不做。
- 不做「自动探测各出站的出口 IP、按出口 IP 分组」——这是一个需要主动探测
  基础设施的独立子系统，值得单独立项，不塞进本次首页/代理组的范围。
- 不把 `proxy-groups` 接入 `main.rs` 的真实运行时——`main.rs` 目前完全不读
  `config.yaml`，这是更早、更大的独立缺口，本次不动它。落盘与纯逻辑今天
  就是真实、可测的，只是还没有调用点让它控制真实流量。
- 不做**代理组本身**的新增/编辑/删除表单——组的定义
  （`id`/`name`/`kind`/`proxies`/`strategy`）延续手写 `config.yaml` 的
  路子。这与 §6 的节点/规则编辑器不矛盾：那两个编辑的是「即将上线的
  可视化编辑器覆盖的对象」，组的定义相对少改（建一次、长期用），
  UI 只负责 visualize 已有的组，以及持久化 `select` 类型「当前选中
  成员」这一件高频会变的事。
- 不做**已有节点字段的编辑**（比如轮换私钥）——需要先给 `Proxy` 加行号
  追踪，是比新增/删除大一截的改动，见 §6.2 末尾。v1 路径是删除重加。
