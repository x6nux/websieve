<script>
  /**
   * 设置覆盖层（spec §11.3）。
   *
   * 走覆盖层而非第四个标签 —— 不常用，不该占据同级位置。
   *
   * 这是全项目**唯一允许用阴影**的地方（spec §11.4：深度策略 borders-only，
   * 浮层除外）。
   *
   * ## 焦点管理是硬要求，不是可选项
   *
   * 打开时焦点进入、Tab 被困在层内、Esc 关闭、关闭时焦点回到触发元素。
   * 做不到这几条，覆盖层对键盘用户就是个陷阱 —— 焦点跑到背景里，
   * 而背景在视觉上被遮住了，用户在敲一个自己看不见的界面。
   *
   * ## 私钥不进这一层
   *
   * `config_get` 递归脱敏 `client-priv`（任意深度、kebab 与 snake 两种拼法
   * 都认），`config_get_raw` 逐字返回**含明文私钥**的原文。
   *
   * 本组件的契约是**只吃脱敏过的那一份**，并且它自己**从不发起 IPC**：
   * 导出走 `onexport` 交给父组件，由父组件在用户点了导出之后才去取原文。
   * 这样私钥就不会因为「打开了一次设置」而被复制进渲染进程的 JS 堆、
   * devtools 的变量面板和任何一份错误上报里 —— 而那三个地方都不受 0600 保护。
   *
   * 另外它只渲染**具名字段**，不对 config 做通用遍历。所以哪天调用方
   * 不小心把原文配置传了进来，私钥也不会被渲染出去。`SettingsOverlay.test.js`
   * 的「私钥边界」一组守着这条。
   *
   * ## onsave 该走哪条命令（给接线的一头）
   *
   * 这里改的全是**非规则区**的标量字段，而 `wsieve_config::edit` 只提供
   * `replace_rule_line` / `delete_rule_line` 两个规则行改写器，**没有**标量
   * 字段的定点改写。因此 `onsave` 落盘只能走 `config_save_raw`（整份覆盖，
   * 写前 Rust 侧会解析并校验）。
   *
   * 后果就是下面那句 §5.6 的提示所写的：非规则区用户手写的注释会丢。
   * 这是必须提前说的事，不是保存完再道歉的事。规则区的注释不受影响 ——
   * 那条路走的是 `config_save` 的行级定点改写。
   */
  import { tick, onDestroy, untrack } from 'svelte';

  let {
    open = false,
    /** 已脱敏的配置。**不要把 config_get_raw 的返回值传进来。** */
    config = {},
    onclose = () => {},
    onsave = () => {},
    onexport = () => {},
  } = $props();

  let dialog = $state(null);
  /*
   * 初值用 untrack 取。
   *
   * 这里**要的就是**「只捕获初始值」—— 草稿的后续同步由下面那个 effect
   * 在开合翻转时负责，而不是跟着 config 走（跟着走就是本组件最要命的
   * 那个 bug，见 effect 里的长注释）。
   *
   * 但不加 untrack 的话 Svelte 会报 `state_referenced_locally` 警告：
   * 它无法区分「忘了做成响应式」与「刻意只取一次」。untrack 就是把后者
   * 说出口。不初始化成 `{}` 再等 effect 填，是因为 effect 在首次渲染
   * **之后**才跑，那样打开的第一帧全部输入框是空的，然后跳成真实值。
   */
  let draft = $state({ ...untrack(() => config) });
  let error = $state('');
  let restoreFocus = null;

  /**
   * 上一次看到的 open。**普通变量而非 `$state`** —— 它只是给下面那个 effect
   * 记「上次是开还是关」，写成 `$state` 会让这次赋值本身再触发一轮 effect。
   */
  let wasOpen = false;

  /**
   * 焦点回到触发元素。关闭与卸载两条路都走这里 ——
   * 各写一份的话，两者的时机与判空条件迟早分叉。
   *
   * `document.contains` 那一步是必要的：触发元素可能在覆盖层开着的时候
   * 被父组件重渲染掉了，对一个已经脱离文档的节点 focus() 是静默无效的，
   * 焦点会留在 body 上而没有任何迹象。
   */
  function releaseFocus() {
    const el = restoreFocus;
    restoreFocus = null;
    if (el instanceof HTMLElement && document.contains(el)) el.focus();
  }

  $effect(() => {
    /*
     * **只在开合翻转的那一刻取草稿快照**，而不是每次 effect 重跑都取。
     *
     * 计划里的草稿代码是 `if (!open) return; draft = { ...config };` ——
     * 那样 config 会成为本 effect 的依赖，于是父组件每重渲染一次
     * （traffic 事件 1s 一发、connection 事件 200ms 一批，重渲染是常态）
     * 就把 draft 重置一次：用户正打到一半的端口号当场被冲回 7890，
     * 而且看上去像是键盘丢字，极难归因。
     *
     * 用 `untrack(() => config)` 只能解决真实 Svelte 下的这一半问题。
     * 测试环境里解决不了：`@testing-library/svelte` 的
     * `createProps` 把**全部 props 塞进同一个 `$state.raw` 单元**
     * （见 svelte-core/src/props.svelte.js），读任何一个 prop 都等于订阅了
     * 所有 prop 的变化 —— 连 `open` 都躲不掉。
     *
     * 所以判据不放在依赖收集上，而放在**值本身**：只有 open 真的翻转了
     * 才重新取快照。这条判据在两种环境下都成立，也不依赖框架的依赖粒度。
     */
    const isOpen = open;
    if (isOpen === wasOpen) return;
    wasOpen = isOpen;

    if (!isOpen) {
      releaseFocus();
      return;
    }

    draft = { ...config };
    error = '';
    restoreFocus = document.activeElement;

    tick().then(() => {
      dialog?.querySelector('input, select, button')?.focus();
    });
  });

  // 卸载时（父组件直接把整个覆盖层拆掉，没走 open=false 这条路）也要还焦点，
  // 否则焦点掉回 body，键盘用户下一次 Tab 从文档开头重新开始
  onDestroy(releaseFocus);

  /** 焦点陷阱与 Esc 共用的可聚焦元素查询，两处各写一份迟早分叉 */
  const FOCUSABLE =
    'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])';

  function onKeydown(e) {
    if (e.key === 'Escape') {
      e.stopPropagation();
      onclose();
      return;
    }
    if (e.key !== 'Tab') return;

    // 焦点陷阱：Tab 在层内循环，不跑到背景里去
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

  function save() {
    const raw = draft.mixedPort;
    const p = Number(raw);

    /*
     * 错误绝不静默吞掉 —— 参照仓库既有的
     * `port_conflict_is_reported_not_skipped`。
     *
     * 空值单独分支：`Number('')` 是 0，`Number(undefined)` 是 NaN，
     * 两者都能被下面的范围判定挡住，但报出来的话会是「非法：undefined」，
     * 用户看不懂那是「你没填」。
     */
    if (raw === '' || raw == null) {
      error = '混合端口不能填空。有效范围是 1–65535，默认 7890。';
      return;
    }
    if (!Number.isInteger(p) || p < 1 || p > 65535) {
      error = `混合端口非法：${raw}。有效范围是 1–65535。`;
      return;
    }

    error = '';
    // 回传数字而非字符串：这个值最终要作为 YAML 标量落盘，
    // 带引号的 "7890" 与 7890 在 deny_unknown_fields 的强类型解析下不等价
    onsave({ ...draft, mixedPort: p });
  }
</script>

{#if open}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <div class="scrim" onclick={onclose} role="presentation"></div>

  <div
    class="panel"
    role="dialog"
    aria-modal="true"
    aria-label="设置"
    bind:this={dialog}
    onkeydown={onKeydown}
    tabindex="-1">
    <header>
      <h2>设置</h2>
      <button type="button" class="x" aria-label="关闭设置" onclick={onclose}>×</button>
    </header>

    <div class="body">
      <section>
        <h3>入口</h3>
        <div class="field">
          <label for="s-port">混合端口</label>
          <input id="s-port" type="number" min="1" max="65535" class="mono" bind:value={draft.mixedPort} />
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
        <!-- spec §5.4：导出入口必须显式警告。文案要说清「靠什么保护」，
             只说「含私钥」的话用户无从判断这有多危险。 -->
        <p class="hint warn">
          配置文件含 <b>client-priv 私钥（明文）</b>，仅靠 0600 文件权限保护。
          导出的副本不再有这层保护 —— 导出或分享前请确认接收方可信。
        </p>
        <!-- spec §5.6：非规则区的自定义注释会丢失。这一句必须在保存**之前**
             出现：保存完再道歉，用户手写的东西已经没了。 -->
        <p class="hint">
          从这里保存会重写配置的非规则区域，该区域内你手写的<b>注释会丢失</b>。
          规则区的注释始终逐字保留。要完全手工控制，请直接编辑原始 YAML。
        </p>
      </section>

      {#if error}
        <p class="err" role="alert">{error}</p>
      {/if}
    </div>

    <footer>
      <button type="button" class="ghost" onclick={onclose}>取消</button>
      <button type="button" class="solid" onclick={save}>保存</button>
    </footer>
  </div>
{/if}

<style>
  .scrim {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    z-index: 10;
  }

  .panel {
    position: fixed;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    width: min(560px, calc(100vw - 48px));
    max-height: calc(100vh - 64px);
    display: flex;
    flex-direction: column;
    background: var(--surface-1);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius);
    /* 浮层是 borders-only 的唯一例外（spec §11.4）。
       一圈近黑的描边 + 一层大扩散阴影，而不是彩色辉光。 */
    box-shadow:
      0 0 0 1px rgba(0, 0, 0, 0.4),
      var(--shadow-overlay);
    z-index: 11;
  }

  header {
    display: flex;
    align-items: center;
    padding: 12px 16px;
    border-bottom: 1px solid var(--border);
    background: var(--surface-0);
  }
  h2 {
    margin: 0;
    font-size: var(--fs-14);
    font-weight: var(--fw-semibold);
  }

  .x {
    all: unset;
    margin-left: auto;
    padding: 0 6px;
    font-size: var(--fs-18);
    line-height: 1;
    color: var(--text-3);
    cursor: pointer;
  }
  .x:hover {
    color: var(--text-1);
  }
  .x:focus-visible {
    outline: 2px solid var(--outbound-1);
    outline-offset: 1px;
  }

  .body {
    padding: 4px 16px 16px;
    overflow-y: auto;
  }

  section {
    padding: 14px 0;
    border-bottom: 1px solid var(--border);
  }
  section:last-of-type {
    border-bottom: none;
  }

  h3 {
    margin: 0 0 10px;
    font-size: var(--fs-11);
    letter-spacing: 0.09em;
    text-transform: uppercase;
    color: var(--text-4);
    font-weight: var(--fw-semibold);
  }

  .field {
    margin-bottom: 10px;
  }
  .field.row {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .field.row label {
    margin: 0;
  }

  label {
    display: block;
    margin-bottom: 4px;
    font-size: var(--fs-12);
    color: var(--text-2);
  }

  input[type='number'],
  select {
    background: var(--surface-0);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius);
    padding: 6px 9px;
    color: var(--text-1);
    font-size: var(--fs-13);
    font-family: inherit;
    width: 100%;
  }
  input[type='number'] {
    width: 120px;
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
  }

  .hint {
    margin: 5px 0 0;
    font-size: var(--fs-11);
    color: var(--text-4);
    line-height: 1.65;
  }
  .hint b {
    color: var(--text-3);
    font-weight: var(--fw-medium);
  }
  .hint.warn {
    color: var(--state-warn);
  }
  .hint.warn b {
    color: var(--state-warn);
    font-weight: var(--fw-semibold);
  }

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

  .ghost,
  .solid {
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
  .ghost:hover {
    color: var(--text-1);
  }
  /* 主按钮靠表面层级与文本对比区分，不靠彩色填充 ——
     设计令牌里没有 --primary，那是刻意的（§11.4 颜色纪律：
     彩色只给出站色码与状态色，一个「保存」按钮不该抢走它们的语义）。 */
  .solid {
    background: var(--surface-2);
    color: var(--text-1);
    border: 1px solid var(--border-strong);
  }
  .solid:hover {
    border-color: rgba(255, 255, 255, 0.22);
  }
</style>
