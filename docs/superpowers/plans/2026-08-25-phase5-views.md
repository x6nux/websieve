# 阶段 5：五个视图 — 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在阶段 4 已搭好的 Svelte + Vite 控制窗口里，实现五个视图 —— 流量走向（桑基图 + 必需的等价表视图）、规则视图（命中热度染色 + 探针即搜索框）、出站列表、设置覆盖层、空状态 —— 并让每一个都通过键盘遍历与屏幕阅读器审计。

**Architecture:** 逻辑与渲染彻底分离。所有可判定的逻辑（桑基布局、热度归一化、路径闭包、排序、探针高亮）落在**纯 JS 模块**里，用 vitest 穷举单测；Svelte 组件只负责把纯函数的输出映射成 DOM。d3-sankey 只做布局计算，**绝不碰 DOM**，SVG 全部由 Svelte 的 `{#each}` 渲染。这条纪律与阶段 1 的路由引擎同源：能被穷举单测的部分绝不混进不能被单测的部分。

**Tech Stack:** Svelte 5.56（runes，非 store）· Vite 8.2 · `@sveltejs/vite-plugin-svelte` 7.3 · 手写 CSS（无组件库）· `d3-sankey` 0.12.3 + `d3-shape` 3.2（仅布局）· vitest 4.1 + jsdom 30 + @testing-library/svelte 5.4

**依据:** `docs/superpowers/specs/2026-08-24-client-routing-and-ui-design.md` §6.4 §11.3 §11.4 §11.5 §11.6 §12 §14

**视觉依据（已定稿的 mockup，不是草图）:**
- `.superpowers/brainstorm/53735-1787573911/design-direction.html` —— 规则视图、出站视图、色板、两个 signature
- `.superpowers/brainstorm/53735-1787573911/sankey.html` —— 流量视图、表视图兜底

---

## 前置阅读（实现者必读）

- **§11.4 视觉基线** —— 这一节是**规定性**的，不是描述性的。表面层级、边框、文本四级、状态色、字阶、密度全部已定死。**明确拒绝的三个套路**那张表尤其要读进去
- **§11.5 两个 signature** —— 命中热度染色与探针即搜索框。这是本阶段的产品价值所在，不是装饰
- **§11.6 流量走向视图** —— 桑基图的**每一个决定都已经做完了**。着色、聚合、标签、交互、实时更新、集成、空状态七行表格逐条落地即可，不要重新设计
- **§6.4** —— 出站不可用时拒绝连接 + UI 报警，绝不静默回退。UI 侧要把这个「报警」真的做出来
- **§12** —— 错误处理表，其中至少 6 行需要 UI 呈现

**本阶段的死线（写在最前面，因为最容易被将就掉）：**

> **表视图是必需的等价视图，不是可选装饰。** spec §11.6 记录了桑基图的无障碍评级为 **C** —— 结构性流图无法只靠颜色传达。表视图必须可排序、可键盘遍历、屏幕阅读器友好，并且**与图共享同一份数据**。Task 8 的测试就是守这条线的，**不要为了让测试通过而放宽断言**。

**项目既有惯例（请遵守）：**

- 代码注释用**中文**，与仓库现有代码一致
- 刻意的简化用 `ponytail:` 注释标注并写明**上限与升级路径**
- 错误绝不静默跳过

**反 AI 塑料感（本项目有 design hook 强制执行）：**

| 禁止 | 替代 |
|---|---|
| 卡片左侧粗色装饰边框 | borders-only，`rgba(255,255,255,.07)` 均匀一圈 |
| 大面积渐变 | 唯一允许的渐变是桑基流带的 `userSpaceOnUse`，且它承载语义（去向） |
| 无意义留白撑场面 | 规则行 32px、出站行 38px，padding 12–16px，密度像交易台 |
| 彩色 pill 标签 | 规则类型用等宽小写缩写 + 统一低对比灰 |
| 左侧图标导航栏 | segmented control |

**颜色纪律**：约 90% 屏幕为中性结构色。彩色只出现在**出站色码**与**状态色**上，它在规则行、表格行、桑基节点里指的永远是同一件事：**去哪**。

---

## 阶段 4 交接假设

本计划**建立在阶段 4 之上**，不重复它已经做过的事。动工前先核对下列产物：

```
src-tauri/capabilities/control.json     控制窗口 capability（local: true，无 remote 字段）
src-tauri/capabilities/transport.json   传输窗口 capability（只有三个 wsieve_* 权限）

ui/                                     Svelte 5 + Vite 8 项目根
  package.json                          已含 svelte / vite / @fontsource/ibm-plex-*
  vite.config.js
  src/
    index.html
    main.js                             mount(App, { target })
    App.svelte                          骨架，**本阶段整个换掉**
    tokens.css                          ← 设计令牌的唯一定义处（见 Task 1）
    lib/ipc.js                          invoke / listen 的薄封装
```

**核对命令：**

```bash
ls src-tauri/capabilities/control.json src-tauri/capabilities/transport.json
ls ui/src/tokens.css ui/src/lib/ipc.js ui/src/App.svelte
grep -c 'outbound-8' ui/src/tokens.css      # 应为 1：8 个出站色码都在
```

### 三条边界，别越过去

**① `ui/src/tokens.css` 是设计令牌的唯一定义处。**
本阶段只消费、不改名、不新增。需要什么而它没有，回头补在**它**里面（Task 1 Step 3 列了唯一一项缺失）。

**② 事件已在 Rust 侧聚合节流**（§11.2 / 阶段 4 Task 7）：`traffic` 1s、`connection` 200ms 一批、`rule-hit` 1s 增量。
**前端不再二次节流** —— 那只会叠加延迟，而节流的正确位置在产生侧。

**③ 阶段 4 的命令面里有一半是 `not_ready` 占位**（`rule_test` 等待阶段 1 的路由引擎接入、
`outbound_latency_probe` 等待阶段 2 的出站管理器）。本阶段的视图**必须能在命令报未就绪时正常渲染** ——
显示占位符与提示，而不是白屏或抛异常。Task 16 的 `safeInvoke` 就是干这个的。

### 数据契约的三处落差（必须处理，不要假装没有）

阶段 4 的事件 payload 与本阶段视图所需的形状**不完全对齐**。这不是谁写错了，
而是阶段 4 只做到「计数与节流」，逐流明细要等阶段 2 的出站管理器。落差有三处：

| 视图需要 | 阶段 4 提供 | 本阶段的处置 |
|---|---|---|
| 逐流明细 `{ site, rule, outbound, bytes, conns }[]` | `TrafficSample { up_bytes, down_bytes, up_rate, down_rate, active }` —— **只有总量，没有分流** | 桑基图与表视图的数据源是 `connection` 事件的 `ConnectionDelta { id, target, outbound, state }` **在前端聚合**。见 Task 3 的 `flows.js`。**这是本阶段新增的一个模块，别漏掉** |
| 规则列表含 `hits` | `config_get` 返回配置 JSON（无 hits）；`rule-hit` 事件是 `HashMap<String, u64>` **增量**，键是规则字符串 | 视图初始 hits 全为 0，靠 `rule-hit` 事件累加。**首次打开窗口时热度全灰是预期的**，不是 bug |
| 规则排序 / 启停命令 | 阶段 4 **没有** `rule_reorder` / `rule_enable` | 本阶段用 `config_save` 写回整份规则数组。阶段 1 的 `edit.rs` 已保证注释保留 |

**`ConnectionDelta` 没有字节数**（只有 `id / target / outbound / state`）。
因此本阶段的「按字节」视图在阶段 2 补齐逐流字节统计之前，
**只能按连接数计算**。这一点必须在 UI 上如实呈现，不能拿连接数假装成字节数 ——
见 Task 16 的 `byteMode` 处理。

> **给实现者的判断**：若你动工时阶段 2 已完成、`traffic` 事件已带逐流字节，
> 就直接用它，把 `flows.js` 的聚合退化成透传。两条路本计划都留了口子。

---

## 文件结构

```
ui/
  package.json                     修改：加 d3-sankey / d3-shape / vitest 等
  vitest.config.js                 新建
  src/test-setup.js                新建
  src/tokens.test.js               新建 —— 令牌契约测试（定义在阶段 4）

  src/tokens.css                   ← 阶段 4 的产物。本阶段只补一个 .sr-only

  src/lib/format.js                新建 —— 字节/数字/百分比格式化（纯函数）
  src/lib/format.test.js
  src/lib/flows.js                 新建 —— connection 事件 → 逐流明细（纯函数）
  src/lib/flows.test.js
  src/lib/aggregate.js             新建 —— Top N 聚合（纯函数）
  src/lib/aggregate.test.js
  src/lib/sankey-layout.js         新建 —— d3 布局封装（纯函数，不碰 DOM）
  src/lib/sankey-layout.test.js
  src/lib/trace.js                 新建 —— hover 路径闭包（纯函数）
  src/lib/trace.test.js
  src/lib/heat.js                  新建 —— 命中热度归一化（纯函数）
  src/lib/heat.test.js
  src/lib/reorder.js               新建 —— 规则排序（纯函数）
  src/lib/reorder.test.js
  src/lib/palette.js               新建 —— 出站色码分配（纯函数）
  src/lib/palette.test.js

  src/lib/Segmented.svelte         新建 —— segmented control（可复用）
  src/lib/Switch.svelte            新建 —— 开关（可复用）

  src/views/TrafficView.svelte     新建 —— 流量视图外壳（图/表切换）
  src/views/Sankey.svelte          新建 —— 桑基图
  src/views/FlowTable.svelte       新建 —— 表视图（必需等价视图）
  src/views/FlowTable.test.js
  src/views/RulesView.svelte       新建 —— 规则视图
  src/views/RulesView.test.js
  src/views/Probe.svelte           新建 —— signature ②
  src/views/Probe.test.js
  src/views/OutboundsView.svelte   新建 —— 出站列表
  src/views/OutboundsView.test.js
  src/views/SettingsOverlay.svelte 新建 —— 设置覆盖层
  src/views/SettingsOverlay.test.js
  src/views/EmptyState.svelte      新建 —— 空状态/首次运行

  src/App.svelte                   ← 阶段 4 的骨架，本阶段整个换掉
  src/a11y.test.js                 新建 —— 跨组件无障碍审计
```

**为什么逻辑要抽成 `src/lib/*.js` 而不是写在组件里**：桑基布局、热度归一化、路径闭包、Top N 聚合、
逐流聚合这几件事全都是**可判定的纯函数**，抽出来能被穷举单测。留在 `.svelte` 里就只能靠渲染断言
间接测，而渲染断言对 SVG 几何几乎无能为力。这与阶段 1 把路由引擎做成纯同步函数是同一条纪律。

---

## 已实测的技术事实（不要重新踩一遍）

写这份计划时在 `/tmp` 建了一个真实的 Vite + Svelte 项目，把三层桑基图**实际渲染出来**并在浏览器里验证过。以下每一条都是真跑出来的结果，**不是推测**：

### 版本

| 包 | 版本（2026-08 实测） |
|---|---|
| svelte | **5.56.10** —— runes（`$state` / `$derived` / `$props` / `$effect`），**不是** Svelte 4 的 store |
| vite | 8.2.2 |
| @sveltejs/vite-plugin-svelte | 7.3.0 |
| d3-sankey | **0.12.3** |
| d3-shape（顶层解析） | 3.2.0 |
| vitest / jsdom | 4.1.11 / 30.0.1 |

`npm create vite@latest -- --template svelte` 今天脚手架出来的就是 Svelte 5 + runes，`src/main.js` 用的是 `mount(App, {...})` 而非 `new App({...})`。**按 runes 写。**

### d3-sankey 的五个坑（全部实测）

**① 它会就地改写输入对象。** `sankey(graph)` 直接往你传进去的 node 上塞 `sourceLinks` / `targetLinks` / `x0` / `y0`，而且是**循环引用**（link.source 被替换成 node 对象本身）。复用同一份对象跑第二次布局会读到脏数据。**必须 `structuredClone`。**

**② `nodeSort(null)` 才能冻结列内顺序。** 这正是 §11.6「节点顺序一旦确定即冻结」的落地方式。实测：把输入数组倒序后，`nodeSort(null)` 下输出的列内 y 序**严格等于输入数组顺序**；不设则 d3 按自己的重心算法重排。`linkSort(null)` 同理。

```
输入顺序(第0列)：  [其他, google, github, taobao, youtube]
freeze  后 y 序：  [其他, google, github, taobao, youtube]   ← 一致
不freeze 后 y 序： [github, taobao, google, 其他, youtube]   ← 被重排
```

**③ 退化输入会产出 `NaN` / `null`，且不抛错。** 实测：

| 输入 | 结果 |
|---|---|
| 零流带（只有节点） | `x0=NaN, y0=NaN` —— **静默**产出 NaN |
| 全零字节 | 节点 `y0=NaN`，流带 `width=NaN` |
| 引用不存在的节点 | 抛 `Error: missing: ghost`（这个反而是好的） |
| 孤儿节点 + 其他正常流带 | 正常，无 NaN |
| 单条流带 | 正常 |

`NaN` 进了 SVG 属性不会报错，只会**什么都不画**。所以 `toGraph()` 必须在调用 d3 之前把这两种情况挡成 `null`，由调用方走空状态分支。

**④ `sankey.update()` 不重算 `width`。** 它只依据现有的 `node.y0` 与 `link.width` 重排 link 的 `y0`/`y1`。想做宽度插值得自己来。

**⑤ `extent` 必须为标签留出左右留白。** 若用满整幅 SVG，左列的站点名（右对齐在节点左侧）与右列的出站名（左对齐在节点右侧）会被 viewBox 裁掉 —— 实测第一版就是这么糊的。取 mockup 的数值：960 宽下左列 rect 在 x=130、右列在 x=730。

### 三个只有真跑才能发现的 bug

**① 流带必须按 `(source,target)` 聚合。** 同一条规则会被多个站点命中，因此 `rule→outbound` 这条边在原始流水行里**重复出现**。不聚合的话 Svelte 直接抛 `each_key_duplicate` 且**整个组件渲染不出来**（页面全黑，console 里才有错）。这个 bug 在第一次真渲染时就撞上了。

**② SVG 渐变 id 不能用 `encodeURIComponent`。** 出站名是中文，做 id 时的直觉是 `encodeURIComponent('日本节点')` → `%E6%97%A5%E6%9C%AC%E8%8A%82%E7%82%B9`。**实测：`fill="url(#g-%E6%97%A5...)"` 静默不上色**，元素完全不可见，`getComputedStyle` 返回的 fill 值看起来还是对的。必须用不含 `%` 的编码方式（下面 Task 4 给的是逐字符 base36）。

**③ hover 只比相邻会漏掉第二跳。** 判断 `link.sourceId === hovered || link.targetId === hovered` 时，hover 站点节点只能点亮第一跳，`规则→出站` 那一跳会被误暗。实测 12 条流带里 11 条 faded，但其中一条是错的。必须做**双向可达闭包**。

### 性能

单次完整布局（Top 12 × Top 8 × 4 出站 = 24 节点 / 20 流带）：**0.065 ms**，占 60fps 预算的 0.4%。即使放大到 50×20×10（70 流带）也只要 0.216 ms。

**结论：每帧重算完整布局完全可行。** 这一条很关键，它决定了插值策略 —— 不要用 CSS 插值 `stroke-width`（那样节点高度和贝塞尔路径会瞬跳，形状被撕开），而是**插值「字节数」本身，每帧重算布局**，让节点高度、流带宽度、路径三者同步变化。实测 11 个插值步全程列内顺序不变。

---

## Part A — 地基

### Task 1: 对齐并验证阶段 4 的设计令牌

**Files:**
- Modify: `ui/package.json`
- Create: `ui/vitest.config.js`
- Create: `ui/src/test-setup.js`
- Create: `ui/src/tokens.test.js`

> **设计令牌的唯一定义处是阶段 4 的 `ui/src/tokens.css`。**
>
> 本阶段**只消费，不重复定义、不改名、不新增**。两份计划各定一套变量名，
> 实现者照做就会得到两套 CSS 变量互相覆盖 —— 这类问题在浏览器里排查极其恶心，
> 而且第二套会**静默**覆盖第一套，没有任何报错。
>
> 若发现本阶段确实需要而阶段 4 没有的令牌，**回头补在阶段 4 的 `tokens.css` 里**，
> 不要在阶段 5 就地新增。见本 task Step 3 的清单。

- [ ] **Step 1: 装依赖**

阶段 4 已装好 `svelte` / `vite` / `@fontsource/ibm-plex-{sans,mono}`。本阶段只补两类：

```bash
cd ui
npm i d3-sankey d3-shape
npm i -D vitest jsdom @testing-library/svelte @testing-library/jest-dom @testing-library/user-event
```

核对版本 —— 若 svelte 主版本不是 5，**停下来**，本计划全部按 runes 写：

```bash
node -e "for (const p of ['svelte','vite','d3-sankey','vitest']) console.log(p, require('./node_modules/'+p+'/package.json').version)"
```

Expected: `svelte 5.x` · `vite 8.x` · `d3-sankey 0.12.3` · `vitest 4.x`

- [ ] **Step 2: 建 vitest 配置**

`ui/vitest.config.js`：

```js
import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

export default defineConfig({
  plugins: [svelte()],
  test: {
    environment: 'jsdom',
    globals: true,
    setupFiles: ['./src/test-setup.js'],
  },
  // 组件测试要拿到 svelte 的 browser 版入口，否则 mount 行为不对
  resolve: { conditions: ['browser'] },
});
```

> **不要写 `svelte({ hot: false })`** —— plugin-svelte 7.x 已移除该选项，会打印
> `invalid plugin option 'hot' in inline config`。实测踩过。

`ui/src/test-setup.js`：

```js
import '@testing-library/jest-dom/vitest';
```

在 `package.json` 的 `scripts` 里加：

```json
"test": "vitest run",
"test:watch": "vitest"
```

- [ ] **Step 3: 核对阶段 4 的令牌清单**

打开 `ui/src/tokens.css`，逐条确认下面这些**都已存在**。这份清单就是后续所有 Task 的
引用基准 —— 本阶段的 CSS **只许用这里出现的名字**：

| 类别 | 变量名 | 值 | 本阶段用在哪 |
|---|---|---|---|
| 表面 | `--surface-0` | `#16181b` | 画布、状态条、表头 |
| | `--surface-1` | `#1c1f23` | 视图面板、SVG 背景、标签描边色 |
| | `--surface-2` | `#23272c` | segmented 选中态、按钮、tooltip |
| 边框 | `--border` | `rgba(255,255,255,.07)` | 所有分隔线（borders-only） |
| | `--border-strong` | `rgba(255,255,255,.13)` | 输入框、浮层、开关关闭态 |
| 文本 | `--text-1` | `#e6e8ea` | 主要文本、匹配值、出站名 |
| | `--text-2` | `#a4abb3` | 次要文本、表格单元格 |
| | `--text-3` | `#6f777f` | 弱化文本、类型列、命中数 |
| | `--text-4` | `#4d545b` | 表头、单位、聚合行、hint |
| 状态 | `--state-live` | `#3fb27f` | 已连接状态点、开关开启态 |
| | `--state-warn` | `#d99a3f` | 重连中、需解析提示 |
| | `--state-fail` | `#d1595c` | REJECT、断开、错误、报警 |
| | `--state-direct` | `#7d8894` | DIRECT 色码 |
| 出站色码 | `--outbound-1` … `--outbound-8` | 见 tokens.css | 出站色码轮转（**8 个**）、焦点环 |
| 热度 | `--heat-min` | `rgba(255,255,255,.014)` | signature ① 的下界 |
| | `--heat-max` | `rgba(255,255,255,.052)` | signature ① 的上界 |
| 字体 | `--font-sans` / `--font-mono` | IBM Plex | 全局 / 等宽列 |
| 字阶 | `--fs-11` … `--fs-22` | 11/12/13/14/16/18/22 | 全部文本 |
| 字重 | `--fw-regular/medium/semibold` | 400/500/600 | 层级靠字重而非字号 |
| 间距 | `--space-1` … `--space-6` | 4/8/12/16/20/24 | 全部 padding |
| 密度 | `--row-rule` | `32px` | 规则行高 |
| | `--row-outbound` | `38px` | 出站行高 |
| 浮层 | `--shadow-overlay` | `0 8px 32px rgba(0,0,0,.5)` | **仅**设置覆盖层 |
| 圆角 | `--radius` | `4px` | 全部圆角 |

阶段 4 的 `tokens.css` 还已经写好了这些**全局规则**，本阶段直接受益，不要重复写：

- `.mono` / `.num` 的 `font-variant-numeric: tabular-nums`
- `:focus-visible` 的焦点环（`2px solid var(--outbound-1)`）
- `prefers-reduced-motion` 下的全局过渡压制
- `@fontsource` 的字体 `@import`（**零外部网络请求**）

**本阶段需要、而阶段 4 的 `tokens.css` 尚缺的（须回头补在阶段 4，不要在阶段 5 新增）：**

| 缺失 | 用途 | 建议补的定义 |
|---|---|---|
| `.sr-only` | 屏幕阅读器专用文本。表视图的 `<caption>`、桑基图摘要、各处 `<th>` 的隐藏标签都要用它。**这是无障碍硬要求**，Task 8 / 13 / 14 全部依赖 | 见下方代码块 |

```css
/* 补进阶段 4 的 ui/src/tokens.css。
 * 屏幕阅读器可读、视觉上不可见。桑基图评级为 C（§11.6），
 * 表视图的 caption 与各处隐藏标签是无障碍兜底的一部分。 */
.sr-only {
  position: absolute;
  width: 1px;
  height: 1px;
  padding: 0;
  margin: -1px;
  overflow: hidden;
  clip-path: inset(50%);
  white-space: nowrap;
}
```

> 除此之外**没有其他缺失**。阶段 4 的令牌覆盖了本阶段的全部需要，
> 包括容易被忽略的 `--heat-min` / `--heat-max`（signature ①）与
> `--shadow-overlay`（borders-only 的唯一例外）。

- [ ] **Step 4: 写令牌契约测试**

令牌是跨阶段的接口。加一个测试把「阶段 5 依赖的名字」钉死 ——
阶段 4 若改名，这里会立刻红，而不是等到某个视图静默失去样式。

`ui/src/tokens.test.js`：

```js
/**
 * 令牌契约测试。
 *
 * 设计令牌的唯一定义处是阶段 4 的 ui/src/tokens.css，本阶段只消费。
 * 这个测试把「阶段 5 用到的变量名」钉死：阶段 4 若改名或删除，
 * 这里立刻失败，而不是让某个视图在浏览器里静默失去样式 ——
 * 后者是最难排查的一类问题。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const css = readFileSync(fileURLToPath(new URL('./tokens.css', import.meta.url)), 'utf8');

const REQUIRED = [
  '--surface-0', '--surface-1', '--surface-2',
  '--border', '--border-strong',
  '--text-1', '--text-2', '--text-3', '--text-4',
  '--state-live', '--state-warn', '--state-fail', '--state-direct',
  '--heat-min', '--heat-max',
  '--font-sans', '--font-mono',
  '--fs-11', '--fs-12', '--fs-13', '--fs-14', '--fs-18', '--fs-22',
  '--fw-regular', '--fw-medium', '--fw-semibold',
  '--space-1', '--space-2', '--space-3', '--space-4',
  '--row-rule', '--row-outbound',
  '--shadow-overlay', '--radius',
];

describe('设计令牌契约（定义在阶段 4）', () => {
  for (const name of REQUIRED) {
    it(`${name} 存在`, () => {
      expect(css).toMatch(new RegExp(`^\\s*${name}\\s*:`, 'm'));
    });
  }

  it('出站色码有 8 个 —— 轮转的取模基数依赖它', () => {
    for (let i = 1; i <= 8; i++) {
      expect(css).toMatch(new RegExp(`^\\s*--outbound-${i}\\s*:`, 'm'));
    }
  });

  it('.sr-only 存在 —— 无障碍兜底依赖它', () => {
    expect(css).toMatch(/\.sr-only\s*\{/);
  });

  it('字体走 @fontsource 而非外部 CDN —— 代理工具不该自己联网', () => {
    expect(css).toMatch(/@fontsource/);
    expect(css).not.toMatch(/fonts\.googleapis\.com|fonts\.gstatic\.com/);
  });
});
```

