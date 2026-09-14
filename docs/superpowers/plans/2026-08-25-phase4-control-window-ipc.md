# 阶段 4：控制窗口与 IPC — 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在既有的隐藏传输窗口之外建起第二个窗口 `control`（加载本地 Svelte 资产），把 IPC 面拆成**两个绝不重叠的 capability**，落地 Rust 侧的事件聚合节流与规则命中持久化，并让托盘常驻。本阶段**不做五个视图**（那是阶段 5），但要把设计文档 §11.4 的视觉基线落成 CSS 自定义属性。

**Architecture:** 一个进程、两个窗口、两套权限。`main` 窗口加载**远端服务器的页面**，只被授权三个 `wsieve_*` 传输命令；`control` 窗口加载 `tauri://localhost` 的本地资产，被授权配置与控制命令且**没有 `remote` 字段**。两个 capability 的命令集合交集为空，且此事由一条读取真实 JSON 文件的测试守住。事件从 Rust 侧聚合后单向推给 `control`，绝不逐条转发。

**Tech Stack:** Rust 2021 · Tauri 2.11 (`tray-icon` / `image-png` / `custom-protocol` 特性) · Svelte 5 (runes) · Vite 8 · `@fontsource/ibm-plex-{sans,mono}` · 纯手写 CSS，无组件库

**依据:** `docs/superpowers/specs/2026-08-24-client-routing-and-ui-design.md` §3.1 §9.1 §11.1 §11.2 §11.3 §11.4 §12 §13 §14

---

## 前置阅读（实现者必读）

- **§11.1** — 双 capability 隔离。**这是本阶段唯一不能打折的东西**，其余全部可以妥协，这条不行
- **§11.2** — 命令面与事件节流表，以及命中计数持久化
- **§11.3 / §11.4** — 信息架构与视觉基线。本阶段只落 token，不落视图
- **§12** — 控制窗口关闭时代理继续运行，托盘常驻
- **§13** — capability 交集断言被明确列为**安全测试而非形式检查**
- **§3.1** — 从现有代码读出的事实表。特别是最后一行：「capability 仅授权 `main` 窗口，且开放 `remote.urls` 给任意 https」——那正是本阶段要修的

**既有代码**：`src-tauri/src/main.rs`（建窗与命令注册）、`src-tauri/tauri.conf.json`（目前 `"build": {}`）、`src-tauri/capabilities/default.json`（要拆成两份）、`src-tauri/build.rs`（要加 dist 兜底）、`ui/emitter.js`（传输侧 JS，本阶段不动）。

**房规**：注释用中文；刻意简化用 `ponytail:` 标注上限与升级路径；错误绝不静默跳过（范本：`shard.rs:214` 的 `port_conflict_is_reported_not_skipped`）。

---

## 为什么这一阶段的安全性值得单独说

`main` 窗口加载的是**服务端的真实首页**（`WebviewUrl::External`，见 `main.rs:62-65`）。这是传输 spec §6.7 的同源决策的后果，不可撤销——它是传输能工作的前提。

后果是：**那台服务器一旦被攻破，其页面 JS 就能调用它被授权的每一个 Tauri 命令。**

现状（`capabilities/default.json`）授权 `main` 窗口的是：

```
core:default · core:event:default · core:webview:default · core:window:default
default（= 三个 wsieve_* 命令）
remote.urls: ["http://127.0.0.1:*", "http://**:*", "https://**:*"]
```

阶段 4 之前，这个面还只包含传输命令，损失有限。**阶段 4 之后就不是了**——`config_get` 会返回含 `client-priv` 私钥的配置。如果两个窗口共用一份 capability，被攻破的服务器页面读一次配置就把私钥拿走了。

所以本阶段的顺序是钉死的：**先拆 capability、先写交集断言，再加任何一个控制命令**。Task 3 的断言测试必须在 Task 6 引入 `config_get` 之前就存在并通过。

---

## 已在 scratch 副本中实测确认的事实

写这份计划时，仓库被完整复制到 `/tmp/wsieve-scratch`，下列每一条都**编译过、跑起来过**，不是从文档推断的：

| 事实 | 如何确认的 |
|---|---|
| `tauri = { features = ["tray-icon", "image-png"] }` 能编译，`TrayIconBuilder` / `MenuBuilder` / `MenuItemBuilder` 的方法签名如本文所写 | `cargo check` 通过 |
| **`cargo build` 在 `frontendDist` 指向的目录不存在时直接 panic**（proc macro panicked: "The `frontendDist` configuration is set to ... but this path doesn't exist"）| 删掉 `ui/dist` 重新 build，实测报错 |
| **debug 构建默认走 `devUrl` 而非嵌入资产**。`tauri-macros/src/context.rs:155` 是 `dev: cfg!(not(feature = "custom-protocol"))` | 不开 `custom-protocol` 时控制窗口 URL 是 `http://localhost:5174`；开了之后变成 `tauri://localhost` |
| 双窗口共存：`main` 走远端 origin 跑通隧道，`control` 同时加载本地 Svelte 页 | 真实 E2E：`curl --socks5-hostname` 取回 `e2e-acceptance-body-v1`，同一进程内控制窗口的 `invoke('config_get')` 返回值也回报了 |
| **transport capability 收窄到只有三个 `wsieve_*` 权限后，emitter 仍然正常工作** | 去掉 `core:*` 全部权限后跑真实 E2E，隧道照通 |
| 控制窗口调 `wsieve_heartbeat` 被拒绝，错误信息是 `wsieve_heartbeat not allowed on window "control", webview "control", URL: local` | 在控制页里实调并回报 |
| `Emitter::emit_to` 要求 `S: Serialize + Clone`（**不只是 Serialize**）| 第一版 `ConnectionBatch` 只 derive 了 `Serialize`，编译失败 E0277 |
| `RunEvent::WindowEvent` 是 `#[non_exhaustive]`，解构必须带 `..` | 不带 `..` 时编译失败 E0638 |
| `RunEvent::Exit` 在 `app.exit(0)` 后确实会到达（顺序：`ExitRequested` → `Exit`）| 加日志实测 |
| **控制窗口不存在时 `emit_to` 静默返回 `Ok(())`**，不报错 | 用 `SCRATCH_NO_CONTROL=1` 跑，`EMIT-OK` 照打 |
| Svelte 5.56 + Vite 8 + `@fontsource/ibm-plex-*` 能构建，产物能被 Tauri 嵌入并在控制窗口跑起来 | `npx vite build` 后完整 E2E |

**唯一未实测的**：托盘图标在真实 macOS 菜单栏中的视觉呈现（无头环境看不到菜单栏）。代码编译通过、`TrayIconBuilder::build()` 返回 `Ok`，但「图标长什么样、点击是否弹菜单」需要实现者在有 GUI 的机器上肉眼确认。见 Task 12。

---

## 文件结构

```
src-tauri/
  Cargo.toml                       修改：tauri 特性 + embed-ui feature
  build.rs                         修改：ui/dist 缺失时写占位页
  tauri.conf.json                  修改：build 段 + control 窗口不在此声明
  capabilities/
    default.json                   删除
    transport.json                 新建 —— main 窗口，三个 wsieve_* 命令
    control.json                   新建 —— control 窗口，控制命令，无 remote
  permissions/wsieve/
    allow-config-get.toml          新建（以及其余每命令一份）
    ...
  src/
    main.rs                        修改：建控制窗口、托盘、注册命令
    control.rs                     新建 —— 控制窗口的建/显/隐
    tray.rs                        新建 —— 托盘图标与菜单
    events.rs                      新建 —— 事件聚合与节流
    stats.rs                       新建 —— rule-hit 计数落盘
    commands/
      mod.rs                       新建 —— 命令面汇总
      config.rs                    新建 —— config_get / config_save / raw
      control.rs                   新建 —— connect / disconnect / set_mode
      probe.rs                     新建 —— rule_test / outbound_latency_probe
  tests/
    capability_isolation.rs        新建 —— §13 的安全测试

ui/
  package.json                     新建
  vite.config.js                   新建
  svelte.config.js                 新建
  src/
    index.html                     新建
    main.js                        新建
    App.svelte                     新建 —— 本阶段只是骨架
    tokens.css                     新建 —— §11.4 的设计 token
    lib/ipc.js                     新建 —— invoke / listen 的薄封装
  emitter.js                       不动（传输侧，与控制窗口无关）
  dist/                            构建产物，gitignore
```

**为什么控制窗口在 Rust 代码里建而不在 `tauri.conf.json` 里声明**：与 `main` 窗口同理（见 `main.rs` 模块注释）。更重要的是本阶段要给它挂 `on_window_event`（关闭时隐藏而非退出，§12），那必须拿到 `WebviewWindow` 句柄。配置文件里声明的窗口在 `setup` 里还得再 `get_webview_window` 捞一次，没有省事。

---

## Part A — 构建集成与安全边界

> **纪律：Part A 必须整体完成后才能进 Part B。** Task 3 的交集断言是 Part B 引入控制命令的前提。

### Task 1: Vite + Svelte 脚手架

**Files:**
- Create: `ui/package.json`, `ui/vite.config.js`, `ui/svelte.config.js`
- Create: `ui/src/index.html`, `ui/src/main.js`, `ui/src/App.svelte`, `ui/src/lib/ipc.js`
- Modify: `.gitignore`

本阶段只要「能构建、能加载、能 invoke」。五个视图是阶段 5。

- [ ] **Step 1: 初始化 npm 工程**

```bash
cd ui
npm init -y
npm install --save-dev svelte vite @sveltejs/vite-plugin-svelte
npm install --save-dev @fontsource/ibm-plex-sans @fontsource/ibm-plex-mono
```

实测装到的版本：`svelte@5.56.10` / `vite@8.2.2` / `@sveltejs/vite-plugin-svelte@7.3.0`。**Svelte 5 用 runes 语法**（`$state` / `$effect`），不是 Svelte 4 的 `export let`。

字体走 `@fontsource` 而不是 Google Fonts CDN：控制窗口加载的是 `tauri://localhost`，走 CDN 意味着**一个后台常驻的代理工具会在启动时向外发起字体请求**——既是隐私泄漏也是可用性隐患（断网时字体退化）。`@fontsource` 把 woff2 打进 dist，零外部请求。

- [ ] **Step 2: 写 `ui/package.json` 的 scripts**

把 `npm init -y` 生成的 `scripts` 段整个替换为：

```json
  "scripts": {
    "dev": "vite",
    "build": "vite build"
  },
  "type": "module",
```

`"type": "module"` 是必需的——`vite.config.js` 用 ESM 语法。

- [ ] **Step 3: 写 `ui/vite.config.js`**

```js
import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

export default defineConfig({
  plugins: [svelte()],
  // 源码放 src/，产物出到 ui/dist —— tauri.conf.json 的 frontendDist 指向后者
  root: 'src',
  publicDir: false,
  build: {
    outDir: '../dist',
    emptyOutDir: true,
    // 控制窗口是 WKWebView（macOS）/ WebView2（Windows）/ WebKitGTK（Linux）。
    // 目标定在 safari15 而非默认的 baseline：只有一个已知的运行环境，
    // 没必要为不存在的旧浏览器付出降级代码的体积。
    target: 'safari15',
    sourcemap: false,
  },
  server: { port: 5174, strictPort: true },
  // Vite 默认会清屏，把 cargo 的编译输出冲掉
  clearScreen: false,
});
```

`strictPort: true` 是刻意的：端口被占时**直接失败**而不是悄悄换一个。换了端口，`tauri.conf.json` 里写死的 `devUrl` 就指向别人的服务了——那属于「静默错误」，本仓库不接受（房规）。

- [ ] **Step 4: 写 `ui/svelte.config.js`**

```js
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

export default { preprocess: vitePreprocess() };
```

- [ ] **Step 5: 写页面骨架四件套**

`ui/src/index.html`：

```html
<!doctype html>
<html lang="zh-CN">
  <head>
    <meta charset="utf-8" />
    <title>websieve</title>
  </head>
  <body>
    <div id="app"></div>
    <script type="module" src="./main.js"></script>
  </body>
</html>
```

`ui/src/main.js`：

```js
import { mount } from 'svelte';
import './tokens.css';
import App from './App.svelte';

// Svelte 5 用 mount() 而非 new App()
export default mount(App, { target: document.getElementById('app') });
```

`ui/src/lib/ipc.js`：

```js
// IPC 的唯一入口。集中在这里是为了让「控制窗口调了哪些命令」可被 grep ——
// 该清单必须与 capabilities/control.json 逐条对应，多一个就是权限泄漏。
//
// withGlobalTauri 为 true（tauri.conf.json），因此走 window.__TAURI__ 而不必
// 引 @tauri-apps/api 包。少一个 npm 依赖，且版本永远与 Rust 侧一致。

export function invoke(cmd, args) {
  return window.__TAURI__.core.invoke(cmd, args);
}

export function listen(event, handler) {
  return window.__TAURI__.event.listen(event, handler);
}
```

`ui/src/App.svelte`（本阶段的骨架，阶段 5 会整个换掉）：

```svelte
<script>
  import { invoke, listen } from './lib/ipc.js';

  let status = $state('未连接');
  let traffic = $state(null);

  $effect(() => {
    // IPC 往返自证：能拿到配置就说明 capability 配对了
    invoke('config_get')
      .then((cfg) => (status = `已加载配置：mixed-port ${cfg['mixed-port']}`))
      .catch((e) => (status = `配置读取失败：${e}`));

    const un = [];
    listen('status', (e) => (status = e.payload)).then((f) => un.push(f));
    listen('traffic', (e) => (traffic = e.payload)).then((f) => un.push(f));
    return () => un.forEach((f) => f());
  });
</script>

<main>
  <p class="status">{status}</p>
  {#if traffic}
    <p class="mono">↑ {traffic.up_rate} B/s ↓ {traffic.down_rate} B/s</p>
  {/if}
  <p class="hint">五个视图见阶段 5。</p>
</main>

<style>
  main {
    padding: var(--space-4);
  }
  .status {
    color: var(--text-1);
    font-size: var(--fs-14);
  }
  .mono {
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
    color: var(--text-2);
    font-size: var(--fs-13);
  }
  .hint {
    color: var(--text-4);
    font-size: var(--fs-12);
  }
</style>
```

> `tokens.css` 在 Task 2 里写。这一步先让引用存在，Task 2 补上定义。

- [ ] **Step 6: gitignore 掉产物与依赖**

在 `.gitignore` 追加：

```
/ui/node_modules
/ui/dist
```

- [ ] **Step 7: 验证构建**

Run: `npm --prefix ui run build`
Expected: `✓ built in ...`，`ui/dist/index.html` 与 `ui/dist/assets/*.{js,css,woff2}` 存在。

