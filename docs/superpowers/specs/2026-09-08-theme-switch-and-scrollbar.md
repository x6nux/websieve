# 主题切换 + 滚动条覆盖 设计文档

## 背景

真机走查时用户提出两点：

1. 界面需要一个主题切换功能。
2. 当前深色主题下，很多可滚动区域（首页右侧、各表单弹窗内部）出现的是
   系统默认滚动条，在深色背景下显得刺眼发白，视觉上没被主题覆盖到。

与用户确认后的范围：

- 主题分三档：**深色 / 浅色 / 跟随系统**。
- 入口放在设置面板（齿轮图标覆盖层 `SettingsOverlay.svelte`）里。
- 用户的选择要被记住，但**不写入 `config.yaml`**——这是「这台设备上这个人
  怎么看界面」的展示偏好，不是代理配置，与 `client-priv` 那类必须原样
  保护的字段完全是两回事，混进配置文件反而是把不相关的东西耦合在一起。
  存 `localStorage`。
- 滚动条改用 CSS 变量驱动的自定义样式，让它在两个主题下都正确跟随。

不在这次范围内（明确排除，避免范围蔓延）：

- 运行时把 `config.yaml` 接进真实代理进程——那是另一个独立、量级大得多
  的工程（已调研清楚，另开一轮设计）。
- 出站色码（`--outbound-1..8`）与状态色（`--state-live/warn/fail/direct`）
  不重新设计——它们本来就是高饱和的点缀色，深浅两个中性色背景下都压得住，
  两个主题共用同一份数值。
- 不做「自定义主题」「跟随出站色反推整体色相」这类更复杂的可定制方案。

## 架构

### 1. `tokens.css`：新增浅色变量集，深色留作默认兜底

现状：所有变量定义在 `:root` 里，只有一份深色值。

改法：

- **`:root` 保留原有深色数值不变**，作为「没有任何 `data-theme` 属性时」
  的默认外观——这样在 JS 尚未运行的极端情况下（理论上不会发生，但作为
  兜底）界面仍然是今天这个正确的深色样子，不会有一瞬间的无样式内容。
- 新增 `:root[data-theme='light'] { ... }`，覆盖需要反转的变量：
  `--surface-0/1/2`、`--border`、`--border-strong`、`--text-1/2/3/4`、
  `--heat-min/--heat-max`、`--shadow-overlay`。
- 新增 `:root[data-theme='dark'] { ... }`，内容与 `:root` 默认值完全一致
  （显式声明而非只依赖无属性兜底，方便测试断言「深色档确实存在」，也让
  三态里的两态都有据可查，不靠隐含状态）。
- **不覆盖**：`--outbound-1..8`、`--state-*`、字体/字号/字重/间距/圆角/
  行高这些与明暗无关的 token，两个主题共用 `:root` 里的原值。

具体浅色数值（沿用深色那一套「表面明度分级 + 四级文字色」的设计语言，
只是把明暗关系反过来）：

```css
:root[data-theme='light'] {
  --surface-0: #f4f5f6; /* 画布 */
  --surface-1: #ffffff; /* 行/面板 */
  --surface-2: #e8eaec; /* 悬浮/选中 */

  --border: rgba(0, 0, 0, 0.08);
  --border-strong: rgba(0, 0, 0, 0.15);

  --text-1: #1b1d1f; /* 主要 */
  --text-2: #565d64; /* 次要 */
  --text-3: #868d94; /* 弱化 */
  --text-4: #aeb4ba; /* 极弱（占位、禁用） */

  --heat-min: rgba(0, 0, 0, 0.02);
  --heat-max: rgba(0, 0, 0, 0.07);

  --shadow-overlay: 0 8px 32px rgba(0, 0, 0, 0.18);
}
```

### 2. 新文件 `ui/src/lib/theme.js`：偏好读写 + 应用 + 跟随系统

```js
const KEY = 'websieve:theme';
const VALID = ['dark', 'light', 'system'];

export function getThemePreference() { /* 读 localStorage，非法值/未设置时回退 'system' */ }
export function setThemePreference(pref) { /* 写 localStorage + 立即 applyTheme() */ }
function resolveSystemTheme() { /* matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light' */ }
function applyTheme(pref) { /* 计算 resolved(dark/light)，设置 document.documentElement.dataset.theme */ }
export function initTheme() {
  /* 应用一次当前偏好；若偏好是 'system'，订阅 matchMedia 的 change 事件，
     系统切换时实时重新 applyTheme —— 订阅只在偏好是 'system' 时才需要，
     显式选了 dark/light 之后系统怎么变都不该影响界面，否则「选了深色」
     形同虚设 */
}
```

