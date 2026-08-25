<script>
  /**
   * 空状态（spec §11.3 / §11.6）。
   *
   * 两条纪律：
   *   - 无流量时**不画空的坐标骨架**。空骨架传达的是「这里本该有东西
   *     但坏了」，而真相是「还没开始」
   *   - 「暂无数据」本身是一种失败：一个刚装好的代理，这里是用户见到的
   *     第一屏。它必须说清**这里将会出现什么**，以及**怎样让它出现** ——
   *     后者要具体到可照做的一步，而不是「请开始使用」这种空话
   *
   * 反 AI 塑料感：不用插画、不用大图标、不用居中的巨型标题撑场面。
   * 一行说明 + 一条可照做的指令 + 一个动作，与整体的仪器气质一致。
   */
  let {
    title = '',
    hint = '',
    /** 可照做的那一步。文本 + 可选的等宽片段（端口、地址这类要能被复制的东西） */
    step = null,
    action = null,
    onaction = () => {},
  } = $props();
</script>

<div class="empty">
  <p class="t">{title}</p>
  {#if hint}<p class="h">{hint}</p>{/if}
  {#if step}
    <p class="s">
      {step.text}{#if step.code}<code>{step.code}</code>{/if}{#if step.tail}{step.tail}{/if}
    </p>
  {/if}
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
  .s {
    font-size: var(--fs-12);
    color: var(--text-3);
    margin: 14px 0 0;
    line-height: 1.7;
  }
  /* 端口这类要被照抄的东西给等宽 + 一格底色，与说明文字区分开 */
  .s code {
    font-family: var(--font-mono);
    font-size: var(--fs-12);
    color: var(--text-2);
    background: var(--surface-2);
    border-radius: 3px;
    padding: 1px 6px;
    margin: 0 2px;
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
