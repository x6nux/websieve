<script>
  /**
   * 三层桑基图（spec §11.6）。
   *
   * 着色：流带按**最终去向**着色，非按来源。整条路径共享一个
   * userSpaceOnUse 全局横向渐变，左端 opacity ≈.08 → 右端 ≈.52。
   * 视觉语义即「未分类的流量被逐层筛清、各归其类」—— 正是 sieve 这个名字。
   * 左层与中层节点保持中性灰，**只有出站节点满色**。
   *
   * 交互：hover 时把噪音调暗（其余降至 16%）而非把目标点亮。
   * 这比给高亮项加光晕更克制，也更符合 Operate 模式。
   *
   * **粗细代表什么，由数据说了算。** 每一行都带 weightUnit（见 flows.js），
   * 图例、节点读数、朗读摘要全部从 weight.js 取词。当前后端不上报逐流字节，
   * 画的是连接数 —— 图上因此写着「不是吞吐量」，而不是默认标成流量。
   * 单位说不清时（rowsUnit 返回 null）整幅图不画，改出一段说明，
   * 因为一半字节一半连接数的桑基图没有任何含义。
   */
  import { Tween, prefersReducedMotion } from 'svelte/motion';
  import { cubicOut } from 'svelte/easing';
  import { toGraph, layout, applyFrozenOrder, gradientId } from '../lib/sankey-layout.js';
  import { tracePath } from '../lib/trace.js';
  import { pct } from '../lib/format.js';
  import {
    rowsUnit,
    totalWeight,
    formatWeight,
    weightSummary,
    widthLegend,
    unitNoun,
    UNIT_CONFLICT_MSG,
  } from '../lib/weight.js';

  let {
    /** 已经过 prepareFlows 折叠的流水行 */
    rows = [],
    /** (name) => 色值。出站色码由上层统一分配，保证全局一致 */
    colorOf,
    /** 点击出站节点 → 跳到规则视图并筛出该出站的规则（spec §11.6） */
    onPickOutbound = () => {},
    width = 960,
    height = 452,
  } = $props();

  /** 节点顺序一旦确定即冻结，只有出现新节点时才追加 */
  let frozen = $state([]);
  let hovered = $state(null);
  let locked = $state(null);

  /** 粗细的含义。null = 行与行之间对不上，整幅图不画。 */
  const unit = $derived(rowsUnit(rows));
  const conflicted = $derived(rows.length > 0 && unit === null);

  const target = $derived(conflicted ? null : toGraph(rows));
  const values = $derived(target ? target.links.map((l) => l.value) : []);

  // 插值权重值本身，每帧重算布局：节点高度、流带宽度、贝塞尔路径三者
  // 必须同步变化，而 CSS 只能动 stroke-width，另外两个会瞬跳、形状被撕开。
  // 实测单次布局 0.065ms，占 60fps 预算的 0.4%，每帧重算完全负担得起。
  const tween = new Tween([], { duration: 200, easing: cubicOut });
  let prevShape = '';

  $effect(() => {
    const shape = target ? target.links.map((l) => l.key).join('|') : '';
    // 流带集合本身变了（新站点/新规则出现）时直接跳变：
    // 插值两个长度不同的向量没有意义。reduced-motion 同样直接跳变。
    const instant = shape !== prevShape || prefersReducedMotion.current;
    prevShape = shape;
    tween.set(values, instant ? { duration: 0 } : undefined);
  });

  const model = $derived.by(() => {
    if (!target) return null;
    const v = tween.current;
    const usable = v.length === target.links.length;
    const g = applyFrozenOrder(
      {
        nodes: target.nodes,
        links: target.links.map((l, i) => ({ ...l, value: usable ? v[i] : l.value })),
      },
      frozen
    );
    return layout(g, width, height);
  });

  $effect(() => {
    if (!model) return;
    // 只增不改：已有节点保持原位次，新节点追加到末尾
    const known = new Set(frozen);
    const added = model.nodes.map((n) => n.id).filter((id) => !known.has(id));
    if (added.length) frozen = [...frozen, ...added];
  });

  const focus = $derived(locked ?? hovered);
  const hot = $derived(model ? tracePath(model.links, focus) : null);

  /** null = 无 focus，全部原样；否则不在闭包里的降至 16% */
  const dim = (key) => hot !== null && !hot.has(key);

  const dests = $derived(model ? [...new Set(model.links.map((l) => l.dest))] : []);
  /** 合计用**原始行**算，不用插值中的值 —— 读数不能跟着动画抖 */
  const total = $derived(totalWeight(rows));

  const legend = $derived(widthLegend(unit));
  const noun = $derived(unitNoun(unit));

  /** 节点自身的量。插值过程中 d3 会重算 value，取不到时退回 0。 */
  const nodeVal = (n) => n.value ?? 0;
  const readout = (n) => formatWeight(nodeVal(n), unit);

  function activate(n) {
    if (n.layer === 2) onPickOutbound(n.dest);
    else locked = locked === n.id ? null : n.id;
  }

  function onKey(e, n) {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      activate(n);
    } else if (e.key === 'Escape') {
      locked = null;
    }
  }

  const layerNoun = (layer) => (layer === 0 ? '站点' : layer === 1 ? '规则' : '出站');

  /**
   * 图的文字摘要。**只此一处**，容器的 aria-label 与下面那段 .sr-only 共用它 ——
   * 两边各写一句，迟早有一句先被改而另一句不知道，而屏幕阅读器用户听到的
   * 恰恰可能是没被改的那一句。
   *
   * 「完整数据请切换到表视图」不是客套：§11.6 记录桑基图的无障碍评级为 C，
   * 图的职责就是给摘要并指路，真正的数据在表里。
   */
  const summary = $derived(
    `流量走向桑基图：${rows.length} 条流，合计 ${weightSummary(total, unit)}。` +
      `流带粗细代表${noun}。完整数据请切换到表视图。`,
  );
