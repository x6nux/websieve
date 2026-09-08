# 主题切换 + 滚动条覆盖 实现计划

> **执行方式**：使用 superpowers:subagent-driven-development——每个 Task 派一个
> 全新 subagent 实现，做完先过 spec 一致性审查，再过代码质量审查，有问题就
> 派修复 subagent 改完重新审查，全部通过才算这个 Task 完成，再进下一个。

**目标**：设置面板里新增「深色/浅色/跟随系统」三档主题切换（存 localStorage，
不写 config.yaml），并让所有可滚动区域的滚动条颜色跟随主题变化，不再露出
系统默认的刺眼白色滚动条。

**设计依据**：`docs/superpowers/specs/2026-09-08-theme-switch-and-scrollbar.md`
（已写完并经用户确认范围）。下面每个 Task 都会重复贴出与它相关的设计原文，
不需要另外去读设计文档。

**技术栈**：Svelte 5（runes）、原生 CSS 变量、`localStorage`、`matchMedia`。

---

### Task 1：`tokens.css`——浅色变量集 + 显式深色块 + 滚动条样式

**Files:**
- Modify: `ui/src/tokens.css`
- Modify: `ui/src/tokens.test.js`

- [ ] **Step 1：先写失败的测试**

在 `ui/src/tokens.test.js` 的 `describe('设计令牌契约（定义在阶段 4）', ...)`
块之后（同一文件内，另开一个 `describe`）追加：

```js
describe('主题切换：浅色变量集 + 显式深色块', () => {
  it("包含 :root[data-theme='light'] 选择器", () => {
    expect(css).toMatch(/:root\[data-theme=['"]light['"]\]\s*\{/);
  });

  it("包含 :root[data-theme='dark'] 选择器", () => {
    expect(css).toMatch(/:root\[data-theme=['"]dark['"]\]\s*\{/);
  });

  it('浅色块内定义了表面/边框/文字四级变量', () => {
    const m = css.match(/:root\[data-theme=['"]light['"]\]\s*\{([^}]*)\}/);
    expect(m).not.toBeNull();
    const block = m[1];
    for (const name of [
      '--surface-0', '--surface-1', '--surface-2',
      '--border', '--border-strong',
      '--text-1', '--text-2', '--text-3', '--text-4',
      '--heat-min', '--heat-max', '--shadow-overlay',
    ]) {
      expect(block).toMatch(new RegExp(`${name}\\s*:`));
    }
  });

  it('浅色块不重新定义出站色码与状态色——两个主题共用同一份', () => {
    const m = css.match(/:root\[data-theme=['"]light['"]\]\s*\{([^}]*)\}/);
    const block = m[1];
    expect(block).not.toMatch(/--outbound-\d/);
    expect(block).not.toMatch(/--state-(live|warn|fail|direct)/);
  });
});

describe('滚动条：跟随主题变量，不用系统默认', () => {
  it('定义了标准 scrollbar-color/-width', () => {
    expect(css).toMatch(/scrollbar-width\s*:/);
    expect(css).toMatch(/scrollbar-color\s*:/);
  });

  it('定义了 WebKit 系滚动条规则，且颜色引用变量而非写死色值', () => {
    expect(css).toMatch(/::-webkit-scrollbar\s*\{/);
    expect(css).toMatch(/::-webkit-scrollbar-thumb\s*\{/);
    const m = css.match(/::-webkit-scrollbar-thumb\s*\{([^}]*)\}/);
    expect(m[1]).toMatch(/var\(--/);
    expect(m[1]).not.toMatch(/#[0-9a-fA-F]{3,6}/);
  });
});
```

- [ ] **Step 2：跑测试确认失败**

Run: `cd ui && npx vitest run tokens`
Expected: FAIL——上面几条新断言都还没有对应的 CSS。

- [ ] **Step 3：改 `tokens.css`**

在现有 `:root { ... }` 块（保持原样不动，深色数值继续留在这里作为无属性
时的默认兜底）之后，追加：