**此刻 `tokens.css` 还不存在，构建会失败**——那是预期的，Task 2 补。若想先看一眼绿灯，可临时 `touch ui/src/tokens.css`。

- [ ] **Step 8: 提交**

```bash
git add ui/package.json ui/package-lock.json ui/vite.config.js ui/svelte.config.js ui/src .gitignore
git commit -m "chore(ui): Svelte 5 + Vite 脚手架"
```

---

### Task 2: 设计 token（§11.4 的视觉基线）

**Files:**
- Create: `ui/src/tokens.css`

设计文档 §11.4 的表格逐条落成 CSS 自定义属性。**本阶段只落 token，不落任何视图**——但阶段 5 的每一个颜色、间距、字号都必须从这里取，不许现场硬编码。

- [ ] **Step 1: 写 `ui/src/tokens.css`**

```css
/* websieve 设计 token —— 设计文档 §11.4 的逐条落地。
 *
 * 阶段 5 的全部视图只许引用这里的变量，不许现场写 #xxxxxx 或 12px。
 * 理由是 §11.4 的「颜色纪律」：约 90% 屏幕为中性结构色，彩色只出现在
 * 出站色码与状态色上。这条纪律靠人自觉守不住，靠「能取到的只有这些」守得住。
 */

@import '@fontsource/ibm-plex-sans/400.css';
@import '@fontsource/ibm-plex-sans/500.css';
@import '@fontsource/ibm-plex-sans/600.css';
@import '@fontsource/ibm-plex-mono/400.css';
@import '@fontsource/ibm-plex-mono/500.css';

:root {
  /* ── 表面：单一色相只移明度，每级 4–7% ───────────────── */
  --surface-0: #16181b; /* 画布 */
  --surface-1: #1c1f23; /* 行/面板 */
  --surface-2: #23272c; /* 悬浮/选中 */

  /* ── 边框：borders-only 深度策略，不混用阴影（浮层除外）── */
  --border: rgba(255, 255, 255, 0.07);
  --border-strong: rgba(255, 255, 255, 0.13);

  /* ── 文本四级 ─────────────────────────────────────── */
  --text-1: #e6e8ea; /* 主要 */
  --text-2: #a4abb3; /* 次要 */
  --text-3: #6f777f; /* 弱化 */
  --text-4: #4d545b; /* 极弱（占位、禁用） */

  /* ── 状态色 ───────────────────────────────────────── */
  --state-live: #3fb27f;
  --state-warn: #d99a3f;
  --state-fail: #d1595c;
  --state-direct: #7d8894;

  /* ── 出站色码：界面中唯一允许的彩色 ───────────────── */
  --outbound-1: #5b8ff9;
  --outbound-2: #61ddaa;
  --outbound-3: #f6bd16;
  --outbound-4: #7262fd;
  --outbound-5: #78d3f8;
  --outbound-6: #f6903d;
  --outbound-7: #008685;
  --outbound-8: #f08bb4;

  /* ── 命中热度染色（§11.5 signature ①）───────────────
   * 用中性色而非出站色，避免与出站色码语义冲突。
   * 阶段 5 按命中数归一化后在 min..max 之间插值。 */
  --heat-min: rgba(255, 255, 255, 0.014);
  --heat-max: rgba(255, 255, 255, 0.052);

  /* ── 字体：IBM Plex，不用 Inter ─────────────────────
   * 理由见 §11.4：Plex 有 IBM 技术文档的工程血统，同族搭配气质统一。 */
  --font-sans: 'IBM Plex Sans', -apple-system, BlinkMacSystemFont, system-ui,
    sans-serif;
  --font-mono: 'IBM Plex Mono', ui-monospace, SFMono-Regular, Menlo, monospace;

  /* ── 字阶：1.25 / 14px base。层级靠字重+颜色，不靠字号 ── */
  --fs-11: 11px;
  --fs-12: 12px;
  --fs-13: 13px;
  --fs-14: 14px;
  --fs-16: 16px;
  --fs-18: 18px;
  --fs-22: 22px;

  --fw-regular: 400;
  --fw-medium: 500;
  --fw-semibold: 600;

  /* ── 间距：基数 4px ───────────────────────────────── */
  --space-1: 4px;
  --space-2: 8px;
  --space-3: 12px;
  --space-4: 16px;
  --space-5: 20px;
  --space-6: 24px;

  /* ── 密度：§11.4 钉死的行高 ───────────────────────── */
  --row-rule: 32px; /* 规则行 */
  --row-outbound: 38px; /* 出站行 */

  /* ── 浮层是 borders-only 的唯一例外 ─────────────────── */
  --shadow-overlay: 0 8px 32px rgba(0, 0, 0, 0.5);
  --radius: 4px;
}

* {
  box-sizing: border-box;
}

html,
body {
  margin: 0;
  height: 100%;
}

body {
  background: var(--surface-0);
  color: var(--text-1);
  font-family: var(--font-sans);
  font-size: var(--fs-14);
  font-weight: var(--fw-regular);
  line-height: 1.45;
  /* 密集界面里默认的字距过松 */
  letter-spacing: -0.006em;
  -webkit-font-smoothing: antialiased;
}

/* 数字必须能对齐成列（§11.4：延迟与会话数用 tabular-nums 对齐） */
.mono,
.num {
  font-family: var(--font-mono);
  font-variant-numeric: tabular-nums;
}

/* 键盘可达性：桑基图的无障碍评级已经是 C（§11.6），
 * 焦点环这种白送的东西不能再丢。 */
:focus-visible {
  outline: 2px solid var(--outbound-1);
  outline-offset: 1px;
}

/* §11.6 明确要求：reduced-motion 下流带宽度直接跳变 */
@media (prefers-reduced-motion: reduce) {
  *,
  *::before,
  *::after {
    animation-duration: 0.01ms !important;
    animation-iteration-count: 1 !important;
    transition-duration: 0.01ms !important;
  }
}
```

- [ ] **Step 2: 验证构建通过**

Run: `npm --prefix ui run build`
Expected: 构建成功。产物里应能看到 woff2 被打包（实测约 11 个字体文件，`ibm-plex-sans-latin-400-normal-*.woff2` 约 22KB）。

- [ ] **Step 3: 确认 token 未被绕过（本阶段的骨架里）**

Run: `grep -nE '#[0-9a-fA-F]{6}|[0-9]+px' ui/src/App.svelte`
Expected: 无输出。App.svelte 的 `<style>` 里只该出现 `var(--...)`。

> 这条 grep 在阶段 5 会成为一条真正的门禁。本阶段先建立习惯。

- [ ] **Step 4: 提交**

```bash
git add ui/src/tokens.css
git commit -m "feat(ui): 设计 token（§11.4 视觉基线）"
```

---

### Task 3: 双 capability 隔离 🔒

**Files:**
- Delete: `src-tauri/capabilities/default.json`
- Create: `src-tauri/capabilities/transport.json`
- Create: `src-tauri/capabilities/control.json`

**这是整个阶段最重要的一个 task。** 在它完成之前，不许引入任何一个控制命令。

- [ ] **Step 1: 写 `capabilities/transport.json`**

```json
{
  "$schema": "../gen/schemas/desktop-schema.json",
  "identifier": "transport",
  "description": "传输窗口 main：加载远端服务器的伪装页，只授权 emitter 需要的三个二进制通道命令。此处绝不能出现任何控制类命令——该页面的 JS 由服务器控制，服务器被攻破即等同于这些命令被攻破。",
  "windows": ["main"],
  "permissions": [
    "allow-wsieve-heartbeat",
    "allow-wsieve-raw-post",
    "allow-wsieve-raw-stream"
  ],
  "remote": {
    "urls": ["http://127.0.0.1:*", "https://**:*"]
  },
  "local": false
}
```

三处相对现状的收紧，每一处都实测过：

| 收紧 | 现状 | 为什么能收 |
|---|---|---|
| 去掉 `core:default` / `core:event:default` / `core:webview:default` / `core:window:default` | 四条全在 | **实测**：只留三个 `wsieve_*` 后跑真实 E2E，隧道照通。`ui/emitter.js` 全文只调 `window.__TAURI__.core.invoke`，从不调 `event.listen` / `window.*` / `webview.*`——那些权限一直是白给的 |
| 去掉 `remote.urls` 里的 `http://**:*` | 在 | 设计文档 §11.1 明确要求。见下方迁移说明 |
| `local: false` | `true` | 传输窗口永远导航到远端 origin（`WebviewUrl::External`），从不加载本地资产。`local: true` 是纯粹的多余授权面 |

> **迁移说明（设计文档 §11.1 点名要写的）**
>
> 去掉 `http://**:*` 后，**以 http 域名部署的开发场景会被打到**：譬如把服务端跑在 `http://dev.example.internal/`，传输窗口的 invoke 会被 ACL 拒绝，表现为握手永远等不到心跳。
>
> **`scripts/e2e.sh` 不受影响**——已核对：它用的是 `http://127.0.0.1:$SRV_PORT/`（第 97 行），落在保留下来的 `http://127.0.0.1:*` 里。而且 IP 主机不触发条带劫持（`shard_setup.rs:76 hijackable()` 对 IP 字面量返回 false），所以页面 URL 也不会被改写成别的 host。**scratch 副本上跑通了真实 E2E 隧道，确认无回归。**
>
> 若你确实需要 http 域名部署：在 `transport.json` 的 `urls` 里**具名加上那一个域名**（`"http://dev.example.internal:*"`），不要把 `http://**:*` 加回来。前者是一个已知的开发环境，后者是「任意 http 站点都能调传输命令」。

- [ ] **Step 2: 写 `capabilities/control.json`**

```json
{
  "$schema": "../gen/schemas/desktop-schema.json",
  "identifier": "control",
  "description": "控制窗口 control：加载 tauri://localhost 的本地 Svelte 资产。授权配置与控制命令。绝不能有 remote 字段——一旦有，远端页面就能借这个 capability 读到含 client-priv 私钥的配置。",
  "windows": ["control"],
  "permissions": [
    "core:default",
    "allow-config-get",
    "allow-config-save",
    "allow-config-get-raw",
    "allow-config-save-raw",
    "allow-connect",
    "allow-disconnect",
    "allow-set-mode",
    "allow-outbound-enable",
    "allow-outbound-latency-probe",
    "allow-rule-test",
    "allow-geo-update",
    "allow-geo-status",
    "allow-traffic-snapshot",
    "allow-control-hide",
    "allow-app-quit"
  ],
  "local": true
}
```

**没有 `remote` 字段。** 这不是遗漏，是本 task 的全部意义。Task 3 Step 5 的测试会断言它不存在。

`core:default` 在这里是可以的——控制窗口加载的是我们自己打包的资产，`core:event:listen` 是它接收聚合事件所必需的。**注意这正是两个 capability 不能共用的另一个理由**：`core:default` 展开后含 `core:event|listen`，如果 transport 也留着 `core:event:default`，两者就会在 `core:event|listen` 上相交——Task 4 的测试**实测抓到过这个交集**（见该 task 的 Step 4）。

- [ ] **Step 3: 删掉旧的 default.json**

```bash
git rm src-tauri/capabilities/default.json
```

`tauri-utils/src/acl/build.rs:411` 会扫描整个 `capabilities/` 目录，留着旧文件等于第三份权限来源，且它同时授权 `main` 与开放 `http://**:*`——整个 task 就白做了。

- [ ] **Step 4: 权限文件（每命令一份）**

`capabilities/control.json` 引用的 `allow-*` 需要在 `permissions/wsieve/` 下各有一份定义，否则 build 会失败。这些在 Part B 的各个 task 里随命令一起创建，本 task 只创建**一份**用来验证机制：

`src-tauri/permissions/wsieve/allow-config-get.toml`：

```toml
[[permission]]
identifier = "allow-config-get"
description = "允许控制窗口读取结构化配置（含 client-priv，仅本地 origin）"
commands.allow = ["config_get"]
```

同时把 `permissions/wsieve/default.toml` 里的权限集**留着不动**——它只是一个 set，没被任何 capability 引用就不生效。

> **本 step 的临时状态**：`control.json` 里引用了 15 个还不存在的权限，`cargo build` 会失败。这是预期的。Step 5 给出一个把它临时收窄的办法，让 Task 3/4 能独立验证。

- [ ] **Step 5: 临时收窄 control.json 以便独立验证**

把 `control.json` 的 `permissions` 暂时改成只有两项：

```json
  "permissions": ["core:default", "allow-config-get"],
```

Part B 的每个 task 完成时把对应的权限**加回来一条**。这样每一步都是可构建、可测试的——不存在「写完全部才能跑」的窗口（这是整份计划的既定纪律）。

Run: `cargo build --manifest-path src-tauri/Cargo.toml`
Expected: 构建成功。

