# 运行时接入 config.yaml 设计文档

## 背景

现状（已通过实际读代码确认，不是推测）：`main.rs` 的 `run_stack` 完全不读
`config.yaml`。它靠三个环境变量（`WSIEVE_SERVER_PUB`/`WSIEVE_CLIENT_PRIV`/
`WSIEVE_OUTBOUND_NAME`）启动，内置恰好一个出站、一条写死的
`MATCH,<出站名>` 规则，进程生命周期内不变。控制台的规则/出站/代理组/模式
四类编辑全部只改 `config.yaml` 文件本身，从文件到运行时之间没有任何代码
路径——保存成功但对真实流量零影响。

这份设计要解决的就是这个断层：让运行时真正消费 `config.yaml`，并且编辑
保存后**立即**对真实流量生效（用户已确认的产品决策），同时不因为改了
一条规则就把所有已经连上的出站也断开重连（用户已确认的另一条决策）。

## 目标

1. 进程启动时从 `config.yaml` 构建初始运行时状态，不再依赖那三个环境变量。
2. `config_save`/`config_save_raw` 写盘成功后，运行时状态整体重建并原子
   替换，规则/模式变化立即生效。
3. 出站的增删改**增量**处理：没变的出站不重启、不断连接。
4. 代理组的 `select`/`auto`/`load-balance` 三种成员挑选逻辑接入真实路由。
5. `rule_test`/`outbound_enable`/`outbound_latency_probe`/`set_mode`/
   `connect`/`disconnect` 六个命令从 `NotReady` 变成真实实现。
6. 首页新增一个连接总开关，对应 `connect`/`disconnect`。
7. 流量统计与"已连接/未连接"指示器接入真实数据源。
8. Windows 平台的系统代理开关有真实实现（现在只有 macOS）。

## 非目标（明确不做）

- 不做"应用变更"按钮——用户已确认保存即生效，不需要这一步确认 UI。
- 不做代理组**定义**（成员列表本身）的可视化增删——那是另一块已经完成的
  设计里明确排除的范围，本次不重新讨论，代理组定义继续只能手写
  `config.yaml`。
- 不做 macOS/Linux 之外目标平台的系统代理支持——现在只需要补上 Windows，
  macOS 已经是真实实现，Linux 不在本次范围。
- 不做出站延迟的后台周期性自动探测——探测仍然是用户点"测速"触发的一次性
  动作，`auto` 类型代理组读的是"最近一次测过的值"，不会因为长期没手动测过
  而自动过期或重测。这个自动化留给以后需要时再做。
- 不改 `wsieve-route` crate 里 `Decision`/`Target` 的类型定义（仍然是纯
  字符串出站名）——代理组解析在 `src-tauri` 这一层做，不让"出站还是代理组"
  这个概念渗透进纯路由逻辑 crate，保持它对 `wsieve-config` 零依赖的既有
  纪律。

## 架构

### 1. `RuntimeState` 快照 + 原子热替换

新增一个快照结构体，把"一份配置对应的完整运行时状态"打包成一个整体：

```rust
struct RuntimeState {
    rule_set: Arc<RuleSet>,
    outbound_manager: Arc<OutboundManager>,
    router: Arc<Router>,
    groups: Arc<GroupTable>,       // 新增，见 §4
}
```

用 `RwLock<Arc<RuntimeState>>` 托管在 Tauri 状态里（`app.manage(...)`
只在进程启动时调用一次；后续更新是对这把锁的写入，不是重复 `.manage()`——
调研发现现有的 `CurrentCore` 恰恰是反面教材：`run_stack` 的承载代循环里
每一代都重新 `.manage(CurrentCore(...))`，而 Tauri 的 `manage()` 在类型
已注册时是空操作，不会替换内容，导致 IPC 命令读到的 `CurrentCore` 实际
上永远是**第一代**那份、早已过期的状态——这是一个既有的潜藏 bug，本次
设计的新结构必须避免重蹈覆辙，用真正可变的 `RwLock` 而非反复 `manage`）。

**为什么是一个整体快照，而不是给 `RuleSet`/`OutboundManager`/`Router`
各开一把锁分别热替换**：`Router` 与 `OutboundManager` 现在各自维护一份
`outbounds: BTreeMap<String, Arc<OutboundInstance>>`，必须来自同一批
`Arc<OutboundInstance>` 才不会错位（这是代码里已有的注释明确要求的约束）。
分开加锁的话，理论上存在"两把锁各自被刷新，中间那一刻两者不一致"的窗口。
合成一个快照结构体、整体构建、整体替换，从根上排除这类竞态，不需要额外
的跨锁同步逻辑。