- [ ] **Step 5: 跑一遍并提交**

Run: `cd ui && npx vitest run tokens`
Expected: 全部 PASS。

**若 `.sr-only 存在` 这条失败**，说明 Step 3 的缺失项还没补 ——
回到阶段 4 的 `ui/src/tokens.css` 补上，**不要在阶段 5 就地加**。

```bash
git add ui/package.json ui/package-lock.json \
        ui/vitest.config.js ui/src/test-setup.js ui/src/tokens.test.js \
        ui/src/tokens.css
git commit -m "chore(ui): 阶段 5 依赖、vitest 环境与令牌契约测试"
```

---

### Task 2: 格式化纯函数

**Files:**
- Create: `ui/src/lib/format.js`
- Create: `ui/src/lib/format.test.js`

字节数、命中数、百分比、延迟在五个视图里到处都是。集中一处并单测，避免各视图各写一份、格式还不一致。

- [ ] **Step 1: 写失败的测试**

`ui/src/lib/format.test.js`：

```js
import { describe, it, expect } from 'vitest';
import { bytes, count, pct, ms } from './format.js';

describe('bytes', () => {
  it('按二进制进位并保持三位有效数字', () => {
    expect(bytes(0)).toBe('0 B');
    expect(bytes(999)).toBe('999 B');
    expect(bytes(1024)).toBe('1.00 KB');
    expect(bytes(1536)).toBe('1.50 KB');
    expect(bytes(1024 * 1024)).toBe('1.00 MB');
    expect(bytes(748 * 1024 * 1024)).toBe('748 MB');
    expect(bytes(2.41 * 1024 ** 3)).toBe('2.41 GB');
  });

  it('大数不退化成科学计数法', () => {
    expect(bytes(1024 ** 5)).not.toMatch(/e\+/);
  });

  it('非法输入返回占位符而不是 NaN', () => {
    expect(bytes(NaN)).toBe('—');
    expect(bytes(-1)).toBe('—');
    expect(bytes(undefined)).toBe('—');
  });
});

describe('count', () => {
  it('加千分位 —— 命中数要能一眼看出量级', () => {
    expect(count(0)).toBe('0');
    expect(count(999)).toBe('999');
    expect(count(88120)).toBe('88,120');
  });
  it('非法输入返回占位符', () => {
    expect(count(NaN)).toBe('—');
  });
});

describe('pct', () => {
  it('整数百分比', () => {
    expect(pct(0.62)).toBe('62%');
    expect(pct(0)).toBe('0%');
    expect(pct(1)).toBe('100%');
  });
  it('极小占比不显示成 0% —— 那会让用户以为没有流量', () => {
    expect(pct(0.0004)).toBe('<1%');
  });
  it('分母为零时返回占位符', () => {
    expect(pct(NaN)).toBe('—');
  });
});

describe('ms', () => {
  it('延迟带单位', () => {
    expect(ms(38)).toBe('38 ms');
    expect(ms(1240)).toBe('1.24 s');
  });
  it('未测得时是占位符而非 0 —— 0ms 会被误读成「极快」', () => {
    expect(ms(null)).toBe('—');
    expect(ms(undefined)).toBe('—');
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd ui && npx vitest run format`
Expected: `Failed to load .../format.js`

- [ ] **Step 3: 写实现**

`ui/src/lib/format.js`：

```js
/** 展示层格式化。集中一处并单测，避免各视图各写一份、格式还不一致。 */

const DASH = '—';

const UNITS = ['B', 'KB', 'MB', 'GB', 'TB', 'PB', 'EB'];

/**
 * 二进制进位的字节数，保持三位有效数字。
 * 三位有效数字是为了让数字**列宽稳定** —— 变宽的数字列没法纵向扫读。
 */
export function bytes(n) {
  if (typeof n !== 'number' || !Number.isFinite(n) || n < 0) return DASH;
  if (n < 1024) return `${Math.round(n)} B`;
  let v = n;
  let i = 0;
  while (v >= 1024 && i < UNITS.length - 1) {
    v /= 1024;
    i++;
  }
  // 三位有效数字：<10 保留两位小数，<100 保留一位，其余取整
  const s = v < 10 ? v.toFixed(2) : v < 100 ? v.toFixed(1) : String(Math.round(v));
  return `${s} ${UNITS[i]}`;
}

export function count(n) {
  if (typeof n !== 'number' || !Number.isFinite(n) || n < 0) return DASH;
  return Math.round(n).toLocaleString('en-US');
}

/** ratio 是 0..1 的比值。极小但非零的占比显示为 `<1%` 而非 `0%`。 */
export function pct(ratio) {
  if (typeof ratio !== 'number' || !Number.isFinite(ratio)) return DASH;
  if (ratio > 0 && ratio < 0.005) return '<1%';
  return `${Math.round(ratio * 100)}%`;
}

/** 延迟。未测得时给占位符 —— 显示 0 ms 会被误读成「极快」。 */
export function ms(v) {
  if (typeof v !== 'number' || !Number.isFinite(v) || v < 0) return DASH;
  if (v < 1000) return `${Math.round(v)} ms`;
  return `${(v / 1000).toFixed(2)} s`;
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd ui && npx vitest run format`
Expected: 12 个测试全部 PASS

- [ ] **Step 5: 提交**

```bash
git add ui/src/lib/format.js ui/src/lib/format.test.js
git commit -m "feat(ui): 展示层格式化纯函数"
```

---

### Task 3: 连接明细 → 逐流聚合

**Files:**
- Create: `ui/src/lib/flows.js`
- Create: `ui/src/lib/flows.test.js`

**这个模块是为了填补「阶段 4 交接假设」里记录的数据落差。**

阶段 4 的 `traffic` 事件只有总量（`up_bytes` / `down_bytes` / `active`），没有分流明细；
而桑基图与表视图要的是逐条 `{ site, rule, outbound, bytes, conns }`。
可用的数据源是 `connection` 事件的 `ConnectionDelta`：

```rust
// 阶段 4 events.rs
pub struct ConnectionDelta {
    pub id: u64,
    pub target: String,       // "example.com:443"
    pub outbound: String,     // 出站名，或 "DIRECT" / "REJECT"
    pub state: ConnState,     // open | close | reject
}
```

注意它**没有字节数**，也**没有命中的规则**。因此在阶段 2 补齐逐流统计之前：

- 「按连接数」是**真实**的
- 「按字节」**无法计算** —— UI 必须如实说明，绝不能拿连接数假装成字节数

- [ ] **Step 1: 写失败的测试**

`ui/src/lib/flows.test.js`：

```js
import { describe, it, expect } from 'vitest';
import { FlowStore, hostOf } from './flows.js';

const open = (id, target, outbound) => ({ id, target, outbound, state: 'open' });

describe('hostOf', () => {
  it('剥掉端口', () => {
    expect(hostOf('example.com:443')).toBe('example.com');
    expect(hostOf('1.2.3.4:80')).toBe('1.2.3.4');
  });
  it('IPv6 字面量不被冒号误伤', () => {
    expect(hostOf('[2001:db8::1]:443')).toBe('2001:db8::1');
  });
  it('没有端口时原样返回', () => {
    expect(hostOf('example.com')).toBe('example.com');
  });
  it('空输入不抛错', () => {
    expect(hostOf('')).toBe('');
    expect(hostOf(undefined)).toBe('');
  });
});

describe('FlowStore', () => {
  it('把连接按 (站点, 规则, 出站) 聚成流', () => {
    const s = new FlowStore();
    s.apply([open(1, 'a.com:443', 'JP'), open(2, 'a.com:443', 'JP'), open(3, 'b.com:443', 'DIRECT')]);
    const rows = s.rows();
    expect(rows).toHaveLength(2);
    const a = rows.find((r) => r.site === 'a.com');
    expect(a.conns).toBe(2);
    expect(a.outbound).toBe('JP');
  });

  it('同一站点走不同出站算两条流 —— 去向是流的身份', () => {
    const s = new FlowStore();
    s.apply([open(1, 'a.com:443', 'JP'), open(2, 'a.com:443', 'SG')]);
    expect(s.rows()).toHaveLength(2);
  });

  it('close 不减少累计连接数 —— 这是累计量而非当前量', () => {
    const s = new FlowStore();
    s.apply([open(1, 'a.com:443', 'JP')]);
    s.apply([{ id: 1, target: 'a.com:443', outbound: 'JP', state: 'close' }]);
    expect(s.rows()[0].conns).toBe(1);
  });

  it('reject 归到 REJECT 出站', () => {
    const s = new FlowStore();
    s.apply([{ id: 1, target: 'ads.com:443', outbound: 'REJECT', state: 'reject' }]);
    expect(s.rows()[0].outbound).toBe('REJECT');
  });

  it('同一 id 重复 open 不重复计数', () => {
    const s = new FlowStore();
    s.apply([open(1, 'a.com:443', 'JP')]);
    s.apply([open(1, 'a.com:443', 'JP')]);
    expect(s.rows()[0].conns).toBe(1);
  });

  it('规则未知时用占位符而非留空 —— 桑基图中间层不能有空节点', () => {
    const s = new FlowStore();
    s.apply([open(1, 'a.com:443', 'JP')]);
    expect(s.rows()[0].rule).toBeTruthy();
  });

  it('已知规则时带上（阶段 2 补齐 rule 字段后自动生效）', () => {
    const s = new FlowStore();
    s.apply([{ ...open(1, 'a.com:443', 'JP'), rule: 'geosite cn' }]);
    expect(s.rows()[0].rule).toBe('geosite cn');
  });

  it('bytes 字段存在但在无字节数据时为 0', () => {
    const s = new FlowStore();
    s.apply([open(1, 'a.com:443', 'JP')]);
    expect(s.rows()[0].bytes).toBe(0);
    expect(s.hasBytes()).toBe(false);
  });

  it('事件带 bytes 时如实累加并翻转 hasBytes', () => {
    const s = new FlowStore();
    s.apply([{ ...open(1, 'a.com:443', 'JP'), bytes: 1024 }]);
    expect(s.rows()[0].bytes).toBe(1024);
    expect(s.hasBytes()).toBe(true);
  });

  it('有上限，不会无限增长', () => {
    const s = new FlowStore(50);
    for (let i = 0; i < 200; i++) s.apply([open(i, `s${i}.com:443`, 'JP')]);
    expect(s.rows().length).toBeLessThanOrEqual(50);
  });

  it('触顶时保留连接数最多的，而非最先到的', () => {
    const s = new FlowStore(2);
    s.apply([open(1, 'hot.com:443', 'JP'), open(2, 'hot.com:443', 'JP'), open(3, 'hot.com:443', 'JP')]);
    s.apply([open(4, 'cold1.com:443', 'JP'), open(5, 'cold2.com:443', 'JP')]);
    expect(s.rows().some((r) => r.site === 'hot.com')).toBe(true);
  });

  it('reset 清空', () => {
    const s = new FlowStore();
    s.apply([open(1, 'a.com:443', 'JP')]);
    s.reset();
    expect(s.rows()).toHaveLength(0);
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd ui && npx vitest run flows`
Expected: `Failed to load .../flows.js`

- [ ] **Step 3: 写实现**

`ui/src/lib/flows.js`：

```js
/**
 * connection 事件 → 逐流明细。
 *
 * 存在的理由见「阶段 4 交接假设」：阶段 4 的 traffic 事件只有总量，
 * 没有分流；桑基图与表视图需要的逐流明细只能从 connection 事件
 * （ConnectionDelta）在前端聚合出来。
 *
 * **诚实优先**：ConnectionDelta 目前没有字节数，因此 bytes 恒为 0，
 * hasBytes() 返回 false，UI 据此显示「按连接数」而**不是**拿连接数
 * 假装成字节数。等阶段 2 给事件补上 bytes（以及 rule）字段，
 * 这里会自动开始用真实值，无需改动调用方。
 */

/** 从 "host:port" 取出 host。IPv6 字面量形如 "[::1]:443"，不能按最后一个冒号切。 */
export function hostOf(target) {
  if (!target) return '';
  const s = String(target);
  if (s.startsWith('[')) {
    const end = s.indexOf(']');
    return end > 0 ? s.slice(1, end) : s;
  }
  const i = s.lastIndexOf(':');
  // 冒号后面必须全是数字才算端口，否则原样返回
  return i > 0 && /^\d+$/.test(s.slice(i + 1)) ? s.slice(0, i) : s;
}

/** 规则未知时的占位。桑基图的中间层不能有空节点。 */
export const UNKNOWN_RULE = '（规则未记录）';

export class FlowStore {
  /**
   * @param {number} cap 最多保留多少条流。默认 500 —— 远超 Top 12 的需要，
   *   但足够小到不会在长时间运行后吃掉内存。
   */
  constructor(cap = 500) {
    this.cap = cap;
    this.flows = new Map();
    this.seen = new Set();      // 去重：同一连接 id 只计一次
    this.bytesSeen = false;
  }

  /** 事件里是否真的带了字节数。UI 用它决定「按字节」能不能选。 */
  hasBytes() {
    return this.bytesSeen;
  }

  apply(deltas = []) {
    for (const d of deltas) {
      if (!d) continue;
      if (typeof d.bytes === 'number' && d.bytes > 0) this.bytesSeen = true;

      // close 只是状态变更，不产生新连接计数
      const isNew = d.state !== 'close' && !this.seen.has(d.id);
      if (d.state !== 'close') this.seen.add(d.id);

      const site = hostOf(d.target);
      const rule = d.rule || UNKNOWN_RULE;
      const outbound = d.outbound || 'DIRECT';
      // 去向是流的身份：同一站点走不同出站是两条不同的流。
      // 分隔符用 NUL：域名与规则值里都不可能出现它，而 '|' 之类的
      // 可见字符有碰撞风险（规则值可以含任意字符）。
      const key = [site, rule, outbound].join('');

      const cur = this.flows.get(key);
      if (cur) {
        if (isNew) cur.conns += 1;
        cur.bytes += d.bytes ?? 0;
      } else {
        this.flows.set(key, { site, rule, outbound, conns: isNew ? 1 : 0, bytes: d.bytes ?? 0 });
      }
    }
    this.#trim();
  }

  /**
   * 触顶时淘汰**最小**的流而非最旧的。
   * 淘汰最旧会让长期活跃的大流被新来的一次性小流挤掉，
   * 而那恰恰是用户最想看到的东西。
   */
  #trim() {
    if (this.flows.size <= this.cap) return;
    const sorted = [...this.flows.entries()].sort(
      (a, b) => (b[1].bytes - a[1].bytes) || (b[1].conns - a[1].conns)
    );
    this.flows = new Map(sorted.slice(0, this.cap));
    // seen 集合同样需要封顶，否则它会独自无限增长
    if (this.seen.size > this.cap * 20) this.seen = new Set();
  }

  rows() {
    return [...this.flows.values()];
  }

  reset() {
    this.flows.clear();
    this.seen.clear();
    this.bytesSeen = false;
  }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd ui && npx vitest run flows`
Expected: 16 个测试全部 PASS。

**特别确认 `bytes 字段存在但在无字节数据时为 0` 与 `触顶时保留连接数最多的`** ——
前者守住「不拿连接数假装字节数」这条诚实底线，后者决定了长跑之后图上还看不看得见主要流量。

- [ ] **Step 5: 提交**

```bash
git add ui/src/lib/flows.js ui/src/lib/flows.test.js
git commit -m "feat(ui): connection 事件到逐流明细的前端聚合"
```

---

### Task 4: Top N 聚合

**Files:**
- Create: `ui/src/lib/aggregate.js`
- Create: `ui/src/lib/aggregate.test.js`

§11.6 规定：目标站点 Top 12、命中规则 Top 8，超出的压成「其他 N 个」并用最暗的中性色。这是纯粹的数据变换，先做完再谈画图。

- [ ] **Step 1: 写失败的测试**

`ui/src/lib/aggregate.test.js`：

```js
import { describe, it, expect } from 'vitest';
import { topN, prepareFlows, OTHER_SITE, OTHER_RULE } from './aggregate.js';

const flows = (...specs) =>
  specs.map(([site, rule, outbound, b, c]) => ({
    site, rule, outbound, bytes: b, conns: c ?? 1,
  }));

describe('topN', () => {
  it('按值降序取前 N', () => {
    const m = new Map([['a', 10], ['b', 30], ['c', 20]]);
    expect(topN(m, 2)).toEqual(new Set(['b', 'c']));
  });
  it('总数不超过 N 时全留', () => {
    const m = new Map([['a', 10], ['b', 30]]);
    expect(topN(m, 5)).toEqual(new Set(['a', 'b']));
  });
  it('并列时结果稳定（不随 Map 插入顺序抖动）', () => {
    const m1 = new Map([['a', 10], ['b', 10], ['c', 10]]);
    const m2 = new Map([['c', 10], ['b', 10], ['a', 10]]);
    expect([...topN(m1, 2)].sort()).toEqual([...topN(m2, 2)].sort());
  });
});

describe('prepareFlows', () => {
  it('超出 Top N 的站点被压成「其他」', () => {
    const rows = prepareFlows(
      flows(['a', 'r', 'O', 100], ['b', 'r', 'O', 90], ['c', 'r', 'O', 5], ['d', 'r', 'O', 3]),
      { sites: 2, rules: 8 }
    );
    const sites = new Set(rows.map((r) => r.site));
    expect(sites.has('a')).toBe(true);
    expect(sites.has('b')).toBe(true);
    expect(sites.has('c')).toBe(false);
    expect([...sites].some((s) => s.startsWith(OTHER_SITE))).toBe(true);
  });

  it('「其他」的字节数是被折叠项之和', () => {
    const rows = prepareFlows(
      flows(['a', 'r', 'O', 100], ['c', 'r', 'O', 5], ['d', 'r', 'O', 3]),
      { sites: 1, rules: 8 }
    );
    const other = rows.find((r) => r.site.startsWith(OTHER_SITE));
    expect(other.bytes).toBe(8);
    expect(other.conns).toBe(2);
  });

  it('「其他」的标签写明被折叠了几个', () => {
    const rows = prepareFlows(
      flows(['a', 'r', 'O', 100], ['c', 'r', 'O', 5], ['d', 'r', 'O', 3]),
      { sites: 1, rules: 8 }
    );
    expect(rows.find((r) => r.site.startsWith(OTHER_SITE)).site).toBe('其他 2 个站点');
  });

  it('规则也按自己的 Top N 折叠', () => {
    const rows = prepareFlows(
      flows(['a', 'r1', 'O', 100], ['a', 'r2', 'O', 50], ['a', 'r3', 'O', 1]),
      { sites: 12, rules: 2 }
    );
    expect(rows.some((r) => r.rule.startsWith(OTHER_RULE))).toBe(true);
  });

  it('折叠后同 (站点,规则,出站) 的行会合并，不留重复', () => {
    const rows = prepareFlows(
      flows(['x', 'r', 'O', 5], ['y', 'r', 'O', 3], ['z', 'r', 'O', 2]),
      { sites: 0, rules: 8 }
    );
    expect(rows).toHaveLength(1);
    expect(rows[0].bytes).toBe(10);
  });

  it('出站**绝不**折叠 —— 它是颜色语义的载体', () => {
    const rows = prepareFlows(
      flows(['a', 'r', 'O1', 100], ['b', 'r', 'O2', 1], ['c', 'r', 'O3', 1], ['d', 'r', 'O4', 1]),
      { sites: 12, rules: 8 }
    );
    expect(new Set(rows.map((r) => r.outbound)).size).toBe(4);
  });

  it('总字节守恒 —— 折叠不能凭空吞掉流量', () => {
    const src = flows(['a', 'r1', 'O', 100], ['b', 'r2', 'O', 50], ['c', 'r3', 'O', 7],
                      ['d', 'r4', 'O', 3], ['e', 'r5', 'O', 1]);
    const before = src.reduce((s, r) => s + r.bytes, 0);
    const after = prepareFlows(src, { sites: 2, rules: 2 }).reduce((s, r) => s + r.bytes, 0);
    expect(after).toBe(before);
  });

  it('空输入返回空数组', () => {
    expect(prepareFlows([], { sites: 12, rules: 8 })).toEqual([]);
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd ui && npx vitest run aggregate`
Expected: `Failed to load .../aggregate.js`

- [ ] **Step 3: 写实现**

`ui/src/lib/aggregate.js`：

```js
/**
 * Top N 聚合（spec §11.6）。
 *
 * 目标站点 Top 12、命中规则 Top 8，超出的压成「其他 N 个」。
 * **出站永不折叠** —— 它是颜色语义的载体，折叠掉就等于把「去哪」这件事
 * 藏起来，而那正是整个视图存在的理由。
 */

export const OTHER_SITE = '其他';
export const OTHER_RULE = '其他';

export const DEFAULT_LIMITS = { sites: 12, rules: 8 };

/**
 * 取值最大的前 N 个键。
 * 并列时按键名排序作为 tiebreak —— 否则结果会随 Map 插入顺序抖动，
 * 而抖动意味着节点顺序每秒都在变，正是 §11.6 要避免的。
 */
export function topN(totals, n) {
  return new Set(
    [...totals.entries()]
      .sort((a, b) => b[1] - a[1] || String(a[0]).localeCompare(String(b[0])))
      .slice(0, Math.max(0, n))
      .map(([k]) => k)
  );
}

function sumBy(rows, key) {
  const m = new Map();
  for (const r of rows) m.set(r[key], (m.get(r[key]) ?? 0) + r.bytes);
  return m;
}

/**
 * 原始流水 → 折叠后的流水。字节与连接数守恒。
 * 折叠后可能出现重复的 (站点,规则,出站) 三元组，必须合并 —— 否则
 * 桑基图会画出两条叠在一起的带子，Svelte 的 keyed each 还会直接报错。
 */
export function prepareFlows(rows, limits = DEFAULT_LIMITS) {
  if (!rows?.length) return [];

  const keepSites = topN(sumBy(rows, 'site'), limits.sites);
  const keepRules = topN(sumBy(rows, 'rule'), limits.rules);

  const droppedSites = new Set();
  const droppedRules = new Set();
  for (const r of rows) {
    if (!keepSites.has(r.site)) droppedSites.add(r.site);
    if (!keepRules.has(r.rule)) droppedRules.add(r.rule);
  }
  const siteLabel = `${OTHER_SITE} ${droppedSites.size} 个站点`;
  const ruleLabel = `${OTHER_RULE} ${droppedRules.size} 条规则`;

  const merged = new Map();
  for (const r of rows) {
    const site = keepSites.has(r.site) ? r.site : siteLabel;
    const rule = keepRules.has(r.rule) ? r.rule : ruleLabel;
    // 用 NUL 作分隔符：域名与规则值里都不可能出现它，
    // 用 '|' 之类的可见字符则有碰撞风险（规则值可以含任意字符）。
    const k = [site, rule, r.outbound].join('\u0000');
    const cur = merged.get(k);
    if (cur) {
      cur.bytes += r.bytes;
      cur.conns += r.conns ?? 0;
    } else {
      merged.set(k, {
        site,
        rule,
        outbound: r.outbound,
        bytes: r.bytes,
        conns: r.conns ?? 0,
        // 聚合行用最暗的中性色，且不参与「点击跳转到规则视图」
        aggregated: site === siteLabel || rule === ruleLabel,
      });
    }
  }
  return [...merged.values()];
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd ui && npx vitest run aggregate`
Expected: 11 个测试全部 PASS。**特别确认 `总字节守恒`** —— 折叠逻辑最容易在这里出错，而错了以后图上的总量对不上状态条的读数，用户会发现但查不出原因。

- [ ] **Step 5: 提交**