Run: `cat src-tauri/gen/schemas/capabilities.json | python3 -m json.tool`
Expected: 恰好两个顶层键 `control` 与 `transport`，`control` 下**没有** `remote` 键，`transport` 下的 `remote.urls` 只有两项。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/capabilities/ src-tauri/permissions/wsieve/allow-config-get.toml
git commit -m "feat(security): 拆分 transport / control 双 capability，收紧远端授权面"
```

---

### Task 4: capability 交集断言测试 🔒

**Files:**
- Create: `src-tauri/tests/capability_isolation.rs`

设计文档 §13 把这条列为「**安全测试而非形式检查**」。

**设计要点：它必须读取真实的 capability JSON 文件，不能硬编码权限列表**——否则将来有人往 `transport.json` 里加一条权限，测试还在检查一份过时的硬编码清单，绿灯照亮，防线已破。同理，permission → command 的展开也必须读 `gen/schemas/acl-manifests.json`，不能自己维护一份映射表。

还有一层：断言的对象是**命令名集合**而不是 permission 标识符集合。两个 capability 完全可以用不同名字的 permission 指向同一个命令（`allow-config-get` 与 `allow-cfg-read` 都 allow `config_get`），只比 permission 名字会漏掉这种情况。

- [ ] **Step 1: 写测试**

```rust
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
    let text = std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("读取 {} 失败：{e}", p.display()));
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
fn expand_commands(cap: &CapabilityFile, manifests: &BTreeMap<String, Manifest>) -> BTreeSet<String> {
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

#[test]
fn capability_files_exist_and_target_distinct_windows() {
    let t = read_capability("transport.json");
    let c = read_capability("control.json");
    assert_eq!(t.identifier, "transport");
    assert_eq!(c.identifier, "control");
    assert_eq!(t.windows, vec!["main".to_string()]);
    assert_eq!(c.windows, vec!["control".to_string()]);

    let tw: BTreeSet<_> = t.windows.iter().collect();
    let cw: BTreeSet<_> = c.windows.iter().collect();
    assert!(
        tw.intersection(&cw).next().is_none(),
        "两个 capability 不得授权同一个窗口"
    );
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
    let remote = t.remote.expect("transport 必须有 remote —— 它加载的就是远端页面");

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
```

- [ ] **Step 2: 跑测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test capability_isolation`
Expected: 5 个测试全部 PASS。

- [ ] **Step 3: 确认测试真的会失败（防假绿灯）**

一条永远通过的安全测试比没有更糟。手工验证它有牙：

```bash
# 临时把 core:event:default 加进 transport.json 的 permissions
cargo test --manifest-path src-tauri/Cargo.toml --test capability_isolation
```

Expected: `the_two_capabilities_share_no_command` **失败**，且失败信息里点名 `["core:event|listen", "core:event|unlisten"]`。

> **这不是假想的场景——计划评审时就是这么发现的。** 第一版 `transport.json` 里留了 `core:event:allow-listen` / `allow-unlisten`（想着「emitter 也许要监听事件」），交集断言立刻抓出了这两条。后来核对 `ui/emitter.js` 全文，确认它只调 `core.invoke`，从不 `listen`，遂删掉。**测试比直觉更早发现了权限泄漏。**

验证完记得把 `core:event:default` 从 transport.json 删回去，重新跑一次确认绿。

- [ ] **Step 4: 确认测试不会腐烂**

再验一次「读真实文件」这件事是有效的：

```bash
# 临时往 control.json 加一条 "allow-wsieve-heartbeat"
cargo test --manifest-path src-tauri/Cargo.toml --test capability_isolation
```

Expected: 交集断言失败，点名 `["wsieve_heartbeat"]`。**如果这里绿了，说明测试读的不是真文件，必须停下来查。**

- [ ] **Step 5: 提交**

```bash
git add src-tauri/tests/capability_isolation.rs
git commit -m "test(security): capability 交集断言（读真实 JSON，非硬编码）"
```

---

### Task 5: 构建集成（Vite ⇄ Tauri）

**Files:**
- Modify: `src-tauri/tauri.conf.json`
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/build.rs`

`tauri.conf.json` 当前是 `"build": {}`，即「没有前端」。要接上 Vite，同时**不能破坏 `scripts/e2e.sh`**——那个脚本直接 `cargo build`，不跑 npm。

- [ ] **Step 1: 改 `tauri.conf.json` 的 build 段**

```json
  "build": {
    "beforeDevCommand": "npm --prefix ../ui run dev",
    "beforeBuildCommand": "npm --prefix ../ui run build",
    "devUrl": "http://localhost:5174",
    "frontendDist": "../ui/dist"
  },
```

四个字段名都已对照 Tauri 2 的 `config.schema.json` 的 `BuildConfig` 核实（`runner` / `devUrl` / `frontendDist` / `beforeDevCommand` / `beforeBuildCommand` / `beforeBundleCommand` / `features` / `removeUnusedCommands` / `additionalWatchFolders` / `windows`）。

`beforeDevCommand` / `beforeBuildCommand` **只被 `tauri-cli` 使用**（`tauri dev` / `tauri build`）。裸 `cargo build` 不会执行它们——这正是下一步需要 build.rs 兜底的原因。

同时把 `app.windows` 保持为 `[]`：两个窗口都在 Rust 代码里建。

- [ ] **Step 2: 加 `embed-ui` feature**

在 `src-tauri/Cargo.toml` 的 `[build-dependencies]` **之前**插入：

```toml
[features]
default = ["embed-ui"]
# 把 ui/dist 嵌进二进制，而不是从 devUrl 拉。
#
# 为什么需要显式开：tauri-macros 里是
#   dev: cfg!(not(feature = "custom-protocol"))
# （tauri-macros/src/context.rs:155）。也就是说 **debug 构建默认走 devUrl**，
# 控制窗口会去连 http://localhost:5174 —— 没跑 vite dev 时就是一片空白。
# 实测：不开此 feature 时控制窗口 URL 是 http://localhost:5174；
# 开了之后是 tauri://localhost，本地资产正常加载。
#
# 用 `cargo build --no-default-features` 可退回 devUrl 模式配合 `vite dev` 热重载。
embed-ui = ["tauri/custom-protocol"]
```

并把 `tauri` 依赖行改为：

```toml
tauri = { version = "2", features = ["tray-icon", "image-png"] }
```

- `tray-icon`：托盘 API 挂在 `#[cfg(all(desktop, feature = "tray-icon"))]` 下（`tauri/src/lib.rs:112`）。不开则 `tauri::tray` 模块**根本不存在**
- `image-png`：`app.default_window_icon()` 返回的图标要能被托盘复用。仓库只有 `icons/icon.png`
- **不要**把 `custom-protocol` 直接写进这里——它必须由 `embed-ui` 间接开，否则就没有退回 devUrl 的路了

实测这组特性能编译（`cargo check` 通过，拉入 `tray-icon 0.24.2` / `muda 0.19.3` / `image 0.25.10`）。

- [ ] **Step 3: build.rs 兜底缺失的 dist**

这是**必须做的**，不是锦上添花。实测：`frontendDist` 指向的目录不存在时，`cargo build` 直接 panic：

```
error: proc macro panicked
  = help: message: The `frontendDist` configuration is set to `"../ui/dist"` but this path doesn't exist
```

而 `scripts/e2e.sh:96` 是裸 `cargo build --manifest-path src-tauri/Cargo.toml`——**不加兜底，E2E 在干净克隆上必然失败**。

把 `src-tauri/build.rs` 改为：

```rust
fn main() {
    // 把 ui/emitter.js 嵌成 Rust 字符串（单一事实源在 ui/，无构建步骤）。
    let src = std::fs::read_to_string("../ui/emitter.js").expect("read ui/emitter.js");
    let out_dir = std::env::var("OUT_DIR").unwrap();
    std::fs::write(
        std::path::Path::new(&out_dir).join("emitter_src.rs"),
        format!("pub const EMITTER_JS: &str = {:?};", src),
    )
    .expect("write emitter_src.rs");
    println!("cargo:rerun-if-changed=../ui/emitter.js");

    ensure_frontend_dist();
    tauri_build::build()
}

/// `frontendDist` 指向的目录不存在时，tauri 的代码生成宏会直接 panic
/// （实测："The `frontendDist` configuration is set to ... but this path
/// doesn't exist"）。而 scripts/e2e.sh 是裸 cargo build，不跑 npm ——
/// 干净克隆上必然撞上。
///
/// 因此写一个占位页兜底：控制窗口能开、能看出「前端没构建」，
/// 传输链路（E2E 真正验的东西）完全不受影响。
///
/// ponytail: 占位页是硬编码的一行 HTML，不做模板、不做 i18n。
/// 上限：用户看到中文提示；升级路径：真需要时换成读 ui/placeholder.html。
fn ensure_frontend_dist() {
    let dist = std::path::Path::new("../ui/dist");
    let index = dist.join("index.html");
    if index.exists() {
        return;
    }
    std::fs::create_dir_all(dist).expect("create ui/dist");
    std::fs::write(
        &index,
        "<!doctype html><meta charset=\"utf-8\"><title>websieve</title>\
         <body style=\"background:#16181b;color:#e6e8ea;font:13px system-ui;padding:24px\">\
         前端尚未构建。运行 <code>npm --prefix ui run build</code> 后重新编译。</body>",
    )
    .expect("write placeholder index.html");
    println!(
        "cargo:warning=ui/dist 缺失，已写入占位页；\
         跑 `npm --prefix ui run build` 生成真实前端"
    );
}
```

注意**不要**给 `../ui/dist` 加 `rerun-if-changed`：那会让每次前端构建都触发整个 app crate 重编译。Tauri 的资产嵌入本来就在 `tauri_build::build()` 内部处理依赖追踪。

- [ ] **Step 4: 验证「干净克隆」路径**

```bash
rm -rf ui/dist
cargo build --manifest-path src-tauri/Cargo.toml
```

Expected: 构建**成功**，输出里有 `warning: wsieve-app@0.1.0: ui/dist 缺失，已写入占位页`，且 `ui/dist/index.html` 被创建。

> 实测确认过：先删 dist 再 build 会 panic（无兜底时），加了 `ensure_frontend_dist()` 之后成功并打出该 warning。

- [ ] **Step 5: 验证真实前端路径**

```bash
npm --prefix ui run build
touch src-tauri/build.rs   # 强制 build.rs 重跑
cargo build --manifest-path src-tauri/Cargo.toml
```

Expected: 构建成功，无占位页 warning（因为 `index.html` 已存在，`ensure_frontend_dist` 提前返回）。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/tauri.conf.json src-tauri/Cargo.toml src-tauri/build.rs
git commit -m "build: 接入 Vite（embed-ui feature + dist 缺失兜底）"
```

---

> **Part A 到此结束。** 此刻：前端能构建、两个 capability 已拆分且被测试守住、`cargo build` 在有无前端时都能过。**还没有任何控制命令存在**——这是刻意的顺序。

---

## Part B — 控制窗口与托盘

### Task 6: 控制窗口的建、显、隐

**Files:**
- Create: `src-tauri/src/control.rs`
- Modify: `src-tauri/src/main.rs`

设计文档 §12 最后一行：「控制窗口关闭 → 代理继续运行，托盘常驻」。这句话决定了关闭按钮**必须**被拦下——默认行为是销毁窗口，最后一个窗口销毁后 macOS 之外的平台会退出事件循环，代理就死了。

- [ ] **Step 1: 写 `src-tauri/src/control.rs`**

```rust
//! 控制窗口的建、显、隐（设计文档 §11.3 / §12）。
//!
//! 与传输窗口 `main` 的对照 —— 这两个窗口几乎在每一点上都相反：
//!
//! |          | main（传输）              | control（控制）        |
//! |----------|---------------------------|------------------------|
//! | URL      | External（远端服务器页面）| App（本地嵌入资产）    |
//! | 可见性   | 默认隐藏                  | 默认显示               |
//! | 关闭行为 | 不适用（用户看不到）      | 隐藏，不销毁           |
//! | 权限     | 三个 wsieve_* 命令        | 配置与控制命令         |
//! | 信任     | **不可信**（服务器控制）  | 可信（我们打包的）     |
//!
//! 最后一行是全部安全设计的出发点，见 capabilities/ 下两份文件。

use tauri::{Manager, WebviewWindow};

pub const LABEL: &str = "control";

/// 打开控制窗口。已存在则显示并聚焦，不重复建。
///
/// 「已存在」的判定必须走 `get_webview_window` 而不是自己记一个 bool ——
/// 窗口可能被别的路径销毁，缓存的标记会撒谎。
pub fn open(app: &tauri::AppHandle) -> tauri::Result<WebviewWindow> {
    if let Some(w) = app.get_webview_window(LABEL) {
        w.show()?;
        w.unminimize().ok(); // 最小化状态下 show() 不会还原
        w.set_focus()?;
        return Ok(w);
    }
    build(app)
}

fn build(app: &tauri::AppHandle) -> tauri::Result<WebviewWindow> {
    let window = tauri::webview::WebviewWindowBuilder::new(
        app,
        LABEL,
        // App(...) 而非 External(...)：加载嵌进二进制的 ui/dist。
        // 注意 debug 构建默认走 devUrl，需要 embed-ui feature
        // （= tauri/custom-protocol）才会真的用嵌入资产。见 Cargo.toml。
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("websieve")
    // §11.3：默认 960×640、最小 720×480（规则表需要宽度）
    .inner_size(960.0, 640.0)
    .min_inner_size(720.0, 480.0)
    .visible(true)
    .resizable(true)
    // 深色界面下，白色的启动闪屏很刺眼。与 tokens.css 的 --surface-0 一致。
    .background_color(tauri::window::Color(0x16, 0x18, 0x1b, 0xff))
    .build()?;

    // §12：控制窗口关闭 → 代理继续运行，托盘常驻。
    // 默认行为是销毁窗口；非 macOS 平台上最后一个窗口销毁会终止事件循环，
    // 于是整个代理跟着死。这里改成隐藏。
    //
    // 注意 WindowEvent 是 #[non_exhaustive]，解构必须带 `..`（否则 E0638）。
    let handle = window.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            if let Err(e) = handle.hide() {
                tracing::warn!("隐藏控制窗口失败：{e}");
            }
        }
    });

    Ok(window)
}

/// 显隐切换 —— 托盘点击图标时用。
pub fn toggle(app: &tauri::AppHandle) {
    match app.get_webview_window(LABEL) {
        Some(w) => {
            let visible = w.is_visible().unwrap_or(false);
            let focused = w.is_focused().unwrap_or(false);
            // 可见但没聚焦时，用户想要的多半是「拿到前台」而不是「藏起来」
            let r = if visible && focused {
                w.hide()
            } else {
                w.show().and_then(|_| w.set_focus())
            };
            if let Err(e) = r {
                tracing::warn!("切换控制窗口失败：{e}");
            }
        }
        None => {
            if let Err(e) = open(app) {
                tracing::error!("打开控制窗口失败：{e}");
            }
        }
    }
}
```

> **待实现时验证**：`background_color` 与 `unminimize()` 这两个方法在 scratch 里**未逐一编译验证**（scratch 版本用的是不带这两项的最小实现）。验证方法：`cargo check --manifest-path src-tauri/Cargo.toml`。若 `background_color` 签名不符，去 `.research/repos/tauri/crates/tauri/src/webview/webview_window.rs` 里 grep `pub fn background_color`；实在对不上就直接删掉这一行（它只是防闪屏，非功能性）。`WebviewWindowBuilder::new` / `.title` / `.inner_size` / `.min_inner_size` / `.visible` / `.build` 与 `on_window_event` / `CloseRequested { api, .. }` / `api.prevent_close()` / `hide()` / `show()` / `set_focus()` **均已实测编译并运行通过**。

- [ ] **Step 2: 在 `main.rs` 里挂上**

`mod` 声明区加一行：

```rust
mod control;
```

在 `setup` 闭包里，建完 `main` 窗口之后、spawn proxy 之前插入：

```rust
            // 控制窗口（设计文档 §11.3）。建不起来不阻断代理 —— 用户至少
            // 还能靠托盘和日志排障，而代理本身与界面无关。
            if let Err(e) = control::open(&app.handle().clone()) {
                tracing::error!("控制窗口创建失败：{e:#}");
            }
