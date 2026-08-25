<script>
  /**
   * 流量表视图（spec §11.6）。
   *
   * **这是桑基图的必需等价视图，不是可选装饰。** 桑基图的无障碍评级为 C ——
   * 结构性流图无法只靠颜色传达。判据是：只看表的人，不能比只看图的人
   * 少知道任何一件事。图上有的「站点 → 规则 → 出站」三段、粗细代表的量、
   * 占全局的比例、出站小计、以及哪些行是聚合出来的，这里逐项都有对应物。
   *
   * 无障碍要点：
   *   - 用真正的 <table> + <caption> + <th scope="col">，不用 div 模拟
   *   - 每个列头带 aria-sort，排序状态对屏幕阅读器可见
   *   - 排序触发器是 <button>，天然可 Tab 可 Enter
   *   - 出站列**同时**给色块与文字 —— 颜色绝不是唯一的信息载体
   *   - 聚合行用文字标注，不只靠颜色变暗（后者屏幕阅读器拿不到）
   *
   * 列是**按数据决定的**，不是钉死的五列：无字节数据时不摆一列全是 0 的
   * 「字节」，那等于往表里填假数据。阶段 2 补上逐流字节后字节列自动出现。
   */
  import { pct } from '../lib/format.js';
  import {
    rowsUnit,
    rowWeight,
    totalWeight,
    formatWeight,
    weightSummary,
    unitNoun,
    UNIT_CONFLICT_MSG,
    WEIGHT_BYTES,
    WEIGHT_CONNS,
  } from '../lib/weight.js';

  let { rows = [], colorOf } = $props();

  const unit = $derived(rowsUnit(rows));
  const conflicted = $derived(rows.length > 0 && unit === null);
  const total = $derived(totalWeight(rows));

  /**
   * 列的组成随数据变。
   *
   * - 权重列的表头写的是**这个量真正的名字**（当前是「连接数」），
   *   与桑基图图例同源，不会一个说连接一个说字节
   * - 字节列只在真有字节数据时出现
   * - 占比列需要一个说得清的分母，单位冲突时整列消失
   */
  const cols = $derived.by(() => {
    const out = [
      { key: 'site', label: '目标站点', num: false },
      { key: 'rule', label: '命中规则', num: false },
      { key: 'outbound', label: '出站', num: false },
    ];
    if (unit === WEIGHT_BYTES) {
      out.push({ key: 'weight', label: '字节', num: true });
      out.push({ key: 'conns', label: '连接数', num: true });
    } else {
      // 单位是连接数、或说不清时：连接数这一列本身不依赖共同单位，照给
      out.push({ key: 'conns', label: '连接数', num: true });
    }
    if (!conflicted) out.push({ key: 'share', label: '占比', num: true });
    return out;
  });

  /**
   * 默认排序列：打开就看到最大的那条。
   *
   * 它必须是**权重列本身**，而权重列的 key 随单位变（字节时是 'weight'，
   * 连接数时权重就是连接数那一列）。写死成 'weight' 会让连接数模式下
   * 没有任何一列匹配 sortKey，表头的 aria-sort 全是 none，
   * 屏幕阅读器读到的是「没有排序」，而表其实是排过序的。
   */
  const defaultSortKey = $derived(unit === WEIGHT_BYTES ? 'weight' : 'conns');

  let sortKey = $state(null);
  let sortDir = $state('desc');

  /** 用户还没点过任何列头时跟随默认列 */
  const activeKey = $derived(sortKey ?? defaultSortKey);

  /** 排序取值。占比与权重同序，共用一个量 —— 分开算会出现「按占比排却不单调」。 */
  const valueOf = (r, key) => {
    if (key === 'share' || key === 'weight') return rowWeight(r);
    if (key === 'conns') return r.conns ?? 0;
    return r[key];
  };

  const sorted = $derived(
    [...rows].sort((a, b) => {
      const x = valueOf(a, activeKey);
      const y = valueOf(b, activeKey);
      const c =
        typeof x === 'number' && typeof y === 'number'
          ? x - y
          : String(x).localeCompare(String(y), 'zh-Hans-CN');
      return sortDir === 'asc' ? c : -c;
    })
  );

  /**
   * 按某一列汇总。桑基图上一个节点的**高度**就是这个数，
   * 而高度是纯视觉的，屏幕阅读器一点也拿不到 —— 所以表里必须有等价物。
   */
  const talliesBy = (key) => {
    const m = new Map();
    for (const r of rows) m.set(r[key], (m.get(r[key]) ?? 0) + rowWeight(r));
    return [...m.entries()].sort((a, b) => b[1] - a[1]);
  };

  /** 出站小计 —— 图上右列节点的高度。彩色语义的载体，视觉上也给出。 */
  const byOutbound = $derived(talliesBy('outbound'));
  /**
   * 规则小计 —— 图上中间列节点的高度。
   * 只进 sr-only：视觉用户可以点「命中规则」列头把同规则的行排到一起扫读，
   * 屏幕阅读器用户做不到这件事，得把数直接给他。
   */
  const byRule = $derived(talliesBy('rule'));

  function sortBy(col) {
    if (activeKey === col.key) {
      sortDir = sortDir === 'asc' ? 'desc' : 'asc';
    } else {
      sortKey = col.key;
      // 数值列默认降序（先看大的），文本列默认升序（字典序）
      sortDir = col.num ? 'desc' : 'asc';
    }
  }

  const ariaSort = (k) =>
    activeKey === k ? (sortDir === 'asc' ? 'ascending' : 'descending') : 'none';

  const noun = $derived(unitNoun(unit));