</script>

{#if conflicted}
  <!-- 单位对不上时**不画图**。挑一个「看起来对」的单位会把这个 bug 永久藏起来，
       而混了两种量的桑基图本身没有含义，画出来只会误导。 -->
  <p class="conflict" role="alert">{UNIT_CONFLICT_MSG}</p>
{:else if model}
  <figure>
    <!--
      role="group" 而**不是** role="img" —— 这一条是无障碍审计抓出来的。

      `role="img"` 的语义是「一张不可再分的图」，它的子节点对辅助技术
      **一律不暴露**（ARIA 的 presentational children 规则）。而下面那些 rect
      是 tabindex=0 的可聚焦节点：§11.6 要求点击出站节点能跳到规则视图，
      键盘等价物就是它们。两者放在一起自相矛盾 —— axe 的 nested-interactive
      报的正是这个，实际后果则取决于屏幕阅读器的实现：要么那些节点根本读不到
      （于是键盘能 Tab 进去却听不到自己在哪），要么读到一堆按 ARIA 规范
      本不该存在的东西。两种都不可接受。

      `role="group"` 支持可聚焦后代，进入时播报 aria-label。图的职责一个字
      没变（§11.6：图本身评级 C，它只负责给摘要 + 指向表视图），
      只是换了一个不与交互打架的容器角色。

      摘要另外用 .sr-only 在下面写了一份可见于无障碍树的文本：group 的名字
      在部分屏幕阅读器里只在「进入」那一刻播报一次，用户往回走就再也听不到。
      一段能被正常导航读到的文字比一个只播一次的名字可靠。
    -->
    <svg viewBox="0 0 {width} {height}" role="group" aria-label={summary}>
      <defs>
        {#each dests as d (d)}
          <!-- userSpaceOnUse：整条路径共享同一个渐变，而非每段各自从头开始。
               这正是「逐层筛清」这个视觉语义的实现方式。 -->
          <linearGradient id={gradientId(d)} gradientUnits="userSpaceOnUse"
                          x1={model.gradientX[0]} x2={model.gradientX[1]}>
            <stop offset="0" stop-color={colorOf(d)} stop-opacity=".08" />
            <stop offset="1" stop-color={colorOf(d)} stop-opacity=".52" />
          </linearGradient>
        {/each}
      </defs>

      <g class="links">
        {#each model.links as l (l.key)}
          <path d={l.d} fill="none"
                stroke="url(#{gradientId(l.dest)})"
                stroke-width={Math.max(1, l.width)}
                class:faded={dim(l.key)} />
        {/each}
      </g>

      {#each model.nodes as n (n.id)}
        {@const h = Math.max(1, n.y1 - n.y0)}
        <rect class="node" x={n.x0} y={n.y0} width={n.x1 - n.x0} height={h}
              fill={n.layer === 2 ? colorOf(n.dest) : 'rgba(255,255,255,.17)'}
              opacity={n.layer === 2 ? 0.78 : 0.55}
              tabindex="0"
              role="button"
              aria-label={n.layer === 2
                ? `出站 ${n.label}，${readout(n)} ${noun}，占 ${pct(nodeVal(n) / total)}。按回车筛出相关规则。`
                : `${layerNoun(n.layer)} ${n.label}，${readout(n)} ${noun}`}
              onmouseenter={() => (hovered = n.id)}
              onmouseleave={() => (hovered = null)}
              onfocus={() => (hovered = n.id)}
              onblur={() => (hovered = null)}
              onclick={() => activate(n)}
              onkeydown={(e) => onKey(e, n)} />

        <!-- 标签低于阈值时隐藏，hover/focus 才出 —— 避免细流带的标签糊成一片 -->
        {#if h >= 12 || focus === n.id}
          <text class={n.layer === 2 ? 'lbl-out' : 'lbl-m'}
                class:faded-t={hot !== null && focus !== n.id}
                x={n.layer === 0 ? n.x0 - 8 : n.x1 + 8}
                y={(n.y0 + n.y1) / 2 - 1}
                text-anchor={n.layer === 0 ? 'end' : 'start'}>{n.label}</text>
          {#if h >= 26 || focus === n.id}
            <text class="lbl-v" class:faded-t={hot !== null && focus !== n.id}
                  x={n.layer === 0 ? n.x0 - 8 : n.x1 + 8}
                  y={(n.y0 + n.y1) / 2 + 13}
                  text-anchor={n.layer === 0 ? 'end' : 'start'}>
              {readout(n)}{n.layer === 2 ? ` · ${pct(nodeVal(n) / total)}` : ''}
            </text>
          {/if}
        {/if}
      {/each}
    </svg>

    <!-- 图的文字摘要。**必须存在且可被正常导航读到** —— §11.6 把结构性流图的
         无障碍评级定为 C，图的职责因此不是「传达数据」而是「给出摘要并指向
         能传达数据的地方」。放在 svg 之后而不是只挂在 aria-label 上，
         是因为容器名在部分屏幕阅读器里只在进入那一刻播报一次。 -->
    <p class="sr-only">{summary}</p>

    <!-- 图例不是装饰：桑基图这个形态本身在暗示吞吐量，不写清楚就等于默认标错轴。
         它必须跟着 weightUnit 变，且视觉上可见（不能藏进 sr-only）。 -->
    <figcaption>
      <span class="swatch" aria-hidden="true"></span>{legend}
    </figcaption>
  </figure>
{/if}

<style>
  figure { margin: 0; }

  svg { display: block; width: 100%; height: auto; background: var(--surface-1); }

  path {
    /* screen 混合让重叠的流带自然叠加而非互相遮挡 */
    mix-blend-mode: screen;
    transition: opacity 120ms ease-out;
  }
  /* hover 时把噪音调暗，而非把目标点亮 */
  .faded { opacity: .16; }
  .faded-t { opacity: .34; }

  .node { rx: 1.5; cursor: pointer; }
  .node:focus-visible { outline: 2px solid var(--text-1); outline-offset: 2px; }

  /* paint-order: stroke + 画布色描边 —— 压在任何流带上均可读 */
  .lbl-m, .lbl-out, .lbl-v {
    paint-order: stroke;
    stroke: var(--surface-1);
    stroke-linejoin: round;
    pointer-events: none;
  }
  .lbl-m   { font-family: var(--font-mono); font-size: 11px;   fill: var(--text-2); stroke-width: 3.5px; }
  .lbl-out { font-size: var(--fs-12); font-weight: 500; fill: var(--text-1); stroke-width: 3.5px; }
  .lbl-v   { font-family: var(--font-mono); font-size: 10.5px; fill: var(--text-4);
             font-variant-numeric: tabular-nums; stroke-width: 3px; }

  figcaption {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-4) var(--space-3);
    font-size: var(--fs-11);
    color: var(--text-3);
    background: var(--surface-1);
  }
  /* 一小段渐变示意「粗细」这件事本身，用中性色 —— 出站色码在这里
     会被误读成某个具体出站 */
  .swatch {
    width: 22px;
    height: 7px;
    border-radius: 1px;
    background: linear-gradient(
      to right,
      rgba(255, 255, 255, .08),
      rgba(255, 255, 255, .34)
    );
  }

  .conflict {
    margin: 0;
    padding: var(--space-5) var(--space-4);
    font-size: var(--fs-12);
    line-height: 1.7;
    color: var(--state-warn);
    background: var(--surface-1);
  }

  @media (prefers-reduced-motion: reduce) {
    path { transition: none; }
  }
</style>