```css
/* ── 主题：浅色档，仅覆盖与明暗相关的变量 ─────────────────
 * 出站色码与状态色两个主题共用 :root 里的原值，不在这里重复定义——
 * 它们是高饱和点缀色，深浅背景下都压得住，重新设计是无意义的重复劳动。 */
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

/* 深色档显式声明（与上面 :root 的默认值一致）——不只靠「没有 data-theme
 * 属性」这个隐含状态代表深色，三态里的两态都要有据可查，也方便测试直接
 * 断言这个选择器存在。 */
:root[data-theme='dark'] {
  --surface-0: #16181b;
  --surface-1: #1c1f23;
  --surface-2: #23272c;

  --border: rgba(255, 255, 255, 0.07);
  --border-strong: rgba(255, 255, 255, 0.13);

  --text-1: #e6e8ea;
  --text-2: #a4abb3;
  --text-3: #6f777f;
  --text-4: #4d545b;

  --heat-min: rgba(255, 255, 255, 0.014);
  --heat-max: rgba(255, 255, 255, 0.052);

  --shadow-overlay: 0 8px 32px rgba(0, 0, 0, 0.5);
}
```

在文件末尾（`@media (prefers-reduced-motion: reduce) { ... }` 之后）追加：

```css
/* ── 滚动条：跟随主题，不用系统默认 ───────────────────────
 * 两套写法都留着：`scrollbar-color`/`scrollbar-width` 是标准 CSS
 * Scrollbars 规范，现代 Chromium（本项目 Windows 上 WebView2 的内核）已
 * 支持；`::-webkit-scrollbar-*` 是更早期就有、覆盖面更广的写法。两者不
 * 冲突，同时写上比只赌一边更稳。颜色用既有的 --border-strong/--text-3
 * ——滚动条本质是一条更粗的边框，跟着边框/弱化文字色走，两个主题下都已经
 * 有正确的明暗值，不需要再造一套「滚动条专用」token。 */
* {
  scrollbar-width: thin;
  scrollbar-color: var(--border-strong) transparent;
}

::-webkit-scrollbar {
  width: 10px;
  height: 10px;
}
::-webkit-scrollbar-track {
  background: transparent;
}
::-webkit-scrollbar-thumb {
  background: var(--border-strong);
  border-radius: var(--radius);
  border: 2px solid transparent;
  background-clip: padding-box;
}
::-webkit-scrollbar-thumb:hover {
  background: var(--text-3);
}
```

- [ ] **Step 4：跑测试确认通过**

Run: `cd ui && npx vitest run tokens`
Expected: 全部 PASS（含既有的令牌契约测试，无回归）。

- [ ] **Step 5：跑全量前端测试确认无回归**

Run: `cd ui && npx vitest run`
Expected: 全部 PASS——这一步只加了 CSS，理论上不影响任何组件测试，但必须
实际跑一遍确认（比如 `.mini`/`.ghost` 这类既有类名没有意外被新规则的
优先级影响到）。

- [ ] **Step 6：提交**

```bash
git add ui/src/tokens.css ui/src/tokens.test.js
git commit -m "feat(ui): 主题令牌新增浅色档 + 显式深色档，滚动条改用主题变量"
```

---

### Task 2：`ui/src/lib/theme.js`——偏好读写 + 应用 + 跟随系统

**Files:**
- Create: `ui/src/lib/theme.js`
- Create: `ui/src/lib/theme.test.js`

**背景**（设计文档原文）：

> `initTheme()` 在 `ui/src/main.js` 里、`mount(App, ...)` 之前调用一次。
> `localStorage` 不可用时不能让整个应用崩掉——读写都要 try/catch，失败就
> 按 `'system'` 处理。
>
> 偏好为 `'system'` 时，订阅 `matchMedia` 的 `change` 事件，系统切换时实时
> 重新 `applyTheme`——订阅只在偏好是 `'system'` 时才需要，显式选了
> dark/light 之后系统怎么变都不该影响界面。

- [ ] **Step 1：先写失败的测试**

`ui/src/lib/theme.test.js`：