- `initTheme()` 在 `ui/src/main.js` 里、`mount(App, ...)` 之前调用一次。
- `localStorage` 不可用（比如未来某天在更严格的 webview 策略下被禁）时
  不能让整个应用崩掉——读写都要 try/catch，失败就按 `'system'` 处理，
  不影响主功能。这与仓库「房规」里「配置字段的默认值让功能可用而非报错
  拦路」是同一条纪律，只是这次用在偏好存储上。

### 3. `SettingsOverlay.svelte`：新增「外观」区块

放在 `<h2>设置</h2>` 之后、「入口」区块之前（展示偏好，读者第一眼该看到，
不该埋在端口号下面）。

```svelte
<section>
  <h3>外观</h3>
  <Segmented
    label="主题"
    options={[
      { value: 'dark', label: '深色' },
      { value: 'light', label: '浅色' },
      { value: 'system', label: '跟随系统' },
    ]}
    value={themePref}
    onchange={(v) => (themePref = setThemePreference(v))} />
</section>
```

**这一块不走 `draft`/`onsave` 那条路**——它不是 `config.yaml` 的字段，
点选即生效、不需要点底部「保存」按钮，也不受「取消」影响（取消关闭的是
端口/局域网/承载方式那些草稿字段，主题是显示偏好，用户点了就是选定了，
两者的「反悔」语义本来就不同，硬凑一起是把两种不同生命周期的状态混为
一谈）。`themePref` 是组件本地 `$state`，初值用 `untrack(() => getThemePreference())`
取一次（与本组件已有的 `draft` 初值同一套理由，避免打开设置面板时把
父组件重渲染误判成"偏好变了"）。

`setThemePreference` 返回应用后的偏好值（即传入的 `pref`，用于让
`themePref = setThemePreference(v)` 这一行同时完成"写存储"与"更新本地
显示"两件事，不必分两步）。

### 4. 滚动条样式（`tokens.css` 末尾新增）

WebView2（本项目 Windows 上的承载引擎）是 Chromium 内核，同时现代 Chromium
也已支持标准的 `scrollbar-color`/`scrollbar-width`——两套都写，互不冲突，
覆盖面更广（未来若某个平台的 webview 内核只认其中一套也不受影响）：

```css
* {
  scrollbar-width: thin;
  scrollbar-color: var(--border-strong) transparent;
}

::-webkit-scrollbar { width: 10px; height: 10px; }
::-webkit-scrollbar-track { background: transparent; }
::-webkit-scrollbar-thumb {
  background: var(--border-strong);
  border-radius: var(--radius);
  border: 2px solid transparent;
  background-clip: padding-box;
}
::-webkit-scrollbar-thumb:hover { background: var(--text-3); }
```

用 `--border-strong`/`--text-3` 而非新造 token——滚动条本质是一条「更粗的
边框」，语义上就该跟着既有的边框/弱化文字色走，两个主题下都已经有正确的
明暗值，不需要再维护第三套「滚动条专用」颜色。

## 测试计划（要点，具体用例留给写计划阶段）

- `tokens.test.js`：新增断言——`:root[data-theme='light']` 选择器存在，
  且区块内包含 `--surface-0` 等一组关键变量；`::-webkit-scrollbar` 相关
  规则存在。
- `theme.test.js`（新文件）：
  - 非法/缺失的 localStorage 值回退到 `'system'`。
  - `setThemePreference('light')` 后 `document.documentElement.dataset.theme === 'light'`，
    且 `getThemePreference()` 读回 `'light'`。
  - 偏好为 `'system'` 时，`matchMedia('(prefers-color-scheme: dark)')` 匹配
    应解析为 `dark`，不匹配应解析为 `light`。
  - 显式选择 `dark`/`light` 后，模拟系统主题切换（触发 `matchMedia` 的
    `change`），`data-theme` **不应**跟着变。
  - `localStorage` 抛异常时（比如 mock 成不可用）不崩溃，按 `system` 降级。
- `SettingsOverlay.test.js`：新增「外观」分段控件的三个选项都渲染、点击
  `浅色`/`跟随系统` 调用 `setThemePreference` 且不触发 `onsave`（证明与
  草稿保存路径无关）。

## 明确的取舍

- 主题偏好的存储介质选 `localStorage` 而非 `config.yaml`：前者是"这台
  设备、这个渲染进程"的偏好，后者是跨设备复用的代理配置，混在一起会让
  `config_save_raw` 的"非规则区注释会丢失"这条已知代价，平白无故地覆盖到
  一个跟代理规则毫无关系的字段上。
- 不做"跟随出站色反推主题色相"之类更花哨的方案——用户给的范围就是三档
  开关，做多了是自增功能而非按需实现。