```bash
git add ui/src/lib/aggregate.js ui/src/lib/aggregate.test.js
git commit -m "feat(ui): Top N 流量聚合（字节守恒，出站不折叠）"
```

---

## Part B — 流量视图（默认视图）

### Task 5: 桑基布局封装

**Files:**
- Create: `ui/src/lib/sankey-layout.js`
- Create: `ui/src/lib/sankey-layout.test.js`

**这个模块是本阶段技术风险最集中的一处。** 下面的实现已在真实的 Vite + Svelte 项目里渲染验证过，五个 d3-sankey 的坑（见「已实测的技术事实」）都已经在代码里处理掉了。**请照抄，不要"优化"** —— 每一处看起来多余的地方都对应一个实测踩过的坑。

- [ ] **Step 1: 写失败的测试**

`ui/src/lib/sankey-layout.test.js`：

```js
import { describe, it, expect } from 'vitest';
import { toGraph, layout, gradientId } from './sankey-layout.js';

const rows = [
  { site: 'a.com', rule: 'final *', outbound: 'JP', bytes: 100, conns: 3 },
  { site: 'b.com', rule: 'final *', outbound: 'JP', bytes: 50, conns: 2 },
  { site: 'c.com', rule: 'geosite cn', outbound: 'DIRECT', bytes: 30, conns: 1 },
];

describe('toGraph', () => {
  it('把重复的 rule→outbound 边聚合成一条', () => {
    // a 与 b 都命中 final * 并走 JP，这条边在原始行里出现两次。
    // 不聚合的话 Svelte 的 keyed each 会直接抛 each_key_duplicate
    // 且整个组件渲染不出来（实测）。
    const g = toGraph(rows);
    const keys = g.links.map((l) => l.key);
    expect(new Set(keys).size).toBe(keys.length);
    const e = g.links.find((l) => l.key === 'r:final *>o:JP');
    expect(e.value).toBe(150);
  });

  it('三层各自建节点，不串层', () => {
    const g = toGraph(rows);
    expect(g.nodes.filter((n) => n.layer === 0)).toHaveLength(3);
    expect(g.nodes.filter((n) => n.layer === 1)).toHaveLength(2);
    expect(g.nodes.filter((n) => n.layer === 2)).toHaveLength(2);
  });

  it('只有出站节点带 dest（颜色语义只属于出站）', () => {
    const g = toGraph(rows);
    for (const n of g.nodes) {
      if (n.layer === 2) expect(n.dest).toBeTruthy();
      else expect(n.dest).toBeNull();
    }
  });

  it('零字节的行不产生流带 —— d3 会因此产出 NaN', () => {
    expect(toGraph([{ site: 'x', rule: 'y', outbound: 'z', bytes: 0 }])).toBeNull();
  });

  it('空输入返回 null', () => {
    expect(toGraph([])).toBeNull();
    expect(toGraph(null)).toBeNull();
  });
});

describe('layout', () => {
  it('产出三列且全无 NaN', () => {
    const m = layout(toGraph(rows), 960, 452);
    const xs = [...new Set(m.nodes.map((n) => n.x0))].sort((a, b) => a - b);
    expect(xs).toHaveLength(3);
    for (const n of m.nodes) {
      expect(Number.isFinite(n.x0)).toBe(true);
      expect(Number.isFinite(n.y0)).toBe(true);
      expect(Number.isFinite(n.y1)).toBe(true);
    }
    for (const l of m.links) {
      expect(Number.isFinite(l.width)).toBe(true);
      expect(l.d).not.toMatch(/NaN/);
    }
  });

  it('不改写输入图 —— d3 会就地改写并塞进循环引用', () => {
    const g = toGraph(rows);
    const before = JSON.stringify(g);
    layout(g, 960, 452);
    expect(JSON.stringify(g)).toBe(before);
  });

  it('两次布局同一份图结果一致（幂等）', () => {
    const g = toGraph(rows);
    const a = layout(g, 960, 452);
    const b = layout(g, 960, 452);
    expect(a.nodes.map((n) => n.y0)).toEqual(b.nodes.map((n) => n.y0));
  });

  it('列内顺序严格等于输入数组顺序（节点顺序冻结的基础）', () => {
    const g = toGraph(rows);
    g.nodes.reverse();
    const m = layout(g, 960, 452);
    const got = m.nodes.filter((n) => n.layer === 0).sort((a, b) => a.y0 - b.y0).map((n) => n.id);
    const want = g.nodes.filter((n) => n.layer === 0).map((n) => n.id);
    expect(got).toEqual(want);
  });

  it('左右留出标签空间 —— 否则标签被 viewBox 裁掉', () => {
    const m = layout(toGraph(rows), 960, 452);
    const xs = m.nodes.map((n) => n.x0);
    expect(Math.min(...xs)).toBeGreaterThan(60);
    expect(Math.max(...xs)).toBeLessThan(960 - 60);
  });

  it('渐变端点覆盖第一列右缘到末列左缘', () => {
    const m = layout(toGraph(rows), 960, 452);
    const [x1, x2] = m.gradientX;
    expect(x1).toBeLessThan(x2);
    expect(x1).toBeCloseTo(Math.min(...m.nodes.map((n) => n.x1)), 5);
    expect(x2).toBeCloseTo(Math.max(...m.nodes.map((n) => n.x0)), 5);
  });
});

describe('gradientId', () => {
  it('中文出站名产出的 id 不含 % —— 含 % 的 id 在 url(#…) 里静默不上色', () => {
    const id = gradientId('日本节点');
    expect(id).not.toMatch(/%/);
    expect(id).toMatch(/^[A-Za-z][\w-]*$/);
  });
  it('不同名字产出不同 id', () => {
    expect(gradientId('日本节点')).not.toBe(gradientId('新加坡'));
  });
  it('同一名字稳定', () => {
    expect(gradientId('DIRECT')).toBe(gradientId('DIRECT'));
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd ui && npx vitest run sankey-layout`
Expected: `Failed to load .../sankey-layout.js`

- [ ] **Step 3: 写实现**

`ui/src/lib/sankey-layout.js`：