```js
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import {
  getThemePreference,
  setThemePreference,
  initTheme,
  THEME_KEY,
} from './theme.js';

/** 模拟 matchMedia：`matches` 由调用方传入，返回的对象支持
 *  addEventListener('change', cb) / removeEventListener 记录监听器，
 *  测试里手动调用 fireSystemChange 模拟系统切换。 */
function mockMatchMedia(initialMatches) {
  let matches = initialMatches;
  const listeners = new Set();
  const mql = {
    get matches() { return matches; },
    media: '(prefers-color-scheme: dark)',
    addEventListener: (_type, cb) => listeners.add(cb),
    removeEventListener: (_type, cb) => listeners.delete(cb),
  };
  window.matchMedia = vi.fn(() => mql);
  return {
    fireSystemChange(next) {
      matches = next;
      for (const cb of listeners) cb({ matches: next });
    },
    listenerCount: () => listeners.size,
  };
}

beforeEach(() => {
  localStorage.clear();
  document.documentElement.removeAttribute('data-theme');
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe('getThemePreference / setThemePreference', () => {
  it('未设置时回退到 system', () => {
    expect(getThemePreference()).toBe('system');
  });

  it('非法值回退到 system', () => {
    localStorage.setItem(THEME_KEY, '这不是合法的主题值');
    expect(getThemePreference()).toBe('system');
  });

  it('setThemePreference 写入 localStorage 且返回写入的值', () => {
    expect(setThemePreference('light')).toBe('light');
    expect(localStorage.getItem(THEME_KEY)).toBe('light');
    expect(getThemePreference()).toBe('light');
  });

  it('setThemePreference 立即把 data-theme 应用到 <html>', () => {
    mockMatchMedia(false);
    setThemePreference('light');
    expect(document.documentElement.dataset.theme).toBe('light');
    setThemePreference('dark');
    expect(document.documentElement.dataset.theme).toBe('dark');
  });
});

describe('跟随系统', () => {
  it("偏好为 system 且系统匹配 dark 时，applies 'dark'", () => {
    mockMatchMedia(true);
    setThemePreference('system');
    expect(document.documentElement.dataset.theme).toBe('dark');
  });

  it("偏好为 system 且系统不匹配 dark 时，applies 'light'", () => {
    mockMatchMedia(false);
    setThemePreference('system');
    expect(document.documentElement.dataset.theme).toBe('light');
  });

  it('initTheme 之后，系统切换会实时反映（偏好是 system 时）', () => {
    const mm = mockMatchMedia(false);
    localStorage.setItem(THEME_KEY, 'system');
    initTheme();
    expect(document.documentElement.dataset.theme).toBe('light');
    mm.fireSystemChange(true);
    expect(document.documentElement.dataset.theme).toBe('dark');
  });

  it('显式选择 dark 后，系统切换不应影响 data-theme', () => {
    const mm = mockMatchMedia(false);
    localStorage.setItem(THEME_KEY, 'dark');
    initTheme();
    expect(document.documentElement.dataset.theme).toBe('dark');
    mm.fireSystemChange(true);
    expect(document.documentElement.dataset.theme).toBe('dark');
  });

  it('显式选择 light 后，系统切换不应影响 data-theme', () => {
    const mm = mockMatchMedia(true);
    localStorage.setItem(THEME_KEY, 'light');
    initTheme();
    expect(document.documentElement.dataset.theme).toBe('light');
    mm.fireSystemChange(false);
    expect(document.documentElement.dataset.theme).toBe('light');
  });
});

describe('localStorage 不可用时的降级', () => {
  it('getItem 抛异常时不崩溃，按 system 处理', () => {
    const orig = Storage.prototype.getItem;
    Storage.prototype.getItem = () => { throw new Error('boom'); };
    mockMatchMedia(false);
    expect(() => getThemePreference()).not.toThrow();
    expect(getThemePreference()).toBe('system');
    Storage.prototype.getItem = orig;
  });

  it('setItem 抛异常时不崩溃，仍然应用到 DOM', () => {
    const orig = Storage.prototype.setItem;
    Storage.prototype.setItem = () => { throw new Error('boom'); };
    mockMatchMedia(false);
    expect(() => setThemePreference('light')).not.toThrow();
    expect(document.documentElement.dataset.theme).toBe('light');
    Storage.prototype.setItem = orig;
  });
});
```

- [ ] **Step 2：跑测试确认失败**

Run: `cd ui && npx vitest run theme`
Expected: FAIL——`./theme.js` 还不存在。

- [ ] **Step 3：写实现**

`ui/src/lib/theme.js`：