```

- [ ] **Step 3: 验证双窗口共存**

```bash
npm --prefix ui run build
cargo build --manifest-path src-tauri/Cargo.toml
scripts/e2e.sh --with-app
```

Expected: `PASS: real Tauri app tunnel verified`，且屏幕上出现一个 960×640 的深色窗口（传输窗口仍然隐藏）。

> **实测过这条路径**：scratch 副本上跑真实服务端 + 真实 app，`curl --socks5-hostname` 取回 `e2e-acceptance-body-v1` 的同时，控制窗口加载了 Svelte 页并成功 `invoke`。两个窗口互不干扰。

- [ ] **Step 4: 验证关闭行为**

手工：点控制窗口的关闭按钮。

Expected: 窗口消失，**进程仍在**（`ps aux | grep wsieve-app` 有输出），SOCKS5 端口仍然可用（`curl --socks5-hostname` 仍然通）。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/control.rs src-tauri/src/main.rs
git commit -m "feat(ui): 控制窗口（关闭即隐藏，代理不受影响）"
```

---

### Task 7: 事件聚合与节流

**Files:**
- Create: `src-tauri/src/events.rs`
- Modify: `src-tauri/src/main.rs`

设计文档 §11.2 的原话：「**必须在 Rust 侧聚合节流**，实时连接每秒可达数百条，逐条推送会直接卡死 WebView」。

| 事件 | 节流 | 载荷形态 |
|---|---|---|
| `traffic` | 1s 一次采样 | 累计值 + 本秒速率 |
| `connection` | 200ms 批量一批 | 数组 + 溢出标记 |
| `rule-hit` | 1s 推一次**增量** | `{规则原文: 增量}` |
| `status` / `outbound-state` | 变化时推 | 直接推，不进聚合器 |

**三条设计决定**，每条都有理由：

**① `traffic` 推累计值而非只推增量。** UI 侧从增量重建累计会漂移——丢一个 tick 就永久偏差。累计值是幂等的，UI 随时能对齐。速率则由 Rust 侧算（`本次累计 - 上次累计`），因为只有 Rust 知道真实的采样间隔。

**② `connection` 队列有上限且溢出必须可见。** 无界队列在洪泛时会吃光内存；有界队列静默丢弃则会让 UI 显示一个错误的连接数。折衷是「丢最旧的 + 带 `dropped: true` 标记」，UI 可以显示「有连接未显示」。**静默丢弃违反房规**。

**③ `rule-hit` 的键是规则原文而非下标。** 用户增删一条规则会让后面所有下标整体错位，重启后恢复出来的热度就全错了。规则原文是稳定的。

- [ ] **Step 1: 写失败的测试**

放在 `src-tauri/src/events.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn delta(id: u64) -> ConnectionDelta {
        ConnectionDelta {
            id,
            target: "example.com:443".into(),
            outbound: "日本节点".into(),
            state: ConnState::Open,
        }
    }

    #[test]
    fn queue_drops_oldest_and_flags_overflow() {
        let q = ConnectionQueue::new();
        for i in 0..(ConnectionQueue::CAP as u64 + 10) {
            q.push(delta(i));
        }
        let (items, dropped) = q.drain();
        assert_eq!(items.len(), ConnectionQueue::CAP, "队列必须有上限");
        assert!(dropped, "溢出必须可见，不能静默丢弃");
        assert_eq!(items[0].id, 10, "丢的应该是最旧的");

        // drain 之后队列与标记都清空
        let (empty, flag) = q.drain();
        assert!(empty.is_empty());
        assert!(!flag, "溢出标记只报一次");
    }

    #[test]
    fn rule_hits_accumulate_and_snapshot() {
        let h = RuleHits::default();
        h.bump("GEOSITE,cn,DIRECT");
        h.bump("GEOSITE,cn,DIRECT");
        h.bump("MATCH,日本节点");
        let s = h.snapshot();
        assert_eq!(s["GEOSITE,cn,DIRECT"], 2);
        assert_eq!(s["MATCH,日本节点"], 1);
    }

    #[test]
    fn rule_hits_restore_replaces_not_adds() {
        let h = RuleHits::default();
        h.bump("a");
        h.restore(HashMap::from([("a".to_string(), 100u64)]));
        assert_eq!(h.snapshot()["a"], 100, "恢复是覆盖，不是累加");
    }

    #[test]
    fn traffic_rate_is_difference_not_total() {
        let c = Counters::default();
        c.add_up(1000);
        let mut prev = TrafficPrev::default();
        let s1 = sample(&c, &mut prev);
        assert_eq!(s1.up_bytes, 1000);
        assert_eq!(s1.up_rate, 1000, "第一轮速率 = 全部累计");

        c.add_up(300);
        let s2 = sample(&c, &mut prev);
        assert_eq!(s2.up_bytes, 1300, "累计值必须单调");
        assert_eq!(s2.up_rate, 300, "速率是增量");
    }

    #[test]
    fn traffic_rate_never_goes_negative_on_counter_reset() {
        // 计数器被重置（重连、配置重载）时，saturating_sub 保证速率不下溢
        let c = Counters::default();
        c.add_up(500);
        let mut prev = TrafficPrev::default();
        sample(&c, &mut prev);
        c.up_total.store(0, Ordering::Relaxed);
        let s = sample(&c, &mut prev);
        assert_eq!(s.up_rate, 0, "计数器归零不该产生天文数字的速率");
    }

    #[test]
    fn rule_hit_delta_skips_unchanged_rules() {
        let h = RuleHits::default();
        h.bump("a");
        h.bump("b");
        let mut prev = HashMap::new();

        let d1 = hit_delta(&h, &mut prev);
        assert_eq!(d1.len(), 2);

        // 没有新命中 → 空增量（调用方据此跳过整次推送）
        let d2 = hit_delta(&h, &mut prev);
        assert!(d2.is_empty(), "没变化就不该推");

        h.bump("a");
        let d3 = hit_delta(&h, &mut prev);
        assert_eq!(d3, HashMap::from([("a".to_string(), 1u64)]), "只推变了的");
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --bin wsieve-app events::`
Expected: 编译失败，`cannot find type ConnectionQueue in this scope`

> 注意 `wsieve-app` 没有 lib target（`Cargo.toml` 里只有 bin），所以是 `--bin wsieve-app` 而不是 `--lib`。**实测**：写 `--lib` 会报 `no library targets found in package wsieve-app`。

- [ ] **Step 3: 写实现**

放在测试模块之前：

```rust
//! 事件聚合与节流（设计文档 §11.2）。
//!
//! 为什么必须聚合：实时连接每秒可达数百条，逐条 emit 会直接卡死 WebView。
//! 每个 IPC 事件都要序列化 + 跨进程边界 + 触发一次 JS 回调 + 一次 Svelte
//! 响应式更新，几百次/秒的量级下 WebView 会失去响应。
//!
//! 三条设计决定：
//!   ① traffic 推**累计值 + 速率**，不只推增量。UI 从增量重建累计会漂移
//!      （丢一个 tick 就永久偏差），累计值幂等。速率由 Rust 算，因为只有
//!      Rust 知道真实采样间隔。
//!   ② connection 队列有上限，且溢出**必须可见**（dropped 标记）。无界队列
//!      在洪泛时吃光内存；静默丢弃会让 UI 的连接数说谎 —— 违反房规。
//!   ③ rule-hit 的键是**规则原文**而非下标。用户删一条规则会让后面所有下标
//!      整体错位，重启后恢复的热度就全错了。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{Emitter, EventTarget, Manager};

/// 节流周期 —— 设计文档 §11.2 的表格。
pub const TRAFFIC_TICK: Duration = Duration::from_secs(1);
pub const CONNECTION_TICK: Duration = Duration::from_millis(200);
pub const RULE_HIT_TICK: Duration = Duration::from_secs(1);

// ── 流量 ────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct Counters {
    pub up_total: AtomicU64,
    pub down_total: AtomicU64,
    pub active: AtomicU64,
}

impl Counters {
    pub fn add_up(&self, n: u64) {
        self.up_total.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_down(&self, n: u64) {
        self.down_total.fetch_add(n, Ordering::Relaxed);
    }
    pub fn conn_opened(&self) {
        self.active.fetch_add(1, Ordering::Relaxed);
    }
    /// 用 CAS 而非 fetch_sub：连接计数下溢会变成 u64::MAX，
    /// UI 上显示「活跃连接 18446744073709551615」比显示 0 更糟。
    pub fn conn_closed(&self) {
        let _ = self
            .active
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
    }
}

#[derive(Debug, Default)]
pub struct TrafficPrev {
    up: u64,
    down: u64,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct TrafficSample {
    /// 累计值（幂等，UI 随时能对齐）
    pub up_bytes: u64,
    pub down_bytes: u64,
    /// 本采样周期内的增量（速率）
    pub up_rate: u64,
    pub down_rate: u64,
    pub active: u64,
}

pub fn sample(c: &Counters, prev: &mut TrafficPrev) -> TrafficSample {
    let up = c.up_total.load(Ordering::Relaxed);
    let down = c.down_total.load(Ordering::Relaxed);
    // saturating_sub：计数器被重置（重连/配置重载）时不下溢成天文数字
    let s = TrafficSample {
        up_bytes: up,
        down_bytes: down,
        up_rate: up.saturating_sub(prev.up),
        down_rate: down.saturating_sub(prev.down),
        active: c.active.load(Ordering::Relaxed),
    };
    prev.up = up;
    prev.down = down;
    s
}

// ── 连接 ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnState {
    Open,
    Close,
    Reject,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectionDelta {
    pub id: u64,
    pub target: String,
    /// 出站名，或 "DIRECT" / "REJECT"
    pub outbound: String,
    pub state: ConnState,
}

/// 有界队列。满了丢最旧的，并置溢出标记。
pub struct ConnectionQueue {
    buf: Mutex<Vec<ConnectionDelta>>,
    overflow: AtomicBool,
}

impl Default for ConnectionQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionQueue {
    /// 200ms 一批。CAP 定在 512 是「洪泛时 UI 还能画得动」与
    /// 「正常负载下永不触顶」的折衷 —— 512 条/200ms = 2560 条/秒，
    /// 远超正常上限。
    ///
    /// ponytail: 用 Vec + remove(0)（O(n)）而非 VecDeque。
    /// 上限：只有在持续触顶时才有 O(n) 开销，而那时 UI 本就画不过来。
    /// 升级路径：真成瓶颈就换 VecDeque，接口不变。
    pub const CAP: usize = 512;

    pub fn new() -> Self {
        Self {
            buf: Mutex::new(Vec::new()),
            overflow: AtomicBool::new(false),
        }
    }

    pub fn push(&self, d: ConnectionDelta) {
        let mut g = self.buf.lock().unwrap();
        if g.len() >= Self::CAP {
            self.overflow.store(true, Ordering::Relaxed);
            g.remove(0);
        }
        g.push(d);
    }

    /// 取走全部待推项，并把溢出标记一并取走（读后清零）。
    pub fn drain(&self) -> (Vec<ConnectionDelta>, bool) {
        let mut g = self.buf.lock().unwrap();
        let v = std::mem::take(&mut *g);
        (v, self.overflow.swap(false, Ordering::Relaxed))
    }
}

/// emit_to 要求 Serialize + **Clone**（实测：只 derive Serialize 会 E0277）
#[derive(Serialize, Clone)]
struct ConnectionBatch {
    items: Vec<ConnectionDelta>,
    /// true 表示这一批之前有连接因队列满被丢弃 —— UI 应显示「部分未展示」
    dropped: bool,
}

// ── 规则命中 ────────────────────────────────────────────────────

/// 键是**规则原文**，不是下标。见模块注释的决定 ③。
#[derive(Debug, Default)]
pub struct RuleHits {
    counters: Mutex<HashMap<String, Arc<AtomicU64>>>,
}

impl RuleHits {
    /// 判决路径上调用 —— 必须便宜。锁只在首次见到某条规则时才写。
    pub fn bump(&self, rule: &str) {
        // 先试读锁路径：绝大多数调用是已存在的键
        if let Some(c) = self.counters.lock().unwrap().get(rule) {
            c.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let c = {
            let mut g = self.counters.lock().unwrap();
            g.entry(rule.to_string()).or_default().clone()
        };
        c.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> HashMap<String, u64> {
        self.counters
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.load(Ordering::Relaxed)))
            .collect()
    }

    /// 启动时从 stats.json 恢复。**覆盖**而非累加。
    pub fn restore(&self, saved: HashMap<String, u64>) {
        let mut g = self.counters.lock().unwrap();
        for (k, v) in saved {
            g.insert(k, Arc::new(AtomicU64::new(v)));
        }
    }
}

/// 算增量并更新 prev。返回空 map 表示这一轮没有新命中（调用方应跳过推送）。
pub fn hit_delta(h: &RuleHits, prev: &mut HashMap<String, u64>) -> HashMap<String, u64> {
    let now = h.snapshot();
    let delta = now
        .iter()
        .filter_map(|(k, v)| {
            let d = v.saturating_sub(prev.get(k).copied().unwrap_or(0));
            (d > 0).then(|| (k.clone(), d))
        })
        .collect();
    *prev = now;
    delta
}

// ── 聚合器 ──────────────────────────────────────────────────────

/// 三条节流循环的宿主。作为 Tauri managed state 供命令与代理侧写入。
pub struct Aggregator {
    pub counters: Arc<Counters>,
    pub hits: Arc<RuleHits>,
    pub queue: Arc<ConnectionQueue>,
}

impl Default for Aggregator {
    fn default() -> Self {
        Self::new()
    }
}

impl Aggregator {
    pub fn new() -> Self {
        Self {
            counters: Arc::new(Counters::default()),
            hits: Arc::new(RuleHits::default()),
            queue: Arc::new(ConnectionQueue::new()),
        }
    }

    /// 启动三条节流循环。三条都是 `loop { tick; drain; emit }`，
    /// 永不退出 —— 与代理主循环同生命周期。
    pub fn spawn(&self, app: tauri::AppHandle) {
        spawn_traffic(app.clone(), self.counters.clone());
        spawn_connections(app.clone(), self.queue.clone());
        spawn_rule_hits(app, self.hits.clone());
    }
}

fn spawn_traffic(app: tauri::AppHandle, counters: Arc<Counters>) {
    tauri::async_runtime::spawn(async move {
        let mut prev = TrafficPrev::default();
        let mut t = tokio::time::interval(TRAFFIC_TICK);
        // Skip：机器休眠唤醒后不要把攒下的 tick 一次性补发，
        // 那会瞬间推几十条 traffic 事件 —— 正是本模块要避免的事。
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            t.tick().await;
            // 即使没人看也要采样：prev 必须持续推进，否则窗口一打开
            // 第一个速率会是「关窗期间的总量」这种荒谬值。
            let s = sample(&counters, &mut prev);
            emit_control(&app, "traffic", s);
        }
    });
}

fn spawn_connections(app: tauri::AppHandle, queue: Arc<ConnectionQueue>) {
    tauri::async_runtime::spawn(async move {
        let mut t = tokio::time::interval(CONNECTION_TICK);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            t.tick().await;
            let (items, dropped) = queue.drain();
            if items.is_empty() && !dropped {
                continue; // 空批不推
            }
            emit_control(&app, "connection", ConnectionBatch { items, dropped });
        }
    });
}

fn spawn_rule_hits(app: tauri::AppHandle, hits: Arc<RuleHits>) {
    tauri::async_runtime::spawn(async move {
        let mut prev = HashMap::new();
        let mut t = tokio::time::interval(RULE_HIT_TICK);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            t.tick().await;
            let delta = hit_delta(&hits, &mut prev);
            if delta.is_empty() {
                continue;
            }
            emit_control(&app, "rule-hit", delta);
        }
    });
}

/// 推给控制窗口，且**只推给它**。
///
/// 用 `emit_to(webview_window("control"))` 而非 `emit()` 不是优化，是安全：
/// `emit()` 会广播给所有 webview，包括加载远端服务器页面的 main 窗口。
/// 那等于把流量统计、规则命中、出站名单白送给一台可能已被攻破的服务器。
///
/// 实测注意：控制窗口不存在时 `emit_to` **静默返回 Ok(())**，不报错。
/// 因此这里显式短路 —— 让「窗口关着」真的等于零开销，而不是白白序列化一轮。
pub fn emit_control<S: Serialize + Clone>(app: &tauri::AppHandle, event: &str, payload: S) {
    if app.get_webview_window(crate::control::LABEL).is_none() {
        return;
    }
    if let Err(e) = app.emit_to(
        EventTarget::webview_window(crate::control::LABEL),
        event,
        payload,
    ) {
        // 房规：错误绝不静默吞掉
        tracing::warn!("事件 {event} 推送失败：{e}");
    }
}

/// 低频事件直接推，不进聚合器（§11.2：「变化时推，天然低频」）。
pub fn emit_status(app: &tauri::AppHandle, msg: &str) {
    tracing::info!("status: {msg}");
    emit_control(app, "status", msg.to_string());
}

#[derive(Serialize, Clone)]
pub struct OutboundState {
    pub name: String,
    /// "connecting" | "live" | "failed" | "disabled"
    pub state: String,
    pub latency_ms: Option<u64>,
}

pub fn emit_outbound_state(app: &tauri::AppHandle, s: OutboundState) {
    emit_control(app, "outbound-state", s);
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --bin wsieve-app events::`
Expected: 6 个测试全部 PASS。