```js
/**
 * 桑基布局（spec §11.6）。
 *
 * **d3 只做布局计算，绝不碰 DOM** —— SVG 全部由 Svelte 的 {#each} 渲染。
 * 不使用 d3 的 enter/exit/update：那会与 Svelte 的响应式打架，两套东西
 * 争同一批 DOM 节点。
 *
 * 以下五条是实测踩出来的，改动前请先复现：
 *   1. sankey() 会**就地改写**输入对象，还会塞进循环引用 → 必须深拷贝
 *   2. nodeSort(null) + linkSort(null) 才能让列内顺序 = 输入数组顺序
 *   3. 零流带 / 全零字节时 d3 静默产出 NaN → 必须在调用前挡住
 *   4. extent 用满整幅 SVG 会让两侧标签被裁掉 → 留 gutter
 *   5. 渐变 id 含 % 时 url(#…) 静默不上色 → 不能用 encodeURIComponent
 */
import { sankey, sankeyLinkHorizontal } from 'd3-sankey';

export const NODE_W = 12;
export const NODE_PAD = 10;

/** 标签留白。数值取自 mockup：960 宽下左列在 x=130、右列在 x=730。 */
export const GUTTER_L = 130;
export const GUTTER_R = 218;

const linkPath = sankeyLinkHorizontal();

/**
 * SVG 渐变 id。出站名可能是中文，而 `encodeURIComponent` 产出的 `%xx`
 * 放进 `url(#…)` 会**静默失效**（元素完全不上色，且 getComputedStyle
 * 看起来还是对的，极难排查）。逐字符转 base36 既避开 %，也保证
 * 首字符是字母、结果稳定可逆推。
 */
export function gradientId(name) {
  return 'g' + [...String(name)].map((c) => c.codePointAt(0).toString(36)).join('-');
}

/**
 * 流水行 → 三层图。返回 null 表示数据不足以画图（调用方走空状态）。
 *
 * 流带必须按 (source,target) 聚合：同一条规则会被多个站点命中，
 * 因此 rule→outbound 这条边在原始行里重复出现。不聚合的话
 * d3 会画出两条叠在一起的带子，Svelte 的 keyed each 更会直接抛
 * each_key_duplicate 让整个组件渲染不出来（实测）。
 */
export function toGraph(rows) {
  if (!rows?.length) return null;

  const seen = new Set();
  const nodes = [];
  const agg = new Map();

  const addNode = (id, label, layer, dest) => {
    if (seen.has(id)) return;
    seen.add(id);
    nodes.push({ id, label, layer, dest });
  };
  const addEdge = (source, target, value, dest) => {
    const key = `${source}>${target}`;
    const cur = agg.get(key);
    if (cur) cur.value += value;
    else agg.set(key, { key, source, target, value, dest });
  };

  for (const r of rows) {
    // 零字节的流带会让 d3 的 ky 缩放系数变成 Infinity，全图 NaN
    if (!(r.bytes > 0)) continue;
    const s = `s:${r.site}`;
    const u = `r:${r.rule}`;
    const o = `o:${r.outbound}`;
    // 左层与中层节点保持中性灰，只有出站节点满色 → 只有它带 dest
    addNode(s, r.site, 0, null);
    addNode(u, r.rule, 1, null);
    addNode(o, r.outbound, 2, r.outbound);
    addEdge(s, u, r.bytes, r.outbound);
    addEdge(u, o, r.bytes, r.outbound);
  }

  const links = [...agg.values()];
  return links.length ? { nodes, links } : null;
}

/**
 * 计算布局。`graph.nodes` 的数组顺序**就是**最终的列内顺序 ——
 * 调用方通过重排该数组来实现「节点顺序一旦确定即冻结」。
 */
export function layout(graph, width, height) {
  const gen = sankey()
    .nodeId((d) => d.id)
    // 层号由我们显式给定，不让 d3 从图结构推导 —— 推导出的 depth
    // 在出现跨层边时会漂移
    .nodeAlign((d) => d.layer)
    .nodeSort(null)   // ← 冻结列内顺序 = 输入数组顺序
    .linkSort(null)
    .nodeWidth(NODE_W)
    .nodePadding(NODE_PAD)
    .extent([[GUTTER_L, 12], [width - GUTTER_R + NODE_W, height - 12]]);

  // structuredClone：d3 会往节点上塞 sourceLinks/targetLinks（循环引用），
  // 并把 link.source 从字符串替换成节点对象。复用同一份对象跑第二次
  // 布局会读到脏数据。
  const out = gen(structuredClone(graph));

  return {
    nodes: out.nodes,
    links: out.links.map((l) => ({
      key: l.key,
      d: linkPath(l),
      width: l.width,
      value: l.value,
      dest: l.dest,
      sourceId: l.source.id,
      targetId: l.target.id,
    })),
    // 全局横向渐变的端点：第一列右缘 → 末列左缘。
    // 用 userSpaceOnUse 而非默认的 objectBoundingBox，才能让**整条路径
    // 共享同一个渐变**，视觉语义即「未分类的流量被逐层筛清」。
    gradientX: [
      Math.min(...out.nodes.map((n) => n.x1)),
      Math.max(...out.nodes.map((n) => n.x0)),
    ],
  };
}

/** 按冻结的顺序重排节点数组；新节点追加到末尾（顺序因此只增不改）。 */
export function applyFrozenOrder(graph, frozenOrder) {
  if (!frozenOrder?.length) return graph;
  const rank = new Map(frozenOrder.map((id, i) => [id, i]));
  return {
    nodes: [...graph.nodes].sort(
      (a, b) => (rank.get(a.id) ?? Number.MAX_SAFE_INTEGER) - (rank.get(b.id) ?? Number.MAX_SAFE_INTEGER)
    ),
    links: graph.links,
  };
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd ui && npx vitest run sankey-layout`
Expected: 13 个测试全部 PASS。

**特别确认这三个**：
- `把重复的 rule→outbound 边聚合成一条` —— 不过这条，组件会整个渲染不出来
- `列内顺序严格等于输入数组顺序` —— 这是「节点顺序冻结」的地基，塌了整个实时更新就没法看
- `中文出站名产出的 id 不含 %` —— 不过这条，中文出站的流带会**静默透明**

- [ ] **Step 5: 提交**

```bash
git add ui/src/lib/sankey-layout.js ui/src/lib/sankey-layout.test.js
git commit -m "feat(ui): 桑基布局封装（d3 只算数，不碰 DOM）"
```

---

### Task 6: hover 路径闭包

**Files:**
- Create: `ui/src/lib/trace.js`
- Create: `ui/src/lib/trace.test.js`

§11.6：「hover 时**把噪音调暗**而非把目标点亮：目标路径保持原样，其余降至 16% 不透明度。」

关键在于「目标路径」的定义。**只判断流带是否与 hover 节点相邻是错的** —— 实测 hover 站点节点时，`规则→出站` 那一跳会被误暗，视觉上路径断成半截。正确做法是从 hover 节点做**双向可达闭包**。

- [ ] **Step 1: 写失败的测试**

`ui/src/lib/trace.test.js`：

```js
import { describe, it, expect } from 'vitest';
import { tracePath } from './trace.js';

//  s:yt ──A──▶ r:final ──B──▶ o:jp
//  s:ot ──E──▶ r:final
//  s:tb ──C──▶ r:cn    ──D──▶ o:direct
const links = [
  { key: 'A', sourceId: 's:yt', targetId: 'r:final' },
  { key: 'B', sourceId: 'r:final', targetId: 'o:jp' },
  { key: 'C', sourceId: 's:tb', targetId: 'r:cn' },
  { key: 'D', sourceId: 'r:cn', targetId: 'o:direct' },
  { key: 'E', sourceId: 's:ot', targetId: 'r:final' },
];

describe('路径闭包', () => {
  it('hover 站点点亮两跳直到出站（不是只点亮第一跳）', () => {
    expect([...tracePath(links, 's:yt')].sort()).toEqual(['A', 'B']);
  });

  it('hover 规则同时点亮上下游', () => {
    expect([...tracePath(links, 'r:final')].sort()).toEqual(['A', 'B', 'E']);
  });

  it('hover 出站回溯到全部来源站点', () => {
    expect([...tracePath(links, 'o:jp')].sort()).toEqual(['A', 'B', 'E']);
  });

  it('不串到无关分支', () => {
    const hot = tracePath(links, 's:yt');
    expect(hot.has('C')).toBe(false);
    expect(hot.has('D')).toBe(false);
  });

  it('无 hover 时返回 null（表示「全部原样」，不是「全部变暗」）', () => {
    expect(tracePath(links, null)).toBeNull();
    expect(tracePath(links, undefined)).toBeNull();
  });

  it('孤立节点返回空集合而非报错', () => {
    expect(tracePath(links, 'o:nobody').size).toBe(0);
  });

  it('图中存在环时不死循环', () => {
    const cyc = [
      { key: 'X', sourceId: 'a', targetId: 'b' },
      { key: 'Y', sourceId: 'b', targetId: 'a' },
    ];
    expect([...tracePath(cyc, 'a')].sort()).toEqual(['X', 'Y']);
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd ui && npx vitest run trace`
Expected: `Failed to load .../trace.js`

- [ ] **Step 3: 写实现**

`ui/src/lib/trace.js`：

```js
/**
 * hover 高亮的路径闭包（spec §11.6）。
 *
 * 「其余降至 16%」里的「其余」必须按**双向可达闭包**算，不能只看相邻：
 * hover 站点节点时，只比较 sourceId/targetId 相邻会漏掉「规则→出站」
 * 那一跳，路径视觉上断成半截（实测 12 条流带里 11 条 faded，其中一条是错的）。
 *
 * 返回 null 表示没有 hover —— 语义是「全部原样」，与「空集合」
 * （hover 到一个孤立节点，全部变暗）截然不同，调用方必须区分。
 */
export function tracePath(links, nodeId) {
  if (!nodeId) return null;

  const fwd = new Map();
  const bwd = new Map();
  for (const l of links) {
    if (!fwd.has(l.sourceId)) fwd.set(l.sourceId, []);
    if (!bwd.has(l.targetId)) bwd.set(l.targetId, []);
    fwd.get(l.sourceId).push(l);
    bwd.get(l.targetId).push(l);
  }

  const hot = new Set();
  // 用显式栈而非递归：真实数据下深度只有 2，但环形输入会让递归爆栈，
  // 而 hot 去重同时也是环的终止条件
  const walk = (start, adj, step) => {
    const stack = [start];
    while (stack.length) {
      const id = stack.pop();
      for (const l of adj.get(id) ?? []) {
        if (hot.has(l.key)) continue;
        hot.add(l.key);
        stack.push(step(l));
      }
    }
  };
  walk(nodeId, fwd, (l) => l.targetId);
  walk(nodeId, bwd, (l) => l.sourceId);
  return hot;
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd ui && npx vitest run trace`
Expected: 7 个测试全部 PASS。**`hover 站点点亮两跳` 是这组的核心** —— 它锁住的正是实测踩到的那个 bug。

- [ ] **Step 5: 提交**

```bash
git add ui/src/lib/trace.js ui/src/lib/trace.test.js
git commit -m "feat(ui): hover 路径的双向可达闭包"
```

---

### Task 7: 桑基图组件

**Files:**
- Create: `ui/src/views/Sankey.svelte`

现在把三个纯函数模块拼成图。**Svelte 5 runes 语法**，注意 `$state` / `$derived` / `$props` / `$effect`。

**插值策略（这是本 task 唯一的设计决定，其余都是照抄 mockup）：**

不要用 CSS 插值 `stroke-width`。节点高度（`rect` 的 y/height）、流带宽度、贝塞尔路径（`d`）三者必须**同步**变化，而 CSS 只能动 `stroke-width`，另外两个会瞬跳，形状被撕开。

正确做法：**插值「字节数」本身，每帧重算完整布局**。实测单次布局 0.065 ms（占 60fps 预算 0.4%），完全负担得起；且实测 11 个插值步全程列内顺序不变。

- [ ] **Step 1: 写组件**

`ui/src/views/Sankey.svelte`：

```svelte
<script>
  /**
   * 三层桑基图（spec §11.6）。
   *
   * 着色：流带按**最终去向**着色，非按来源。整条路径共享一个
   * userSpaceOnUse 全局横向渐变，左端 opacity ≈.08 → 右端 ≈.52。
   * 视觉语义即「未分类的流量被逐层筛清、各归其类」—— 正是 sieve 这个名字。
   * 左层与中层节点保持中性灰，**只有出站节点满色**。
   *
   * 交互：hover 时把噪音调暗（其余降至 16%）而非把目标点亮。
   * 这比给高亮项加光晕更克制，也更符合 Operate 模式。
   */
  import { Tween, prefersReducedMotion } from 'svelte/motion';
  import { cubicOut } from 'svelte/easing';
  import { toGraph, layout, applyFrozenOrder, gradientId } from '../lib/sankey-layout.js';
  import { tracePath } from '../lib/trace.js';
  import { bytes, count, pct } from '../lib/format.js';

  let {
    /** 已经过 prepareFlows 折叠的流水行 */
    rows = [],
    /** (name) => 色值。出站色码由上层统一分配，保证全局一致 */
    colorOf,
    /** 点击出站节点 → 跳到规则视图并筛出该出站的规则（spec §11.6） */
    onPickOutbound = () => {},
    width = 960,
    height = 452,
  } = $props();

  /** 节点顺序一旦确定即冻结，只有出现新节点时才追加 */
  let frozen = $state([]);
  let hovered = $state(null);
  let locked = $state(null);

  const target = $derived(toGraph(rows));
  const values = $derived(target ? target.links.map((l) => l.value) : []);

  // 插值「字节数」，每帧重算布局 —— 见本 task 开头的说明
  const tween = new Tween([], { duration: 200, easing: cubicOut });
  let prevShape = '';

  $effect(() => {
    const shape = target ? target.links.map((l) => l.key).join('|') : '';
    // 流带集合本身变了（新站点/新规则出现）时直接跳变：
    // 插值两个长度不同的向量没有意义。reduced-motion 同样直接跳变。
    const instant = shape !== prevShape || prefersReducedMotion.current;
    prevShape = shape;
    tween.set(values, instant ? { duration: 0 } : undefined);
  });

  const model = $derived.by(() => {
    if (!target) return null;
    const v = tween.current;
    const usable = v.length === target.links.length;
    const g = applyFrozenOrder(
      {
        nodes: target.nodes,
        links: target.links.map((l, i) => ({ ...l, value: usable ? v[i] : l.value })),
      },
      frozen
    );
    return layout(g, width, height);
  });

  $effect(() => {
    if (!model) return;
    // 只增不改：已有节点保持原位次，新节点追加到末尾
    const known = new Set(frozen);
    const added = model.nodes.map((n) => n.id).filter((id) => !known.has(id));
    if (added.length) frozen = [...frozen, ...added];
  });

  const focus = $derived(locked ?? hovered);
  const hot = $derived(model ? tracePath(model.links, focus) : null);

  /** null = 无 focus，全部原样；否则不在闭包里的降至 16% */
  const dim = (key) => hot !== null && !hot.has(key);

  const dests = $derived(model ? [...new Set(model.links.map((l) => l.dest))] : []);
  const total = $derived(rows.reduce((s, r) => s + r.bytes, 0));

  const nodeTotal = (n) => n.value ?? 0;

  function activate(n) {
    if (n.layer === 2) onPickOutbound(n.dest);
    else locked = locked === n.id ? null : n.id;
  }

  function onKey(e, n) {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      activate(n);
    } else if (e.key === 'Escape') {
      locked = null;
    }
  }
</script>

{#if model}
  <svg viewBox="0 0 {width} {height}" role="img"
       aria-label="流量走向桑基图：{rows.length} 条流，共 {bytes(total)}。完整数据请切换到表视图。">
    <defs>
      {#each dests as d (d)}
        <!-- userSpaceOnUse：整条路径共享同一个渐变，而非每段各自从头开始。
             这正是「逐层筛清」这个视觉语义的实现方式。 -->
        <linearGradient id={gradientId(d)} gradientUnits="userSpaceOnUse"
                        x1={model.gradientX[0]} x2={model.gradientX[1]}>
          <stop offset="0" stop-color={colorOf(d)} stop-opacity=".08" />
          <stop offset="1" stop-color={colorOf(d)} stop-opacity=".52" />
        </linearGradient>
      {/each}
    </defs>

    <g class="links">
      {#each model.links as l (l.key)}
        <path d={l.d} fill="none"
              stroke="url(#{gradientId(l.dest)})"
              stroke-width={Math.max(1, l.width)}
              class:faded={dim(l.key)} />
      {/each}
    </g>

    {#each model.nodes as n (n.id)}
      {@const h = Math.max(1, n.y1 - n.y0)}
      <rect class="node" x={n.x0} y={n.y0} width={n.x1 - n.x0} height={h}
            fill={n.layer === 2 ? colorOf(n.dest) : 'rgba(255,255,255,.17)'}
            opacity={n.layer === 2 ? 0.78 : 0.55}
            tabindex="0"
            role="button"
            aria-label={n.layer === 2
              ? `出站 ${n.label}，${bytes(nodeTotal(n))}，占 ${pct(nodeTotal(n) / total)}。按回车筛出相关规则。`
              : `${n.layer === 0 ? '站点' : '规则'} ${n.label}，${bytes(nodeTotal(n))}`}
            onmouseenter={() => (hovered = n.id)}
            onmouseleave={() => (hovered = null)}
            onfocus={() => (hovered = n.id)}
            onblur={() => (hovered = null)}
            onclick={() => activate(n)}
            onkeydown={(e) => onKey(e, n)} />

      <!-- 标签低于阈值时隐藏，hover/focus 才出 —— 避免细流带的标签糊成一片 -->
      {#if h >= 12 || focus === n.id}
        <text class={n.layer === 2 ? 'lbl-out' : 'lbl-m'}
              class:faded-t={hot !== null && focus !== n.id}
              x={n.layer === 0 ? n.x0 - 8 : n.x1 + 8}
              y={(n.y0 + n.y1) / 2 - 1}
              text-anchor={n.layer === 0 ? 'end' : 'start'}>{n.label}</text>
        {#if h >= 26 || focus === n.id}
          <text class="lbl-v" class:faded-t={hot !== null && focus !== n.id}
                x={n.layer === 0 ? n.x0 - 8 : n.x1 + 8}
                y={(n.y0 + n.y1) / 2 + 13}
                text-anchor={n.layer === 0 ? 'end' : 'start'}>
            {bytes(nodeTotal(n))}{n.layer === 2 ? ` · ${pct(nodeTotal(n) / total)}` : ''}
          </text>
        {/if}
      {/if}
    {/each}
  </svg>
{/if}

<style>
  svg { display: block; width: 100%; height: auto; background: var(--surface-1); }

  path {
    /* screen 混合让重叠的流带自然叠加而非互相遮挡 */
    mix-blend-mode: screen;
    transition: opacity 120ms ease-out;
  }
  /* hover 时把噪音调暗，而非把目标点亮 */
  .faded { opacity: .16; }
  .faded-t { opacity: .34; }

  .node { rx: 1.5; cursor: pointer; }
  .node:focus-visible { outline: 2px solid var(--text-1); outline-offset: 2px; }

  /* paint-order: stroke + 画布色描边 —— 压在任何流带上均可读 */
  .lbl-m, .lbl-out, .lbl-v {
    paint-order: stroke;
    stroke: var(--surface-1);
    stroke-linejoin: round;
    pointer-events: none;
  }
  .lbl-m   { font-family: var(--font-mono); font-size: 11px;   fill: var(--text-2); stroke-width: 3.5px; }
  .lbl-out { font-size: var(--fs-12); font-weight: 500; fill: var(--text-1); stroke-width: 3.5px; }
  .lbl-v   { font-family: var(--font-mono); font-size: 10.5px; fill: var(--text-4);
             font-variant-numeric: tabular-nums; stroke-width: 3px; }

  @media (prefers-reduced-motion: reduce) {
    path { transition: none; }
  }
</style>
```

- [ ] **Step 2: 确认能编译**

Run: `cd ui && npx vite build`
Expected: 构建成功。若报 `each_key_duplicate` 相关的编译期警告，回头检查 Task 5 的聚合逻辑。

- [ ] **Step 3: 在真实浏览器里看一眼**

这一步**不要跳过**。桑基图是本阶段唯一没法靠单测验收的东西 —— 几何对不对、标签有没有被裁、颜色语义成不成立，只有眼睛能判断。

```bash
cd ui && npm run dev
```

用一份写死的样例数据（就用 mockup 里那 7 条）渲染，逐条对照
`.superpowers/brainstorm/53735-1787573911/sankey.html`：

| 检查项 | 期望 |
|---|---|
| 三列位置 | 960 宽下约在 x=130 / 430 / 730 |
| 左右标签 | 完整可见，没被裁掉 |
| 节点满色 | **只有**右列出站节点是彩色，左中两列是中性灰 |
| 流带渐变 | 左端几乎无色，越往右越饱和 |
| 中文出站的流带 | **有颜色**（若透明，是渐变 id 出了问题，回看 Task 5） |
| hover 一个站点 | 该路径**两跳都亮**，其余降到 16% |
| 标签可读性 | 压在流带上仍清晰（描边生效） |

- [ ] **Step 4: 验证顺序冻结与空状态**

临时在页面上加两个按钮：一个随机改所有 `bytes`、一个清空数据。

| 操作 | 期望 |
|---|---|
| 连点几次「改数据」 | 流带宽度平滑变化，**节点上下顺序始终不变** |
| 「清空」 | 图整个消失（组件返回空），**不残留空的坐标骨架** |
| 系统开启「减弱动态效果」后改数据 | 直接跳变，无过渡 |

macOS 下开减弱动态效果：系统设置 → 辅助功能 → 显示 → 减弱动态效果。

- [ ] **Step 5: 提交**

```bash
git add ui/src/views/Sankey.svelte
git commit -m "feat(ui): 三层桑基图（去向着色、顺序冻结、hover 降噪）"
```

---

### Task 8: 表视图 —— 必需的等价视图

**Files:**
- Create: `ui/src/views/FlowTable.svelte`
- Create: `ui/src/views/FlowTable.test.js`

> **这是本阶段无障碍要求的落点，不是可选装饰。**
>
> spec §11.6 记录了桑基图的无障碍评级为 **C** —— 结构性流图无法只靠颜色传达。
> 因此表视图必须：**可排序、可键盘遍历、屏幕阅读器友好**，并且与图**共享同一份数据**。
>
> 「默认不显示日志」的准确落地是：日志从独立标签**降级为流量视图内部的表形态**，
> 图/表一键切换。不是删掉。

- [ ] **Step 1: 写失败的测试**

`ui/src/views/FlowTable.test.js`：

```js
import { describe, it, expect } from 'vitest';
import { render, screen, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import FlowTable from './FlowTable.svelte';

const rows = [
  { site: 'a.com', rule: 'final *', outbound: 'JP', bytes: 10, conns: 3 },
  { site: 'b.com', rule: 'geosite cn', outbound: 'DIRECT', bytes: 90, conns: 1 },
  { site: 'c.com', rule: 'keyword x', outbound: 'SG', bytes: 50, conns: 7 },
];
const colorOf = () => '#5b8ff9';

const body = () => within(screen.getAllByRole('rowgroup')[1]);
const firstCol = () =>
  body().getAllByRole('row').map((r) => within(r).getAllByRole('cell')[0].textContent.trim());

describe('表视图 —— 桑基图的无障碍等价视图', () => {
  it('是真正的 table，有 caption 供屏幕阅读器定位', () => {
    render(FlowTable, { rows, colorOf });
    expect(screen.getByRole('table')).toHaveAccessibleName(/流量/);
  });

  it('五个列头都用 th + scope=col', () => {
    render(FlowTable, { rows, colorOf });
    const ths = screen.getAllByRole('columnheader');
    expect(ths).toHaveLength(5);
    for (const th of ths) expect(th).toHaveAttribute('scope', 'col');
  });

  it('每个列头都暴露 aria-sort，当前排序列不是 none', () => {
    render(FlowTable, { rows, colorOf });
    const ths = screen.getAllByRole('columnheader');
    for (const th of ths) expect(th).toHaveAttribute('aria-sort');
    expect(ths.filter((th) => th.getAttribute('aria-sort') !== 'none')).toHaveLength(1);
  });

  it('默认按字节降序 —— 打开就看到最大的流量', () => {
    render(FlowTable, { rows, colorOf });
    expect(firstCol()).toEqual(['b.com', 'c.com', 'a.com']);
  });

  it('点列头切换排序，再点一次反向', async () => {
    const u = userEvent.setup();
    render(FlowTable, { rows, colorOf });
    await u.click(screen.getByRole('button', { name: /字节/ }));
    expect(firstCol()).toEqual(['a.com', 'c.com', 'b.com']);
    await u.click(screen.getByRole('button', { name: /字节/ }));
    expect(firstCol()).toEqual(['b.com', 'c.com', 'a.com']);
  });

  it('文本列按字典序排', async () => {
    const u = userEvent.setup();
    render(FlowTable, { rows, colorOf });
    await u.click(screen.getByRole('button', { name: /目标站点/ }));
    expect(firstCol()).toEqual(['a.com', 'b.com', 'c.com']);
  });

  it('列头可用键盘 Tab 到达并 Enter 触发', async () => {
    const u = userEvent.setup();
    render(FlowTable, { rows, colorOf });
    await u.tab();
    expect(screen.getByRole('button', { name: /目标站点/ })).toHaveFocus();
    await u.keyboard('{Enter}');
    expect(screen.getAllByRole('columnheader')[0].getAttribute('aria-sort')).not.toBe('none');
  });

  it('出站不只靠色块传达 —— 必须有文字', () => {
    render(FlowTable, { rows, colorOf });
    expect(screen.getByText('DIRECT')).toBeInTheDocument();
    expect(screen.getByText('JP')).toBeInTheDocument();
  });

  it('数值列用 tabular-nums 类，能纵向对齐扫读', () => {
    render(FlowTable, { rows, colorOf });
    const cells = within(body().getAllByRole('row')[0]).getAllByRole('cell');
    expect(cells[3].className).toMatch(/\bn\b/);
    expect(cells[4].className).toMatch(/\bn\b/);
  });

  it('空数据时不渲染空表骨架', () => {
    render(FlowTable, { rows: [], colorOf });
    expect(screen.queryByRole('table')).toBeNull();
  });

  it('排序后行数不变 —— 排序不能吞行', async () => {
    const u = userEvent.setup();
    render(FlowTable, { rows, colorOf });
    await u.click(screen.getByRole('button', { name: /连接/ }));
    expect(body().getAllByRole('row')).toHaveLength(3);
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd ui && npx vitest run FlowTable`
Expected: `Failed to resolve import ./FlowTable.svelte`

- [ ] **Step 3: 写实现**

`ui/src/views/FlowTable.svelte`：

```svelte
<script>
  /**
   * 流量表视图（spec §11.6）。
   *
   * **这是桑基图的必需等价视图，不是可选装饰。** 桑基图的无障碍评级为 C ——
   * 结构性流图无法只靠颜色传达。因此这张表必须可排序、可键盘遍历、
   * 屏幕阅读器友好，并与图共享同一份数据。
   *
   * 无障碍要点：
   *   - 用真正的 <table> + <caption> + <th scope="col">，不用 div 模拟
   *   - 每个列头带 aria-sort，排序状态对屏幕阅读器可见
   *   - 排序触发器是 <button>，天然可 Tab 可 Enter
   *   - 出站列**同时**给色块与文字 —— 颜色绝不是唯一的信息载体
   */
  import { bytes, count } from '../lib/format.js';

  let { rows = [], colorOf } = $props();

  const COLS = [
    { key: 'site',     label: '目标站点', num: false },
    { key: 'rule',     label: '命中规则', num: false },
    { key: 'outbound', label: '出站',     num: false },
    { key: 'bytes',    label: '字节',     num: true  },
    { key: 'conns',    label: '连接',     num: true  },
  ];

  let sortKey = $state('bytes');
  let sortDir = $state('desc');

  const sorted = $derived(
    [...rows].sort((a, b) => {
      const x = a[sortKey];
      const y = b[sortKey];
      const c = typeof x === 'number' && typeof y === 'number'
        ? x - y
        : String(x).localeCompare(String(y), 'zh-Hans-CN');
      return sortDir === 'asc' ? c : -c;
    })
  );

  function sortBy(col) {
    if (sortKey === col.key) {
      sortDir = sortDir === 'asc' ? 'desc' : 'asc';
    } else {
      sortKey = col.key;
      // 数值列默认降序（先看大的），文本列默认升序（字典序）
      sortDir = col.num ? 'desc' : 'asc';
    }
  }

  const ariaSort = (k) =>
    sortKey === k ? (sortDir === 'asc' ? 'ascending' : 'descending') : 'none';

  const total = $derived(rows.reduce((s, r) => s + r.bytes, 0));
</script>

{#if rows.length}
  <table>
    <caption class="sr-only">
      流量明细：共 {rows.length} 条，{bytes(total)}。
      列为目标站点、命中规则、出站、字节、连接数，点击列头可排序。
    </caption>
    <thead>
      <tr>
        {#each COLS as col (col.key)}
          <th scope="col" aria-sort={ariaSort(col.key)} class:n={col.num}>
            <button type="button" onclick={() => sortBy(col)}>
              {col.label}<span class="arrow" aria-hidden="true"
                >{sortKey === col.key ? (sortDir === 'asc' ? '↑' : '↓') : ''}</span>
            </button>
          </th>
        {/each}
      </tr>
    </thead>
    <tbody>
      {#each sorted as r (`${r.site}|${r.rule}|${r.outbound}`)}
        <tr class:agg={r.aggregated}>
          <td class="mono">{r.site}</td>
          <td class="mono">{r.rule}</td>
          <td>
            <!-- 色块是辅助，文字才是信息 —— 颜色绝不是唯一载体 -->
            <span class="chip" style:background={colorOf(r.outbound)} aria-hidden="true"></span>{r.outbound}
          </td>
          <td class="n mono">{bytes(r.bytes)}</td>
          <td class="n mono">{count(r.conns)}</td>
        </tr>
      {/each}
    </tbody>
  </table>
{/if}

<style>
  table { width: 100%; border-collapse: collapse; font-size: 12.5px; }

  th {
    text-align: left;
    font-size: 10px;
    letter-spacing: .08em;
    text-transform: uppercase;
    color: var(--text-4);
    font-weight: 600;
    background: var(--surface-0);
    border-bottom: 1px solid var(--border);
    padding: 0;
  }
  th.n { text-align: right; }

  th button {
    all: unset;
    display: block;
    width: 100%;
    padding: 8px 16px;
    cursor: pointer;
    box-sizing: border-box;
    font: inherit;
    color: inherit;
    letter-spacing: inherit;
    text-transform: inherit;
    text-align: inherit;
  }
  th button:hover { color: var(--text-2); }

  .arrow { display: inline-block; width: 1em; color: var(--text-2); }

  td {
    padding: 7px 16px;
    border-bottom: 1px solid rgba(255, 255, 255, .035);
    color: var(--text-2);
  }
  tr:last-child td { border-bottom: none; }

  td.n {
    text-align: right;
    font-variant-numeric: tabular-nums;
  }

  /* 聚合行用最暗的中性色 —— 它不是一个真实的站点 */
  .agg td:first-child, .agg td:nth-child(2) { color: var(--text-4); }

  .chip {
    display: inline-block;
    width: 6px; height: 6px;
    border-radius: 2px;
    margin-right: 7px;
    vertical-align: middle;
  }
</style>
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd ui && npx vitest run FlowTable`
Expected: 11 个测试全部 PASS。

**这一组是无障碍死线，逐条确认，不要放宽断言。** 尤其：
- `是真正的 table，有 caption` —— div 模拟的表格对屏幕阅读器等于不存在
- `列头可用键盘 Tab 到达并 Enter 触发` —— 只能鼠标点的排序，对键盘用户等于没有
- `出站不只靠色块传达` —— 这条直接对应「无法只靠颜色传达」

- [ ] **Step 5: 提交**

```bash
git add ui/src/views/FlowTable.svelte ui/src/views/FlowTable.test.js
git commit -m "feat(ui): 流量表视图（桑基图的必需无障碍等价视图）"
```

---

### Task 9: 空状态与流量视图外壳

**Files:**
- Create: `ui/src/views/EmptyState.svelte`
- Create: `ui/src/lib/Segmented.svelte`
- Create: `ui/src/views/TrafficView.svelte`

把图、表、时间窗口、图表切换、空状态拼成默认视图。

§11.6 的两条空状态规定：
- 无流量时**不画空的坐标骨架**，显示引导文案
- 流数少于 3 时桑基图本不适用，**自动降级为流量列表**（即表视图）

- [ ] **Step 1: 写 Segmented control**

§11.4 明确拒绝左侧图标导航，改用 segmented control。这个组件在流量视图（时间窗口）与顶层导航都要用，所以先做成可复用的。

`ui/src/lib/Segmented.svelte`：

```svelte
<script>
  /**
   * Segmented control（spec §11.4）。
   *
   * 用它而非左侧图标导航栏：后者是同类产品的通用套路，且挤占宽度。
   *
   * 无障碍：用 role="radiogroup" 而非一堆 button —— 语义上这是
   * 「在若干选项里选一个」，不是「若干个独立动作」。屏幕阅读器会
   * 播报「N 之 M」，键盘可用左右方向键切换。
   */
  let { options = [], value, onchange = () => {}, label = '' } = $props();

  function onKey(e) {
    const i = options.findIndex((o) => o.value === value);
    let next = null;
    if (e.key === 'ArrowRight' || e.key === 'ArrowDown') next = (i + 1) % options.length;
    if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') next = (i - 1 + options.length) % options.length;
    if (next === null) return;
    e.preventDefault();
    onchange(options[next].value);
  }
</script>

<div class="seg" role="radiogroup" aria-label={label} onkeydown={onKey}>
  {#each options as o (o.value)}
    <button type="button" role="radio"
            aria-checked={o.value === value}
            class:on={o.value === value}
            tabindex={o.value === value ? 0 : -1}
            onclick={() => onchange(o.value)}>{o.label}</button>
  {/each}
</div>

<style>
  .seg {
    display: flex;
    background: var(--surface-0);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius);
    padding: 2px;
    gap: 2px;
  }
  .seg button {
    all: unset;
    font-size: var(--fs-12);
    padding: 3px 10px;
    border-radius: 3px;
    color: var(--text-3);
    cursor: pointer;
  }
  .seg button:hover { color: var(--text-2); }
  .seg button.on {
    background: var(--surface-2);
    color: var(--text-1);
    font-weight: 500;
  }
  .seg button:focus-visible { outline: 2px solid var(--outbound-1); outline-offset: -1px; }
</style>
```

- [ ] **Step 2: 写空状态组件**

`ui/src/views/EmptyState.svelte`：

```svelte
<script>
  /**
   * 空状态（spec §11.3 / §11.6）。
   *
   * 两条纪律：
   *   - 无流量时**不画空的坐标骨架**。空骨架传达的是「这里本该有东西
   *     但坏了」，而真相是「还没开始」
   *   - 无任何出站时让位给「添加第一个服务器」引导
   *
   * 反 AI 塑料感：不用插画、不用大图标、不用居中的巨型标题撑场面。
   * 一行说明 + 一个动作，与整体的仪器气质一致。
   */
  let { title = '', hint = '', action = null, onaction = () => {} } = $props();
</script>

<div class="empty">
  <p class="t">{title}</p>
  {#if hint}<p class="h">{hint}</p>{/if}
  {#if action}
    <button type="button" onclick={onaction}>{action}</button>
  {/if}
</div>

<style>
  .empty {
    padding: 56px 24px;
    text-align: center;
  }
  .t { font-size: var(--fs-13); color: var(--text-2); margin: 0; }
  .h {
    font-size: var(--fs-12);
    color: var(--text-4);
    margin: 6px 0 0;
    line-height: 1.7;
  }
  button {
    margin-top: 16px;
    background: var(--surface-2);
    color: var(--text-1);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius);
    padding: 6px 14px;
    font-size: var(--fs-12);
    font-family: inherit;
    cursor: pointer;
  }
  button:hover { border-color: rgba(255, 255, 255, .22); }
</style>
```

- [ ] **Step 3: 写流量视图外壳**

`ui/src/views/TrafficView.svelte`：

```svelte
<script>
  /**
   * 流量视图 —— 默认视图，打开即见走向（spec §11.3）。
   *
   * 内部有「图 ⇄ 表」切换、时间窗口（5m / 1h / 本次运行）、
   * 字节/连接数切换。图与表**共享同一份数据**。
   */
  import Segmented from '../lib/Segmented.svelte';
  import Sankey from './Sankey.svelte';
  import FlowTable from './FlowTable.svelte';
  import EmptyState from './EmptyState.svelte';
  import { prepareFlows } from '../lib/aggregate.js';
  import { bytes } from '../lib/format.js';

  let {
    /** 原始流水行：{ site, rule, outbound, bytes, conns } */
    flows = [],
    colorOf,
    window: win = '1h',
    onWindowChange = () => {},
    onPickOutbound = () => {},
  } = $props();

  let shape = $state('chart');   // chart | table

  const rows = $derived(prepareFlows(flows));
  const total = $derived(rows.reduce((s, r) => s + r.bytes, 0));

  // 流数少于 3 时桑基图本不适用，自动降级为表（spec §11.6）
  const degraded = $derived(rows.length > 0 && rows.length < 3);
  const showTable = $derived(shape === 'table' || degraded);
</script>

<section class="view" aria-label="流量走向">
  <div class="bar">
    <Segmented label="时间窗口"
      options={[
        { value: '5m',  label: '5 分钟' },
        { value: '1h',  label: '1 小时' },
        { value: 'run', label: '本次运行' },
      ]}
      value={win} onchange={onWindowChange} />

    <div class="push">
      <Segmented label="显示形态"
        options={[{ value: 'chart', label: '图' }, { value: 'table', label: '表' }]}
        value={showTable ? 'table' : 'chart'}
        onchange={(v) => (shape = v)} />
      <span class="total mono" aria-live="polite">{bytes(total)}</span>
    </div>
  </div>

  {#if !rows.length}
    <EmptyState
      title="还没有流量经过。"
      hint="连接建立后，这里会显示「目标站点 → 命中规则 → 出站」的完整走向。" />
  {:else if showTable}
    {#if degraded}
      <p class="hint">流数少于 3 条，桑基图不适用，已切换为列表。</p>
    {/if}
    <FlowTable {rows} {colorOf} />
  {:else}
    <Sankey {rows} {colorOf} {onPickOutbound} />
  {/if}
</section>

<style>
  .view { background: var(--surface-1); }
  .bar {
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 11px 16px;
    border-bottom: 1px solid var(--border);
    background: var(--surface-0);
  }
  .push { margin-left: auto; display: flex; align-items: center; gap: 12px; }
  .total { font-size: var(--fs-12); color: var(--text-2); }
  .hint {
    margin: 0;
    padding: 8px 16px;
    font-size: var(--fs-12);
    color: var(--text-4);
    border-bottom: 1px solid var(--border);
  }
</style>
```

- [ ] **Step 4: 手工验收三种状态**

`npm run dev`，用不同的样例数据分别确认：

| 数据 | 期望 |
|---|---|
| `flows = []` | 引导文案，**没有空的坐标骨架**，没有空表头 |
| 2 条流 | 自动降级为表，并显示「流数少于 3 条」提示 |
| 7 条流（mockup 那份） | 桑基图，与 mockup 对得上 |
| 点「表」 | 同一份数据换成表格，总字节读数不变 |
| Tab 键走一遍 | segmented 用左右方向键切换，表头能 Tab 到 |

**「总字节读数在图/表之间不变」是个真断言**：若不同，说明 `prepareFlows` 的守恒被破坏了，回头看 Task 3。

- [ ] **Step 5: 提交**

```bash
git add ui/src/lib/Segmented.svelte \
        ui/src/views/EmptyState.svelte \
        ui/src/views/TrafficView.svelte
git commit -m "feat(ui): 流量视图外壳、segmented control 与空状态"
```

---

## Part C — 规则视图（两个 signature 所在）

### Task 10: 命中热度归一化

**Files:**
- Create: `ui/src/lib/heat.js`
- Create: `ui/src/lib/heat.test.js`

**signature ①（spec §11.5）：** 命中次数不靠读数字，靠**行背景的中性染色**（`rgba(255,255,255,.014 ~ .052)`，按命中数归一化）。最热的规则微亮，死规则几乎透明。滚过 200 条规则，哪几条在真正干活一眼可辨。

**这里唯一的实质决定是刻度的选择。** mockup 里的命中数是 `211 → 88,120`，跨三个数量级。线性归一化下 `211/88120 = 0.24%`，乘以 `.052` 得 `.000124` —— 肉眼完全不可见，除榜首外全部规则染色一律等于零，signature 就废了。**必须用对数刻度。**

- [ ] **Step 1: 写失败的测试**

`ui/src/lib/heat.test.js`：

```js
import { describe, it, expect } from 'vitest';
import { heat, HEAT_MAX } from './heat.js';

describe('命中热度归一化', () => {
  it('零命中是纯透明 —— 死规则必须看得出来是死的', () => {
    expect(heat(0, 88120)).toBe(0);
  });

  it('榜首拿满值', () => {
    expect(heat(88120, 88120)).toBeCloseTo(HEAT_MAX, 6);
  });

  it('单调递增', () => {
    const v = [1, 10, 100, 1000, 10000].map((h) => heat(h, 10000));
    for (let i = 1; i < v.length; i++) expect(v[i]).toBeGreaterThan(v[i - 1]);
  });

  it('对数刻度：跨三个数量级仍分得开', () => {
    // mockup 的真实数据：211 / 1,033 / 42,663 / 88,120
    const max = 88120;
    const a = heat(211, max);
    const b = heat(1033, max);
    const c = heat(42663, max);
    expect(a).toBeGreaterThan(b * 0.5);   // 不被压到零附近
    expect(b).toBeGreaterThan(a);
    expect(c).toBeGreaterThan(b);
    // 线性刻度下 211/88120 会得到 .00012，肉眼不可见。
    // 对数刻度必须显著高于它。
    expect(a).toBeGreaterThan((211 / max) * HEAT_MAX * 10);
  });

  it('最低的非零命中仍高于可见阈值', () => {
    // .014 是 mockup 里最暗的一档；低于它就等于没染色
    expect(heat(1, 88120)).toBeGreaterThanOrEqual(0);
    expect(heat(211, 88120)).toBeGreaterThan(0.008);
  });

  it('不超过上限 —— 过亮会盖过探针高亮', () => {
    expect(heat(999999, 100)).toBeLessThanOrEqual(HEAT_MAX);
  });

  it('全表只有一条规则时不除零', () => {
    expect(Number.isFinite(heat(1, 1))).toBe(true);
    expect(Number.isFinite(heat(5, 5))).toBe(true);
  });

  it('非法输入不产出 NaN —— NaN 进 CSS 会让整行没有背景', () => {
    expect(heat(NaN, 100)).toBe(0);
    expect(heat(10, NaN)).toBe(0);
    expect(heat(undefined, 100)).toBe(0);
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd ui && npx vitest run heat`
Expected: `Failed to load .../heat.js`

- [ ] **Step 3: 写实现**

`ui/src/lib/heat.js`：

```js
/**
 * 命中热度 → 行背景不透明度（signature ①，spec §11.5）。
 *
 * 用**中性色**而非出站色染色：出站色码在整个产品里只表达一件事（去哪），
 * 拿它来表达热度会让两种语义打架。
 *
 * 刻度必须是**对数**的。实测命中数跨三个数量级（mockup 里 211 → 88,120），
 * 线性归一化下除榜首外全部贴近 0，染色也就废了 ——
 * 而「哪几条规则是死的」正是这个 signature 唯一要回答的问题。
 */

/** 上限取自 mockup 的最亮一档；再高会盖过探针的命中高亮 */
export const HEAT_MAX = 0.052;

export function heat(hits, maxHits) {
  if (typeof hits !== 'number' || !Number.isFinite(hits) || hits <= 0) return 0;
  if (typeof maxHits !== 'number' || !Number.isFinite(maxHits) || maxHits <= 0) return 0;
  // 全表只有一条规则（或全部同值）时，它就是最热的
  if (maxHits <= 1) return HEAT_MAX;
  const t = Math.log(hits) / Math.log(maxHits);
  return Math.min(HEAT_MAX, Math.max(0, t) * HEAT_MAX);
}

/** 直接给出可用的 CSS 值，避免各处重复拼字符串 */
export function heatColor(hits, maxHits) {
  const a = heat(hits, maxHits);
  return a > 0 ? `rgba(255,255,255,${a.toFixed(4)})` : 'transparent';
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd ui && npx vitest run heat`
Expected: 8 个测试全部 PASS。**`对数刻度：跨三个数量级仍分得开` 是这组的核心** —— 它是这个 signature 成不成立的分界线。

- [ ] **Step 5: 提交**

```bash
git add ui/src/lib/heat.js ui/src/lib/heat.test.js
git commit -m "feat(ui): 命中热度对数归一化（signature ①）"
```

---

### Task 11: 规则排序（拖拽 + 键盘等价物）

**Files:**
- Create: `ui/src/lib/reorder.js`
- Create: `ui/src/lib/reorder.test.js`

规则顺序**就是**语义（首命中即返回），所以拖拽排序不是便利功能而是核心操作。

**HTML5 drag 事件在 jsdom 下不可靠**，所以把「把第 i 项移到第 j 位」抽成纯函数单测，组件里只留事件接线。

**拖拽必须有键盘等价物**（Alt+↑/↓），否则这个核心操作对键盘用户等于不存在 —— 这是无障碍硬要求，不是加分项。

- [ ] **Step 1: 写失败的测试**

`ui/src/lib/reorder.test.js`：

```js
import { describe, it, expect } from 'vitest';
import { move, keyboardMove } from './reorder.js';

const L = ['a', 'b', 'c', 'd'];

describe('move', () => {
  it('下移', () => expect(move(L, 0, 2)).toEqual(['b', 'c', 'a', 'd']));
  it('上移', () => expect(move(L, 3, 1)).toEqual(['a', 'd', 'b', 'c']));
  it('相邻交换', () => expect(move(L, 1, 2)).toEqual(['a', 'c', 'b', 'd']));
  it('原地不动返回原数组引用（省一次无谓渲染）', () => expect(move(L, 1, 1)).toBe(L));

  it('越界不抛错也不损坏数据', () => {
    expect(move(L, -1, 2)).toBe(L);
    expect(move(L, 0, 99)).toBe(L);
    expect(move(L, 99, 0)).toBe(L);
  });

  it('不改写输入数组', () => {
    const copy = [...L];
    move(L, 0, 3);
    expect(L).toEqual(copy);
  });

  it('元素不丢不重', () => {
    const out = move(L, 0, 3);
    expect(out).toHaveLength(L.length);
    expect([...out].sort()).toEqual([...L].sort());
  });
});

describe('keyboardMove —— 拖拽的键盘等价物', () => {
  it('Alt+↑ 上移一位并跟随焦点', () => {
    const r = keyboardMove(L, 2, 'ArrowUp');
    expect(r.list).toEqual(['a', 'c', 'b', 'd']);
    expect(r.index).toBe(1);
  });

  it('Alt+↓ 下移一位并跟随焦点', () => {
    const r = keyboardMove(L, 1, 'ArrowDown');
    expect(r.list).toEqual(['a', 'c', 'b', 'd']);
    expect(r.index).toBe(2);
  });

  it('首项再上移不越界', () => {
    const r = keyboardMove(L, 0, 'ArrowUp');
    expect(r.index).toBe(0);
    expect(r.list).toEqual(L);
  });

  it('末项再下移不越界', () => {
    const r = keyboardMove(L, 3, 'ArrowDown');
    expect(r.index).toBe(3);
    expect(r.list).toEqual(L);
  });

  it('其他按键原样返回', () => {
    const r = keyboardMove(L, 1, 'Enter');
    expect(r.list).toBe(L);
    expect(r.index).toBe(1);
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd ui && npx vitest run reorder`
Expected: `Failed to load .../reorder.js`

- [ ] **Step 3: 写实现**

`ui/src/lib/reorder.js`：

```js
/**
 * 规则排序的纯逻辑。
 *
 * 规则顺序**就是**语义（首命中即返回），所以排序是核心操作而非便利功能。
 *
 * 抽成纯函数是为了能单测：HTML5 drag 事件在 jsdom 下不可靠，
 * 但「把第 i 项移到第 j 位」是可穷举的。
 */

/** 把 from 位置的元素移到 to 位置。越界或原地不动时返回原数组引用。 */
export function move(list, from, to) {
  if (
    from === to ||
    !Number.isInteger(from) || !Number.isInteger(to) ||
    from < 0 || to < 0 ||
    from >= list.length || to >= list.length
  ) {
    return list;
  }
  const out = [...list];
  const [item] = out.splice(from, 1);
  out.splice(to, 0, item);
  return out;
}

/**
 * 键盘排序：Alt+↑ / Alt+↓。
 *
 * **拖拽必须有键盘等价物**，否则这个核心操作对键盘用户等于不存在。
 * 返回新的 index 让调用方把焦点跟到移动后的位置 —— 焦点丢失是
 * 键盘操作最常见的断裂点。
 */
export function keyboardMove(list, index, key) {
  if (key === 'ArrowUp') {
    return { list: move(list, index, index - 1), index: Math.max(0, index - 1) };
  }
  if (key === 'ArrowDown') {
    return { list: move(list, index, index + 1), index: Math.min(list.length - 1, index + 1) };
  }
  return { list, index };
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd ui && npx vitest run reorder`
Expected: 12 个测试全部 PASS

- [ ] **Step 5: 提交**

```bash
git add ui/src/lib/reorder.js ui/src/lib/reorder.test.js
git commit -m "feat(ui): 规则排序纯逻辑（含键盘等价物）"
```

---

### Task 12: 探针即搜索框

**Files:**
- Create: `ui/src/views/Probe.svelte`
- Create: `ui/src/views/Probe.test.js`

**signature ②（spec §11.5）：** 顶部输入框**不是过滤器，而是试算探针**。输入域名 → 命中行升亮、其余整体降噪 → 显示「前 N 条已试未命中」。把核心排查动作（「为什么这个域名没走代理」）压缩为一次输入。

后端复用路由层纯函数（§4.2 纪律①），保证试算结果与真实判决**永远一致**。

**两阶段求值在 UI 上的落地（§11.2）：** `rule_test(target, resolve)` 的 `resolve` 参数直接对应两阶段。UI 默认 `false`（快、不发 DNS）；若命中的是 IP 类规则，提示「需解析才能确定」并提供**一键重测**。

- [ ] **Step 1: 写失败的测试**

`ui/src/views/Probe.test.js`：

```js
import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import Probe from './Probe.svelte';

const colorOf = () => '#5b8ff9';

describe('探针即搜索框（signature ②）', () => {
  it('输入框有可访问的名字，且不假装成搜索框', async () => {
    render(Probe, { result: null, colorOf, ontest: vi.fn() });
    const input = screen.getByRole('textbox');
    expect(input).toHaveAccessibleName(/试算|域名/);
  });

  it('输入后触发试算，默认不解析 DNS', async () => {
    const ontest = vi.fn();
    const u = userEvent.setup();
    render(Probe, { result: null, colorOf, ontest, debounce: 0 });
    await u.type(screen.getByRole('textbox'), 'a.com');
    await vi.waitFor(() => expect(ontest).toHaveBeenCalled());
    const [target, opts] = ontest.mock.calls.at(-1);
    expect(target).toBe('a.com');
    expect(opts.resolve).toBe(false);
  });

  it('显示命中第几条与判决', () => {
    render(Probe, {
      colorOf, ontest: vi.fn(),
      result: { index: 3, decision: 'Outbound', outbound: '日本节点', tried: 2, needResolve: false },
    });
    expect(screen.getByText(/第\s*3\s*条/)).toBeInTheDocument();
    expect(screen.getByText('日本节点')).toBeInTheDocument();
  });

  it('显示「前 N 条已试未命中」—— 这是排查的关键信息', () => {
    render(Probe, {
      colorOf, ontest: vi.fn(),
      result: { index: 3, decision: 'Outbound', outbound: 'JP', tried: 2, needResolve: false },
    });
    expect(screen.getByText(/前\s*2\s*条已试/)).toBeInTheDocument();
  });

  it('命中 IP 类规则时提示需解析，并给一键重测', async () => {
    const ontest = vi.fn();
    const u = userEvent.setup();
    render(Probe, {
      colorOf, ontest,
      result: { index: 5, decision: null, tried: 4, needResolve: true },
    });
    expect(screen.getByText(/需解析/)).toBeInTheDocument();
    const btn = screen.getByRole('button', { name: /解析后重测/ });
    await u.click(btn);
    expect(ontest.mock.calls.at(-1)[1].resolve).toBe(true);
  });

  it('判决结果用 aria-live 播报 —— 屏幕阅读器要能听到试算结论', () => {
    const { container } = render(Probe, {
      colorOf, ontest: vi.fn(),
      result: { index: 1, decision: 'Direct', tried: 0, needResolve: false },
    });
    expect(container.querySelector('[aria-live]')).toBeTruthy();
  });

  it('DIRECT / REJECT 判决也能正确显示', () => {
    render(Probe, {
      colorOf, ontest: vi.fn(),
      result: { index: 2, decision: 'Reject', tried: 1, needResolve: false },
    });
    expect(screen.getByText('REJECT')).toBeInTheDocument();
  });

  it('清空输入时不残留上一次的判决', async () => {
    const u = userEvent.setup();
    const ontest = vi.fn();
    render(Probe, { colorOf, ontest, result: null, debounce: 0 });
    const input = screen.getByRole('textbox');
    await u.type(input, 'a.com');
    await u.clear(input);
    await vi.waitFor(() => {
      const last = ontest.mock.calls.at(-1);
      expect(last[0]).toBe('');
    });
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd ui && npx vitest run Probe`
Expected: `Failed to resolve import ./Probe.svelte`

- [ ] **Step 3: 写实现**

`ui/src/views/Probe.svelte`：

```svelte
<script>
  /**
   * 探针即搜索框（signature ②，spec §11.5）。
   *
   * 这个输入框**不是过滤器，是试算探针**。输入一个域名，命中的那条
   * 规则升亮、其余整体降噪，并显示「前 N 条已试未命中」——
   * 把「为什么这域名没走代理」这个核心排查动作压缩成一次输入。
   *
   * 后端复用路由层纯函数（spec §4.2 纪律①），保证试算结果与真实判决
   * **永远一致**。试算与实际不一致的排查工具比没有更糟。
   *
   * 两阶段求值（spec §11.2）：默认 resolve=false，只跑第一轮 ——
   * 快，且不发 DNS 查询。命中 IP 类规则时提示「需解析才能确定」
   * 并给一键重测（resolve=true 跑完两轮）。
   */
  import { onDestroy } from 'svelte';

  let {
    /** { index, decision, outbound, tried, needResolve } | null */
    result = null,
    colorOf,
    ontest = () => {},
    debounce = 220,
  } = $props();

  let text = $state('');
  let timer = null;

  function schedule(v) {
    clearTimeout(timer);
    // 防抖：每敲一个字符就试算一次会让 IPC 打满，
    // 但探针的价值就在即时反馈，所以不能太长
    timer = setTimeout(() => ontest(v, { resolve: false }), debounce);
  }

  onDestroy(() => clearTimeout(timer));

  function onInput(e) {
    text = e.currentTarget.value;
    schedule(text.trim());
  }

  function retestWithDns() {
    clearTimeout(timer);
    ontest(text.trim(), { resolve: true });
  }

  const label = $derived.by(() => {
    if (!result) return null;
    if (result.decision === 'Direct') return { name: 'DIRECT', color: 'var(--state-direct)' };
    if (result.decision === 'Reject') return { name: 'REJECT', color: 'var(--state-fail)' };
    if (result.outbound) return { name: result.outbound, color: colorOf(result.outbound) };
    return null;
  });
</script>

<div class="probe">
  <div class="row">
    <label class="tag" for="probe-in">试算</label>
    <input id="probe-in" class="in" type="text" value={text}
           autocomplete="off" spellcheck="false"
           placeholder="输入域名或 IP，看它会走哪条规则"
           aria-label="试算：输入域名查看分流判决"
           oninput={onInput} />
  </div>

  <p class="verdict" aria-live="polite">
    {#if !result}
      <span class="muted">输入即试算，结果与真实判决一致。</span>
    {:else if result.needResolve}
      <span class="warn">命中第 {result.index} 条，但它是 IP 类规则，<b>需解析才能确定</b>。</span>
      <button type="button" onclick={retestWithDns}>解析后重测</button>
    {:else}
      命中第 {result.index} 条 · 判决
      {#if label}
        <span class="chip" style:background={label.color} aria-hidden="true"></span>
        <span class="name">{label.name}</span>
      {/if}
      <span class="sep" aria-hidden="true">·</span>
      <span class="muted mono">前 {result.tried} 条已试，未命中</span>
    {/if}
  </p>
</div>

<style>
  .probe { padding: 14px 16px; border-bottom: 1px solid var(--border); }
  .row { display: flex; align-items: center; gap: 10px; }

  .tag {
    font-size: var(--fs-11);
    letter-spacing: .07em;
    text-transform: uppercase;
    color: var(--text-4);
    font-weight: 600;
    flex: none;
  }

  .in {
    flex: 1;
    background: var(--surface-0);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius);
    padding: 7px 11px;
    color: var(--text-1);
    font-size: var(--fs-13);
    font-family: var(--font-mono);
  }
  .in::placeholder { color: var(--text-4); }
  .in:focus-visible {
    outline: 2px solid rgba(91, 143, 249, .5);
    outline-offset: -1px;
    border-color: var(--border-strong);
  }

  .verdict {
    display: flex;
    align-items: center;
    gap: 7px;
    margin: 9px 0 0;
    font-size: var(--fs-12);
    color: var(--text-2);
    min-height: 18px;
  }
  .muted { color: var(--text-3); }
  .warn  { color: var(--state-warn); }
  .warn b { color: var(--text-1); font-weight: 500; }
  .name  { color: var(--text-1); }
  .sep   { color: var(--text-4); }

  .chip { width: 6px; height: 6px; border-radius: 2px; flex: none; }

  .verdict button {
    background: var(--surface-2);
    color: var(--text-2);
    border: 1px solid var(--border-strong);
    border-radius: 3px;
    padding: 2px 8px;
    font-size: var(--fs-11);
    font-family: inherit;
    cursor: pointer;
  }
  .verdict button:hover { color: var(--text-1); }
</style>
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd ui && npx vitest run Probe`
Expected: 8 个测试全部 PASS。

**特别确认 `命中 IP 类规则时提示需解析，并给一键重测`** —— 它对应 §11.2 的两阶段求值协议在 UI 上的落地。少了这一步，用户会拿到一个「看起来确定实则不确定」的判决，比不给判决更糟。

- [ ] **Step 5: 提交**

```bash
git add ui/src/views/Probe.svelte ui/src/views/Probe.test.js
git commit -m "feat(ui): 探针即搜索框（signature ②，含两阶段重测）"
```

---

### Task 13: 规则视图

**Files:**
- Create: `ui/src/views/RulesView.svelte`
- Create: `ui/src/views/RulesView.test.js`

把热度染色、探针、拖拽排序、启用/停用拼成规则视图。这是**排查主场**，探针置于顶部。

**§11.4 明确拒绝的第三条最关键：规则类型不做成彩色 pill 标签。** 类型用**等宽小写缩写 + 统一低对比灰**，靠列位置识别，**颜色全部让给出站**。彩色 pill 会把规则列表变成彩虹糖，恰好摧毁它唯一需要的能力 —— 扫读。

列布局取自 mockup：`18px 74px 1fr 150px 66px`（标记 / 类型 / 匹配值 / 出站 / 命中）。

- [ ] **Step 1: 写失败的测试**

`ui/src/views/RulesView.test.js`：

```js
import { describe, it, expect, vi } from 'vitest';
import { render, screen, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import RulesView from './RulesView.svelte';

const rules = [
  { id: 1, type: 'geosite', value: 'category-ads', target: 'REJECT', hits: 18204, enabled: true },
  { id: 2, type: 'suffix',  value: 'googleapis.com', target: '新加坡', hits: 3891, enabled: true },
  { id: 3, type: 'keyword', value: 'google',  target: '日本节点', hits: 9417, enabled: true },
  { id: 4, type: 'geosite', value: 'cn',      target: 'DIRECT',  hits: 42663, enabled: true },
  { id: 5, type: 'final',   value: '*',       target: '日本节点', hits: 88120, enabled: true },
];
const colorOf = () => '#5b8ff9';
const base = () => ({ rules, colorOf, probe: null, ontest: vi.fn(),
                      onreorder: vi.fn(), ontoggle: vi.fn() });

describe('规则视图', () => {
  it('是一个带表头的列表，能被屏幕阅读器当表读', () => {
    render(RulesView, base());
    expect(screen.getByRole('table')).toBeInTheDocument();
    expect(screen.getAllByRole('columnheader').length).toBeGreaterThanOrEqual(4);
  });

  it('规则类型是低对比灰文本，不是彩色 pill', () => {
    // 反 AI 塑料感 + spec §11.4 明确拒绝：颜色全部让给出站
    const { container } = render(RulesView, base());
    const type = container.querySelector('.type');
    expect(type).toBeTruthy();
    const cls = type.getAttribute('class') ?? '';
    expect(cls).not.toMatch(/pill|badge|tag-/);
  });

  it('命中数越高背景越亮（signature ①）', () => {
    const { container } = render(RulesView, base());
    const rows = [...container.querySelectorAll('tbody tr')];
    const alpha = (el) => {
      const m = (el.getAttribute('style') ?? '').match(/rgba\(255,\s*255,\s*255,\s*([\d.]+)\)/);
      return m ? parseFloat(m[1]) : 0;
    };
    // final（88,120）应比 suffix（3,891）亮
    expect(alpha(rows[4])).toBeGreaterThan(alpha(rows[1]));
  });

  it('零命中的规则背景是透明的 —— 死规则要一眼可辨', () => {
    const dead = [...rules, { id: 6, type: 'domain', value: 'dead.com', target: 'DIRECT', hits: 0, enabled: true }];
    const { container } = render(RulesView, { ...base(), rules: dead });
    const last = [...container.querySelectorAll('tbody tr')].at(-1);
    const style = last.getAttribute('style') ?? '';
    expect(style).toMatch(/transparent|rgba\(255,\s*255,\s*255,\s*0\)/);
  });

  it('探针命中时该行被标出，其余降噪', () => {
    render(RulesView, { ...base(), probe: { index: 3, decision: 'Outbound', outbound: '日本节点', tried: 2, needResolve: false } });
    const rows = screen.getAllByRole('row').slice(1);
    expect(rows[2].className).toMatch(/hit/);
    expect(rows[0].className).toMatch(/dim/);
  });

  it('命中行用 aria-current 标注 —— 不只靠颜色', () => {
    render(RulesView, { ...base(), probe: { index: 3, decision: 'Outbound', outbound: 'JP', tried: 2, needResolve: false } });
    const rows = screen.getAllByRole('row').slice(1);
    expect(rows[2]).toHaveAttribute('aria-current', 'true');
  });

  it('每行有启用开关，且开关有可访问名字', () => {
    render(RulesView, base());
    const sw = screen.getAllByRole('switch');
    expect(sw).toHaveLength(rules.length);
    expect(sw[0]).toHaveAccessibleName(/启用|category-ads/);
  });

  it('点开关触发 ontoggle', async () => {
    const u = userEvent.setup();
    const p = base();
    render(RulesView, p);
    await u.click(screen.getAllByRole('switch')[0]);
    expect(p.ontoggle).toHaveBeenCalledWith(1, false);
  });

  it('Alt+↓ 把规则下移（拖拽的键盘等价物）', async () => {
    const u = userEvent.setup();
    const p = base();
    render(RulesView, p);
    const handle = screen.getAllByRole('button', { name: /移动|拖拽/ })[0];
    handle.focus();
    await u.keyboard('{Alt>}{ArrowDown}{/Alt}');
    expect(p.onreorder).toHaveBeenCalledWith(0, 1);
  });

  it('拖拽把手对键盘可达', () => {
    render(RulesView, base());
    const handles = screen.getAllByRole('button', { name: /移动|拖拽/ });
    expect(handles).toHaveLength(rules.length);
    for (const h of handles) expect(h.tabIndex).toBeGreaterThanOrEqual(0);
  });

  it('规则引用不存在的出站时标红（spec §12）', () => {
    const bad = [{ id: 9, type: 'domain', value: 'x.com', target: 'GHOST', hits: 0, enabled: true, unknownOutbound: true }];
    const { container } = render(RulesView, { ...base(), rules: bad });
    expect(container.querySelector('tbody tr').className).toMatch(/invalid/);
  });

  it('无规则时给空状态引导，不渲染空表', () => {
    render(RulesView, { ...base(), rules: [] });
    expect(screen.queryByRole('table')).toBeNull();
    expect(screen.getByText(/还没有规则|添加/)).toBeInTheDocument();
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd ui && npx vitest run RulesView`
Expected: `Failed to resolve import ./RulesView.svelte`

- [ ] **Step 3: 写实现**

`ui/src/views/RulesView.svelte`：

```svelte
<script>
  /**
   * 规则视图 —— 排查主场（spec §11.3 / §11.5）。
   *
   * 两个 signature 都在这里：
   *   ① 命中热度染色：行背景按命中数中性染色，死规则几乎透明
   *   ② 探针即搜索框：顶部输入即试算，命中行升亮、其余降噪
   *
   * spec §11.4 明确拒绝：规则类型**不做成彩色 pill**。类型用等宽小写
   * 缩写 + 统一低对比灰，靠列位置识别，颜色全部让给出站。彩色 pill 会把
   * 规则列表变成彩虹糖，恰好摧毁它唯一需要的能力 —— 扫读。
   */
  import Probe from './Probe.svelte';
  import EmptyState from './EmptyState.svelte';
  import { heatColor } from '../lib/heat.js';
  import { keyboardMove } from '../lib/reorder.js';
  import { count } from '../lib/format.js';

  let {
    rules = [],
    colorOf,
    /** 探针结果，见 Probe.svelte */
    probe = null,
    ontest = () => {},
    onreorder = () => {},
    ontoggle = () => {},
    onadd = () => {},
  } = $props();

  const maxHits = $derived(rules.reduce((m, r) => Math.max(m, r.hits ?? 0), 0));

  /** 探针激活时，命中行升亮、其余整体降噪 */
  const hitIndex = $derived(probe && !probe.needResolve ? probe.index - 1 : -1);
  const probing = $derived(probe !== null);

  function chip(target) {
    if (target === 'DIRECT') return 'var(--state-direct)';
    if (target === 'REJECT') return 'var(--state-fail)';
    return colorOf(target);
  }

  let dragFrom = $state(null);

  function onDragStart(e, i) {
    dragFrom = i;
    e.dataTransfer.effectAllowed = 'move';
    // Firefox 要求必须 setData 才会真的开始拖
    e.dataTransfer.setData('text/plain', String(i));
  }
  function onDragOver(e) {
    e.preventDefault();
    e.dataTransfer.dropEffect = 'move';
  }
  function onDrop(e, i) {
    e.preventDefault();
    if (dragFrom !== null && dragFrom !== i) onreorder(dragFrom, i);
    dragFrom = null;
  }

  /** 拖拽的键盘等价物：Alt+↑/↓。没有它，排序对键盘用户等于不存在。 */
  function onHandleKey(e, i) {
    if (!e.altKey) return;
    if (e.key !== 'ArrowUp' && e.key !== 'ArrowDown') return;
    e.preventDefault();
    const r = keyboardMove(rules, i, e.key);
    if (r.index !== i) onreorder(i, r.index);
  }
</script>

<section class="view" aria-label="分流规则">
  <Probe {colorOf} result={probe} {ontest} />

  {#if !rules.length}
    <EmptyState
      title="还没有规则。"
      hint="规则决定流量往哪走，顺序即优先级 —— 首命中即返回。至少需要一条 MATCH 兜底。"
      action="添加第一条规则"
      onaction={onadd} />
  {:else}
    <table>
      <caption class="sr-only">
        分流规则共 {rules.length} 条，按顺序匹配、首命中即返回。
        使用拖拽把手或 Alt 加上下方向键调整顺序。
      </caption>
      <thead>
        <tr>
          <th scope="col"><span class="sr-only">顺序</span></th>
          <th scope="col">类型</th>
          <th scope="col">匹配值</th>
          <th scope="col">出站</th>
          <th scope="col" class="r">命中</th>
          <th scope="col"><span class="sr-only">启用</span></th>
        </tr>
      </thead>
      <tbody>
        {#each rules as r, i (r.id)}
          <tr
            style:background={probing ? undefined : heatColor(r.hits, maxHits)}
            class:hit={i === hitIndex}
            class:dim={probing && i !== hitIndex}
            class:invalid={r.unknownOutbound}
            class:off={!r.enabled}
            aria-current={i === hitIndex ? 'true' : undefined}
            ondragover={onDragOver}
            ondrop={(e) => onDrop(e, i)}>

            <td class="mark">
              <button type="button" class="handle"
                      aria-label={`移动规则 ${r.type} ${r.value}，使用 Alt 加上下方向键`}
                      draggable="true"
                      ondragstart={(e) => onDragStart(e, i)}
                      onkeydown={(e) => onHandleKey(e, i)}>
                <span aria-hidden="true">{i === hitIndex ? '▸' : '⠿'}</span>
              </button>
            </td>

            <!-- 类型：等宽小写缩写 + 统一低对比灰。不是 pill，不带颜色。 -->
            <td class="type mono">{r.type}</td>
            <td class="val mono" title={r.value}>{r.value}</td>

            <td class="out">
              <span class="chip" style:background={chip(r.target)} aria-hidden="true"></span>{r.target}
              {#if r.unknownOutbound}
                <span class="err" title="该出站不存在，此规则将被跳过">不存在</span>
              {/if}
            </td>

            <td class="hits mono r">{count(r.hits)}</td>

            <td class="sw-cell">
              <button type="button" role="switch" class="sw"
                      aria-checked={r.enabled}
                      aria-label={`启用规则 ${r.type} ${r.value}`}
                      class:off={!r.enabled}
                      onclick={() => ontoggle(r.id, !r.enabled)}></button>
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>

<style>
  .view { background: var(--surface-1); }

  table { width: 100%; border-collapse: collapse; table-layout: fixed; }

  /* 列宽取自 mockup：标记 / 类型 / 匹配值 / 出站 / 命中 / 开关 */
  th:nth-child(1), td:nth-child(1) { width: 30px; }
  th:nth-child(2), td:nth-child(2) { width: 74px; }
  th:nth-child(4), td:nth-child(4) { width: 150px; }
  th:nth-child(5), td:nth-child(5) { width: 66px; }
  th:nth-child(6), td:nth-child(6) { width: 44px; }

  th {
    height: 28px;
    font-size: 10px;
    letter-spacing: .08em;
    text-transform: uppercase;
    color: var(--text-4);
    font-weight: 600;
    text-align: left;
    padding: 0 8px;
    background: var(--surface-0);
    border-bottom: 1px solid var(--border);
  }
  th.r, td.r { text-align: right; }

  tbody tr {
    height: var(--row-rule);
    border-bottom: 1px solid rgba(255, 255, 255, .035);
  }
  tbody tr:last-child { border-bottom: none; }

  td {
    padding: 0 8px;
    font-size: 12.5px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .type { color: var(--text-3); font-size: var(--fs-12); }
  .val  { color: var(--text-1); }
  .out  { color: var(--text-2); }
  .hits { color: var(--text-3); font-size: var(--fs-12); font-variant-numeric: tabular-nums; }

  .chip {
    display: inline-block;
    width: 6px; height: 6px;
    border-radius: 2px;
    margin-right: 7px;
    vertical-align: middle;
  }

  /* 探针激活：其余降噪，命中行升亮（signature ②） */
  .dim .val, .dim .type, .dim .out, .dim .hits { color: var(--text-4); }
  .dim .chip { opacity: .32; }

  .hit { background: rgba(91, 143, 249, .11); }
  .hit .val { color: #fff; font-weight: 500; }
  .hit .mark { color: var(--outbound-1); }
  .hit .type, .hit .hits { color: var(--text-2); }
  .hit .out { color: var(--text-1); }

  /* 引用了不存在的出站（spec §12）：标红但不阻断，该规则视为不匹配跳过 */
  .invalid .out { color: var(--state-fail); }
  .err {
    margin-left: 6px;
    font-size: var(--fs-11);
    color: var(--state-fail);
    opacity: .85;
  }

  .off .val, .off .type { opacity: .45; }

  .handle {
    all: unset;
    display: block;
    width: 100%;
    text-align: center;
    color: var(--text-4);
    font-size: var(--fs-11);
    cursor: grab;
  }
  .handle:hover { color: var(--text-3); }
  .handle:focus-visible { outline: 2px solid var(--outbound-1); outline-offset: 1px; }

  .sw-cell { text-align: right; }
  .sw {
    all: unset;
    display: inline-block;
    width: 26px; height: 15px;
    border-radius: 8px;
    background: var(--state-live);
    position: relative;
    cursor: pointer;
    vertical-align: middle;
  }
  .sw::after {
    content: '';
    position: absolute;
    right: 2px; top: 2px;
    width: 11px; height: 11px;
    border-radius: 50%;
    background: #fff;
  }
  .sw.off { background: rgba(255, 255, 255, .13); }
  .sw.off::after { right: auto; left: 2px; background: var(--text-3); }
  .sw:focus-visible { outline: 2px solid var(--outbound-1); outline-offset: 2px; }
</style>
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd ui && npx vitest run RulesView`
Expected: 12 个测试全部 PASS。

**这几条不要放宽：**
- `规则类型是低对比灰文本，不是彩色 pill` —— 直接对应 §11.4 拒绝的第三个套路
- `零命中的规则背景是透明的` —— signature ① 的全部价值就在于死规则一眼可辨
- `Alt+↓ 把规则下移` + `拖拽把手对键盘可达` —— 排序是核心操作，键盘用户必须能用
- `命中行用 aria-current 标注` —— 探针的结论不能只靠背景色传达

- [ ] **Step 5: 对照 mockup 手工验收**

`npm run dev`，对照 `design-direction.html` 的「规则视图 —— 探针已激活」：

| 检查项 | 期望 |
|---|---|
| 行高 | 32px，密集如交易台 |
| 类型列 | 等宽小写、统一灰，**没有任何彩色** |
| 出站色码 | 6px 圆角小方块，是全屏唯一彩色（连同状态色） |
| 命中数 | 右对齐，tabular-nums 纵向成列 |
| 热度染色 | 最热行微亮，死规则完全透明，**中间几档分得开** |
| 输入域名后 | 命中行蓝底升亮，其余整行降噪 |

- [ ] **Step 6: 提交**

```bash
git add ui/src/views/RulesView.svelte ui/src/views/RulesView.test.js
git commit -m "feat(ui): 规则视图（热度染色 + 探针联动 + 键盘可达排序）"
```

---

## Part D — 出站与设置

### Task 14: 出站列表

**Files:**
- Create: `ui/src/lib/Switch.svelte`
- Create: `ui/src/views/OutboundsView.svelte`
- Create: `ui/src/views/OutboundsView.test.js`

§11.4 明确拒绝第二个套路：**不做服务器卡片网格 + 圆形延迟指示**，改用紧凑行列表，延迟与会话数用 `tabular-nums` 对齐成列。

行高 38px。每行只留：**色码 · 名字 · 延迟 · 会话数 · 开关**。

**延迟指标的定义（§11.2）** —— 这个在 UI 上要如实呈现，不能含糊：

| 场景 | 取值 |
|---|---|
| 稳态（有流量） | 最近 N 次上行 `POST /api/sync` 响应耗时的**中位数**（零额外流量） |
| 刚建立、尚无流量 | 握手 RTT |
| 手动点「测试」 | 发一个 PADDING TU 的 POST 并计时 |

**§6.4 的安全决策要在 UI 上真的做出来**：出站不可用时是**拒绝连接 + UI 报警**，绝不静默回退。所以「重连中」「已停止」这两个状态必须显眼。

- [ ] **Step 1: 抽出 Switch 组件**

规则行与出站行都要开关，抽出来避免两份实现漂移。

`ui/src/lib/Switch.svelte`：

```svelte
<script>
  /**
   * 开关。用 role="switch" 而非 checkbox —— 语义是「开/关一个持续状态」，
   * 不是「勾选一个选项」，屏幕阅读器的播报也因此不同。
   */
  let { checked = false, label = '', onchange = () => {}, disabled = false } = $props();
</script>

<button type="button" role="switch"
        aria-checked={checked}
        aria-label={label}
        {disabled}
        class:off={!checked}
        onclick={() => onchange(!checked)}></button>

<style>
  button {
    all: unset;
    display: inline-block;
    width: 28px; height: 16px;
    border-radius: 8px;
    background: var(--state-live);
    position: relative;
    cursor: pointer;
    vertical-align: middle;
    flex: none;
  }
  button::after {
    content: '';
    position: absolute;
    right: 2px; top: 2px;
    width: 12px; height: 12px;
    border-radius: 50%;
    background: #fff;
  }
  button.off { background: rgba(255, 255, 255, .13); }
  button.off::after { right: auto; left: 2px; background: var(--text-3); }
  button:disabled { opacity: .4; cursor: not-allowed; }
  button:focus-visible { outline: 2px solid var(--outbound-1); outline-offset: 2px; }
</style>
```

- [ ] **Step 2: 写失败的测试**

`ui/src/views/OutboundsView.test.js`：

```js
import { describe, it, expect, vi } from 'vitest';
import { render, screen, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import OutboundsView from './OutboundsView.svelte';

const outbounds = [
  { id: 'a', name: '日本节点', state: 'live', latency: 38, sessions: 4, enabled: true, host: true },
  { id: 'b', name: '新加坡',   state: 'live', latency: 72, sessions: 4, enabled: true },
  { id: 'c', name: '德国备用', state: 'reconnecting', latency: null, sessions: 0, enabled: false },
];
const colorOf = () => '#5b8ff9';
const base = () => ({ outbounds, colorOf, ontoggle: vi.fn(), onprobe: vi.fn(), onadd: vi.fn() });

describe('出站列表', () => {
  it('是行列表而非卡片网格（spec §11.4 拒绝套路二）', () => {
    const { container } = render(OutboundsView, base());
    expect(container.querySelector('.grid, [class*="card"]')).toBeNull();
    expect(screen.getAllByRole('row').length).toBeGreaterThanOrEqual(3);
  });

  it('延迟与会话数用 tabular-nums 对齐成列', () => {
    const { container } = render(OutboundsView, base());
    const cell = container.querySelector('.num');
    expect(cell).toBeTruthy();
    expect(cell.className).toMatch(/\bnum\b/);
  });

  it('未测得延迟显示占位符而非 0 ms', () => {
    render(OutboundsView, base());
    expect(screen.queryByText('0 ms')).toBeNull();
  });

  it('状态不只靠颜色 —— 重连中必须有文字', () => {
    render(OutboundsView, base());
    expect(screen.getByText(/重连中/)).toBeInTheDocument();
  });

  it('宿主出站有标注', () => {
    render(OutboundsView, base());
    expect(screen.getByText(/宿主/)).toBeInTheDocument();
  });

  it('每行开关有可访问名字并能触发', async () => {
    const u = userEvent.setup();
    const p = base();
    render(OutboundsView, p);
    const sw = screen.getAllByRole('switch');
    expect(sw).toHaveLength(3);
    expect(sw[0]).toHaveAccessibleName(/日本节点/);
    await u.click(sw[0]);
    expect(p.ontoggle).toHaveBeenCalledWith('a', false);
  });

  it('有手动测延迟的按钮', async () => {
    const u = userEvent.setup();
    const p = base();
    render(OutboundsView, p);
    await u.click(screen.getAllByRole('button', { name: /测试延迟/ })[0]);
    expect(p.onprobe).toHaveBeenCalledWith('a');
  });

  it('每行状态用 aria-label 完整播报', () => {
    render(OutboundsView, base());
    const rows = screen.getAllByRole('row').slice(1);
    expect(rows[2].getAttribute('aria-label') ?? rows[2].textContent).toMatch(/重连/);
  });

  it('无出站时给「添加第一个服务器」引导（spec §11.3）', async () => {
    const u = userEvent.setup();
    const p = { ...base(), outbounds: [] };
    render(OutboundsView, p);
    const btn = screen.getByRole('button', { name: /添加第一个服务器/ });
    await u.click(btn);
    expect(p.onadd).toHaveBeenCalled();
  });

  it('失败状态显眼 —— §6.4 要求出站不可用时 UI 报警', () => {
    const bad = [{ id: 'x', name: 'X', state: 'failed', latency: null, sessions: 0, enabled: true }];
    const { container } = render(OutboundsView, { ...base(), outbounds: bad });
    expect(container.querySelector('.state-failed, [data-state="failed"]')).toBeTruthy();
  });
});
```

- [ ] **Step 3: 运行测试确认失败**

Run: `cd ui && npx vitest run OutboundsView`
Expected: `Failed to resolve import ./OutboundsView.svelte`

- [ ] **Step 4: 写实现**

`ui/src/views/OutboundsView.svelte`：

```svelte
<script>
  /**
   * 出站列表（spec §11.4）。
   *
   * 行式而非卡片网格：节点多了以后卡片网格既浪费空间又难扫读。
   * 每行只留 色码 · 名字 · 延迟 · 会话数 · 开关，延迟与会话数用
   * tabular-nums 对齐成列，纵向一扫就能比较。
   *
   * 延迟的定义（spec §11.2）：稳态取最近 N 次上行 POST /api/sync 响应
   * 耗时的中位数（零额外流量）；刚建立时取握手 RTT；手动测试则发一个
   * PADDING TU 并计时。**不另开探测子流** —— 复用既有流量路径，
   * 既省实现也少一个可探测面。
   *
   * spec §6.4：出站不可用时是「拒绝连接 + UI 报警」，绝不静默回退。
   * 所以 reconnecting / failed 两个状态必须显眼。
   */
  import Switch from '../lib/Switch.svelte';
  import EmptyState from './EmptyState.svelte';
  import { ms, count } from '../lib/format.js';

  let {
    outbounds = [],
    colorOf,
    ontoggle = () => {},
    onprobe = () => {},
    onadd = () => {},
  } = $props();

  const STATE = {
    live:         { text: '',       color: 'var(--text-3)' },
    starting:     { text: '连接中', color: 'var(--state-warn)'  },
    reconnecting: { text: '重连中', color: 'var(--state-warn)'  },
    failed:       { text: '已断开', color: 'var(--state-fail)'  },
    stopped:      { text: '已停止', color: 'var(--text-4)' },
  };

  const desc = (o) => {
    const s = STATE[o.state] ?? STATE.stopped;
    const parts = [o.name];
    if (o.host) parts.push('宿主');
    parts.push(s.text || '已连接');
    if (o.latency != null) parts.push(`延迟 ${ms(o.latency)}`);
    parts.push(`${count(o.sessions)} 会话`);
    return parts.join('，');
  };
</script>

<section class="view" aria-label="出站服务器">
  {#if !outbounds.length}
    <EmptyState
      title="还没有配置出站服务器。"
      hint="websieve 的出站需要一对密钥，从你自建的服务端获取。"
      action="添加第一个服务器"
      onaction={onadd} />
  {:else}
    <table>
      <caption class="sr-only">
        出站服务器共 {outbounds.length} 个。每行显示名称、状态、延迟、会话数与启用开关。
      </caption>
      <thead>
        <tr>
          <th scope="col"><span class="sr-only">色码</span></th>
          <th scope="col">名称</th>
          <th scope="col" class="r">延迟</th>
          <th scope="col" class="r">会话</th>
          <th scope="col"><span class="sr-only">操作</span></th>
          <th scope="col"><span class="sr-only">启用</span></th>
        </tr>
      </thead>
      <tbody>
        {#each outbounds as o (o.id)}
          {@const s = STATE[o.state] ?? STATE.stopped}
          <tr aria-label={desc(o)}
              data-state={o.state}
              class="state-{o.state}">
            <td class="c">
              <span class="chip" style:background={colorOf(o.name)} aria-hidden="true"></span>
            </td>

            <td class="name">
              {o.name}
              {#if o.host}<span class="sub">· 宿主</span>{/if}
            </td>

            <!-- 状态有文字时占用延迟列：重连中的节点没有有意义的延迟 -->
            <td class="num r" style:color={s.text ? s.color : undefined}>
              {s.text || ms(o.latency)}
            </td>

            <td class="num r">{o.sessions > 0 ? count(o.sessions) : '—'}</td>

            <td class="c">
              <button type="button" class="probe"
                      aria-label={`测试延迟：${o.name}`}
                      disabled={o.state !== 'live'}
                      onclick={() => onprobe(o.id)}>测速</button>
            </td>

            <td class="c">
              <Switch checked={o.enabled} label={`启用出站 ${o.name}`}
                      onchange={(v) => ontoggle(o.id, v)} />
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>

<style>
  .view { background: var(--surface-1); }

  table { width: 100%; border-collapse: collapse; table-layout: fixed; }

  th:nth-child(1), td:nth-child(1) { width: 26px; }
  th:nth-child(3), td:nth-child(3) { width: 72px; }
  th:nth-child(4), td:nth-child(4) { width: 62px; }
  th:nth-child(5), td:nth-child(5) { width: 56px; }
  th:nth-child(6), td:nth-child(6) { width: 46px; }

  th {
    height: 28px;
    font-size: 10px;
    letter-spacing: .08em;
    text-transform: uppercase;
    color: var(--text-4);
    font-weight: 600;
    text-align: left;
    padding: 0 8px;
    background: var(--surface-0);
    border-bottom: 1px solid var(--border);
  }
  th.r, td.r { text-align: right; }

  tbody tr {
    height: var(--row-outbound);
    border-bottom: 1px solid rgba(255, 255, 255, .035);
  }
  tbody tr:last-child { border-bottom: none; }

  td {
    padding: 0 8px;
    font-size: var(--fs-13);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  td.c { text-align: center; }

  .name { font-weight: 500; }
  .sub  { color: var(--text-4); font-weight: 400; font-size: var(--fs-12); }

  /* tabular-nums：延迟与会话数纵向对齐成列，一扫就能比较 */
  .num {
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
    font-size: var(--fs-12);
    color: var(--text-3);
  }

  .chip { display: inline-block; width: 6px; height: 6px; border-radius: 2px; }

  /* spec §6.4：出站不可用要报警，不能是个安静的灰字 */
  .state-failed .name { color: var(--state-fail); }
  .state-reconnecting .name, .state-stopped .name { color: var(--text-2); }

  .probe {
    background: transparent;
    color: var(--text-4);
    border: 1px solid var(--border);
    border-radius: 3px;
    padding: 2px 7px;
    font-size: var(--fs-11);
    font-family: inherit;
    cursor: pointer;
  }
  .probe:hover:not(:disabled) { color: var(--text-2); border-color: var(--border-strong); }
  .probe:disabled { opacity: .35; cursor: not-allowed; }
</style>
```

- [ ] **Step 5: 运行测试并提交**

Run: `cd ui && npx vitest run OutboundsView`
Expected: 10 个测试全部 PASS

```bash
git add ui/src/lib/Switch.svelte \
        ui/src/views/OutboundsView.svelte \
        ui/src/views/OutboundsView.test.js
git commit -m "feat(ui): 出站行列表（tabular-nums 对齐，状态不只靠颜色）"
```

---

### Task 15: 设置覆盖层

**Files:**
- Create: `ui/src/views/SettingsOverlay.svelte`
- Create: `ui/src/views/SettingsOverlay.test.js`

§11.3：**设置走覆盖层而非第四个标签** —— 不常用，不该占据同级位置。

覆盖层是本阶段唯一允许用阴影的地方（§11.4：深度策略 borders-only，**浮层除外**）。

**焦点管理是覆盖层的硬要求**，不是可选项：打开时焦点进入、Tab 循环被困在层内、Esc 关闭、关闭时焦点回到触发元素。做不到这几条，覆盖层对键盘用户就是个陷阱 —— 焦点跑到背景里而视觉上被遮住。

**§5.4 的要求也在这里落地**：导出/分享配置的入口必须显式警告「该文件含私钥」。

- [ ] **Step 1: 写失败的测试**

`ui/src/views/SettingsOverlay.test.js`：

```js
import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import SettingsOverlay from './SettingsOverlay.svelte';

const config = {
  mixedPort: 7890, allowLan: false, mode: 'rule', systemProxy: false,
  logLevel: 'info', geoAutoUpdate: true, carrier: 'shared',
};
const base = () => ({ open: true, config, onclose: vi.fn(), onsave: vi.fn(), onexport: vi.fn() });

describe('设置覆盖层', () => {
  it('是 modal dialog 而非第四个标签（spec §11.3）', () => {
    render(SettingsOverlay, base());
    const d = screen.getByRole('dialog');
    expect(d).toHaveAttribute('aria-modal', 'true');
    expect(d).toHaveAccessibleName(/设置/);
  });

  it('打开时焦点进入层内', async () => {
    render(SettingsOverlay, base());
    await vi.waitFor(() => {
      expect(screen.getByRole('dialog').contains(document.activeElement)).toBe(true);
    });
  });

  it('Esc 关闭', async () => {
    const u = userEvent.setup();
    const p = base();
    render(SettingsOverlay, p);
    await u.keyboard('{Escape}');
    expect(p.onclose).toHaveBeenCalled();
  });

  it('关闭按钮有可访问名字', () => {
    render(SettingsOverlay, base());
    expect(screen.getByRole('button', { name: /关闭/ })).toBeInTheDocument();
  });

  it('open=false 时不渲染', () => {
    render(SettingsOverlay, { ...base(), open: false });
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('每个表单控件都有关联的 label', () => {
    render(SettingsOverlay, base());
    for (const el of [
      screen.getByLabelText(/混合端口/),
      screen.getByLabelText(/允许局域网/),
      screen.getByLabelText(/系统代理/),
    ]) expect(el).toBeInTheDocument();
  });

  it('端口非法时给出错误且不静默吞掉', async () => {
    const u = userEvent.setup();
    const p = base();
    render(SettingsOverlay, p);
    const port = screen.getByLabelText(/混合端口/);
    await u.clear(port);
    await u.type(port, '99999');
    await u.click(screen.getByRole('button', { name: /保存/ }));
    expect(screen.getByRole('alert')).toBeInTheDocument();
    expect(p.onsave).not.toHaveBeenCalled();
  });

  it('导出入口显式警告含私钥（spec §5.4）', () => {
    render(SettingsOverlay, base());
    expect(screen.getByText(/私钥/)).toBeInTheDocument();
  });

  it('保存提示会丢失非规则区注释（spec §5.6）', () => {
    render(SettingsOverlay, base());
    expect(screen.getByText(/注释/)).toBeInTheDocument();
  });

  it('合法输入能保存', async () => {
    const u = userEvent.setup();
    const p = base();
    render(SettingsOverlay, p);
    await u.click(screen.getByLabelText(/允许局域网/));
    await u.click(screen.getByRole('button', { name: /保存/ }));
    expect(p.onsave).toHaveBeenCalled();
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd ui && npx vitest run SettingsOverlay`
Expected: `Failed to resolve import ./SettingsOverlay.svelte`

- [ ] **Step 3: 写实现**

`ui/src/views/SettingsOverlay.svelte`：

```svelte
<script>
  /**
   * 设置覆盖层（spec §11.3）。
   *
   * 走覆盖层而非第四个标签 —— 不常用，不该占据同级位置。
   *
   * 这是全项目**唯一允许用阴影**的地方（spec §11.4：深度策略 borders-only，
   * 浮层除外）。
   *
   * 焦点管理是硬要求：打开时焦点进入、Tab 被困在层内、Esc 关闭、
   * 关闭时焦点回到触发元素。做不到这几条，覆盖层对键盘用户就是个陷阱。
   */
  import { tick } from 'svelte';

  let { open = false, config = {}, onclose = () => {}, onsave = () => {}, onexport = () => {} } = $props();

  let dialog = $state(null);
  let draft = $state({ ...config });
  let error = $state('');
  let restoreFocus = null;

  $effect(() => {
    if (!open) return;
    draft = { ...config };
    error = '';
    restoreFocus = document.activeElement;
    tick().then(() => {
      dialog?.querySelector('input, select, button')?.focus();
    });
    return () => {
      // 关闭时焦点回到触发元素 —— 否则焦点会掉回 body，键盘用户当场迷路
      if (restoreFocus instanceof HTMLElement) restoreFocus.focus();
    };
  });

  function onKeydown(e) {
    if (e.key === 'Escape') {
      e.stopPropagation();
      onclose();
      return;
    }
    if (e.key !== 'Tab') return;
    // 焦点陷阱：Tab 在层内循环，不跑到背景里去
    const f = dialog?.querySelectorAll(
      'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])'
    );
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

  function save() {
    const p = Number(draft.mixedPort);
    // 错误绝不静默吞掉 —— 参照仓库既有的 port_conflict_is_reported_not_skipped
    if (!Number.isInteger(p) || p < 1 || p > 65535) {
      error = `混合端口非法：${draft.mixedPort}。有效范围是 1–65535。`;
      return;
    }
    error = '';
    onsave({ ...draft, mixedPort: p });
  }
</script>

{#if open}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <div class="scrim" onclick={onclose} role="presentation"></div>

  <div class="panel" role="dialog" aria-modal="true" aria-label="设置"
       bind:this={dialog} onkeydown={onKeydown} tabindex="-1">
    <header>
      <h2>设置</h2>
      <button type="button" class="x" aria-label="关闭设置" onclick={onclose}>×</button>
    </header>

    <div class="body">
      <section>
        <h3>入口</h3>
        <div class="field">
          <label for="s-port">混合端口</label>
          <input id="s-port" type="number" min="1" max="65535" class="mono"
                 bind:value={draft.mixedPort} />
          <p class="hint">SOCKS5 与 HTTP 共用同一端口，靠首字节嗅探区分。</p>
        </div>
        <div class="field row">
          <input id="s-lan" type="checkbox" bind:checked={draft.allowLan} />
          <label for="s-lan">允许局域网连接</label>
        </div>
        <div class="field row">
          <input id="s-sys" type="checkbox" bind:checked={draft.systemProxy} />
          <label for="s-sys">自动设置系统代理</label>
        </div>
        <p class="hint">退出时会恢复原设置；若进程崩溃，下次启动时兜底清理。</p>
      </section>

      <section>
        <h3>承载</h3>
        <div class="field">
          <label for="s-carrier">WebView 承载方式</label>
          <select id="s-carrier" bind:value={draft.carrier}>
            <option value="shared">shared —— 单 WebView 承载全部出站</option>
            <option value="isolated">isolated —— 每出站独立 WebView</option>
          </select>
          <p class="hint">
            shared 省内存但是单点故障：WebView 崩溃时全部出站同时断开。
            isolated 提供故障隔离，代价是内存随出站数线性增长。
          </p>
        </div>
      </section>

      <section>
        <h3>配置文件</h3>
        <button type="button" class="ghost" onclick={onexport}>导出配置…</button>
        <!-- spec §5.4：导出入口必须显式警告 -->
        <p class="hint warn">
          配置文件含 <b>client-priv 私钥（明文）</b>，仅靠 0600 权限保护。
          导出或分享前请确认接收方可信。
        </p>
        <!-- spec §5.6：非规则区的自定义注释会丢失，需在保存提示中明示 -->
        <p class="hint">
          从这里保存会重写配置的非规则区域，该区域内你手写的<b>注释会丢失</b>。
          规则区的注释始终逐字保留。要完全手工控制，请用原始 YAML 编辑。
        </p>
      </section>

      {#if error}
        <p class="err" role="alert">{error}</p>
      {/if}
    </div>

    <footer>
      <button type="button" class="ghost" onclick={onclose}>取消</button>
      <button type="button" class="primary" onclick={save}>保存</button>
    </footer>
  </div>
{/if}

<style>
  .scrim {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, .5);
    z-index: 10;
  }

  .panel {
    position: fixed;
    top: 50%; left: 50%;
    transform: translate(-50%, -50%);
    width: min(560px, calc(100vw - 48px));
    max-height: calc(100vh - 64px);
    display: flex;
    flex-direction: column;
    background: var(--surface-1);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius);
    /* 浮层是唯一允许用阴影的地方（spec §11.4） */
    box-shadow: 0 0 0 1px rgba(0, 0, 0, .4), 0 16px 48px rgba(0, 0, 0, .5);
    z-index: 11;
  }

  header {
    display: flex;
    align-items: center;
    padding: 12px 16px;
    border-bottom: 1px solid var(--border);
    background: var(--surface-0);
  }
  h2 { margin: 0; font-size: var(--fs-14); font-weight: 600; }

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

  .body { padding: 4px 16px 16px; overflow-y: auto; }

  section { padding: 14px 0; border-bottom: 1px solid var(--border); }
  section:last-of-type { border-bottom: none; }

  h3 {
    margin: 0 0 10px;
    font-size: var(--fs-11);
    letter-spacing: .09em;
    text-transform: uppercase;
    color: var(--text-4);
    font-weight: 600;
  }

  .field { margin-bottom: 10px; }
  .field.row { display: flex; align-items: center; gap: 8px; }
  .field.row label { margin: 0; }

  label {
    display: block;
    margin-bottom: 4px;
    font-size: var(--fs-12);
    color: var(--text-2);
  }

  input[type='number'], select {
    background: var(--surface-0);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius);
    padding: 6px 9px;
    color: var(--text-1);
    font-size: var(--fs-13);
    font-family: inherit;
    width: 100%;
  }
  input[type='number'] { width: 120px; font-family: var(--font-mono); }

  .hint {
    margin: 5px 0 0;
    font-size: var(--fs-11);
    color: var(--text-4);
    line-height: 1.65;
  }
  .hint b { color: var(--text-3); font-weight: 500; }
  .hint.warn { color: var(--state-warn); }
  .hint.warn b { color: var(--state-warn); font-weight: 600; }

  .err {
    margin: 12px 0 0;
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

  .ghost, .primary {
    border-radius: var(--radius);
    padding: 6px 14px;
    font-size: var(--fs-12);
    font-family: inherit;
    cursor: pointer;
  }
  .ghost {
    background: transparent;
    color: var(--text-2);
    border: 1px solid var(--border-strong);
  }
  .primary {
    background: var(--surface-2);
    color: var(--text-1);
    border: 1px solid var(--border-strong);
  }
  .primary:hover { border-color: rgba(255, 255, 255, .22); }
</style>
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd ui && npx vitest run SettingsOverlay`
Expected: 10 个测试全部 PASS。

**焦点相关的三条必须真过**（`打开时焦点进入层内` / `Esc 关闭` / 焦点陷阱）—— 覆盖层的焦点管理做不对，键盘用户会掉进一个看不见的背景里。

- [ ] **Step 5: 手工验证焦点陷阱**

单测覆盖不到「Tab 到最后一个再 Tab 会不会跑出去」。`npm run dev` 打开设置，**只用键盘**：

| 操作 | 期望 |
|---|---|
| 打开设置 | 焦点自动进入层内第一个控件 |
| 连续 Tab 到底再 Tab | 回到第一个控件，**不跑到背景的规则表上** |
| Shift+Tab 到顶再 Shift+Tab | 回到最后一个控件 |
| Esc | 关闭，且焦点**回到**原先打开设置的齿轮按钮 |

- [ ] **Step 6: 提交**

```bash
git add ui/src/views/SettingsOverlay.svelte ui/src/views/SettingsOverlay.test.js
git commit -m "feat(ui): 设置覆盖层（焦点陷阱 + 私钥警告 + 注释丢失提示）"
```

---

## Part E — 组装与验收

### Task 16: 出站色码分配与视图组装

**Files:**
- Create: `ui/src/lib/palette.js`
- Create: `ui/src/lib/palette.test.js`
- Modify: `ui/src/App.svelte`

出站色码必须**全局一致** —— 同一个出站在规则行、流量表、桑基节点里是同一个颜色，否则「颜色 = 去哪」这条纪律就破了。所以分配逻辑要集中一处。

- [ ] **Step 1: 写失败的测试**

`ui/src/lib/palette.test.js`：

```js
import { describe, it, expect } from 'vitest';
import { makePalette } from './palette.js';

describe('出站色码分配', () => {
  it('同一名字永远同色 —— 「颜色 = 去哪」的前提', () => {
    const c = makePalette(['JP', 'SG', 'DE']);
    expect(c('JP')).toBe(c('JP'));
  });

  it('不同名字不同色（在色码数以内）', () => {
    const c = makePalette(['JP', 'SG', 'DE']);
    expect(new Set([c('JP'), c('SG'), c('DE')]).size).toBe(3);
  });

  it('DIRECT 与 REJECT 用固定的状态色，不占出站色码', () => {
    const c = makePalette(['JP']);
    expect(c('DIRECT')).toBe('var(--state-direct)');
    expect(c('REJECT')).toBe('var(--state-fail)');
  });

  it('顺序无关 —— 出站列表重排不该让全屏换色', () => {
    const a = makePalette(['JP', 'SG', 'DE']);
    const b = makePalette(['DE', 'SG', 'JP']);
    expect(a('SG')).toBe(b('SG'));
  });

  it('未知名字有兜底色而非 undefined', () => {
    const c = makePalette(['JP']);
    expect(typeof c('从未见过')).toBe('string');
    expect(c('从未见过')).toBeTruthy();
  });

  it('8 个以内互不重色', () => {
    const names = Array.from({ length: 8 }, (_, i) => `out-${i}`);
    const c = makePalette(names);
    expect(new Set(names.map(c)).size).toBe(8);
  });

  it('超过色码数时循环复用而非产出无效值', () => {
    const names = Array.from({ length: 20 }, (_, i) => `out-${i}`);
    const c = makePalette(names);
    for (const n of names) expect(c(n)).toMatch(/^var\(--outbound-[1-8]\)$/);
  });
});
```

- [ ] **Step 2: 写实现**

`ui/src/lib/palette.js`：

```js
/**
 * 出站色码分配（spec §11.4）。
 *
 * 出站色码是界面中**唯一允许出现的彩色**，它在规则行、流量表、桑基节点里
 * 指的永远是同一件事：去哪。因此分配必须全局一致且**与顺序无关** ——
 * 出站列表重排一下就全屏换色，会当场摧毁这条语义。
 *
 * DIRECT / REJECT 用固定状态色，不占出站色码：它们不是「一个出站」，
 * 而是两种处置方式。
 */

const SLOTS = 8;

const BUILTIN = {
  DIRECT: 'var(--state-direct)',
  REJECT: 'var(--state-fail)',
};

/** 与顺序无关的稳定哈希（FNV-1a 变体，够用且短） */
function hash(s) {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

/**
 * 返回 (name) => CSS 色值。
 *
 * 先按名字排序再分配槽位：在出站数 ≤ 8 时保证互不相同，
 * 且不受传入顺序影响。
 *
 * ponytail: 阶段 4 的 tokens.css 给了 8 个色码，第 9 个及以后会与前面重色。
 * 上限：两个出站同色时，颜色不再唯一标识去向，需靠文字区分（文字始终都在，
 * 见 Task 17 的「色块非唯一载体」测试）。
 * 升级路径：改为按名字哈希到 HSL 色环（固定明度饱和度，只转色相）。
 * 现在不做 —— 自建服务器场景 3–5 个出站是常态，8 个已经很宽裕。
 */
export function makePalette(names = []) {
  const slot = new Map();
  const sorted = [...new Set(names)].filter((n) => !(n in BUILTIN)).sort();
  sorted.forEach((n, i) => slot.set(n, `var(--outbound-${(i % SLOTS) + 1})`));

  return (name) => {
    if (name in BUILTIN) return BUILTIN[name];
    const hit = slot.get(name);
    if (hit) return hit;
    // 未在列表里出现过的名字（如刚删掉的出站仍留在历史流量里）走哈希兜底
    return `var(--outbound-${(hash(String(name)) % SLOTS) + 1})`;
  };
}
```

- [ ] **Step 3: 运行测试**

Run: `cd ui && npx vitest run palette`
Expected: 6 个测试全部 PASS

- [ ] **Step 4: 组装 App.svelte**

把三个视图 + 设置覆盖层接到状态条与 segmented 导航上。

**这一步是本阶段与阶段 4 的接缝，三处落差都在这里收口**（见「阶段 4 交接假设」）：

1. `traffic` 事件只有总量 → 逐流明细靠 `FlowStore` 从 `connection` 事件聚合
2. 无逐流字节 → `byteMode` 如实降级为「按连接数」
3. 一半命令是 `not_ready` 占位 → `safeInvoke` 吞掉未就绪错误，视图照常渲染

`ui/src/App.svelte`（整个替换阶段 4 的骨架）：

```svelte
<script>
  /**
   * 控制窗口外壳（spec §11.3）。
   *
   *   ┌─ 状态条（常驻）─ 状态点 · 出站数 · 活跃连接 · sparkline · 速率 ─┐
   *   ├──────────────────────────────────────────────────────────────┤
   *   │  [ 流量 ]   规则   出站                              ⚙        │
   *   ├──────────────────────────────────────────────────────────────┤
   *   │   当前视图                                                    │
   *   └──────────────────────────────────────────────────────────────┘
   *
   * 流量是默认视图（打开即见走向）；设置走覆盖层而非第四个标签。
   */
  import './tokens.css';
  import Segmented from './lib/Segmented.svelte';
  import TrafficView from './views/TrafficView.svelte';
  import RulesView from './views/RulesView.svelte';
  import OutboundsView from './views/OutboundsView.svelte';
  import SettingsOverlay from './views/SettingsOverlay.svelte';
  import { FlowStore } from './lib/flows.js';
  import { makePalette } from './lib/palette.js';
  import { bytes, count } from './lib/format.js';
  import { invoke, listen } from './lib/ipc.js';

  let view = $state('traffic');
  let settingsOpen = $state(false);
  let win = $state('1h');

  let status = $state({ connected: false, active: 0, downRate: 0, upRate: 0 });
  let spark = $state([]);
  let rules = $state([]);
  let outbounds = $state([]);
  let config = $state(null);
  let probe = $state(null);
  /** §6.4 / §12：出站不可用等故障要在 UI 上报警，绝不静默 */
  let alerts = $state([]);

  // 逐流明细在前端聚合（见 Task 3）。用 $state 包一个版本号来触发重算 ——
  // FlowStore 是普通对象，改它内部不会让 $derived 失效。
  const store = new FlowStore();
  let flowVersion = $state(0);
  const flows = $derived((flowVersion, store.rows()));

  /**
   * 阶段 4 的命令面里有一半是 not_ready 占位（rule_test 等阶段 1 接入、
   * outbound_latency_probe 等阶段 2）。视图必须能在命令未就绪时正常渲染 ——
   * 显示占位与提示，而不是白屏或抛异常。
   */
  async function safeInvoke(cmd, args) {
    try {
      return await invoke(cmd, args);
    } catch (e) {
      const msg = String(e?.message ?? e);
      // not_ready 是预期状态，不当作错误报警
      if (!/未就绪|not.?ready/i.test(msg)) {
        alerts = [...alerts, { message: `${cmd} 失败：${msg}` }].slice(-5);
      }
      return null;
    }
  }

  // 事件已在 Rust 侧聚合节流（§11.2）：traffic 1s、connection 200ms、
  // rule-hit 1s 增量。前端**不再**二次节流，那只会叠加延迟。
  $effect(() => {
    const pending = [
      listen('traffic', (e) => {
        const t = e.payload;
        status = { ...status, active: t.active, downRate: t.down_rate, upRate: t.up_rate };
        spark = [...spark, t.down_rate].slice(-40);
      }),
      listen('connection', (e) => {
        store.apply(e.payload.items ?? []);
        flowVersion++;
        if (e.payload.dropped) {
          // 溢出要说出来 —— 悄悄丢数据会让用户对着一张不准的图排查
          alerts = [...alerts, { message: '连接事件溢出，部分流量未计入统计。' }].slice(-5);
        }
      }),
      listen('rule-hit', (e) => {
        // 增量计数，键是规则字符串（阶段 4 events.rs 的 hit_delta）
        const d = e.payload ?? {};
        rules = rules.map((r) => (d[r.key] ? { ...r, hits: r.hits + d[r.key] } : r));
      }),
      listen('status', (e) => (status = { ...status, ...e.payload })),
      listen('outbound-state', (e) => (outbounds = e.payload ?? [])),
    ];
    return () => pending.forEach((p) => p.then((un) => un()));
  });

  $effect(() => {
    loadConfig();
    // 窗口刚开时补齐历史，不必等下一个 1s tick
    safeInvoke('traffic_snapshot').then((t) => {
      if (t) status = { ...status, active: t.active };
    });
  });

  async function loadConfig() {
    const c = await safeInvoke('config_get');
    if (!c) return;
    config = c;
    // config_get 返回的是配置 JSON，规则是字符串数组。
    // hits 初始为 0，靠 rule-hit 事件累加 —— 首次打开时热度全灰是预期的。
    const names = new Set((c.proxies ?? []).map((p) => p.name));
    rules = (c.rules ?? []).map((line, i) => {
      const [type, ...rest] = String(line).split(',').map((x) => x.trim());
      const t = type.toUpperCase();
      const isMatch = t === 'MATCH' || t === 'FINAL';
      const target = isMatch ? rest[0] : rest[1];
      return {
        id: i,
        key: line,                       // rule-hit 事件的键
        type: t.toLowerCase().replace('domain-', ''),
        value: isMatch ? '*' : rest[0],
        target,
        hits: 0,
        enabled: true,
        unknownOutbound: target && !['DIRECT', 'REJECT'].includes(target) && !names.has(target),
      };
    });
  }

  const colorOf = $derived(makePalette(outbounds.map((o) => o.name)));

  async function runProbe(target, { resolve }) {
    if (!target) { probe = null; return; }
    const r = await safeInvoke('rule_test', { target, resolve });
    // 未就绪时给一个明确的「不可用」而非假装没试算
    probe = r
      ? { index: r.index + 1, decision: r.decision, outbound: r.decision,
          tried: r.tried, needResolve: r.resolved === null && !resolve }
      : null;
  }

  /** 规则排序/启停走 config_save 写回 —— 阶段 4 没有专门的命令 */
  async function saveRules(next) {
    rules = next;
    await safeInvoke('config_save', {
      config: { ...config, rules: next.filter((r) => r.enabled).map((r) => r.key) },
    });
  }

  function reorder(from, to) {
    const out = [...rules];
    const [x] = out.splice(from, 1);
    out.splice(to, 0, x);
    saveRules(out);
  }

  function toggleRule(id, enabled) {
    saveRules(rules.map((r) => (r.id === id ? { ...r, enabled } : r)));
  }

  function pickOutbound(name) {
    // §11.6：点击出站节点 → 跳到规则视图
    view = 'rules';
    probe = null;
  }
</script>

<div class="app">
  <!-- 状态条常驻 -->
  <div class="status" role="status" aria-live="polite">
    <span class="dot" class:off={!status.connected} aria-hidden="true"></span>
    <span class="st-label">{status.connected ? '已连接' : '未连接'}</span>
    <span class="st-meta mono">
      {count(outbounds.filter((o) => o.enabled).length)} 出站 ·
      {count(status.active)} 活跃连接
    </span>

    <!-- sparkline 是趋势提示，真实数值在右侧 —— 对屏幕阅读器隐藏 -->
    <div class="spark" aria-hidden="true">
      {#each spark as v, i (i)}
        <i style:height="{Math.max(1, Math.min(22, Math.log10(v + 1) * 3.4))}px"></i>
      {/each}
    </div>
    <span class="rate mono">↓ {bytes(status.downRate)}/s</span>
  </div>

  <!-- segmented 导航，不是左侧图标栏 -->
  <div class="nav">
    <Segmented label="视图"
      options={[
        { value: 'traffic',   label: '流量' },
        { value: 'rules',     label: '规则' },
        { value: 'outbounds', label: '出站' },
      ]}
      value={view} onchange={(v) => (view = v)} />
    <button type="button" class="gear" aria-label="打开设置"
            onclick={() => (settingsOpen = true)}>⚙</button>
  </div>

  {#if alerts.length}
    <div class="alerts" role="alert">
      {#each alerts as a, i (i)}<p>{a.message}</p>{/each}
    </div>
  {/if}

  <main>
    {#if view === 'traffic'}
      <TrafficView {flows} {colorOf} window={win}
                   hasBytes={store.hasBytes()}
                   onWindowChange={(v) => (win = v)}
                   onPickOutbound={pickOutbound} />
    {:else if view === 'rules'}
      <RulesView {rules} {colorOf} {probe} ontest={runProbe}
                 onreorder={reorder} ontoggle={toggleRule} />
    {:else}
      <OutboundsView {outbounds} {colorOf}
                     ontoggle={(id, on) => safeInvoke('outbound_enable', { id, enabled: on })}
                     onprobe={(id) => safeInvoke('outbound_latency_probe', { id })} />
    {/if}
  </main>

  <SettingsOverlay open={settingsOpen} config={config ?? {}}
                   onclose={() => (settingsOpen = false)}
                   onsave={(c) => safeInvoke('config_save', { config: c })
                     .then(() => { settingsOpen = false; loadConfig(); })}
                   onexport={() => safeInvoke('config_get_raw')} />
</div>

<style>
  .app { display: flex; flex-direction: column; height: 100vh; }

  .status {
    display: flex;
    align-items: center;
    gap: var(--space-5);
    padding: var(--space-3) var(--space-4);
    border-bottom: 1px solid var(--border);
    background: var(--surface-0);
    flex: none;
  }
  .dot {
    width: 7px; height: 7px;
    border-radius: 50%;
    background: var(--state-live);
    box-shadow: 0 0 0 3px rgba(63, 178, 127, .14);
    flex: none;
  }
  .dot.off { background: var(--text-4); box-shadow: 0 0 0 3px rgba(255, 255, 255, .05); }

  .st-label { font-size: var(--fs-13); font-weight: var(--fw-medium); }
  .st-meta  { font-size: var(--fs-12); color: var(--text-3); }

  .spark { margin-left: auto; display: flex; align-items: flex-end; gap: 2px; height: 22px; }
  .spark i { width: 3px; background: var(--text-4); border-radius: 1px; display: block; }

  .rate { font-size: var(--fs-12); color: var(--text-2); min-width: 92px; text-align: right; }

  .nav {
    display: flex;
    align-items: center;
    padding: var(--space-2) var(--space-4);
    border-bottom: 1px solid var(--border);
    background: var(--surface-0);
    flex: none;
  }
  .gear {
    all: unset;
    margin-left: auto;
    padding: 2px var(--space-2);
    color: var(--text-3);
    cursor: pointer;
    font-size: var(--fs-14);
  }
  .gear:hover { color: var(--text-1); }

  .alerts {
    padding: var(--space-2) var(--space-4);
    background: rgba(209, 89, 92, .1);
    border-bottom: 1px solid var(--state-fail);
    flex: none;
  }
  .alerts p { margin: 0; font-size: var(--fs-12); color: var(--state-fail); }

  main { flex: 1; overflow-y: auto; }
</style>
```

> **`const flows = $derived((flowVersion, store.rows()));` 里的逗号不是笔误。**
> 逗号表达式让 `flowVersion` 被读到（从而建立依赖），值取 `store.rows()`。
> `FlowStore` 是普通类实例，改它内部不会触发 Svelte 的响应式 ——
> 必须靠一个 `$state` 版本号显式驱动。若嫌隐晦，写成
> `{ flowVersion; return store.rows(); }` 的 `$derived.by` 形式亦可。

- [ ] **Step 5: 按连接数 / 按字节的诚实降级**

`TrafficView` 要接收 `hasBytes` 并据此调整。在 `TrafficView.svelte` 的 props 里加：

```svelte
  let {
    flows = [],
    colorOf,
    hasBytes = false,          // ← 新增
    window: win = '1h',
    onWindowChange = () => {},
    onPickOutbound = () => {},
  } = $props();

  // 无逐流字节时一律按连接数，并如实告知 —— 绝不拿连接数假装成字节数
  const metric = $derived(hasBytes ? 'bytes' : 'conns');
  const rows = $derived(
    prepareFlows(flows).map((r) => ({ ...r, bytes: metric === 'bytes' ? r.bytes : r.conns }))
  );
```

并在工具条右侧显示当前口径：

```svelte
      <span class="total mono">
        {metric === 'bytes' ? bytes(total) : `${count(total)} 连接`}
      </span>
      {#if !hasBytes}
        <span class="hint-inline" title="逐流字节统计将在出站管理器就绪后可用">按连接数</span>
      {/if}
```

**这一条是诚实底线**：一个显示「2.41 GB」而实际是连接数的界面，比不显示更糟 ——
用户会据此做判断，而判断的基础是假的。

- [ ] **Step 6: 提交**

```bash
git add ui/src/lib/palette.js ui/src/lib/palette.test.js ui/src/App.svelte
git commit -m "feat(ui): 出站色码全局分配与视图组装"
```

---

### Task 17: 无障碍审计

**Files:**
- Create: `ui/src/a11y.test.js`

单个组件的测试已经覆盖了各自的无障碍要求。这一 task 做的是**跨组件的整体审计** —— 那些只在组装之后才暴露的问题。

- [ ] **Step 1: 装 axe 并写审计测试**

```bash
cd ui && npm i -D axe-core vitest-axe
```

`ui/src/a11y.test.js`：

```js
/**
 * 跨组件无障碍审计。
 *
 * 单组件测试守各自的契约，这里守的是组装后才暴露的问题：
 * 对比度、重复 id、缺失的可访问名、颜色是唯一信息载体等。
 *
 * spec §11.6 记录桑基图的无障碍评级为 C —— 因此表视图是必需的等价视图。
 * 这一组测试是那条纪律的守门人。
 */
import { describe, it, expect } from 'vitest';
import { render } from '@testing-library/svelte';
import { axe } from 'vitest-axe';

import FlowTable from './views/FlowTable.svelte';
import RulesView from './views/RulesView.svelte';
import OutboundsView from './views/OutboundsView.svelte';
import SettingsOverlay from './views/SettingsOverlay.svelte';

const colorOf = () => '#5b8ff9';
const noop = () => {};

const flows = [
  { site: 'a.com', rule: 'final *', outbound: 'JP', bytes: 10, conns: 3 },
  { site: 'b.com', rule: 'geosite cn', outbound: 'DIRECT', bytes: 90, conns: 1 },
];
const rules = [
  { id: 1, type: 'geosite', value: 'cn', target: 'DIRECT', hits: 42663, enabled: true },
  { id: 2, type: 'final', value: '*', target: 'JP', hits: 88120, enabled: true },
];
const outbounds = [
  { id: 'a', name: 'JP', state: 'live', latency: 38, sessions: 4, enabled: true, host: true },
];

const cases = [
  ['流量表', FlowTable, { rows: flows, colorOf }],
  ['规则视图', RulesView, { rules, colorOf, probe: null, ontest: noop, onreorder: noop, ontoggle: noop }],
  ['出站列表', OutboundsView, { outbounds, colorOf, ontoggle: noop, onprobe: noop, onadd: noop }],
  ['设置覆盖层', SettingsOverlay, { open: true, config: { mixedPort: 7890 }, onclose: noop, onsave: noop, onexport: noop }],
];

describe('无障碍审计', () => {
  for (const [name, Comp, props] of cases) {
    it(`${name} 无 axe 违规`, async () => {
      const { container } = render(Comp, props);
      const r = await axe(container);
      const violations = r.violations ?? [];
      if (violations.length) {
        // 报告要能直接看懂是哪条规则、哪个元素
        const msg = violations
          .map((v) => `${v.id}: ${v.help}\n  ${v.nodes.map((n) => n.html).join('\n  ')}`)
          .join('\n');
        throw new Error(`${name} 有 ${violations.length} 条无障碍违规：\n${msg}`);
      }
      expect(violations).toHaveLength(0);
    });
  }

  it('每个可交互元素都有可访问名字', async () => {
    for (const [name, Comp, props] of cases) {
      const { container, unmount } = render(Comp, props);
      const els = container.querySelectorAll('button, input, select, [role="switch"], [role="radio"]');
      for (const el of els) {
        const named =
          el.getAttribute('aria-label') ||
          el.getAttribute('aria-labelledby') ||
          el.textContent.trim() ||
          (el.id && container.querySelector(`label[for="${el.id}"]`));
        expect(named, `${name} 里有个没名字的元素：${el.outerHTML.slice(0, 90)}`).toBeTruthy();
      }
      unmount();
    }
  });

  it('色块一律 aria-hidden —— 颜色绝不是唯一的信息载体', () => {
    for (const [name, Comp, props] of cases) {
      const { container, unmount } = render(Comp, props);
      for (const chip of container.querySelectorAll('.chip')) {
        expect(chip.getAttribute('aria-hidden'), `${name} 的色块没有 aria-hidden`).toBe('true');
        // 色块旁边必须有文字，否则这一格的信息只存在于颜色里
        expect(chip.parentElement.textContent.trim().length).toBeGreaterThan(0);
      }
      unmount();
    }
  });
});
```

- [ ] **Step 2: 运行审计**

Run: `cd ui && npx vitest run a11y`
Expected: 全部 PASS。

**若有违规，修组件而不是放宽断言。** 常见的几类与修法：

| axe 规则 | 含义 | 修法 |
|---|---|---|
| `button-name` | 按钮没有可访问名字 | 加 `aria-label`；纯图标按钮尤其容易漏 |
| `color-contrast` | 对比度不足 | 提一级文本色（`--ink-4` → `--ink-3`）。**不要**调令牌值 |
| `duplicate-id` | id 重复 | 表单控件 id 要带组件前缀 |
| `label` | 表单控件无 label | 补 `<label for>` 或 `aria-label` |
| `aria-required-children` | role 用错了 | 检查 `radiogroup`/`switch` 的结构 |

> **`color-contrast` 的特别说明**：`--ink-4`（#4d545b）在 `--panel`（#1c1f23）上的对比度约 2.6:1，
> 低于 WCAG AA 的 4.5:1。它在 mockup 里用于**次要元数据**（聚合行标签、单位、hint 文本）。
> 处置方式：这类文本必须**始终有一份同等信息以更高对比度呈现**（如表格里的数值列本身），
> 或改用 `--ink-3`（#6f777f，约 4.0:1）。**不要为了过测试而改动令牌值** —— 令牌是 spec 定死的，
> 该改的是「哪些内容配用哪一级文本色」。

- [ ] **Step 3: 键盘遍历手工验收**

自动化审计查不出「Tab 顺序合不合理」。`npm run dev`，**拔掉鼠标**走一遍：

| 检查项 | 期望 |
|---|---|
| Tab 顺序 | 状态条 → 导航 → 齿轮 → 主视图内容，与视觉顺序一致 |
| 焦点可见 | 每一站都有清晰的焦点环，**没有任何地方 outline:none** |
| segmented | Tab 进入后用**左右方向键**切换，不是 Tab 逐个走 |
| 规则排序 | Tab 到把手，Alt+↓ 能移动，**焦点跟随移动后的行** |
| 桑基节点 | 可 Tab 到，Enter 激活，Esc 取消锁定 |
| 设置覆盖层 | 焦点被困在层内，Esc 关闭且焦点归位 |
| 全程 | **不需要鼠标就能完成**：切视图、试算、改规则顺序、开关出站、改设置 |

- [ ] **Step 4: 屏幕阅读器抽查**

macOS 上 `Cmd+F5` 开 VoiceOver，至少确认三条：

| 检查项 | 期望播报 |
|---|---|
| 进入流量表 | 「表格，流量明细：共 N 条…」，能用 `Ctrl+Opt+方向键` 逐格遍历 |
| 桑基图 | 播报 aria-label 里的摘要，并提示「完整数据请切换到表视图」 |
| 探针试算后 | 判决结果被 `aria-live` 自动播报，无需手动导航过去 |

**桑基图这条是 §11.6 的核心论断的验证**：图本身评级 C，所以它的职责只是给出摘要 + 指向表视图。若 VoiceOver 在图上什么都读不出来，说明 `role="img"` 或 `aria-label` 没生效。

- [ ] **Step 5: 提交**

```bash
git add ui/package.json ui/package-lock.json ui/src/a11y.test.js
git commit -m "test(ui): 跨组件无障碍审计（axe + 可访问名 + 色块非唯一载体）"
```

---

### Task 18: 全量验收

**Files:** 无新增

- [ ] **Step 1: 跑全部测试**

```bash
cd ui && npx vitest run
```

Expected: 全绿。各 task 的测试数合计约 **175 个**：

| 模块 | 测试数 | Task |
|---|---|---|
| tokens（令牌契约） | 37 | 1 |
| format | 12 | 2 |
| flows | 16 | 3 |
| aggregate | 11 | 4 |
| sankey-layout | 13 | 5 |
| trace | 7 | 6 |
| FlowTable | 11 | 8 |
| heat | 8 | 10 |
| reorder | 12 | 11 |
| Probe | 8 | 12 |
| RulesView | 12 | 13 |
| OutboundsView | 10 | 14 |
| SettingsOverlay | 10 | 15 |
| palette | 7 | 16 |
| a11y | 7 | 17 |

- [ ] **Step 2: 构建**

```bash
cd ui && npx vite build
```

Expected: 构建成功。**核对产物体积** —— §11.6 预算是 d3 约 10KB：

```bash
ls -la ui/dist/assets/*.js
```

参考值：实测 Svelte 5 + d3-sankey + d3-shape 的最小可用桑基图产物约 **51 KB（gzip 19 KB）**，其中 d3 部分约 2.5 KB（gzip）。若产物显著超过 150 KB，检查是不是误引了完整的 `d3` 包而非 `d3-sankey` / `d3-shape` 两个子包。

- [ ] **Step 3: 在真实 Tauri 窗口里跑一遍**

前面的验证都在浏览器里。控制窗口最终跑在 **Tauri 2 的 WKWebView** 里，有几件事只有在那里才能发现：

```bash
npm run tauri dev
```

| 检查项 | 期望 | 出问题时 |
|---|---|---|
| 窗口尺寸 | 默认 960×640，最小 720×480（§11.3） | 检查 `tauri.conf.json` 的窗口配置 |
| `mix-blend-mode: screen` | 流带重叠处自然叠加 | WKWebView 对 blend mode 支持良好，若异常检查父元素是否创建了不该有的层叠上下文 |
| IBM Plex 字体 | 正确加载 | **字体必须本地打包**，控制窗口是 `local: true` 的 capability，不该去 Google Fonts 拉字体 |
| `prefers-reduced-motion` | 系统设置改变后生效 | Svelte 的 `prefersReducedMotion` 是 `MediaQuery`，会实时响应 |
| SVG 渐变 | 中文出站的流带**有颜色** | 见 Task 5 的 `gradientId` |

> **字体这条要特别处理。** mockup 用的是 Google Fonts CDN，那只是演示。控制窗口必须
> **本地打包 IBM Plex Sans / Mono**（`npm i @fontsource/ibm-plex-sans @fontsource/ibm-plex-mono`
> 并在 `tokens.css` 里 `@import`），理由有二：一是 §11.1 的控制 capability 是 `local: true`
> 且**无 remote 字段**，外部请求本就不该发生；二是这是个网络工具，界面自己去连外网既讽刺又
> 会在断网时掉字体。

- [ ] **Step 4: 逐条对照 mockup**

把 `design-direction.html` 与 `sankey.html` 在浏览器里打开，与运行中的应用**并排对照**：

| 视图 | 对照点 |
|---|---|
| 规则视图 | 行高 32px · 类型列无彩色 · 热度染色梯度 · 探针激活时的升亮/降噪 |
| 出站视图 | 行高 38px · 色码 6px · 延迟与会话数右对齐成列 · 「宿主」标注 |
| 流量视图 | 三列位置 · 只有出站节点满色 · 渐变从左到右 · 标签描边可读 |
| 表视图 | 列头小写大写间距 · 数值列右对齐 · 色块 + 文字 |
| 色板 | 三级表面明度差 4–7% · 状态色 · 出站色码 |

- [ ] **Step 5: 反 AI 塑料感自查**

逐条确认**没有**出现：

- [ ] 卡片左侧粗色装饰边框
- [ ] 大面积渐变（桑基流带的语义渐变除外）
- [ ] 无意义留白撑场面（每个 padding 都能说出理由）
- [ ] 彩色 pill 标签
- [ ] 左侧图标导航栏
- [ ] 圆形延迟指示器 / 服务器卡片网格
- [ ] 居中的巨型空状态插画
- [ ] 出站色码之外的装饰性颜色

**判据是一句话：屏幕上每一处彩色，都必须能回答「这个颜色在说什么」。** 答不上来的就是装饰，删掉。

---

## 阶段 5 完成标准

全部勾选后本阶段才算完成：

- [ ] `cd ui && npx vitest run` 全绿（约 175 个测试）
- [ ] `npx vite build` 成功，产物无异常膨胀（参考：约 51 KB / gzip 19 KB，不含字体）
- [ ] **表视图的 11 个测试全部真过** —— 这是无障碍死线，不接受放宽断言
- [ ] **没有在阶段 5 新定义任何设计令牌** —— `grep -n '^\s*--' ui/src/*.css ui/src/**/*.svelte` 应只在 `tokens.css` 有命中
- [ ] 令牌契约测试通过；`.sr-only` 已补进**阶段 4 的** `tokens.css`
- [ ] 出站色码轮转基数是 **8**，与 `tokens.css` 的 `--outbound-1..8` 对齐
- [ ] 阶段 4 的命令报 `not_ready` 时视图仍正常渲染（`safeInvoke` 生效），不白屏不抛异常
- [ ] 无逐流字节时 UI 显示「按连接数」，**没有拿连接数假装成字节数**
- [ ] axe 审计零违规；`color-contrast` 若有违规，是调整「哪些内容用哪级文本色」而**不是**改动 `tokens.css` 的令牌值
- [ ] **拔掉鼠标能完成全部核心操作**：切视图、试算、改规则顺序、开关出站、改设置
- [ ] 桑基图在 VoiceOver 下能播报摘要并指向表视图
- [ ] 桑基图连续更新数据时**节点顺序不变**，只有流带宽度平滑变化
- [ ] `prefers-reduced-motion` 开启时直接跳变，无过渡
- [ ] 空状态**不画空的坐标骨架**；流数少于 3 时自动降级为表
- [ ] 中文出站名的流带**有颜色**（渐变 id 的坑已避开）
- [ ] 逐条对照过两份 mockup
- [ ] 反 AI 塑料感自查的 8 条全部通过
- [ ] 字体已本地打包，运行时**无任何外部网络请求**

---

## 与 spec 的偏差记录

实现过程中发现的、需要回写 spec 的几点：

**① §11.6 说「只引入 `d3-sankey` + `d3-shape`（约 10KB）」，实测更小。**
`d3-sankey` 0.12.3 + 其依赖（`d3-shape` 1.3.7 嵌套 + `d3-array` 2.x）在 tree-shake 后约 2.5 KB（gzip）。
预算宽裕，无需为体积做任何妥协。

**② §11.6 的「仅对流带宽度做 200ms ease-out 插值」需要更精确的表述。**
若字面理解为「只插值 stroke-width」，节点高度与贝塞尔路径会瞬跳，形状被撕开。
正确落地是**插值字节数并每帧重算布局**（实测 0.065 ms/次，占 60fps 预算 0.4%），
这样宽度、节点高度、路径三者同步变化。**结果与 spec 的意图一致，实现路径不同。**
建议把 §11.6 的这一行改为「对流量值做 200ms ease-out 插值，逐帧重算布局」。

**③ §11.4 的 `--ink-4`（#4d545b）在 panel 上对比度约 2.6:1，低于 WCAG AA。**
mockup 里它用于次要元数据。本计划的处置是：这类文本必须始终有一份同等信息以更高对比度呈现，
或改用 `--ink-3`。**没有改动令牌值** —— 令牌是 spec 定死的。
但 spec 应当记录这个已知取舍，否则下一个人会以为是疏忽。

**④ §11.6 未规定「hover 高亮」的精确范围。**
「目标路径保持原样」里的「路径」必须按双向可达闭包算，只看相邻会让路径断成半截（实测）。
建议在 spec 中明确。

**⑤ §11.5 未规定命中热度的归一化刻度。**
线性刻度在真实数据（跨三个数量级）下会让 signature 失效。本计划采用对数刻度。
建议 spec 补一句。

---

## 与阶段 4 的接缝（写计划时发现，实现前必读）

阶段 4 与阶段 5 是并行写的，核对后发现三处需要显式处理的落差。
**这些不是谁写错了**，而是阶段 4 只做到「计数与节流」，逐流明细依赖阶段 2 的出站管理器。

**① 设计令牌的唯一定义处是阶段 4 的 `ui/src/tokens.css`。**
本计划的早期草稿曾自带一套令牌（`--canvas` / `--ink` / `--out-N`），与阶段 4 的
（`--surface-0` / `--text-1` / `--outbound-N`）**撞名不撞值**。已全部改为消费阶段 4 的名字。
纪律：**阶段 5 只消费，不定义、不改名、不新增。** Task 1 的令牌契约测试守这条线。

唯一需要补的是 `.sr-only`（无障碍兜底依赖它）—— **补在阶段 4 的 `tokens.css` 里**，
不要在阶段 5 就地新增。

**② 出站色码是 8 个，不是 3 个或 6 个。**
mockup 只画了 3 个，本计划早期草稿按 6 个设计，阶段 4 实际给了 8 个（`--outbound-1..8`）。
**采用 8 个** —— 轮转的取模基数必须与 `tokens.css` 对齐，否则会生成不存在的变量名，
表现为色块透明（CSS 变量未定义时 `background: var(--outbound-9)` 静默失效）。

**③ 逐流明细在阶段 4 不存在，需前端聚合。**

| 视图需要 | 阶段 4 有什么 | 处置 |
|---|---|---|
| `{ site, rule, outbound, bytes, conns }[]` | `TrafficSample` 只有总量 | Task 3 的 `FlowStore` 从 `connection` 事件聚合 |
| 逐流字节 | `ConnectionDelta` 无 `bytes` 字段 | 降级为按连接数，**并在 UI 上如实标注** |
| 命中的规则 | `ConnectionDelta` 无 `rule` 字段 | 用占位符 `（规则未记录）`，桑基中间层不留空节点 |
| `rule_reorder` / `rule_enable` 命令 | **不存在** | 走 `config_save` 写回整份规则数组 |
| 规则的 `hits` | `rule-hit` 事件是增量，键是规则字符串 | 初始 0，靠事件累加。首次打开热度全灰是预期的 |

**建议回写阶段 2 的计划**：给 `ConnectionDelta` 补上 `bytes` 与 `rule` 两个字段。
两者都是路由判决时就已知的信息，在事件里带上几乎零成本，
而少了它们，流量视图的核心指标（字节）与中间层（规则）都只能降级。
本阶段的 `FlowStore` 已经预留了这两个字段的读取路径，阶段 2 补上后**无需改动前端**。

---


## 交给后续阶段的接口

阶段 6（TUN）与后续迭代会用到的约定：

```js
// 流水行（traffic 事件的 payload.flows）
{ site: string, rule: string, outbound: string, bytes: number, conns: number }

// 规则行（config_get 的 rules）
{ id: number, type: string, value: string, target: string,
  hits: number, enabled: boolean, unknownOutbound?: boolean }

// 出站行（outbound-state 事件）
{ id: string, name: string, enabled: boolean, host?: boolean,
  state: 'live' | 'starting' | 'reconnecting' | 'failed' | 'stopped',
  latency: number | null, sessions: number }

// 探针结果（rule_test 的返回）
{ index: number, decision: 'Outbound'|'Direct'|'Reject'|null,
  outbound?: string, tried: number, needResolve: boolean }
```

**已知的未尽事项**（本阶段不做）：

- **规则的新增与编辑表单**。本阶段做的是排序、启用/停用与试算 —— 这三个足以支撑排查主场。
  新增/编辑需要一个规则类型感知的表单（CIDR 校验、GEOSITE 类别补全），单独成 task 更合适
- **原始 YAML 编辑器**（`config_get_raw` / `config_save_raw` 的 UI）。逃生舱，等有人真的需要时再做
- **托盘菜单**（§11.3 的「托盘常驻」）。属于阶段 4 的窗口/托盘范畴，不是视图
- **时间窗口切换的后端接线**。UI 已备好 `onWindowChange`，`traffic_snapshot(window)` 命令的实现在阶段 4
- **桑基图的点击锁定跳转筛选**。`onPickOutbound` 已接到「切到规则视图」，
  但「筛出该出站的规则」需要 RulesView 支持 filter prop，留到规则编辑那一批一起做






