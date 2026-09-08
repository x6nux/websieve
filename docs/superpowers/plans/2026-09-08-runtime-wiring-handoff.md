# 交接文档——运行时接入 config.yaml（Part 1 / Task 3 中途）

> 写这份文档的原因：39 个后台子代理被用户手动全部中断（`TaskStop`），
> 中断时 Task 3 正在进行到一半，**当前工作树编译不过**。这不是一个可以
> 安全 `git stash pop` 就继续的状态——下一个接手的 agent（或人）需要先
> 看完这份文档，再决定怎么走。

## 现在在哪个分支上

这份提交落在一个新分支上，从 `feat/routing-and-control-ui` 的
`9a41ef6`（`docs(plan): 阶段 5 全量验收`）切出来，分支名见本次提交所在分支
（如果你在读这份文件，运行 `git branch --show-current` 确认）。

`feat/routing-and-control-ui` 本身在 `9a41ef6` 是**干净、全绿、可用**的
状态——里面已经包含且已合入的东西：

1. 阶段 2/3/4/5 的代理组 + 首页 + 可视化编辑器全部工作
   （计划：`docs/superpowers/plans/2026-09-07-proxy-groups-home-and-editors.md`）。
2. 真机走查发现并修复的两个真实 bug（TUN 开关在空配置下报错、出站重连
   会刷新掉 control 窗口丢状态）——见
   `docs/superpowers/plans/2026-09-07-live-walkthrough-checklist.md`。
3. 主题切换（深色/浅色/跟随系统）+ 全局滚动条主题化，完整实现+测试+提交
   （spec：`docs/superpowers/specs/2026-09-08-theme-switch-and-scrollbar.md`，
   plan：`docs/superpowers/plans/2026-09-08-theme-switch-and-scrollbar-plan.md`）。
4. 全量测试套件（Rust workspace + `ui` 下 vitest）在 `9a41ef6` 处应为全绿
   （除下面提到的一个已知环境性失败）。

**如果你只是想要一个干净、可用、可以正常跑的 websieve，回到
`feat/routing-and-control-ui@9a41ef6` 就够了，不需要碰这个分支。**

这个新分支承载的是**下一步、尚未完成**的工作：让运行时真正消费
`config.yaml`（当前 `main.rs` 完全不读它，见下）。

## 已知环境性失败（不是回归）

`cargo test` 里 `router::tests::a_direct_connect_failure_is_reported_with_the_target`
在本机环境下可能失败——这是本会话之前就确认过的**环境性**失败（与网络
沙箱/DNS 行为有关，不是代码回归）。看到它单独失败可以忽略；如果还有
别的测试也红，才需要关注。

## 大背景：为什么要做这件事

`main.rs` 目前的启动路径完全不读 `config.yaml`：它靠三个环境变量
（`WSIEVE_SERVER_PUB` / `WSIEVE_CLIENT_PRIV` / `WSIEVE_OUTBOUND_NAME`）
拼出**唯一一个**硬编码的 `OutboundCfg`，规则表也硬编码成一条
`MATCH,<这一个出站>`。这是阶段 2/3 遗留的启动路径，从来没有跟上后面
几个阶段做的配置文件/UI 工作。

后果是：控制界面里对规则/出站/代理组/模式做的任何编辑都只落盘到
`config.yaml`，对正在跑的流量**零影响**；`rule_test` / `connect` /
`disconnect` / `set_mode` / `outbound_enable` / `outbound_latency_probe`
这些 IPC 命令全部返回 `not_ready`；首页的连接指示器永远显示未连接；
流量统计永远是空的。

完整方案见两份已提交、已定稿的文档，**不需要重新设计**：

- 架构 spec：`docs/superpowers/specs/2026-09-08-runtime-config-wiring-design.md`
  （8 个小节：RuntimeState 快照、从 config.yaml 启动、增量出站更新、
  代理组路由解析、命令接线、首页连接开关、流量/连接事件、Windows 系统代理）。
- Part 1（基础设施）实现计划：
  `docs/superpowers/plans/2026-09-08-runtime-wiring-part1-foundation-plan.md`
  （5 个 Task，本文档只覆盖到 Task 3 的中途）。

这两份文档已经把「关键既有代码」的真实签名核对过一遍并写进了计划里，
值得先读，能省掉不少重新翻源码的时间。

## Part 1 五个 Task 的真实状态

| Task | 内容 | 状态 |
|---|---|---|
| 1 | `RuntimeState` 快照结构 + `diff_outbounds` 纯函数 | ✅ 完成，已提交，已过 spec/质量双审（`6ab010e` + `72916ba`） |
| 2 | 抽出 `spawn_carrier_windows`，做成幂等 | ✅ 完成，已提交，已过双审（`5894a5f` + `9a41ef6`） |
| 3 | 改造启动路径读 `config.yaml`，构建多出站 `StartupPlan` | 🔴 **进行到一半，未提交，编译不过**——见下一节 |
| 4 | 把 `RuntimeState` 接进 Tauri managed state，`config_save*` 命令触发 rebuild+swap | ⬜ 未开始 |
| 5 | Part 1 收尾验证 | ⬜ 未开始 |