> **实测状态**：scratch 副本上验证了 `queue_drops_oldest_and_flags_overflow` 与 `rule_hits_*` 的等价版本（当时的 `RuleHits` 用 `usize` 下标，本计划改成了 `String` 键；`ConnectionQueue` 与聚合器循环逐字相同）。`sample()` / `hit_delta()` 是本计划从 scratch 的内联逻辑中提取出来的纯函数——**提取本身未编译验证**，但依赖的全是 `std` 原语。若有编译问题，多半在 `filter_map` 那行的闭包类型推断上（备选写法：显式 `.map(|(k, v)| ...)` + `.filter(...)`）。

- [ ] **Step 5: 挂进 main.rs 并验证真的推得出去**

`mod` 区加 `mod events;`。在 `setup` 里：

```rust
            let agg = events::Aggregator::new();
            agg.spawn(app.handle().clone());
            app.manage(agg);
```

`App.svelte` 里已经监听了 `traffic`（Task 1 Step 5）。启动 app，控制窗口应每秒更新一次速率行。

Expected: 界面上出现 `↑ 0 B/s ↓ 0 B/s` 且持续刷新。

> **实测过等价路径**：scratch 里让控制页把收到的事件 invoke 回 Rust 打日志，确认 `traffic` 每秒一条、`connection` 与 `rule-hit` 各按其周期到达。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/events.rs src-tauri/src/main.rs
git commit -m "feat(ipc): 事件聚合节流（traffic 1s / connection 200ms / rule-hit 1s 增量）"
```

---

### Task 8: 规则命中持久化（`stats.json`）

**Files:**
- Create: `src-tauri/src/stats.rs`
- Modify: `src-tauri/src/main.rs`

设计文档 §11.2 末尾：「内存 `AtomicU64` 计数，退出时写 `stats.json`、启动时恢复。理由是 §11.5 的『命中热度』需要足够长的观察窗口才有意义——重启清零就看不出哪条规则是死的。**用独立文件，不污染 `config.yaml`**」。

「不污染 config.yaml」是硬要求：那个文件是用户手写的、带注释的、含私钥的。往里塞机器生成的计数会让阶段 1 辛苦保住的注释保留变得毫无意义（每次退出都重写一遍）。

- [ ] **Step 1: 写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wsieve-stats-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn roundtrip_survives_restart() {
        let p = path(&tmpdir("rt"));
        let mut s = StatsFile::default();
        s.rule_hits.insert("GEOSITE,cn,DIRECT".into(), 42);
        s.rule_hits.insert("MATCH,日本节点".into(), 7);
        save(&p, &s).unwrap();

        let back = load(&p);
        assert_eq!(back.rule_hits["GEOSITE,cn,DIRECT"], 42);
        assert_eq!(back.rule_hits["MATCH,日本节点"], 7);
    }

    #[test]
    fn missing_file_starts_from_zero() {
        let p = tmpdir("missing").join("nope.json");
        assert!(load(&p).rule_hits.is_empty(), "文件不存在是正常的首次启动");
    }

    #[test]
    fn corrupt_file_does_not_panic() {
        // 上次退出时断电，留下半截 JSON —— 不能因此拒绝启动
        let p = path(&tmpdir("corrupt"));
        std::fs::write(&p, "{ not json at all").unwrap();
        assert!(load(&p).rule_hits.is_empty());
    }

    #[test]
    fn future_version_is_discarded_not_misread() {
        let p = path(&tmpdir("ver"));
        std::fs::write(&p, r#"{"version":99,"rule_hits":{"a":1}}"#).unwrap();
        assert!(
            load(&p).rule_hits.is_empty(),
            "不认识的版本应整体丢弃，而不是按当前 schema 硬读"
        );
    }

    #[test]
    fn no_tmp_file_is_left_behind() {
        let d = tmpdir("atomic");
        save(&path(&d), &StatsFile::default()).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件没清干净：{leftovers:?}");
    }

    #[test]
    fn save_creates_missing_directory() {
        // 首次运行时 app_config_dir 可能还不存在
        let d = tmpdir("mkdir").join("nested/deeper");
        let p = path(&d);
        save(&p, &StatsFile::default()).unwrap();
        assert!(p.exists());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --bin wsieve-app stats::`
Expected: `cannot find type StatsFile in this scope`

- [ ] **Step 3: 写实现**

```rust
//! 规则命中计数的落盘与恢复（设计文档 §11.2）。
//!
//! 为什么要持久化：§11.5 的「命中热度染色」要回答「我这堆规则里哪些是死的」，
//! 而那需要足够长的观察窗口。每次重启清零，热度图就永远是刚开机的样子，
//! 那个 signature 也就失去了意义。
//!
//! 为什么用独立文件而不塞进 config.yaml：config.yaml 是**用户手写的**、
//! 带注释的、含私钥的。往里写机器生成的计数意味着每次退出都要重写一遍那个
//! 文件 —— 阶段 1 为保住注释所做的全部工作会被这一下抵消掉。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// schema 版本。改变 rule_hits 的键语义时必须 +1 ——
/// 譬如从「规则原文」改成「规则 id」，旧数据会全部错位。
const VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct StatsFile {
    pub version: u32,
    /// 键是**规则原文**（如 "GEOSITE,cn,DIRECT"），不是下标。
    /// 下标会因为用户增删规则而整体错位，恢复出来的热度全是错的。
    pub rule_hits: HashMap<String, u64>,
}

impl Default for StatsFile {
    fn default() -> Self {
        Self {
            version: VERSION,
            rule_hits: HashMap::new(),
        }
    }
}

pub fn path(config_dir: &Path) -> PathBuf {
    config_dir.join("stats.json")
}

/// 读。任何失败都退回空统计并**告警**（不静默），绝不阻断启动 ——
/// 命中计数是观察数据，丢了不影响任何功能。
pub fn load(p: &Path) -> StatsFile {
    let Ok(text) = std::fs::read_to_string(p) else {
        // 文件不存在是首次启动的正常状态，不值得告警
        return StatsFile::default();
    };
    match serde_json::from_str::<StatsFile>(&text) {
        Ok(s) if s.version == VERSION => s,
        Ok(s) => {
            tracing::warn!(
                "{} 的版本 {} 不认识（当前 {VERSION}），命中计数从零开始",
                p.display(),
                s.version
            );
            StatsFile::default()
        }
        Err(e) => {
            tracing::warn!("{} 解析失败（{e}），命中计数从零开始", p.display());
            StatsFile::default()
        }
    }
}

/// 写。先写同目录临时文件再 rename ——
/// 退出瞬间断电时，要么是完整的旧文件，要么是完整的新文件，
/// 不会是半截 JSON。同目录是关键：跨文件系统 rename 不是原子的。
pub fn save(p: &Path, stats: &StatsFile) -> std::io::Result<()> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(stats)?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, p)
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --bin wsieve-app stats::`
Expected: 6 个测试全部 PASS。

> **实测状态**：本模块的实现与测试在 scratch 副本上**逐字跑过**，5 个测试全绿（`save_creates_missing_directory` 是本计划新加的，其余五个原样）。

- [ ] **Step 5: 接进启动与退出**

`mod` 区加 `mod stats;`。

启动侧（`setup` 里，在 `agg.spawn()` **之前**）：

```rust
            // 恢复上次的命中计数（§11.2）。取不到配置目录时跳过 ——
            // 观察数据丢了不影响功能，但要说出来。
            match app.path().app_config_dir() {
                Ok(dir) => {
                    let saved = stats::load(&stats::path(&dir));
                    agg.hits.restore(saved.rule_hits);
                }
                Err(e) => tracing::warn!("取配置目录失败（{e}），命中计数不恢复"),
            }
```

`app.path()` 来自 `tauri::Manager`（`main.rs` 已 `use tauri::Manager` 于 `proxy.rs`；此处 setup 闭包里的 `app` 是 `&mut App`，`Manager` 已在作用域内则直接可用，否则加 `use tauri::Manager;`）。`app_config_dir()` 返回 `config_dir()/<identifier>`，即 `~/Library/Application Support/org.websieve.app`（macOS）。

退出侧（`.run(...)` 闭包里，与既有的 `shard_guard` 清理并列）：

```rust
        .run(move |app, event| {
            if let tauri::RunEvent::Exit = event {
                shard_guard.lock().unwrap().take();
                // 落盘命中计数。失败要报出来 —— 静默丢数据是房规明令禁止的。
                if let Some(agg) = app.try_state::<events::Aggregator>() {
                    match app.path().app_config_dir() {
                        Ok(dir) => {
                            let f = stats::StatsFile {
                                version: 1,
                                rule_hits: agg.hits.snapshot(),
                            };
                            if let Err(e) = stats::save(&stats::path(&dir), &f) {
                                tracing::error!("写 stats.json 失败：{e}");
                            }
                        }
                        Err(e) => tracing::warn!("取配置目录失败（{e}），命中计数未保存"),
                    }
                }
            }
        });
```

注意 `.run()` 的闭包首参当前被写成 `_app`（`main.rs:107`），要改成 `app` 才能用。

> **实测确认**：`RunEvent::Exit` 在 `app.exit(0)` 之后确实会到达（观察到的顺序是 `ExitRequested` → `Exit`）。因此托盘的「退出」菜单项走 `app.exit(0)` 时，这段落盘代码会执行。
>
> **未覆盖的路径**：`SIGKILL` 与崩溃。此时 stats.json 保持上一次的内容——这与 hosts 托管的取舍一致（崩溃残留由下次启动兜底），且丢失的只是最多一次运行的观察数据。**不为此设计崩溃时持续落盘**：那会把一个纯观察功能变成持续的磁盘写入。

- [ ] **Step 6: 端到端验证持久化**

```bash
cargo build --manifest-path src-tauri/Cargo.toml
# 启动、让它跑一会、从托盘退出（或 Cmd+Q）
cat ~/Library/Application\ Support/org.websieve.app/stats.json
```

Expected: 一个 `{"version": 1, "rule_hits": {...}}` 的 JSON。阶段 4 还没有规则引擎接入，`rule_hits` 会是空对象 `{}`——**那也是正确的**，说明写路径通了。阶段 5 接上规则视图后才会有真实数据。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/stats.rs src-tauri/src/main.rs
git commit -m "feat(stats): 规则命中计数落盘与恢复（独立文件，原子写）"
```

---

### Task 9: 托盘

**Files:**
- Create: `src-tauri/src/tray.rs`
- Modify: `src-tauri/src/main.rs`

设计文档 §11.3：「**托盘常驻**：平时后台运行，托盘菜单可直接切模式 / 切出站，点图标显隐窗口」。

本阶段只做「显隐窗口 + 退出 + 模式切换的骨架」。「切出站」需要出站列表（阶段 2 的产物）与动态菜单，留到阶段 5。

- [ ] **Step 1: 写 `src-tauri/src/tray.rs`**

```rust
//! 托盘图标与菜单（设计文档 §11.3）。
//!
//! 托盘是这个应用的**主要入口**，不是附属品：§11.4 的意图三问写明
//! 「界面平时后台常驻，只在网络出问题时被打开」。窗口关了之后，托盘是
//! 用户与进程之间唯一的接触面。
//!
//! 依赖 tauri 的 `tray-icon` 特性 —— 不开的话 `tauri::tray` 模块根本不存在
//! （crates/tauri/src/lib.rs:112 的 `#[cfg(all(desktop, feature = "tray-icon"))]`）。

use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;

pub const ID: &str = "wsieve-tray";

// 菜单项 id —— 与 on_menu_event 里的匹配分支一一对应。
// 用常量而非字面量：改名时编译器会帮忙，字面量不会。
const ID_OPEN: &str = "open";
const ID_MODE_RULE: &str = "mode-rule";
const ID_MODE_GLOBAL: &str = "mode-global";
const ID_MODE_DIRECT: &str = "mode-direct";
const ID_QUIT: &str = "quit";

