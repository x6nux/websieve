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
    /** 编辑时的初值：{ type, value, target, noResolve }。type 只做大小写/空白
     * 归一化（见 draftFrom），不做词形翻译——调用方必须先把 `parseRuleLine`
     * 产出的短写（'suffix'、'keyword'、'final' 等，注意不是 'domain-suffix'
     * 这种拼出来的形式）用 config-map.js 的 `ruleTypeToFormType` 转成表单
     * 认的大写类型，再传进来：`initial = { type: ruleTypeToFormType(row.type), ... }`。 */
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

  /**
   * initial → 草稿。**只做大小写归一化**（防御性的：手写/拼接的 `initial`
   * 大小写可能不一致），不做词形翻译。
   *
   * 它不认识、也不会去猜 `parseRuleLine` 的展示短写（'suffix'、'keyword'、
   * 'final' 等）——那一步翻译必须在调用方发生，靠 config-map.js 的
   * `ruleTypeToFormType` 完成（`final` → `MATCH`、`suffix` → `DOMAIN-SUFFIX`
   * 等）。谁接下来要把「编辑」模式接上，传入的 `initial.type` 必须已经是
   * `ruleTypeToFormType` 的输出，而不是 `rules[].type` 原样传入——原样传入
   * 在这里只会被大写化成一个 TYPES 里不存在的值（比如 'SUFFIX'），
   * 下拉框选不中任何选项，且这里不会报错提醒你。
   */
  function draftFrom(v) {
    if (!v) return blank();
    return {
      type: String(v.type ?? 'DOMAIN').toUpperCase(),
      value: v.value ?? '',
      target: v.target ?? '',
      noResolve: !!v.noResolve,
    };
  }

  let dialog = $state(null);
  let draft = $state(draftFrom(untrack(() => initial)));
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

    draft = draftFrom(initial);
    localError = '';
    restoreFocus = document.activeElement;

    tick().then(() => {
      // 故意不把 button 加进这个选择器（与 SettingsOverlay 的等价查询不同）：
      // 这个面板 DOM 里第一个可聚焦元素是 header 里的 × 关闭按钮，
      // 选上 button 就会把初始焦点抢给它，而不是第一个真正的表单字段。
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