Part 2-5（代理组路由解析、命令激活、流量/连接事件、Windows 系统代理）
按设计文档的说法，要等 Part 1 的真实代码形状定下来之后才规划——目前
连计划都还没写，不要提前动手。

## Task 3 现在具体卡在哪——这是本文档最重要的部分

### 已经改完、逻辑自洽、可以信任的部分

以下文件的改动已经想清楚、写完，**问题不在这些文件本身**：

1. **`src-tauri/Cargo.toml`**：新增 `dirs = "6"` 依赖，注释解释了原因
   （在 `AppHandle` 存在之前就要算出 `config.yaml` 的路径，复现 Tauri
   自己 `PathResolver::app_config_dir()` 的逻辑：
   `dirs::config_dir().join(identifier)`）。

2. **`src-tauri/src/bootstrap.rs`**：`AppConfig` 去掉了
   `server_url` / `server_pub` / `client_priv` 三个字段（这三个值现在
   应该来自 `config.yaml` 里的每个 `Proxy`，不再是全局唯一一份）。
   `hex32` 函数改成 `pub(crate)` 供 `runtime_state.rs` 复用来解析
   `Proxy.server_pub` / `client_priv` 的十六进制字符串。

3. **`src-tauri/src/commands/config.rs`**：`ensure_config_exists` 改成
   `pub(crate)`，注释写明是给 `main.rs` 的启动路径复用（首次启动、
   `config.yaml` 不存在时要落一份默认配置）。

4. **`src-tauri/src/runtime_state.rs`**：新增
   - `StartupPlan` 结构体（`outbound_cfgs: Vec<OutboundCfg>`、
     `carrier: Option<CarrierPlan>`（**`None` 当且仅当 `outbound_cfgs`
     为空**——`CarrierPlan::build` 拒绝空出站列表，这是设计阶段就发现
     并写进 spec §2.1 的约束）、`rule_lines: Vec<String>`、
     `mode: wsieve_route::Mode`、`global_outbound: String`）。
   - `rule_lines_with_reject_fallback(config)`：`config.rules` 为空时
     用隐式 `MATCH,REJECT` 兜底，因为 `wsieve_route::RuleSet::build`
     不管 `mode` 是什么都硬性要求有一条终结的 `MATCH` 规则，而「规则
     为空」是全新用户的默认状态，必须能正常启动而不是直接崩掉。
   - `build_startup_plan(config: &wsieve_config::Config) -> anyhow::Result<StartupPlan>`：
     纯函数（不碰网络/IO），对每个 `Proxy` 用 `bootstrap::hex32` 解出
     `server_pub`/`client_priv`，把 `mux_prefs: Vec<u8>` 转成
     `Vec<MuxId>`（`MuxId::from_u8`，遇到非法值报错），`session_bases`
     先填占位 `vec![None; extra_sessions+1]`（文档里明确写了：真实值需要
     异步 IO 才能算出来，留给 `main.rs` 在拿到这份 `StartupPlan` 之后
     自己去补）。
   - 有一组 `#[cfg(test)] mod startup_plan_tests`，其中一条叫
     `a_proxy_using_the_crate_default_mux_prefs_currently_fails_to_convert`
     的测试记录了一个**已知但未处理**的不一致：crate 自带的
     `default_mux_prefs()`（`vec![0,1,2,3,4]`）不能被 `MuxId::from_u8`
     干净地全部转换成功。**这条测试值得在继续 Task 3 之前先弄清楚**——
     要么是 `default_mux_prefs()` 该改，要么是 `MuxId::from_u8` 该改，
     要么这就是预期行为只是需要一个更明确的错误提示，没有定论，需要
     判断。