</script>

{#if conflicted}
  <!-- 图在这种情况下整幅消失；表不必，因为「站点/规则/出站/连接数」这些
       逐行事实不依赖共同单位。只有需要统一分母的占比列消失。 -->
  <p class="conflict" role="alert">{UNIT_CONFLICT_MSG}</p>
{/if}

{#if rows.length}
  <table aria-label="流量明细">
    <caption>
      <span class="sum">
        共 {rows.length} 条流{#if !conflicted}，合计 {weightSummary(total, unit)}{/if}
      </span>
      {#if !conflicted}
        <!-- 出站小计：桑基图右列节点高度的表内等价物 -->
        <span class="tallies">
          {#each byOutbound as [name, v] (name)}
            <span class="tally">
              <span class="chip" style:background={colorOf(name)} aria-hidden="true"></span>{name}
              <b>{formatWeight(v, unit)}</b>
            </span>
          {/each}
        </span>
      {/if}
      <span class="sr-only">
        列为{cols.map((c) => c.label).join('、')}，点击列头可排序。
        {#if noun}数值列的量是{noun}。{/if}
        {#if !conflicted}
          按规则小计：{byRule.map(([n, v]) => `${n} ${formatWeight(v, unit)}`).join('，')}。
        {/if}
      </span>
    </caption>
    <thead>
      <tr>
        {#each cols as col (col.key)}
          <th scope="col" aria-sort={ariaSort(col.key)} class:n={col.num}>
            <button type="button" onclick={() => sortBy(col)}>
              {col.label}<span class="arrow" aria-hidden="true"
                >{activeKey === col.key ? (sortDir === 'asc' ? '↑' : '↓') : ''}</span>
            </button>
          </th>
        {/each}
      </tr>
    </thead>
    <tbody>
      {#each sorted as r (`${r.site}|${r.rule}|${r.outbound}`)}
        <tr class:agg={r.aggregated}>
          {#each cols as col (col.key)}
            {#if col.key === 'site'}
              <td class="mono">
                {r.site}{#if r.aggregated}<span class="tag">（聚合）</span>{/if}
              </td>
            {:else if col.key === 'rule'}
              <td class="mono">{r.rule}</td>
            {:else if col.key === 'outbound'}
              <td>
                <!-- 色块是辅助，文字才是信息 —— 颜色绝不是唯一载体 -->
                <span class="chip" style:background={colorOf(r.outbound)} aria-hidden="true"></span>{r.outbound}
              </td>
            {:else if col.key === 'weight'}
              <td class="n mono">{formatWeight(rowWeight(r), unit)}</td>
            {:else if col.key === 'conns'}
              <td class="n mono">{formatWeight(r.conns ?? 0, WEIGHT_CONNS)}</td>
            {:else}
              <td class="n mono">{pct(total ? rowWeight(r) / total : 0)}</td>
            {/if}
          {/each}
        </tr>
      {/each}
    </tbody>
  </table>
{/if}

<style>
  table { width: 100%; border-collapse: collapse; font-size: 12.5px; }

  caption {
    caption-side: top;
    display: flex;
    align-items: center;
    gap: var(--space-3);
    flex-wrap: wrap;
    padding: var(--space-2) var(--space-4);
    text-align: left;
    font-size: var(--fs-11);
    color: var(--text-3);
    background: var(--surface-1);
  }
  .sum { color: var(--text-2); }
  .tallies { display: flex; gap: var(--space-3); flex-wrap: wrap; margin-left: auto; }
  .tally { display: inline-flex; align-items: center; color: var(--text-3); }
  .tally b {
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
    font-weight: var(--fw-medium);
    color: var(--text-2);
    margin-left: var(--space-1);
  }

  th {
    text-align: left;
    font-size: 10px;
    letter-spacing: .08em;
    text-transform: uppercase;
    color: var(--text-4);
    font-weight: 600;
    background: var(--surface-0);
    border-bottom: 1px solid var(--border);
    padding: 0;
  }
  th.n { text-align: right; }

  th button {
    all: unset;
    display: block;
    width: 100%;
    padding: 8px 16px;
    cursor: pointer;
    box-sizing: border-box;
    font: inherit;
    color: inherit;
    letter-spacing: inherit;
    text-transform: inherit;
    text-align: inherit;
  }
  th button:hover { color: var(--text-2); }

  .arrow { display: inline-block; width: 1em; color: var(--text-2); }

  td {
    padding: 7px 16px;
    border-bottom: 1px solid rgba(255, 255, 255, .035);
    color: var(--text-2);
  }
  tr:last-child td { border-bottom: none; }

  td.n {
    text-align: right;
    font-variant-numeric: tabular-nums;
  }

  /* 聚合行用最暗的中性色 —— 它不是一个真实的站点。
     颜色只是辅助，真正传达这件事的是同一格里的「（聚合）」二字。 */
  .agg td:first-child, .agg td:nth-child(2) { color: var(--text-4); }
  .tag { color: var(--text-4); font-family: var(--font-sans); font-size: var(--fs-11); }

  .chip {
    display: inline-block;
    width: 6px; height: 6px;
    border-radius: 2px;
    margin-right: 7px;
    vertical-align: middle;
  }

  .conflict {
    margin: 0;
    padding: var(--space-3) var(--space-4);
    font-size: var(--fs-12);
    line-height: 1.7;
    color: var(--state-warn);
    background: var(--surface-1);
    border-bottom: 1px solid var(--border);
  }
</style>