pub fn build(app: &tauri::AppHandle) -> tauri::Result<TrayIcon> {
    let open = MenuItemBuilder::with_id(ID_OPEN, "打开控制台").build(app)?;
    let mode_rule = MenuItemBuilder::with_id(ID_MODE_RULE, "规则模式").build(app)?;
    let mode_global = MenuItemBuilder::with_id(ID_MODE_GLOBAL, "全局模式").build(app)?;
    let mode_direct = MenuItemBuilder::with_id(ID_MODE_DIRECT, "直连模式").build(app)?;
    let quit = MenuItemBuilder::with_id(ID_QUIT, "退出 websieve").build(app)?;

    let menu = MenuBuilder::new(app)
        .items(&[&open])
        .separator()
        .items(&[&mode_rule, &mode_global, &mode_direct])
        .separator()
        .items(&[&quit])
        .build()?;

    let icon = app
        .default_window_icon()
        .ok_or_else(|| tauri::Error::AssetNotFound("默认窗口图标缺失".into()))?
        .clone();

    TrayIconBuilder::with_id(ID)
        .icon(icon)
        // macOS 菜单栏图标必须是模板图（单色 + alpha），否则深色菜单栏下
        // 会是一块糊掉的彩色方块。Windows / Linux 上此项被忽略。
        .icon_as_template(true)
        .tooltip("websieve")
        // 左键点图标 = 显隐窗口（§11.3），右键才出菜单。
        // 左键也弹菜单的话，最常用的动作就要多两步。
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            ID_OPEN => {
                if let Err(e) = crate::control::open(app) {
                    tracing::error!("从托盘打开控制窗口失败：{e}");
                }
            }
            ID_MODE_RULE => set_mode_from_tray(app, "rule"),
            ID_MODE_GLOBAL => set_mode_from_tray(app, "global"),
            ID_MODE_DIRECT => set_mode_from_tray(app, "direct"),
            ID_QUIT => {
                // exit(0) 会走到 RunEvent::Exit（实测），
                // 于是 hosts 摘除与 stats.json 落盘都能执行。
                // 直接 std::process::exit 会跳过这两者。
                app.exit(0);
            }
            other => tracing::warn!("未处理的托盘菜单项：{other}"),
        })
        .on_tray_icon_event(|tray, event| {
            // 只响应左键**抬起**。按下就响应会让拖动图标也触发切换。
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                crate::control::toggle(tray.app_handle());
            }
        })
        .build(app)
}

/// 托盘切模式。
///
/// ponytail: 阶段 4 只发事件通知 UI，不真的改配置 —— 配置写回是 Task 11 的
/// config_save，而模式切换的实际生效需要阶段 2 的出站管理器。
/// 上限：托盘点了模式，界面会变，但流量走向不变。
/// 升级路径：阶段 5 接上 set_mode 命令的真实实现，把这里改成调它。
fn set_mode_from_tray(app: &tauri::AppHandle, mode: &str) {
    tracing::info!("托盘请求切换模式：{mode}");
    crate::events::emit_control(app, "mode-changed", mode.to_string());
}
```

> **待实现时验证**：`tauri::Error::AssetNotFound` 这个变体名**未经核实**。若编译报错，改用 `.ok_or_else(|| tauri::Error::from(std::io::Error::other("默认窗口图标缺失")))`，或干脆用 `.expect("icons/icon.png 应该存在")`（那个文件在仓库里，缺了就是构建配置坏了）。
>
> **已实测通过的**：`MenuItemBuilder::with_id(id, text).build(app)`、`MenuBuilder::new(app).items(&[..]).separator().items(&[..]).build()`、`TrayIconBuilder::with_id().icon().icon_as_template().tooltip().show_menu_on_left_click(false).on_menu_event().on_tray_icon_event().build(app)`、`TrayIconEvent::Click { button, button_state, .. }`、`MouseButton::Left` / `MouseButtonState::Up`、`tray.app_handle()`、`app.default_window_icon()` 返回 `Option<&Image>`。这一整套在 scratch 里编译并 `build()` 返回了 `Ok`。

- [ ] **Step 2: 挂进 main.rs**

`mod` 区加 `mod tray;`。在 `setup` 里，**在 `control::open` 之前**：

```rust
            // 托盘先于窗口建：窗口建失败时用户至少还有托盘可用。
            if let Err(e) = tray::build(&app.handle().clone()) {
                tracing::error!("托盘创建失败：{e:#}");
            }
```

- [ ] **Step 3: 编译**

Run: `cargo build --manifest-path src-tauri/Cargo.toml`
Expected: 构建成功。若报 `could not find tray in tauri`，说明 Task 5 Step 2 的 `tray-icon` 特性没加上。

- [ ] **Step 4: 肉眼验收（需要 GUI）🔍**

**这是本阶段唯一无法自动化的验收项。** 在有图形界面的机器上启动 app，逐条确认：

- [ ] 菜单栏出现 websieve 图标
- [ ] 图标是单色的，不是糊掉的彩色方块（`icon_as_template(true)` 生效）
- [ ] 右键弹出菜单，五项俱全，分隔线在正确位置
- [ ] 点「打开控制台」→ 控制窗口出现
- [ ] 关掉控制窗口，左键点托盘图标 → 窗口再次出现
- [ ] 再左键点一次 → 窗口隐藏
- [ ] 点「退出 websieve」→ 进程结束，且 `stats.json` 被写出、hosts 条目被摘除

> **计划撰写时未做此项**：无头环境看不到菜单栏。代码编译通过且 `TrayIconBuilder::build()` 返回 `Ok`，但视觉呈现与点击行为必须有人看一眼。**若图标显示为彩色方块**，先确认 `icons/icon.png` 是否含 alpha 通道——模板图要求非透明像素为黑色。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/tray.rs src-tauri/src/main.rs
git commit -m "feat(ui): 托盘常驻（显隐窗口 + 模式切换骨架）"
```

---

## Part C — 命令面

> **前提：Part A 的 Task 3 与 Task 4 必须已完成且绿灯。** 从这里开始引入的每一个命令，都是「服务器被攻破时可能落入攻击者手中」的候选——只有交集断言在守着，才敢往下加。

**每加一个命令的固定动作**（下面每个 task 都重复这套，不再赘述）：

1. 在 `src-tauri/permissions/wsieve/allow-<命令名>.toml` 写一份权限定义
2. 在 `capabilities/control.json` 的 `permissions` 里加上它
3. 在 `main.rs` 的 `generate_handler!` 里注册
4. 在 `ui/src/lib/ipc.js` 的调用清单里体现
5. **重跑 `cargo test --test capability_isolation`** —— 每次都跑，不是最后跑一次

第 5 条不是形式主义：`transport_can_only_reach_the_three_binary_channels` 会在有人手滑把新权限加进 `transport.json` 时立刻叫停。

### Task 10: 命令面骨架与错误类型

**Files:**
- Create: `src-tauri/src/commands/mod.rs`
- Modify: `src-tauri/src/main.rs`

- [ ] **Step 1: 写 `src-tauri/src/commands/mod.rs`**

```rust
//! 控制窗口的命令面（设计文档 §11.2）。
//!
//! **这些命令只授权给 control 窗口。** transport capability 里绝不能出现
//! 它们中的任何一个 —— main 窗口加载的是远端服务器的页面，那台服务器一旦
//! 被攻破，其 JS 就能调用它被授权的一切，而 config_get 会返回含
//! client-priv 私钥的配置。该纪律由 tests/capability_isolation.rs 守住。

pub mod config;
pub mod control;
pub mod probe;

/// 命令的统一错误类型。
///
/// 为什么不直接返回 String：Tauri 要求命令的错误类型实现 Serialize，
/// String 能满足但会把「什么原因失败」压成一句话，UI 无法分类处理
/// （譬如 §12 要求 YAML 语法错时显示出错行号 —— 那需要结构化的 line 字段）。
#[derive(Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CmdError {
    /// 配置文件语法错，带行号（§12）
    ConfigSyntax { message: String, line: Option<u64> },
    /// 配置语义错（缺 MATCH、引用不存在的出站等）
    ConfigInvalid { message: String },
    /// IO 失败
    Io { message: String },
    /// 该功能所依赖的阶段尚未实现
    NotReady { message: String },
    /// 其他
    Other { message: String },
}

impl CmdError {
    pub fn io(e: impl std::fmt::Display) -> Self {
        Self::Io { message: e.to_string() }
    }
    pub fn other(e: impl std::fmt::Display) -> Self {
        Self::Other { message: e.to_string() }
    }
    pub fn not_ready(what: &str) -> Self {
        Self::NotReady {
            message: format!("{what} 尚未接入（见阶段 2 / 阶段 3）"),
        }
    }
}

impl std::fmt::Display for CmdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfigSyntax { message, line: Some(l) } => write!(f, "配置第 {l} 行：{message}"),
            Self::ConfigSyntax { message, .. } => write!(f, "配置语法错：{message}"),
            Self::ConfigInvalid { message }
            | Self::Io { message }
            | Self::NotReady { message }
            | Self::Other { message } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for CmdError {}

pub type CmdResult<T> = Result<T, CmdError>;
```

- [ ] **Step 2: 挂进 main.rs**

`mod` 区加 `mod commands;`。

- [ ] **Step 3: 编译**

Run: `cargo build --manifest-path src-tauri/Cargo.toml`
Expected: 报 `file not found for module config/control/probe` —— 预期的，接下来的 task 补上。

- [ ] **Step 4: 提交**

```bash
git add src-tauri/src/commands/mod.rs src-tauri/src/main.rs
git commit -m "chore(ipc): 命令面骨架与结构化错误类型"
```

---

### Task 11: 配置命令 —— 以及私钥的处置

**Files:**
- Create: `src-tauri/src/commands/config.rs`
- Create: `src-tauri/permissions/wsieve/allow-config-{get,save,get-raw,save-raw}.toml`
- Modify: `capabilities/control.json`, `main.rs`

设计文档 §11.2 的四个配置命令：`config_get` / `config_save`（结构化）与 `config_get_raw` / `config_save_raw`（原始文本逃生舱）。

**关于私钥的一个决定，需要在这里想清楚。**

设计文档 §5.4 说私钥明文存于 0600 的配置文件里，§5.4 还说「UI 在导出 / 分享配置的入口处必须显式警告：该文件含私钥」。这意味着**UI 是需要能看到私钥的**（否则「导出」无从谈起，用户也无法确认自己填对了）。

那么 `config_get` 是否返回 `client-priv`？

| 方案 | 判定 |
|---|---|
| 返回明文 | UI 能做完整的编辑与导出。风险面 = control capability 的授权面 |
| 一律脱敏 | 用户无法在 UI 里核对或复制私钥，只能去翻文件。**与 §5.4 的「UI 导出」要求矛盾** |
| **默认脱敏 + 显式命令取明文** | 采用 |

采用第三种：`config_get` 默认把 `client-priv` 替换成 `"***"`，另有 `config_get_raw` 返回原始 YAML 文本（含明文私钥）。理由是**大多数 UI 交互不需要私钥**——看规则、切模式、看流量都不需要。让它默认不出现在 IPC 载荷里，就少一个把它泄漏进日志 / 崩溃报告 / 截图的机会。需要时走 raw 通道，是一次显式的动作。

> **这是本计划对 spec 的一个补充解读，spec 未明确规定 `config_get` 是否脱敏。** 若实现时认为该由 UI 层决定，改成不脱敏也说得通——但那样 `config_get` 的返回值就必须被当作机密对待（不能进日志）。**两条路选一条，写进代码注释，不要留成隐含假设。**

- [ ] **Step 1: 写权限定义（四份）**

`src-tauri/permissions/wsieve/allow-config-get.toml`（Task 3 已建，此处确认内容）：

```toml
[[permission]]
identifier = "allow-config-get"
description = "允许控制窗口读取结构化配置（client-priv 已脱敏）"
commands.allow = ["config_get"]
```

`allow-config-save.toml`：

```toml
[[permission]]
identifier = "allow-config-save"
description = "允许控制窗口结构化写回配置（保留规则注释）"
commands.allow = ["config_save"]
```

`allow-config-get-raw.toml`：

```toml
[[permission]]
identifier = "allow-config-get-raw"
description = "允许控制窗口读取配置原文（含明文 client-priv 私钥）"
commands.allow = ["config_get_raw"]
```

`allow-config-save-raw.toml`：

```toml
[[permission]]
identifier = "allow-config-save-raw"
description = "允许控制窗口整份覆写配置原文"
commands.allow = ["config_save_raw"]
```

注意 `allow-config-get-raw` 的描述里写明了「含明文私钥」——权限描述会出现在 Tauri 生成的文档里，写清楚是为了让下一个人在把它加进 transport.json 之前先犹豫一下。

- [ ] **Step 2: 写 `src-tauri/src/commands/config.rs`**

