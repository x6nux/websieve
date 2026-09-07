<script>
  /**
   * 首页（设计文档「代理组与首页视图」§3）。
   *
   * 四张卡片：节点选择、系统代理/虚拟网卡、分流模式、流量统计。视觉上延续既有设计令牌
   * （borders-only、密度高、IBM Plex）——不引入圆角阴影卡片皮肤，参考 Clash Verge 的是
   * 「首页该放什么」这条功能分区，不是它的视觉风格（后者与本项目「密集像交易台，
   * 克制像 Proxyman」的定位正相反）。
   *
   * 「节点选择卡片」只列 kind === 'select' 的组——auto/load-balance 没有用户手动可切的
   * 「当前值」，不该出现在这张卡片里。
   */
  import Segmented from '../lib/Segmented.svelte';
  import Switch from '../lib/Switch.svelte';
  import EmptyState from './EmptyState.svelte';
  import { bytes } from '../lib/format.js';
  import { PRESET_OPTIONS } from '../lib/routing-preset.js';

  let {
    /** proxy-groups 的全量列表（已脱敏 config 的一部分） */
    groups = [],
    systemProxy = false,
    tunEnabled = false,
    /** 与 RulesView 顶部同一份状态：direct / global / china / custom */
    preset = 'custom',
    /** 状态条已有的 sparkline 采样点，这里放大复用，不重新聚合 */
    spark = [],
    downRate = 0,
    upRate = 0,
    onselectmember = () => {},
    onsystemproxychange = () => {},
    ontunchange = () => {},
    onpresetchange = () => {},
  } = $props();

  const selectGroups = $derived(groups.filter((g) => g.kind === 'select'));
  let activeGroupName = $state('');
  const activeGroup = $derived(
    selectGroups.find((g) => g.name === activeGroupName) ?? selectGroups[0] ?? null,
  );

  function pickGroup(name) {
    activeGroupName = name;
  }
  function pickMember(member) {
    if (activeGroup) onselectmember(activeGroup.name, member);
  }
</script>

<div class="home">
  <section class="card">
    <h2>节点选择</h2>
    {#if !selectGroups.length}
      <EmptyState
        title="还没有配置代理组。"
        hint="代理组让规则指向一个可切换的组而不是固定节点。在 config.yaml 的 proxy-groups 段加一个 kind: select 的组，填好 proxies 成员列表，这里就会出现对应的选择器。" />
    {:else}
      {#if selectGroups.length > 1}
        <label class="row">
          <span class="lbl">代理组</span>
          <select
            aria-label="代理组"
            value={activeGroup?.name}
            onchange={(e) => pickGroup(e.currentTarget.value)}>
            {#each selectGroups as g (g.name)}
              <option value={g.name}>{g.name}</option>
            {/each}
          </select>
        </label>
      {/if}
      {#if activeGroup}
        <label class="row">
          <span class="lbl">节点</span>
          <select
            aria-label="节点"
            value={activeGroup.selected}
            onchange={(e) => pickMember(e.currentTarget.value)}>
            {#each activeGroup.proxies as name (name)}
              <option value={name}>{name}</option>
            {/each}
          </select>
        </label>
      {/if}
    {/if}
  </section>

  <section class="card">
    <h2>系统代理 / 虚拟网卡</h2>
    <div class="row">
      <span class="lbl">系统代理</span>
      <Switch checked={systemProxy} label="系统代理" onchange={onsystemproxychange} />
    </div>
    <div class="row">
      <span class="lbl">虚拟网卡（TUN）</span>
      <Switch checked={tunEnabled} label="虚拟网卡（TUN）" onchange={ontunchange} />
    </div>
  </section>

  <section class="card">
    <h2>分流模式</h2>
    <Segmented label="分流预设" options={PRESET_OPTIONS} value={preset} onchange={onpresetchange} />
  </section>

  <section class="card">
    <h2>流量统计</h2>
    <div class="spark" aria-hidden="true">
      {#each spark as v, i (i)}
        <!-- 对数刻度：一次流量尖峰不该把其余柱子全部压扁成看不出差异的平线。
             *6 后夹在 [1, 40]px 之间——40 留出一点余量，对应 .spark 容器的 44px 高。 -->
        <i style:height="{Math.max(1, Math.min(40, Math.log10(v + 1) * 6))}px"></i>
      {/each}
    </div>
    <p class="rates mono">
      <span>↓ {bytes(downRate)}/s</span>
      <span>↑ {bytes(upRate)}/s</span>
    </p>
  </section>
</div>

<style>
  .home {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: var(--space-3);
    padding: var(--space-4);
    background: var(--surface-1);
  }

  .card {
    border: 1px solid var(--border);
    border-radius: var(--radius);
    padding: var(--space-4);
    background: var(--surface-0);
  }
  .card h2 {
    margin: 0 0 var(--space-3);
    font-size: var(--fs-13);
    font-weight: var(--fw-semibold);
    color: var(--text-2);
  }

  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
    padding: 6px 0;
  }
  .lbl {
    font-size: var(--fs-12);
    color: var(--text-3);
  }

  select {
    background: var(--surface-2);
    color: var(--text-1);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius);
    padding: 4px 8px;
    font-family: inherit;
    font-size: var(--fs-12);
  }

  .spark {
    display: flex;
    align-items: flex-end;
    gap: 2px;
    height: 44px;
  }
  .spark i {
    display: block;
    width: 4px;
    background: var(--outbound-1);
    opacity: 0.7;
    border-radius: 1px;
  }
  .rates {
    display: flex;
    gap: var(--space-3);
    margin: var(--space-2) 0 0;
    font-size: var(--fs-12);
    color: var(--text-2);
  }
</style>