不引入 `arc-swap`（调研确认整个 workspace 里没有这个依赖）——标准库的
`RwLock<Arc<T>>` 已经够用：写者持锁时间只覆盖"把 `Arc` 指针换掉"这一步，
读者（IPC 命令、路由决策）拿锁只是克隆一次 `Arc`（引用计数 +1，纳秒级），
不会长时间持锁阻塞路由热路径。

### 2. 启动流程：从读 env 变量改成读 `config.yaml`

`main.rs` 现在的启动序列（`bootstrap::load_cfg()` 读三个 env 变量、
`outbound::try_plan_ports` 只给单个出站分段）整体替换成：

1. 用已有的 `config_get`/`config_save_raw` 背后那套"读取或首次生成默认
   `config.yaml`"逻辑加载 `Config`（这条路径已经存在、已经测过，直接复用，
   不重新发明）。
2. 用 `Config.proxies` 构建初始 `OutboundManager`（可以是空的——首次运行
   `proxies: []` 是完全正常、预期内的状态，代理这时应该拒绝连接而不是
   偷偷直连，这一点 `DEFAULT_CONFIG_YAML` 的注释已经写明，运行时要如实
   照做，不能因为"没有出站"就走某种兜底直连）。
3. 用 `Config.rules`/`Config.mode` 构建初始 `RuleSet`。
4. 用 `Config.proxy_groups` 构建初始 `GroupTable`（见 §4）。
5. 混合端口入口、TUN 等仍按 `Config` 里对应的字段初始化（这些逻辑本来
   就已经在读某种配置源，只是把源从 env 变量换成 `Config` 的字段）。

`WSIEVE_SERVER_PUB`/`WSIEVE_CLIENT_PRIV`/`WSIEVE_OUTBOUND_NAME` 这三个
env 变量的读取整体删除——不再需要，`proxies:` 列表里每一项已经带着
`server-pub`/`client-priv`/`url`。其余非出站相关的 env 变量（`WSIEVE_SOCKS`
的兜底、`WSIEVE_MUX_PREFS`、`WSIEVE_SHARD_BASE_PORT`、`WSIEVE_SHOW_WINDOW`
这类开发期/高级选项）继续保留原样——它们不是"这个出站该连谁"这类产品级
配置，是运行参数，目前也没有对应的 `config.yaml` 字段，本次不新增。

### 3. 出站的增量更新

`RuleSet` 本身很便宜（纯字符串解析 + 校验，不碰 GeoDB），**每次保存都
整体重建，不需要增量**——重建成本可以忽略不计。

真正需要"增量"的是出站，因为它背后是真实网络连接。`OutboundManager` 目前
没有运行时增删的 API（只有面向已存在名字的 `set_enabled`），方案是每次
保存后对比新旧 `proxies:` 列表，构建新 `OutboundManager` 时：

- **没变的出站**（名字与关键字段都一致）：把旧快照里那个
  `Arc<OutboundInstance>` 原样搬进新 `OutboundManager`——只是克隆一次
  `Arc`，连接、会话状态原地保留，不重启。
- **新增的出站**：构建新的 `OutboundInstance` 并启动。
- **删除的出站**：对旧实例调用既有的 `stop_one` 优雅关闭，再丢弃。
- **字段被改动的出站**（比如换了 `server-pub`）：按"先停旧的、再当新增
  处理"走，因为身份变了，复用旧连接没有意义。
- 端口分段（`try_plan_ports`）每次整体重算——这是纯计算（字符串/数组
  操作），不涉及真实 socket，重算的成本可以忽略。

`Router` 的出站表用同一批（新建 + 复用）`Arc<OutboundInstance>` 构建，
天然与 `OutboundManager` 保持一致，不需要额外同步代码。

### 4. 代理组接入路由

不改 `wsieve-route` 的 `Decision`/`Target` 类型（仍是纯字符串出站名），
解析在 `src-tauri` 这一层做：

- 新增 `GroupTable`：从 `Config.proxy_groups` 构建，`id → (kind, members,
  selected)` 的映射，随 `RuntimeState` 一起整体重建。
