<script>
  /**
   * 探针即搜索框（signature ②，spec §11.5）。
   *
   * 这个输入框**不是过滤器，是试算探针**。三处刻意的差别，别把它做回搜索框：
   *
   *   ① **列表长度不变**。过滤器删掉不匹配的行，探针一行都不删 ——
   *      命中行升亮、其余整体降噪。因为「哪条规则命中」这个答案
   *      只在**它前面还有哪些规则没命中**的上下文里才有意义。
   *   ② **输出是判决，不是结果数**。搜索框报「找到 3 条」，探针报
   *      「命中第 3 条 · 判决 日本节点 · 前 2 条已试，未命中」。
   *   ③ **它回答的是「为什么」**。「前 N 条已试未命中」是排查的关键信息，
   *      搜索框从不告诉你它排除了什么。
   *
   * 因此这里没有放大镜图标、没有「清除」叉号、没有结果计数 ——
   * 那三样都是搜索框的语汇，摆上去就等于告诉用户「这是个过滤器」。
   * 标签是动词「试算」。
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
    /**
     * 命令失败时的 CmdError：`{ kind, message }`。
     *
     * `rule_test` 目前恒以 `kind: 'not-ready'` 被拒（正在服役的 RuleSet
     * 还没进 managed state）。这是**真实响应**而不是占位，照实显示即可 ——
     * 既不能吞掉（用户会以为「什么都没命中」），也不能伪造一个判决。
     */
    error = null,
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
    <input
      id="probe-in"
      class="in"
      type="text"
      value={text}
      autocomplete="off"
      spellcheck="false"
      placeholder="输入域名或 IP，看它会走哪条规则"
      aria-label="试算：输入域名查看分流判决"
      aria-describedby="probe-verdict"
      oninput={onInput} />
  </div>

  <p class="verdict" id="probe-verdict" aria-live="polite">
    {#if error}
      <!-- 命令报错时照实说，并说清是「功能没接上」还是「这个域名没命中」——
           把两者混为一谈会让用户去改一份根本没被读到的配置。 -->
      <span class="warn">试算不可用：{error.message}</span>
      {#if error.kind === 'not-ready'}
        <span class="muted">规则列表照常可读可排序，只是暂时算不出判决。</span>
      {/if}
    {:else if !result}
      <span class="muted">输入即试算，结果与真实判决一致。列表不会被过滤。</span>
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
  .probe {
    padding: 14px 16px;
    border-bottom: 1px solid var(--border);
  }
  .row {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .tag {
    font-size: var(--fs-11);
    letter-spacing: 0.07em;
    text-transform: uppercase;
    color: var(--text-4);
    font-weight: var(--fw-semibold);
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
  .in::placeholder {
    color: var(--text-4);
  }
  .in:focus-visible {
    outline: 2px solid var(--outbound-1);
    outline-offset: -1px;
    border-color: var(--border-strong);
  }

  .verdict {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 7px;
    margin: 9px 0 0;
    font-size: var(--fs-12);
    color: var(--text-2);
    min-height: 18px;
  }
  .muted {
    color: var(--text-3);
  }
  .warn {
    color: var(--state-warn);
  }
  .warn b {
    color: var(--text-1);
    font-weight: var(--fw-medium);
  }
  .name {
    color: var(--text-1);
  }
  .sep {
    color: var(--text-4);
  }

  .chip {
    width: 6px;
    height: 6px;
    border-radius: 2px;
    flex: none;
  }

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
  .verdict button:hover {
    color: var(--text-1);
  }
</style>
