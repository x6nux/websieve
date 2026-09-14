<script>
  /**
   * 流量视图 —— 默认视图，打开即见走向（spec §11.3）。
   *
   * 内部有「图 ⇄ 表」切换与时间窗口。图与表**共享同一份数据**，
   * 因此两边的合计读数必然相等 —— 不等就说明 prepareFlows 的守恒被破坏了。
   *
   * 这里不提供「字节 / 连接数」切换：那会把「按什么计量」做成用户偏好，
   * 而它其实是**数据源的事实**（后端有没有上报逐流字节）。给出一个切不动的
   * 开关，或者切了以后显示一列全是 0 的字节，都是在骗人。措辞一律由
   * weightUnit 推出，见 lib/weight.js。
   *
   * 空状态按**真实状态**分支，不是一句通用的「暂无数据」：没连上、连上了
   * 还没有流量、一个出站都没有，这三种情况用户要做的下一步完全不同。
   */
  import Segmented from '../lib/Segmented.svelte';
  import Sankey from './Sankey.svelte';
  import FlowTable from './FlowTable.svelte';
  import EmptyState from './EmptyState.svelte';
  import { prepareFlows } from '../lib/aggregate.js';
  import { rowsUnit, totalWeight, weightSummary } from '../lib/weight.js';

  let {
    /** 原始流水行：{ site, rule, outbound, weight, weightUnit, conns, bytes } */
    flows = [],
    colorOf,
    /** 代理是否已在运行。决定空状态该让用户做什么 */
    connected = false,
    /** 混合入口端口。空状态要把它原样给出去，用户照着填代理设置 */
    mixedPort = null,
    /** 已配置的出站个数。为 0 时连不上任何地方，先去加服务器 */
    outboundCount = null,
    window: win = '1h',
    onWindowChange = () => {},
    onPickOutbound = () => {},
    /** 让用户从空状态直接连上，不必回到别处找按钮 */
    onConnect = null,
    /** 一个出站都没有时，引导去设置 */
    onAddOutbound = null,
  } = $props();

  let shape = $state('chart');   // chart | table

  const rows = $derived(prepareFlows(flows));
  const unit = $derived(rowsUnit(rows));
  const total = $derived(totalWeight(rows));

  // 流数少于 3 时桑基图本不适用，自动降级为表（spec §11.6）
  const degraded = $derived(rows.length > 0 && rows.length < 3);
  const showTable = $derived(shape === 'table' || degraded);

  /**
   * 空状态的文案与动作。三种情况互斥，按「用户下一步该做什么」排序：
   * 连出站都没有 → 没连上 → 连上了在等第一条连接。
   */
  const empty = $derived.by(() => {
    if (outboundCount === 0) {
      return {
        title: '还没有配置任何出站。',
        hint: '没有出站可去，规则再对也无处可送 —— 代理会拒绝连接而不是偷偷直连。',
        step: { text: '先添加一个服务器，再回到这里看流量走向。' },
        action: onAddOutbound ? '去添加服务器' : null,
        onaction: onAddOutbound ?? (() => {}),
      };
    }
    if (!connected) {
      return {
        title: '代理未运行，因此没有流量经过。',
        hint: '启动后，这里会实时显示「目标站点 → 命中规则 → 出站」的完整走向：哪个站点被哪条规则拦下、最后送去了哪个出站。',
        step: { text: '点下面的按钮启动代理。' },
        action: onConnect ? '启动代理' : null,
        onaction: onConnect ?? (() => {}),
      };
    }
    return {
      title: '代理已在运行，还没有连接经过。',
      hint: '这里会实时显示「目标站点 → 命中规则 → 出站」的完整走向：哪个站点被哪条规则拦下、最后送去了哪个出站。',
      step: mixedPort
        ? {
            text: '把浏览器或系统代理指向 ',
            code: `127.0.0.1:${mixedPort}`,
            tail: '（HTTP 与 SOCKS5 同口），然后随便打开一个网页。',
          }
        : { text: '把浏览器或系统代理指向本机的混合入口，然后随便打开一个网页。' },
      action: null,
      onaction: () => {},
    };
  });
</script>

<section class="view" aria-label="流量走向">
  <div class="bar">
    <Segmented label="时间窗口"
      options={[
        { value: '5m',  label: '5 分钟' },
        { value: '1h',  label: '1 小时' },
        { value: 'run', label: '本次运行' },
      ]}
      value={win} onchange={onWindowChange} />

    <div class="push">
      <Segmented label="显示形态"
        options={[{ value: 'chart', label: '图' }, { value: 'table', label: '表' }]}
        value={showTable ? 'table' : 'chart'}
        onchange={(v) => (shape = v)} />
      <!-- 合计带量词（「25 条连接」而非光秃秃的「25」），否则会被读成体积。
           **刻意不加 aria-live**：traffic 事件 1s 一次，活动区域会让屏幕阅读器
           每秒打断一次用户正在读的内容，且这些播报会排队堆积 —— 那不是
           把信息给他，是让他没法用。需要合计时，表视图的 caption 里有，
           他可以自己去读。 -->
      <span class="total mono">{weightSummary(total, unit)}</span>
    </div>
  </div>

  {#if !rows.length}
    <EmptyState
      title={empty.title}
      hint={empty.hint}
      step={empty.step}
      action={empty.action}
      onaction={empty.onaction} />
  {:else if showTable}
    {#if degraded}
      <p class="hint">流数少于 3 条，桑基图不适用，已切换为列表。</p>
    {/if}
    <FlowTable {rows} {colorOf} />
  {:else}
    <Sankey {rows} {colorOf} {onPickOutbound} />
  {/if}
</section>

<style>
  .view { background: var(--surface-1); }
  .bar {
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 11px 16px;
    border-bottom: 1px solid var(--border);
    background: var(--surface-0);
  }
  .push { margin-left: auto; display: flex; align-items: center; gap: 12px; }
  .total { font-size: var(--fs-12); color: var(--text-2); }
  .hint {
    margin: 0;
    padding: 8px 16px;
    font-size: var(--fs-12);
    color: var(--text-4);
    border-bottom: 1px solid var(--border);
  }
</style>