- `Router::via_outbound` 现在直接 `self.outbounds.get(name)`；改成先查
  `name` 是不是 `GroupTable` 里的一个组 id——是的话按 `kind` 解析出真正
  的成员名（`select` 读 `selected` 字段；`auto` 调用已经写好测过的
  `wsieve_route::group::auto_pick`；`load-balance` 调用
  `load_balance_pick`），再拿解析出的成员名去查 `outbounds` 表。
- `RuleSet::build` 传入的 `known_outbounds` 集合要把代理组 id 也一并
  加进去，规则里写 `MATCH,我的代理组` 才能通过校验（否则会被当成"引用了
  不存在的出站"拒绝保存）。
- `auto`/`load-balance` 需要每个成员的实时延迟数据，而这块目前完全没有
  埋点——`OutboundInstance` 需要新增一个字段记"最近一次测得的延迟"，由
  §5 的 `outbound_latency_probe` 命令写入、`auto_pick` 读取。这是新增的
  一小块状态，不是简单接线。
- `auto_pick`/`load_balance_pick` 按每次拨号（每个新连接）调用一次，
  不缓存结果——两者本身都是对一个小列表的线性扫描，足够便宜。

### 5. 命令接活

有了 `RuntimeState`，以下命令基本是"从托管状态里取出对应部分调用"：

- **`rule_test`**：`state.rule_set.evaluate(...)`，复用现成的求解逻辑，
  不重新实现一遍。
- **`outbound_enable`**：`state.outbound_manager.set_enabled(name, on)`——
  这个方法已经存在，直接调用。
- **`outbound_latency_probe`**：真实测一次延迟，写回对应
  `OutboundInstance` 的新字段（§4 提到的埋点）。
- **`set_mode`**：用新的 mode 重新 `RuleSet::build(...)`，走 §1 的整体
  替换流程——不需要改路由引擎逻辑，`Mode::Direct`/`Mode::Global` 的短路
  已经在 `RuleSet::evaluate` 里实现好了，`set_mode` 只是换个参数重新
  `build`。
- **`connect`/`disconnect`**：`disconnect` 把 `OutboundManager` 里全部
  出站 `set_enabled(false)`；`connect` 反过来全部设 `true`。

### 6. 首页连接总开关

首页新增一个总开关（具体视觉位置留给写计划阶段决定，建议放在顶部连接
状态指示器旁边，逻辑上离它最近），对应调用 `connect`/`disconnect`。开关
状态从 §7 的连接指示器数据推导（是否至少有一个出站处于 live/connecting），
不是开关自己维护一份独立状态——否则真实状态与开关视觉状态可能分叉，这是
`OutboundsView` 现有的启停开关已经踩过、并且专门写了警示注释的坑，新开关
要吸取同一条教训。

### 7. 流量统计 + 连接指示器

- 两处真实数据流经的 `copy_bidirectional` 调用（`shard.rs` 与 `router.rs`
  各一处）已经能拿到 `(u64, u64)` 字节数返回值，只是从来没有调用
  `events::Counters` 的 `add_up`/`add_down`/`conn_opened`/`conn_closed`。
  管线本身（`Aggregator`、1s 定时器、`traffic_snapshot` 命令、前端图表）
  已经是通的、已经测过，缺的只是在这两处实际调用一下现成的函数。
- 连接指示器：调研发现**其实已经在发状态变化事件**，只是事件名和载荷
  形状跟前端期望的对不上——现在发的是 `wsieve-outbound-status`，携带一个
  Rust `Debug` 格式的原始字符串；前端等的是 `outbound-state`，携带
  `{name, state, latency_ms}` 这种结构化数据。修法是把已经存在的那个
  状态变化回调（`OutboundInstance::set_status` 触发的 `on_status`）改成
  调用 `events::emit_outbound_state`，映射 `Status` 枚举到期望的
  `state` 字符串——不需要新增调用点，只需要改这一个回调内部调用什么。

### 8. Windows 系统代理

后端现在只有 macOS 的 `networksetup` 命令行调用一条真实实现。Windows 下
走注册表：写 `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion
\Internet Settings` 下的 `ProxyServer`/`ProxyEnable`，改完调
`InternetSetOptionW`（`INTERNET_OPTION_SETTINGS_CHANGED` +
`INTERNET_OPTION_REFRESH`）通知系统与已打开的浏览器生效。退出或用户手动
关闭开关时要能恢复到原来的设置（原样复刻 macOS 实现"退出时恢复原设置，
进程崩溃时下次启动兜底清理"的既有安全纪律，`custody` 模块这条护栏是
跨平台共享的，Windows 实现只是多补一个 `PlatformProxyBackend` 的具体
落地，不改 `custody` 的整体状态机）。