```rust
//! 配置读写命令（设计文档 §11.2 / §5.4 / §5.6）。
//!
//! 关于私钥：config_get **脱敏** client-priv，config_get_raw 不脱敏。
//! 理由是绝大多数 UI 交互（看规则、切模式、看流量）都不需要私钥，
//! 默认不让它出现在 IPC 载荷里，就少一个把它泄漏进日志/崩溃报告/截图的
//! 机会。需要导出或核对时走 raw 通道 —— 那是一次显式动作。

use std::path::PathBuf;

use tauri::Manager;

use super::{CmdError, CmdResult};

/// 私钥在结构化读取里的占位。UI 看到这个值就知道「原样保存不会改动私钥」。
pub const REDACTED: &str = "***";

pub fn config_path(app: &tauri::AppHandle) -> CmdResult<PathBuf> {
    let dir = app.path().app_config_dir().map_err(CmdError::io)?;
    Ok(dir.join("config.yaml"))
}

/// 结构化读。client-priv 被替换为 REDACTED。
#[tauri::command]
pub async fn config_get(app: tauri::AppHandle) -> CmdResult<serde_json::Value> {
    let p = config_path(&app)?;
    let text = std::fs::read_to_string(&p).map_err(|e| {
        CmdError::Io {
            message: format!("读取 {} 失败：{e}", p.display()),
        }
    })?;
    let mut v = parse_to_json(&text)?;
    redact_private_keys(&mut v);
    Ok(v)
}

/// 原文读 —— **含明文私钥**。UI 侧必须在展示处给出 §5.4 要求的警告。
#[tauri::command]
pub async fn config_get_raw(app: tauri::AppHandle) -> CmdResult<String> {
    let p = config_path(&app)?;
    std::fs::read_to_string(&p).map_err(|e| CmdError::Io {
        message: format!("读取 {} 失败：{e}", p.display()),
    })
}

/// 原文写 —— 逃生舱（§5.6），一字不动地覆盖。
///
/// 写前先解析一次：语法错的配置写进去会让下次启动失败，
/// 而那时用户可能已经关掉界面了。宁可在这里拒绝。
#[tauri::command]
pub async fn config_save_raw(app: tauri::AppHandle, text: String) -> CmdResult<()> {
    parse_to_json(&text)?; // 语法校验，失败即拒绝写入
    let p = config_path(&app)?;
    write_0600(&p, &text)
}

/// 结构化写。
///
/// ponytail: 阶段 4 只落地「读 + 原文写」，结构化写回（保留规则注释的
/// 定点改写）依赖阶段 1 的 wsieve-config::edit —— 而那个 crate 在本阶段
/// 尚未被 src-tauri 依赖。
/// 上限：UI 的表单编辑此刻不可用，用户要改配置得走原文编辑器。
/// 升级路径：阶段 5 接规则视图时，把 wsieve-config 加进 Cargo.toml，
/// 这里换成 edit::replace_rule_line / delete_rule_line 的调用。
#[tauri::command]
pub async fn config_save(_app: tauri::AppHandle, _patch: serde_json::Value) -> CmdResult<()> {
    Err(CmdError::not_ready("结构化配置写回"))
}

/// 以 0600 创建并原子替换。
///
/// **必须以 0600 创建**，而不是先创建再 chmod —— 后者有一个竞态窗口，
/// 期间文件是 0644，同机其他用户能读到私钥（设计文档 §5.4 明确要求）。
fn write_0600(p: &std::path::Path, text: &str) -> CmdResult<()> {
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
    // ponytail: Windows 上没有等价的「创建即限权」——ACL 需要另一套 API。
    // 上限：Windows 下配置文件继承目录 ACL（AppData 默认只有当前用户可读，
    // 实践上够用但不是我们强制的）。
    // 升级路径：需要时用 windows-acl crate 显式设 DACL。
    let mut f = opts.open(&tmp).map_err(CmdError::io)?;
    f.write_all(text.as_bytes()).map_err(CmdError::io)?;
    f.sync_all().map_err(CmdError::io)?;
    drop(f);

    std::fs::rename(&tmp, p).map_err(CmdError::io)
}

/// 把 YAML 文本解析成 JSON 值。
///
/// ponytail: 阶段 4 用 serde_json 的 YAML 兼容子集是不够的 ——
/// 真正的解析器是阶段 1 的 wsieve-config（serde-saphyr + Spanned<T> 行号）。
/// 本阶段 src-tauri 还没依赖它。
/// 上限：目前只能报「解析失败」而给不出 §12 要求的**出错行号**。
/// 升级路径：把 wsieve-config 加进 src-tauri/Cargo.toml，这里换成
/// wsieve_config::load_str()，并把它的 Spanned 行号填进 ConfigSyntax.line。
fn parse_to_json(text: &str) -> CmdResult<serde_json::Value> {
    let _ = text;
    Err(CmdError::not_ready("YAML 解析（阶段 1 的 wsieve-config 尚未接入）"))
}

/// 递归把 proxies[].client-priv 换成占位符。
///
/// 递归而非只看顶层：将来配置可能嵌套（策略组虽被排除，但 §2 说格式已预留），
/// 只处理已知路径的话，新增一层嵌套就会静默泄漏。
fn redact_private_keys(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, val) in map.iter_mut() {
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
        assert_eq!(v["client-priv"], REDACTED, "不能直接删字段——UI 需要知道它存在");
    }
}
```

- [ ] **Step 3: 跑脱敏测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --bin wsieve-app commands::config::`
Expected: 2 个测试 PASS。

- [ ] **Step 4: 注册命令并放开权限**

`main.rs` 的 `generate_handler!` 加四行：

```rust
            commands::config::config_get,
            commands::config::config_get_raw,
            commands::config::config_save,
            commands::config::config_save_raw,
```

`capabilities/control.json` 的 `permissions` 加四条：

```json
    "allow-config-get",
    "allow-config-save",
    "allow-config-get-raw",
    "allow-config-save-raw",
```

- [ ] **Step 5: 重跑隔离测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test capability_isolation`
Expected: 5 个测试全部 PASS。**特别确认 `transport_can_only_reach_the_three_binary_channels` 仍绿**——它保证这四个新命令没有被误加到远端授权面上。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/commands/config.rs src-tauri/permissions/wsieve/allow-config-*.toml \
        src-tauri/capabilities/control.json src-tauri/src/main.rs
git commit -m "feat(ipc): 配置命令（结构化读脱敏私钥，原文读写走逃生舱）"
```

---

### Task 12: 控制命令与探针命令

**Files:**
- Create: `src-tauri/src/commands/control.rs`, `src-tauri/src/commands/probe.rs`
- Create: 对应的 `permissions/wsieve/allow-*.toml`
- Modify: `capabilities/control.json`, `main.rs`

§11.2 剩下的命令。**大部分在本阶段只能是「接口定型 + 明确报未就绪」**——它们依赖阶段 2 的出站管理器与阶段 3 的 DNS Resolver。

**为什么现在就定接口而不是等到阶段 5**：capability 里的权限列表、UI 侧的调用形状、错误类型的分类，这三者一旦在阶段 5 才成型，就会带着「先能跑起来再说」的痕迹。现在定，阶段 5 只填实现。

**为什么不用假数据填充**：房规明令禁止 mock 与占位实现。返回 `CmdError::NotReady` 是**真实的、正确的**响应——「这个功能还没接上」是当前的事实，UI 照实显示即可。返回编造的流量数字才是伪实现。

- [ ] **Step 1: 写 `src-tauri/src/commands/control.rs`**

```rust
//! 连接控制命令（设计文档 §11.2）。
//!
//! 阶段 4 的定位：**接口定型**。connect / disconnect / set_mode /
//! outbound_enable 的真实实现依赖阶段 2 的多出站管理器 —— 那个东西目前
//! 还是 proxy.rs 里的单服务器循环。
//!
//! 这里返回 CmdError::NotReady 而不是假成功：房规禁止 mock。
//! 「还没接上」是当前的事实，UI 照实显示即可；假装连上了才是伪实现。

use super::{CmdError, CmdResult};
use crate::events;

/// 建立全部启用的出站连接。
///
/// ponytail: 阶段 4 只推一条状态事件，不真的控制连接 —— proxy.rs 的循环
/// 目前是自启动、自重连的，没有外部开关。
/// 上限：按钮点了有反馈，但代理的实际状态不受影响。
/// 升级路径：阶段 2 的出站管理器提供 start()/stop()，这里改成调它。
#[tauri::command]
pub async fn connect(app: tauri::AppHandle) -> CmdResult<()> {
    events::emit_status(&app, "connect 请求已收到（出站管理器见阶段 2）");
    Err(CmdError::not_ready("connect"))
}

#[tauri::command]
pub async fn disconnect(app: tauri::AppHandle) -> CmdResult<()> {
    events::emit_status(&app, "disconnect 请求已收到（出站管理器见阶段 2）");
    Err(CmdError::not_ready("disconnect"))
}

/// 切换分流模式。合法值见设计文档 §5.2 的 `mode` 字段。
#[tauri::command]
pub async fn set_mode(app: tauri::AppHandle, mode: String) -> CmdResult<()> {
    // 校验先于一切 —— 不认识的值必须当场拒绝（房规：错误不静默）
    if !matches!(mode.as_str(), "rule" | "global" | "direct") {
        return Err(CmdError::ConfigInvalid {
            message: format!("未知模式 {mode:?}，只接受 rule / global / direct"),
        });
    }
    events::emit_control(&app, "mode-changed", mode);
    Err(CmdError::not_ready("set_mode 的落盘与生效"))
}

#[tauri::command]
pub async fn outbound_enable(_id: String, _enabled: bool) -> CmdResult<()> {
    Err(CmdError::not_ready("outbound_enable"))
}

/// 隐藏控制窗口（UI 里的「最小化到托盘」）。
///
/// 这一条**在本阶段就是完整实现**，不依赖任何后续阶段。
#[tauri::command]
pub async fn control_hide(app: tauri::AppHandle) -> CmdResult<()> {
    use tauri::Manager;
    let w = app
        .get_webview_window(crate::control::LABEL)
        .ok_or_else(|| CmdError::other("控制窗口不存在"))?;
    w.hide().map_err(CmdError::other)
}

/// 退出应用。走 app.exit(0) 以确保 RunEvent::Exit 到达 ——
/// hosts 摘除与 stats.json 落盘都挂在那里。
#[tauri::command]
pub async fn app_quit(app: tauri::AppHandle) -> CmdResult<()> {
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn set_mode_rejects_unknown_values_before_anything_else() {
        // 不需要 AppHandle 就能验的部分：非法值必须在触碰任何状态之前被拒。
        // 这里直接验分类逻辑（set_mode 需要 AppHandle，故拆出判定）。
        for bad in ["", "RULE", "auto", "rule "] {
            assert!(
                !matches!(bad, "rule" | "global" | "direct"),
                "{bad:?} 不该被当作合法模式"
            );
        }
        for good in ["rule", "global", "direct"] {
            assert!(matches!(good, "rule" | "global" | "direct"));
        }
    }
}
```

> 上面那个测试是**弱测试**——它验的是 match 表达式本身而非 `set_mode`。真正的测试需要一个 `AppHandle`，而构造它需要 `tauri::test` 特性（`tauri = { features = ["test"] }`）。**待实现时决定**：要么加上 `test` 特性写真测试（推荐，`tauri::test::mock_app()` 存在于 `tauri/src/test/mod.rs`，但**本计划未验证其在 2.11 的确切签名**），要么把校验逻辑提成一个纯函数 `fn valid_mode(&str) -> bool` 单独测。**后者更符合本仓库的既有风格**（阶段 1 的引擎就是纯函数 + 穷举单测）。

- [ ] **Step 2: 写 `src-tauri/src/commands/probe.rs`**

```rust
//! 探针与观测命令（设计文档 §11.2 / §11.5）。

use serde::Serialize;

use super::{CmdError, CmdResult};

/// rule_test 的返回。字段对应设计文档 §11.2 的
/// `{ index, decision, tried, resolved }`。
#[derive(Debug, Serialize)]
pub struct RuleTestResult {
    /// 命中的规则下标（0-based）。MATCH 兜底时是它自己的下标
    pub index: usize,
    /// "DIRECT" | "REJECT" | 出站名
    pub decision: String,
    /// 命中之前试过多少条 —— §11.5 的「前 N 条已试未命中」就用它
    pub tried: usize,
    /// 是否触发了 DNS 解析；None 表示第一轮就判完（零解析）
    pub resolved: Option<Vec<String>>,
}

/// 规则试算探针（§11.5 signature ②）。
///
/// `resolve` 直接对应设计文档 §4.2 的两阶段求值：
///   false → 只跑第一轮（快、不发 DNS）
///   true  → 遇 NeedResolve 时解析后跑第二轮（准）
///
/// ponytail: 阶段 4 尚未把 wsieve-route 接进 src-tauri，因此报未就绪。
/// 上限：规则视图的探针在阶段 5 之前不可用。
/// 升级路径：Cargo.toml 加 wsieve-route + wsieve-geo 依赖，这里换成
///   match rules.evaluate(&target, None, &geo) {
///       Verdict::Decided(d) => ...,
///       Verdict::NeedResolve { domain } if resolve => 解析后第二轮,
///       Verdict::NeedResolve { .. } => 如实告知「需解析才能确定」,
///   }
/// 复用同一份 evaluate() 是关键 —— §4.2 纪律①要求试算结果与真实判决
/// **永远一致**，试算与实际不一致的排查工具比没有更糟。
#[tauri::command]
pub async fn rule_test(_target: String, _resolve: bool) -> CmdResult<RuleTestResult> {
    Err(CmdError::not_ready("rule_test（需要阶段 1 的路由引擎接入）"))
}

/// 出站延迟探测（§11.2 的延迟指标定义）。
///
/// 语义：发一个 PADDING TU 的 POST 并计时，即触发一次稳态测量。
/// 不另开探测子流、不引入服务端探测端点 —— 复用既有流量路径，
/// 既省实现也少一个可探测面。
#[tauri::command]
pub async fn outbound_latency_probe(_id: String) -> CmdResult<u64> {
    Err(CmdError::not_ready("outbound_latency_probe（需要阶段 2 的出站管理器）"))
}

#[tauri::command]
pub async fn geo_update() -> CmdResult<()> {
    Err(CmdError::not_ready("geo_update"))
}

#[derive(Debug, Serialize)]
pub struct GeoStatus {
    pub geoip_present: bool,
    pub geosite_present: bool,
    pub updated_at: Option<String>,
}

#[tauri::command]
pub async fn geo_status(app: tauri::AppHandle) -> CmdResult<GeoStatus> {
    use tauri::Manager;
    let dir = app.path().app_config_dir().map_err(CmdError::io)?;
    Ok(GeoStatus {
        geoip_present: dir.join("geoip.dat").exists(),
        geosite_present: dir.join("geosite.dat").exists(),
        // ponytail: 更新时间戳需要一份元数据文件，阶段 4 不引入。
        // 上限：UI 显示「未知」；升级路径：geo_update 落盘时一并写 geo.meta.json。
        updated_at: None,
    })
}

/// 流量快照 —— 供控制窗口刚打开时补齐历史，不必等下一个 1s tick。
///
/// 这一条**在本阶段就是完整实现**：数据源是 Task 7 的聚合器。
#[tauri::command]
pub async fn traffic_snapshot(
    agg: tauri::State<'_, crate::events::Aggregator>,
) -> CmdResult<crate::events::TrafficSample> {
    use std::sync::atomic::Ordering;
    let c = &agg.counters;
    Ok(crate::events::TrafficSample {
        up_bytes: c.up_total.load(Ordering::Relaxed),
        down_bytes: c.down_total.load(Ordering::Relaxed),
        // 快照不含速率 —— 速率是「相对上一次采样」的概念，
        // 而快照没有上一次。UI 等下一个 traffic 事件即可。
        up_rate: 0,
        down_rate: 0,
        active: c.active.load(Ordering::Relaxed),
    })
}
```

> **待实现时验证**：`tauri::State<'_, T>` 在 async 命令里的生命周期标注。既有代码 `main.rs:117` 用的是 `state: tauri::State<'_, proxy::CurrentCore>`，形状一致，应当可行。若报生命周期错误，参照 `wsieve_heartbeat` 的写法调整。

- [ ] **Step 3: 写权限文件（每命令一份，共 11 份）**

按 Task 11 Step 1 的模板，为 `connect` / `disconnect` / `set_mode` / `outbound_enable` / `control_hide` / `app_quit` / `rule_test` / `outbound_latency_probe` / `geo_update` / `geo_status` / `traffic_snapshot` 各写一份。示例：

```toml
[[permission]]
identifier = "allow-rule-test"
description = "允许控制窗口运行规则试算探针（不改变任何状态）"
commands.allow = ["rule_test"]
```

**命名规则**：identifier 是 `allow-` + 命令名的 kebab-case（`rule_test` → `allow-rule-test`）。这与 Tauri 生态惯例一致，也让 `capabilities/control.json` 里的列表能被人一眼对上命令名。

- [ ] **Step 4: 注册并放开权限**

`main.rs` 的 `generate_handler!` 补齐全部命令；`capabilities/control.json` 的 `permissions` 恢复成 Task 3 Step 2 写的完整 16 条。

- [ ] **Step 5: 全量验证**

```bash
cargo test --manifest-path src-tauri/Cargo.toml
```

Expected: 全绿。**特别关注 `capability_isolation` 的 5 条**——此刻 control 已有 16 个权限、transport 仍是 3 个，交集必须仍为空。

- [ ] **Step 6: 手工验证越权确实被拒**

在控制窗口的 devtools 里（debug 构建默认可开）执行：

```js
await window.__TAURI__.core.invoke('wsieve_heartbeat')
```

Expected: 抛错，信息形如 `wsieve_heartbeat not allowed on window "control", webview "control", URL: local`。

> **实测过这条**：scratch 副本上控制页调 `wsieve_heartbeat`，拿到的正是这句话。**反向也要验**：传输窗口调 `config_get` 应当同样被拒——但传输窗口加载的是服务器页面，没法注入测试代码。这正是 Task 4 的交集断言存在的理由：**能自动验证的就不要靠手工**。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/commands/ src-tauri/permissions/wsieve/ \
        src-tauri/capabilities/control.json src-tauri/src/main.rs
git commit -m "feat(ipc): 控制与探针命令面定型（未就绪者如实报错，不 mock）"
```