```js
/**
 * 主题偏好（设计文档「主题切换 + 滚动条覆盖」§2）。
 *
 * 存 localStorage 而非 config.yaml——这是「这台设备、这个人怎么看界面」的
 * 展示偏好，不是代理配置，两者生命周期不同，不该耦合在同一份文件里。
 *
 * 三态：'dark' | 'light' | 'system'。只有 'system' 会订阅系统主题变化；
 * 显式选了 dark/light 之后系统怎么变都不该影响界面，否则「选深色」形同虚设。
 */

export const THEME_KEY = 'websieve:theme';
const VALID = ['dark', 'light', 'system'];

let unsubscribeSystemWatch = null;

function safeGetItem(key) {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function safeSetItem(key, value) {
  try {
    localStorage.setItem(key, value);
  } catch {
    // localStorage 不可用时静默降级——展示偏好丢失不该拖累主功能。
  }
}

export function getThemePreference() {
  const v = safeGetItem(THEME_KEY);
  return VALID.includes(v) ? v : 'system';
}

function resolveSystemTheme() {
  return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
}

function resolve(pref) {
  return pref === 'system' ? resolveSystemTheme() : pref;
}

function applyTheme(pref) {
  document.documentElement.dataset.theme = resolve(pref);
}

function watchSystemIfNeeded(pref) {
  if (unsubscribeSystemWatch) {
    unsubscribeSystemWatch();
    unsubscribeSystemWatch = null;
  }
  if (pref !== 'system') return;

  const mql = window.matchMedia('(prefers-color-scheme: dark)');
  const onChange = () => applyTheme('system');
  mql.addEventListener('change', onChange);
  unsubscribeSystemWatch = () => mql.removeEventListener('change', onChange);
}

export function setThemePreference(pref) {
  const next = VALID.includes(pref) ? pref : 'system';
  safeSetItem(THEME_KEY, next);
  applyTheme(next);
  watchSystemIfNeeded(next);
  return next;
}

/** 应用启动时调用一次：应用当前已保存的偏好，若是 system 则订阅系统切换。 */
export function initTheme() {
  const pref = getThemePreference();
  applyTheme(pref);
  watchSystemIfNeeded(pref);
}
```

- [ ] **Step 4：跑测试确认通过**

Run: `cd ui && npx vitest run theme`
Expected: 全部 PASS。

- [ ] **Step 5：接进应用启动**

`ui/src/main.js`：

```js
import { mount } from 'svelte';
import './tokens.css';
import { initTheme } from './lib/theme.js';
import App from './App.svelte';

initTheme();

// Svelte 5 用 mount() 而非 new App()
export default mount(App, { target: document.getElementById('app') });
```

（只在 `import './tokens.css'` 之后、`mount` 之前插入 `initTheme()` 这一行
与新的 import，其余原样保留。）

- [ ] **Step 6：跑全量前端测试确认无回归**

Run: `cd ui && npx vitest run`
Expected: 全部 PASS。

- [ ] **Step 7：提交**

```bash
git add ui/src/lib/theme.js ui/src/lib/theme.test.js ui/src/main.js
git commit -m "feat(ui): 新增主题偏好模块——存 localStorage，支持跟随系统"
```

---

### Task 3：`SettingsOverlay.svelte`——「外观」区块

**Files:**
- Modify: `ui/src/views/SettingsOverlay.svelte`
- Modify: `ui/src/views/SettingsOverlay.test.js`

**背景**（设计文档原文）：

> 这一块不走 `draft`/`onsave` 那条路——它不是 `config.yaml` 的字段，点选即
> 生效、不需要点底部「保存」按钮，也不受「取消」影响。`themePref` 是组件
> 本地 `$state`，初值用 `untrack(() => getThemePreference())` 取一次。

- [ ] **Step 1：先写失败的测试**

在 `ui/src/views/SettingsOverlay.test.js` 里加（若顶部尚未
`import { setThemePreference, getThemePreference } from '../lib/theme.js';`，
按实际使用加上；测试里需要在每个 `it` 前清空 `localStorage` 与
`document.documentElement` 的 `data-theme`，若文件里已有全局
`beforeEach` 做清理，就近加进去，不要另起一套）：

```js
describe('设置面板——外观（主题切换）', () => {
  beforeEach(() => {
    localStorage.clear();
    document.documentElement.removeAttribute('data-theme');
  });

  it('渲染深色/浅色/跟随系统三个选项，默认选中当前偏好', () => {
    localStorage.setItem('websieve:theme', 'light');
    render(SettingsOverlay, { open: true, config: {} });
    const radios = screen.getAllByRole('radio', { name: /深色|浅色|跟随系统/ });
    expect(radios).toHaveLength(3);
    expect(screen.getByRole('radio', { name: '浅色' })).toHaveAttribute('aria-checked', 'true');
  });

  it('点击「深色」立即应用，且不调用 onsave', async () => {
    const u = userEvent.setup();
    const onsave = vi.fn();
    render(SettingsOverlay, { open: true, config: {}, onsave });
    await u.click(screen.getByRole('radio', { name: '深色' }));
    expect(document.documentElement.dataset.theme).toBe('dark');
    expect(onsave).not.toHaveBeenCalled();
  });

  it('点击「跟随系统」后关闭面板（不点保存），主题选择仍然生效', async () => {
    const u = userEvent.setup();
    const onclose = vi.fn();
    render(SettingsOverlay, { open: true, config: {}, onclose });
    await u.click(screen.getByRole('radio', { name: '浅色' }));
    await u.click(screen.getByRole('button', { name: '取消' }));
    expect(localStorage.getItem('websieve:theme')).toBe('light');
  });
});
```

