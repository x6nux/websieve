<script>
  /**
   * 开关。用 `role="switch"` 而非 checkbox —— 语义是「开/关一个持续状态」，
   * 不是「勾选一个选项」，屏幕阅读器的播报也因此不同（前者读「开/关」，
   * 后者读「已选中/未选中」）。
   *
   * 抽成组件是因为规则行与出站行都要它：两份实现迟早在尺寸、焦点环、
   * 禁用态上漂移，而这类漂移在肉眼验收里几乎看不出来。
   */
  let {
    checked = false,
    label = '',
    onchange = () => {},
    disabled = false,
  } = $props();
</script>

<button
  type="button"
  role="switch"
  aria-checked={checked}
  aria-label={label}
  {disabled}
  class:off={!checked}
  onclick={() => onchange(!checked)}
></button>

<style>
  button {
    all: unset;
    display: inline-block;
    width: 28px;
    height: 16px;
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
    right: 2px;
    top: 2px;
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: #fff;
  }
  button.off {
    background: rgba(255, 255, 255, 0.13);
  }
  button.off::after {
    right: auto;
    left: 2px;
    background: var(--text-3);
  }
  button:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  button:focus-visible {
    outline: 2px solid var(--outbound-1);
    outline-offset: 2px;
  }
</style>
