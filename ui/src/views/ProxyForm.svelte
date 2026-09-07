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
    const es = String(draft.extraSessions ?? '').trim();
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