这一块与 §1-7 没有代码依赖关系，可以独立排期/并行开发，但为了这次设计
的完整性放在同一份文档里，写计划阶段会拆成独立的 Task 组。

## 数据流示意

```
用户在「规则」视图点保存
  → config_save（行级定点改写，已有）
  → 写盘前 wsieve_config::validate() 校验（已有，不变）
  → 写盘成功
  → 新增步骤：从磁盘重新读取 Config，与当前 RuntimeState 对比
     - proxies 增删改 → 增量构建新 OutboundManager（§3）
     - rules/mode → 整体重建新 RuleSet（§1 结尾提到，重建成本可忽略）
     - proxy-groups → 整体重建新 GroupTable
     - 三者 + 新 Router 打包成新 RuntimeState
  → 写锁替换 RwLock<Arc<RuntimeState>> 的内容（原子）
  → 后续到达的连接、后续的 rule_test/outbound_enable 等命令调用，
    读到的都是新快照，不需要额外通知/事件——下一次读锁就是新的
```

## 错误处理

- `config_save`/`config_save_raw` 写盘前的 `validate()` 已经挡住了绝大多数
  非法配置（未知字段、非法枚举值、代理组引用不存在的出站等），所以理论上
  重建 `RuntimeState` 这一步收到的都是"已经过验证"的 `Config`，不应该再
  因为配置本身不合法而失败。
- 但"配置合法"不等于"出站一定连得上"——某个出站的服务器可能确实連不通。
  这类失败不应该阻塞整个 `RuntimeState` 重建：`OutboundInstance` 的握手
  失败只影响它自己的状态（现有的 `one_outbound_failing_does_not_kill_
  its_neighbour` 这条测试已经在保这个不变量，新代码要延续，不能因为重建
  流程引入新的耦合把这条不变量破坏掉）。
- 重建过程本身（不是某个出站连不上，而是重建逻辑代码自身）理论上不应该
  panic；如果出现意外错误（比如磁盘配置在写盘和重读之间被外部程序改
  坏成了不合法的 YAML），保守做法是**保留旧快照不替换**、记一条错误日志，
  而不是让整个进程崩溃或者把运行时状态换成一个部分构建的、不一致的
  半成品。

## 测试

- Rust 侧：`RuntimeState` 的增量出站 diff 逻辑是纯函数（给定旧出站表
  与新 `proxies:` 列表，算出"哪些复用、哪些新建、哪些停用"），可以脱离
  真实网络单元测试。代理组解析（`Router::via_outbound` 的新分支）同理，
  给一个假的 `GroupTable` 和假的 `outbounds` 表就能测三种 kind 各自的
  解析结果，不需要真实连接。
- `set_mode`/`rule_test` 接活后，复用已有的 `RuleSet::evaluate` 测试
  风格（构造 `RuleSet`，给一批目标断言决策）即可覆盖，不需要新的测试
  手法。
- 流量统计/连接事件这两处的验证，本项目一贯的态度是"如实标注尚未接入
  运行时"而非造假——具体测试策略（是否需要一个可控的假 `copy_bidirectional`
  场景来验证字节数确实被计数）留给写计划阶段，按彼时看到的真实代码结构
  决定，不在设计阶段预先假定。
- Windows 系统代理：`custody`/`sysproxy` 模块现有的测试大概率是针对
  macOS 分支写的，需要看实际代码结构后再定 Windows 分支怎么测（注册表
  写入这类系统副作用通常需要抽象出一层可 mock 的接口，具体怎么抽象留给
  写计划阶段）。

## 明确的取舍

- 选"整体快照 + 单把锁"而不是"多把细粒度锁"：牺牲了理论上更精细的并发
  粒度，换来消除跨结构体不一致窗口的简单性。给定这不是一个高并发写热点
  （配置保存是用户交互触发，不是每秒多次），这笔交易划算。
- 选"每次都整体重建 `RuleSet`"而不是给规则也做增量 diff：因为
  `RuleSet::build` 已经确认足够便宜，增量 diff 的复杂度换不来有意义的
  性能收益，是过度工程。
- 出站延迟埋点是这次新增的最小状态（一个字段），不做成一套完整的历史
  延迟曲线/统计——`auto` 只需要"最近一次"，做更多是猜测未来需求。