5. **`src-tauri/src/shard.rs`** + **`src-tauri/src/shard_setup.rs`**
   （这两个文件是 Task 3 期间新做的改动，在写这份交接文档之前刚读完，
   之前的会话摘要里没有细节，这里补全）：

   核心变化：把「编排单个出站的本地条带（hosts 劫持 + 端口转发）」
   泛化成「编排一批出站」，原因是 `HostsFile::set_managed` 是**整体
   替换**语义——如果对每个出站各调一次单出站版本的旧 `plan`，后一个
   出站的调用会把前一个出站刚写好的 hosts 行连同其余托管行一起清掉，
   造成「配置了两个出站，其中一个的域名劫持会莫名其妙失效，且没有任何
   报错」。`shard_setup.rs` 顶部的模块文档把这条原因写得很详细，值得读。

   具体的 API 变化（**这是接下来改 `main.rs` 时唯一需要关心的部分**）：

   - 旧：`pub async fn plan(server_url: &str, shard_base_port: u16, extra_sessions: usize, hosts_path: PathBuf, on_upstream: Option<UpstreamHook>) -> ShardPlan`
     （单出站，`ShardPlan` 里直接带一个 `guard: Option<ShardGuard>`）。
   - 新：
     ```rust
     pub struct ShardTarget {
         pub server_url: String,
         pub base_port: u16,
         pub extra_sessions: usize,
         pub on_upstream: Option<UpstreamHook>,
     }
     pub struct ShardPlanEntry {   // 原 ShardPlan 去掉 guard 字段后改的名字
         pub page_url: String,
         pub session_bases: Vec<Option<String>>,
         pub upstream: Option<SocketAddr>,
         pub bypass_error: Option<String>,
     }
     pub struct ShardManyPlan {
         pub entries: Vec<ShardPlanEntry>,  // 与传入的 targets 按下标一一对应
         pub guard: Option<ShardGuard>,     // 整批共享一份；没有任何目标被劫持时是 None
     }
     pub async fn plan_many(targets: Vec<ShardTarget>, hosts_path: PathBuf) -> ShardManyPlan
     ```
   - 每个目标各自独立解析域名、独立起转发器（一个目标失败只降级它自己，
     不影响其他目标），但 **hosts 写入是整批一次性、原子的**——全部
     目标都处理完之后才调用一次 `CustodyGuard::acquire`，覆盖需要劫持
     的域名的并集；这一次写入若失败，本轮**全部**目标一起退回单会话
     （不存在「一部分目标看到写入结果、一部分看不到」的分裂状态）。
   - `ShardGuard::new` 签名也变了：`Vec<Forwarder>` 而不是单个
     `Forwarder`（`src-tauri/src/shard.rs`），语义是「一批转发器与这
     一份 hosts 托管同生共死」。
   - `shard_setup.rs` 里的单元测试已经全部改用 `plan_many` + 一个新的
     测试 helper `target(url, base_port, extra_sessions) -> ShardTarget`。

   这部分改动本身逻辑自洽、测试齐全，**唯一的问题是它改变了 `main.rs`
   要调用的函数签名，而 `main.rs` 还没跟着改**。

### 真正卡住的地方：`main.rs` 完全没有跟进

`git status` 显示 `src-tauri/src/main.rs` **没有出现在改动列表里**——
这是问题的核心。上面列的所有改动都已经让 `bootstrap::AppConfig` 和
`shard_setup` 的对外接口变了形，但 `main.rs` 里的调用点一行都没动，
所以现在的 4 个编译错误分别是：

```
error[E0425]: cannot find function `plan` in module `shard_setup`
   --> src\main.rs:139:60
error[E0609]: no field `server_url` on type `bootstrap::AppConfig`
   --> src\main.rs:140:14
error[E0609]: no field `server_pub` on type `bootstrap::AppConfig`
   --> src\main.rs:252:25
error[E0609]: no field `client_priv` on type `bootstrap::AppConfig`
   --> src\main.rs:253:26
```

复现方式（默认 `target` 目录可能被一个正在跑的 `wsieve-app.exe` 锁住，
见下面「不要碰的东西」，所以要用一个独立的 `CARGO_TARGET_DIR`）：

```bash
CARGO_TARGET_DIR=<起个新名字，别叫 target-handoff-check，那个我已经删了> \
  cargo build --manifest-path src-tauri/Cargo.toml
```

### 接下来要做什么（Task 3 Step 6，计划文档里写的原文步骤，尚未执行）

打开
`docs/superpowers/plans/2026-09-08-runtime-wiring-part1-foundation-plan.md`
里 Task 3 的 Step 6 及之后，对照上面这份 API 变化表，大致要做：

1. `main.rs` 的 `main()` 里，把「读 env 变量拼一个 `OutboundCfg`」的
   那一段（约 67-256 行，见下面的具体行号）换成：先
   `bootstrap::ensure_config_exists`（若还没有 `config.yaml`）→ 读取
   `config.yaml` → `wsieve_config::Config::validate` → 调用
   `runtime_state::build_startup_plan(&config)` 拿到 `StartupPlan`。
2. 原来第 139-145 行调用旧 `shard_setup::plan(...)` 的地方，改成对
   `StartupPlan.outbound_cfgs` 里每个出站构造一个 `ShardTarget`（
   `server_url` 从 `Proxy.server` 拼、`base_port` 来自
   `outbound::try_plan_ports` 按出站分段、`extra_sessions` 来自 mux
   偏好数量、`on_upstream` 只给需要 TUN bypass 的那个装 hook），批量
   调用 `shard_setup::plan_many(targets, custody::hosts::system_path())`，
   拿到 `ShardManyPlan`，把每个 `ShardPlanEntry.session_bases` / `.page_url`
   写回对应出站的 `OutboundCfg.session_bases`（`StartupPlan` 里目前是
   占位 `vec![None; ...]`，就是要在这一步被真实值替换）。