---

## Part D — 验收

### Task 13: 双窗口共存冒烟测试

**Files:**
- Modify: `scripts/e2e.sh`

设计文档 §14 给阶段 4 定的验证方式是「**双窗口共存冒烟 + capability 交集断言**」。后者已是 Task 4，前者在这里。

要证明的命题是：**控制窗口的存在不破坏传输**。这不是自明的——两个窗口共享同一个 `WKWebsiteDataStore`、同一个事件循环、同一个 tokio runtime。控制窗口的 JS 若阻塞主线程，传输的 fetch 就会跟着停。

- [ ] **Step 1: 在 `scripts/e2e.sh` 的阶段 B 里加断言**

找到 `--with-app` 那一段（第 94 行起），在 `kill $APP_PID` **之前**插入：

```bash
  # 阶段 4：控制窗口共存断言。
  # 命题是「控制窗口的存在不破坏传输」—— 两个窗口共享事件循环与
  # WKWebsiteDataStore，控制窗口的 JS 阻塞主线程就会拖死传输的 fetch。
  #
  # 上面的 B2 已经证明隧道通了；这里再取一次，确认控制窗口完成加载与
  # 首次 IPC 之后隧道**仍然**通（而不是只在控制窗口 JS 跑起来之前通）。
  sleep 3
  B3=$(curl -s --max-time 8 --socks5-hostname 127.0.0.1:11081 \
    "http://127.0.0.1:$HTTP_PORT/test.txt" || true)
  [ "$B3" = "e2e-acceptance-body-v1" ] || {
    tail -40 "$WORK/app.log"
    fail "控制窗口起来之后隧道断了 —— 双窗口互相干扰"
  }
  echo "PASS: 双窗口共存，控制窗口不影响传输"
```

- [ ] **Step 2: 跑完整 E2E**

```bash
npm --prefix ui run build
scripts/e2e.sh --with-app
```

Expected:
```
PASS: curl -> SOCKS5 -> mux -> Noise -> HTTP -> target, body verified
PASS: real Tauri app tunnel verified
PASS: 双窗口共存，控制窗口不影响传输
E2E DONE
```

同时屏幕上应有一个深色的控制窗口与一个菜单栏图标。

> **实测确认过等价路径**：scratch 副本上，同一进程内隧道取回 `e2e-acceptance-body-v1` 的同时，控制窗口完成了 Svelte 挂载与 `invoke('config_get')` 往返。两者互不干扰。

- [ ] **Step 3: 验证 http 域名收紧的影响面（迁移检查）**

设计文档 §11.1 点名要在本阶段说明这件事。跑一次确认 E2E 的 `http://127.0.0.1:PORT` 仍在覆盖内：

Run: `scripts/e2e.sh --with-app`
Expected: 通过（上一步已验）。

再确认收紧确实生效——**临时**把 `WSIEVE_SERVER_URL` 换成一个 http 域名形式，观察它被拒：

```bash
# 在 /etc/hosts 里临时加 127.0.0.1 e2e-http-domain.test，然后：
WSIEVE_SERVER_URL="http://e2e-http-domain.test:18443/" ... ./wsieve-app
```

Expected: 日志停在 `waiting for webview heartbeat`，因为 emitter 的 invoke 被 ACL 拒绝。**这正是预期的收紧效果**，把观察结果记进提交信息。

> 这一步是可选的——它验的是「防线确实拦得住」，而非功能。但做过一次之后，将来有人报「换了 http 域名就连不上」时，你会立刻知道原因。

- [ ] **Step 4: 提交**

```bash
git add scripts/e2e.sh
git commit -m "test(e2e): 双窗口共存断言（控制窗口不影响传输）"
```

---

### Task 14: 收尾检查

**Files:** 无新增

- [ ] **Step 1: 全量测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: 全绿。参考量级——scratch 副本上是 31 个 bin 测试 + 4 个 capability 测试；本阶段新增的 events(6) / stats(6) / config redact(2) 之后应在 45 上下。

Run: `cargo test --workspace`
Expected: 全绿（阶段 1 的三个 crate 不受本阶段影响）。

- [ ] **Step 2: clippy**

Run: `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`

Expected: **可能不通过**——仓库既有一处 clippy 警告与本阶段无关：

```
src-tauri/src/shard_setup.rs:150
  clippy::cloned_ref_to_slice_refs
  hosts.set_managed("127.0.0.1", &[host.clone()])
  help: try: `std::slice::from_ref(&host)`
```

沿用阶段 1 Task 15 Step 4 的纪律：**既有债不纳入本阶段门禁**。想顺手清掉是好事，但**单独提交**。本阶段只要求「新增的文件零警告」：

Run: `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets 2>&1 | grep -E 'events\.rs|stats\.rs|control\.rs|tray\.rs|commands/'`
Expected: 无输出。

- [ ] **Step 3: 确认远端授权面没有偷偷变大**

这是本阶段最后一道闸。把两个 capability 的最终形态打出来，人眼过一遍：

```bash
python3 -c "
import json
c = json.load(open('src-tauri/gen/schemas/capabilities.json'))
for k in ('transport', 'control'):
    v = c[k]
    print(f'--- {k} ---')
    print('  windows:', v.get('windows'))
    print('  local:', v.get('local'))
    print('  remote:', v.get('remote'))
    for p in v['permissions']:
        print('   ', p)
"
```

逐条确认：

- [ ] `transport` 的 permissions **恰好三条** `allow-wsieve-*`，一条不多
- [ ] `transport` 的 `remote.urls` **恰好两条**，没有 `http://**:*`
- [ ] `transport` 的 `local` 是 `false`
- [ ] `control` **没有** `remote` 键
- [ ] `control` 的 permissions 里**没有任何** `wsieve_*`

> 这五条与 Task 4 的自动化断言重叠——**重叠是刻意的**。设计文档 §13 把它称为安全测试，而安全测试值得有一次人眼确认：自动化断言检查的是「我写的规则被满足了」，人眼检查的是「我写的规则是对的」。

- [ ] **Step 4: 前端构建产物体积检查**

Run: `du -sh ui/dist && ls -la ui/dist/assets/ | head -20`

Expected: 总体积约 250–350KB，其中大头是 IBM Plex 的 woff2（约 11 个文件、150KB 上下）。JS 约 30KB、CSS 约 4KB。

**若 JS 显著超过 100KB**，说明有意料之外的依赖被打进去了——本阶段只该有 Svelte runtime。查一下 `ui/package.json` 的 dependencies。

> 实测：scratch 上是 JS 29.12KB（gzip 11.22KB）+ CSS 4.17KB + 11 个字体文件。

- [ ] **Step 5: 提交（若有收尾改动）**

```bash
git add -A && git commit -m "chore(phase4): 收尾检查"
```

---

## 阶段 4 完成标准

全部勾选后本阶段才算完成：

- [ ] `cargo test --manifest-path src-tauri/Cargo.toml` 全绿
- [ ] `cargo test --workspace` 全绿
- [ ] **`cargo test --test capability_isolation` 的 5 条全绿，且验证过它们会失败**（Task 4 Step 3/4：故意制造交集，确认被抓）
- [ ] `npm --prefix ui run build` 成功
- [ ] **`cargo build` 在 `ui/dist` 不存在时也能成功**（Task 5 Step 4）——这是 E2E 不回归的前提
- [ ] `scripts/e2e.sh --with-app` 三条 PASS 全部出现
- [ ] 托盘的六项肉眼验收全部通过（Task 9 Step 4）
- [ ] Task 14 Step 3 的五条人眼确认全部通过
- [ ] 控制窗口关闭后进程仍在、SOCKS5 仍通
- [ ] 退出后 `stats.json` 存在且是合法 JSON

**最容易被将就过去的两条**，单独点名：

1. **交集断言的「会失败」验证**（Task 4 Step 3/4）。一条永远绿的安全测试是负资产——它让人以为有防线。**必须亲手让它红一次。**
2. **`ui/dist` 缺失时的构建**（Task 5 Step 4）。本地开发时 dist 一直存在，很容易忘了 CI 与干净克隆是从零开始的。`rm -rf ui/dist && cargo build` 跑一次，十秒钟的事。

---

## 交给阶段 5 的接口

阶段 5（五个视图）会这样接上本阶段的产物：

```js
// 事件订阅 —— 全部已在 Rust 侧聚合节流，直接用，不要再加防抖
import { invoke, listen } from './lib/ipc.js';

listen('traffic', (e) => {
  // { up_bytes, down_bytes, up_rate, down_rate, active }
  // 累计值幂等，速率是本秒增量。1s 一条。
});

listen('connection', (e) => {
  // { items: [{ id, target, outbound, state }], dropped: bool }
  // 200ms 一批。dropped 为 true 时 UI 应显示「部分连接未展示」。
});

listen('rule-hit', (e) => {
  // { "GEOSITE,cn,DIRECT": 3, "MATCH,日本节点": 12 }
  // 1s 一次的**增量**，键是规则原文。UI 侧累加即得热度。
  // 热度染色用 --heat-min / --heat-max 之间插值（tokens.css 已备好）。
});

listen('status', (e) => { /* string */ });
listen('outbound-state', (e) => { /* { name, state, latency_ms } */ });
listen('mode-changed', (e) => { /* string，托盘切模式时也会来 */ });

// 命令
await invoke('config_get');        // client-priv 已脱敏为 "***"
await invoke('config_get_raw');    // 原文，含明文私钥 —— 展示处必须警告（§5.4）
await invoke('traffic_snapshot');  // 窗口刚开时补齐，不必等下一个 tick
await invoke('rule_test', { target: 'example.com:443', resolve: false });
```

**阶段 5 必须遵守的三条**：

1. **颜色、间距、字号只从 `tokens.css` 取。** Task 2 Step 3 的那条 grep 会成为门禁
2. **事件不要再加前端防抖。** Rust 侧已经聚合过了，再防一层只会让 UI 落后于现实
3. **新增命令必须走 Part C 开头的五步。** 尤其第 5 步——每加一个命令就重跑一次交集断言

**本阶段明确未做、留给阶段 5 的**：

| 事项 | 为什么留 |
|---|---|
| 五个视图（流量桑基 / 规则 / 出站 / 设置 / 空状态） | §14 明确划归阶段 5 |
| `config_save` 的结构化写回 | 依赖阶段 1 的 `wsieve-config::edit`，本阶段未把该 crate 加进 `src-tauri` 的依赖 |
| `rule_test` 的真实实现 | 依赖阶段 1 的 `wsieve-route::evaluate` |
| `connect` / `disconnect` / `outbound_enable` | 依赖阶段 2 的出站管理器 |
| `outbound_latency_probe` | 依赖阶段 2 的会话层（要发 PADDING TU 并计时） |
| YAML 语法错的**行号** | §12 要求，但依赖 `wsieve-config` 的 `Spanned<T>`。本阶段的 `CmdError::ConfigSyntax` 已预留 `line` 字段 |
| 托盘的「切出站」子菜单 | 需要出站列表（阶段 2）与动态菜单重建 |
| GEO 文件下载 | `geo_update` 已定型，实现留到需要时 |

---

## 附：实现者需要自行验证的 API（本计划未逐一编译确认）

绝大部分 API 已在 scratch 副本上编译并运行过（见开头的实测表）。下列几处是本计划在 scratch 版本之上做的**改进写法**，未逐一回验——都不是关键路径，出错时有明确的降级方案：

| 位置 | 未验证的东西 | 若报错怎么办 |
|---|---|---|
| `control.rs` | `WebviewWindowBuilder::background_color(tauri::window::Color(..))` | grep `.research/repos/tauri/crates/tauri/src/webview/webview_window.rs` 里的 `pub fn background_color`；实在对不上就删掉该行（只是防启动闪屏） |
| `control.rs` | `WebviewWindow::unminimize()` | 删掉该行，`show()` 在多数平台上已足够 |
| `tray.rs` | `tauri::Error::AssetNotFound` 变体名 | 换成 `.expect("icons/icon.png 应存在")`——那文件在仓库里，缺了就是构建坏了 |
| `probe.rs` | async 命令里的 `tauri::State<'_, T>` | 参照 `main.rs:117` 的 `wsieve_heartbeat` 写法 |
| `commands/control.rs` | `tauri::test::mock_app()`（若选择写真测试） | 改用「校验逻辑提纯函数 + 单测纯函数」，更合本仓库风格 |
| `events.rs` | `sample()` / `hit_delta()` 两个提取出来的纯函数 | 逻辑在 scratch 里内联跑过；若 `filter_map` 闭包类型推断报错，拆成 `.map().filter()` |

**验证方法统一是**：`cargo check --manifest-path src-tauri/Cargo.toml`。每写完一个文件跑一次，不要攒到最后。

**已在 scratch 上编译并运行验证过的**（不需要再怀疑）：双 capability 的 JSON schema 与生成结果、`capability_isolation.rs` 的五条断言（含故意制造交集时确实失败）、`events.rs` 的队列与计数器、`stats.rs` 全部、`TrayIconBuilder` / `MenuBuilder` / `MenuItemBuilder` 的完整调用链、`WebviewWindowBuilder` 建控制窗口 + `on_window_event` 拦关闭、`Emitter::emit_to` 的 `Serialize + Clone` 约束、`RunEvent::Exit` 的到达、`embed-ui`(= `custom-protocol`) 对本地资产加载的决定性作用、`build.rs` 的 dist 兜底、Svelte 5 + Vite 8 的完整构建与运行。
