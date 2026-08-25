<script>
  /**
   * Segmented control（spec §11.4）。
   *
   * 用它而非左侧图标导航栏：后者是同类产品的通用套路，且挤占宽度。
   *
   * 无障碍：用 role="radiogroup" 而非一堆 button —— 语义上这是
   * 「在若干选项里选一个」，不是「若干个独立动作」。屏幕阅读器会
   * 播报「N 之 M」，键盘可用左右方向键切换。
   *
   * 只有选中项 tabindex=0（roving tabindex）：radiogroup 的键盘约定是
   * Tab 进出整组、方向键在组内移动。每一项都能 Tab 到会让选项一多
   * 就需要按十几次 Tab 才能跨过去。
   *
   * ## keydown 挂在 radio 上，不挂在 radiogroup 上
   *
   * 这条是无障碍审计（a11y.test.js）逼出来的。原先挂在容器上，Svelte 报
   * `a11y_interactive_supports_focus`：「带 radiogroup 这个交互角色的元素
   * 必须有 tabindex」—— 这是本仓库当时唯一的构建警告。
   *
   * **但按它说的加 tabindex 是错的。** ARIA 的 roving tabindex 模式要求
   * 容器**不进** tab 序：加了之后 Tab 会先停在一个什么都不是的 div 上，
   * 再按一次才进到选项，而屏幕阅读器会把这一站读成一个空的分组。
   * 那是为了消警告而制造一个真的可用性缺陷。
   *
   * 正确的解法是把处理器挪到真正持有焦点的那个元素上：radio 自身。
   * 事件本来就从它那里冒泡上来，行为完全等价，而容器回归成一个纯粹的
   * 语义分组，警告随之消失 —— 消失是因为问题没了，不是因为被压住了。
   */
  let { options = [], value, onchange = () => {}, label = '' } = $props();

  /** 方向键切换后焦点要跟到新选中项，否则焦点留在旧项上、与选中态脱节 */
  let root;

  function focusSelected() {
    // 等 DOM 把 tabindex 更新完再移动焦点
    queueMicrotask(() => {
      root?.querySelector('[aria-checked="true"]')?.focus();
    });
  }

  function onKey(e) {
    const i = options.findIndex((o) => o.value === value);
    let next = null;
    if (e.key === 'ArrowRight' || e.key === 'ArrowDown') next = (i + 1) % options.length;
    if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') next = (i - 1 + options.length) % options.length;
    if (next === null) return;
    e.preventDefault();
    onchange(options[next].value);
    focusSelected();
  }
</script>

<div class="seg" role="radiogroup" aria-label={label} bind:this={root}>
  {#each options as o (o.value)}
    <button type="button" role="radio"
            aria-checked={o.value === value}
            class:on={o.value === value}
            tabindex={o.value === value ? 0 : -1}
            onkeydown={onKey}
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