（第三个测试的关键点：证明主题选择走的是「点选即落盘」，不受「取消」按钮
影响——取消只应放弃 `draft` 里的端口/局域网/承载方式改动，不该把已经生效
的主题选择也一起回滚，因为主题从来就不在 `draft` 里。）

- [ ] **Step 2：跑测试确认失败**

Run: `cd ui && npx vitest run SettingsOverlay`
Expected: FAIL——「外观」区块还不存在。

- [ ] **Step 3：改 `SettingsOverlay.svelte`**

顶部 import 追加：

```js
import { getThemePreference, setThemePreference } from '../lib/theme.js';
import Segmented from '../lib/Segmented.svelte';
```

（若 `Segmented` 已经在别处被这个文件引入过，不要重复 import；先读文件
确认。）

在 `let draft = $state({ ...untrack(() => config) });` 附近加一行本地状态：

```js
  /** 主题偏好——不进 draft：它不是 config 的字段，点选即生效，不受
   *  「取消」影响。初值只取一次，理由与 draft 的 untrack 注释相同。 */
  let themePref = $state(untrack(() => getThemePreference()));
```

在 `<div class="body">` 内、`<section><h3>入口</h3>...` 之前插入新区块：

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

- [ ] **Step 4：跑测试确认通过**

Run: `cd ui && npx vitest run SettingsOverlay`
Expected: 全部 PASS（含既有测试，无回归——新增的「外观」区块不改变任何
既有 DOM 结构里字段的相对顺序判断，若某条既有测试用了脆弱的「第 N 个
section」之类的定位方式而非按角色/文本查找，需要相应调整选择器，不要
删掉断言本身）。

- [ ] **Step 5：跑全量前端测试确认无回归**

Run: `cd ui && npx vitest run`
Expected: 全部 PASS。

- [ ] **Step 6：跑无障碍审计**

Run: `cd ui && npx vitest run a11y`
Expected: 全部 PASS——新的 `Segmented` 用法应天然满足既有的无障碍规则
（复用组件本身已经过审计），但仍要实跑一遍确认没有引入新的违规（比如
两个 `Segmented` 实例——顶部视图导航与这里的主题选择——如果 `label` 重名
会不会造成 `getByRole` 歧义，需要的话调整 `label` 文案）。

- [ ] **Step 7：提交**

```bash
git add ui/src/views/SettingsOverlay.svelte ui/src/views/SettingsOverlay.test.js
git commit -m "feat(ui): 设置面板新增外观区块——深色/浅色/跟随系统"
```

---

## 收尾

### Task 4：全量验证

**Files:** 无新文件——本任务只跑命令、必要时回头修。

- [ ] **Step 1：全量前端测试**

Run: `cd ui && npx vitest run`
Expected: 全部 PASS。

- [ ] **Step 2：编译真实应用（含前端资产重新嵌入）**

```bash
cd ui && npm run build
export PATH="$HOME/.cargo/bin:$PATH"
cd .. && cargo build --manifest-path src-tauri/Cargo.toml
```
Expected: 前端 build 与 Rust build 都成功。（若此刻有正在运行的
`wsieve-app.exe` 占着文件锁导致链接失败，先确认那不是用户自己正在测试用的
进程——是的话跳过这一步的编译验证，只在前端测试层面确认；不是的话再
`taskkill` 后重试。）

- [ ] **Step 3：更新真机走查清单**

在 `docs/superpowers/plans/2026-09-07-live-walkthrough-checklist.md` 末尾
补三条新清单项（主题三档切换、切换后滚动条颜色跟随、刷新/重启后偏好
仍然保持），供用户下次测试时一并核对。

- [ ] **Step 4：确认没有遗留的未提交改动**

Run: `git status`
Expected: working tree clean（新增的走查清单更新也已提交）。