3. 第 250-256 行构造 `OutboundCfg` 的地方，改成遍历
   `StartupPlan.outbound_cfgs`（已经是完整的 `Vec<OutboundCfg>`，不需要
   再手工拼字段）。
4. 第 204-222 行构造 `CarrierPlan` 的地方，改成直接用
   `StartupPlan.carrier`——注意它是 `Option`，`None` 时（零出站）要
   跳过整段承载窗口建造逻辑，不能像现在这样无条件 `unwrap`/`expect`。
5. 第 541 行的 `run_stack` 函数签名要从接收单个 `OutboundCfg` 泛化成
   接收 `Vec<OutboundCfg>`（或者等价的、按出站分组的结构），内部原来
   「建一个 `OutboundInstance` 塞进一个只有一项的 `BTreeMap`」的逻辑
   要改成对每个出站都建一个实例塞进同一个 `BTreeMap`（`table.insert`
   那段现有代码本来就是照着「泛化到多出站」的模式写的，可以照抄循环
   化，不用整个重设计）。
6. 规则表（第 172-185 行）不能再硬编码
   `MATCH,{outbound_name}`——要从 `StartupPlan.rule_lines` 取。
7. 全部改完之后：
   - `cargo build`（同上，独立 target dir）确认 4 个错误清零。
   - `cargo test --manifest-path src-tauri/Cargo.toml` 全绿（除前面提到
     的那条已知环境性失败）。
   - 手动跑一次真机烟雾测试（可以照抄
     `docs/superpowers/plans/2026-09-07-live-walkthrough-checklist.md`
     准备工作那节的做法，用占位环境变量或一份最小 `config.yaml`）。
   - 提交，continue 到 Task 3 的 spec 审查 + 代码质量审查（按
     subagent-driven-development 的两阶段审查流程，不要跳过）。

**这一步涉及的改动量不小（等价于把 main.rs 大约 200 行的单出站启动
逻辑重写成多出站），建议按 Part 1 计划里 Task 3 自带的 Step 划分，
一步步来，不要一次性整体重写再调试。**

## 环境注意事项（踩过的坑，别重踩）

1. **`wsieve-app.exe` 可能正有一个真实用户会话在跑**——不要杀它的
   进程，不要动它读取的 `%APPDATA%\org.websieve.app\config.yaml`。
   `cargo build` 用默认 `target` 目录可能因为这个进程占用而锁住，遇到
   构建失败先检查是不是这个原因，换一个 `CARGO_TARGET_DIR` 绕过，
   不要尝试解锁或杀进程。
2. 根目录下 `?? node_modules/`、`.shots/`、`scripts/shot-window.ps1`、
   `scripts/ui-drive.ps1`、`src-tauri/gen/schemas/windows-schema.json`
   都是未追踪文件，与本次改动无关（`.shots`/`ui-drive.ps1` 是这次会话
   里用 windows-control MCP 做真机走查时产生的截图/脚本工具；
   `node_modules` 与 `windows-schema.json` 来源不明，不属于这份改动，
   本次提交不会带上它们）。不要把它们加进任何提交。
3. **每次要在浏览器/真机里看到最新 UI**，光 `cargo build` 不够——Tauri
   的 `beforeBuildCommand`（`npm run build` 生成 `ui/dist`）不会被单独
   的 `cargo build` 触发，要先手动 `cd ui && npm run build`。
4. `router::tests::a_direct_connect_failure_is_reported_with_the_target`
   在这台机器上可能单独失败，是环境性的，不代表你的改动引入了回归。

## 这次工作流程上的经验（供继续执行时参考）

- 本 Part 1 计划一直按 subagent-driven-development 走：每个 Task 派一个
  全新的实现子代理 → spec 合规审查（独立验证，不采信实现者自述）→
  代码质量审查 → 有问题打回去修 → 复审通过才算完成。Task 3 剩下的部分
  建议继续这个模式，不要图快跳过审查环节。
- 这一轮会话里 Task 3 的实现子代理连续遇到 4 次 `504 Gateway Timeout`
  （推理网关的临时问题，与代码逻辑无关）。处理方式是原地恢复 3 次
  （每次先确认自己的环境本身还正常、用 `git status`/`git diff` 核实
  子代理没丢进度、逐次拉长等待间隔），第 4 次之后才换一个全新子代理
  接手（把已完成的部分原样描述给它，而不是让它从零开始）。如果再遇到
  同样的网关错误，这个处理顺序是已经验证有效的。
