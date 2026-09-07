<script>
  /**
   * 规则视图 —— 排查主场（spec §11.3 / §11.5）。
   *
   * 两个 signature 都在这里：
   *   ① 命中热度染色：行背景按命中数中性染色，死规则几乎透明
   *   ② 探针即搜索框：顶部输入即试算，命中行升亮、其余降噪，**一行都不删**
   *
   * spec §11.4 明确拒绝：规则类型**不做成彩色 pill**。类型用等宽小写
   * 缩写 + 统一低对比灰，靠列位置识别，颜色全部让给出站。彩色 pill 会把
   * 规则列表变成彩虹糖，恰好摧毁它唯一需要的能力 —— 扫读。
   *
   * ## 排序的三条纪律
   *
   * ① **拖拽与键盘走同一条路**。两者都只调 `onreorder(from, to)`，
   *    绝不各算各的 —— 分叉的那天没人会发现，因为没人同时用两种方式排序。
   * ② **移动后播报位置**。拖拽的视觉反馈在键盘路径上一个字都不存在，
   *    按完 Alt+↓ 若不播报，屏幕阅读器用户得到的是彻底的静默。
   * ③ **保存失败必须回滚并说出来**。乐观更新 + 静默失败 = 列表显示的顺序
   *    与文件里的顺序不一致，而分流结果只认后者。那比不让排序更糟。
   */
  import Probe from './Probe.svelte';
  import EmptyState from './EmptyState.svelte';
  import Segmented from '../lib/Segmented.svelte';
  import { heatColor } from '../lib/heat.js';
  import { keyboardMove, positionAnnouncement } from '../lib/reorder.js';
  import { count } from '../lib/format.js';

  /**
   * 内置分流预设的可选项。`custom` 是「规则」——用户在 config.yaml 里手写
   * 的那份，其余三个是产品内置、完全无视文件 `rules:` 数组的预设。
   *
   * `direct`/`global` 对应路由引擎自己的 `mode` 三态之二
   * （`wsieve_route::Mode::Direct`/`Global`，见 engine.rs 的短路逻辑：
   * 两者从不咨询任何规则）。`china` 对应 `mode: rule` + 内置的
   * `wsieve_route::CHINA_PRESET_RULES`，与 `custom`（同样是 `mode: rule`，
   * 但规则来自文件）的区别只在于规则来源，靠正交的 `rule-preset` 字段区分。
   */
  const PRESET_OPTIONS = [
    { value: 'direct', label: '全局直连' },
    { value: 'global', label: '全局代理' },
    { value: 'china', label: '中国大陆' },
    { value: 'custom', label: '规则' },
  ];

  let {
    rules = [],
    colorOf,
    /** 当前生效的分流预设：direct / global / china / custom */
    preset = 'custom',
    /** 已脱敏 config 里的 global-outbound，供 direct/global/china 的说明文案引用 */
    globalOutbound = '',
    /** 探针结果，见 Probe.svelte */
    probe = null,
    /** 探针命令的 CmdError（`rule_test` 目前恒为 not-ready） */
    probeError = null,
    /**
     * 上一次排序/启停写回的 CmdError：`{ kind, message }`。
     *
     * `config_save` 本身是**完整实现**（按行号定点改写、保留注释），
     * 但它会因并发校验失败（配置被外部改过）、语义校验失败、IO 失败而拒绝。
     * 这些都必须原样呈现 —— 一个「排序看起来生效了但从未落盘」的列表
     * 比不让排序更糟。
     */
    saveError = null,
    ontest = () => {},
    onreorder = () => {},
    ontoggle = () => {},
    onadd = () => {},
    onpresetchange = () => {},
  } = $props();

  const maxHits = $derived(rules.reduce((m, r) => Math.max(m, r.hits ?? 0), 0));

  /** 探针激活时，命中行升亮、其余整体降噪 */
  const hitIndex = $derived(probe && !probe.needResolve ? probe.index - 1 : -1);
  const probing = $derived(probe !== null);

  /**
   * 排序后播报位置（纪律②）。用独立的 aria-live 区域而不是复用探针的：
   * 两者的播报时机会互相打断，屏幕阅读器只会读到后到的那一条。
   */
  let announcement = $state('');

  /**
   * 排序后要把焦点找回来的那条规则（纪律②的另一半）。
   *
   * **移动 DOM 节点会让它失去焦点。** 带 key 的 `{#each}` 重排时走的是
   * `insertBefore`，而把一个已在文档里的元素 insertBefore 到别处，等价于
   * 先移除再插入 —— 焦点随即落回 `<body>`。实测：按一次 Alt+↓ 之后
   * `document.activeElement` 就是 `body` 了，**第二次按键根本没有接收者**，
   * 键盘排序在第一步之后就断了。所以焦点必须手工找回。
   *
   * 三个刻意的选择，每一个都是踩出来的：
   *
   * - **按 id 找，不按下标。** 下标会取到还没重排的那一行，把焦点抢到
   *   隔壁规则上 —— 用户再按一次，改的就是他没打算改的那条。
   * - **普通变量而非 `$state`。** 写成 `$state` 会让「记下待聚焦项」这个
   *   赋值本身就触发下面那个 effect，而那一刻 DOM 还没重排：聚焦的是
   *   马上要被移动的节点，随后的重排照样把它 blur 掉，等于没做。
   *   只让 `rules` 与 `tbodyEl` 当触发源。
   * - **`tbodyEl` 必须是 `$state`。** `bind:this` 的赋值若不是响应式的，
   *   effect 在它还是 null 时跑过一次就再也不会重跑，焦点永远找不回来。
   */
  let pendingFocus = null;
  let tbodyEl = $state(null);

  $effect(() => {
    // 读这两个建立依赖：DOM 重排完（rules 换了新引用）之后这个 effect 才重跑
    void rules;
    const tb = tbodyEl;
    if (pendingFocus === null || !tb) return;
    const el = tb.querySelector(`.handle[data-rule-id="${pendingFocus}"]`);
    pendingFocus = null;
    el?.focus();
  });

  function chip(target) {
    if (target === 'DIRECT') return 'var(--state-direct)';
    if (target === 'REJECT') return 'var(--state-fail)';
    return colorOf(target);
  }

  /** 规则的可读名字，播报与 aria-label 共用一处，避免两边措辞漂移 */
  function ruleLabel(r) {
    return r.type === 'final' || r.type === 'match' ? `${r.type} ${r.target}` : `${r.type} ${r.value}`;
  }

  let dragFrom = $state(null);
  let dragOver = $state(null);

  function onDragStart(e, i) {
    dragFrom = i;
    e.dataTransfer.effectAllowed = 'move';
    // Firefox 要求必须 setData 才会真的开始拖
    e.dataTransfer.setData('text/plain', String(i));
  }
  function onDragOver(e, i) {
    e.preventDefault();
    e.dataTransfer.dropEffect = 'move';
    dragOver = i;
  }
  function onDragEnd() {
    dragFrom = null;
    dragOver = null;
  }
  function onDrop(e, i) {
    e.preventDefault();
    if (dragFrom !== null && dragFrom !== i) {
      // 拖拽与键盘走同一个出口（纪律①）
      commitMove(dragFrom, i);
    }
    onDragEnd();
  }

  /**
   * 排序的唯一出口。拖拽与 Alt+↑/↓ 都走这里 ——
   * 两条路径若各自实现，迟早有一条先被改而另一条不知道。
   */
  function commitMove(from, to) {
    // 名字与总数在**动手之前**取。排序由父组件回传新数组完成，
    // `onreorder` 返回时 `rules` 还是旧的那一份，事后再读会读到过期状态；
    // 而移动一次不改变条数，总数用旧的那份也是对的。
    const label = ruleLabel(rules[from]);
    const total = rules.length;
    onreorder(from, to);
    announcement = positionAnnouncement(to, total, label);
  }

  /** 拖拽的键盘等价物：Alt+↑/↓。没有它，排序对键盘用户等于不存在。 */
  function onHandleKey(e, i) {
    if (!e.altKey) return;
    if (e.key !== 'ArrowUp' && e.key !== 'ArrowDown') return;
    e.preventDefault();
    const r = keyboardMove(rules, i, e.key);
    if (r.index === i) {
      // 到头了。静默会让用户以为是按键没生效，继续猛按。
      announcement = e.key === 'ArrowUp' ? '已在最前，无法再上移。' : '已在最后，无法再下移。';
      return;
    }
    // 焦点会因 DOM 重排而丢失（见 pendingFocus 的注释）。记下这条规则的 id，
    // 等父组件把新数组传回来、DOM 重排完之后再按 id 找回来。
    pendingFocus = rules[i].id;
    commitMove(i, r.index);
  }

  /**
   * 「中国大陆」预设的展示用规则行——只读，镜像
   * `wsieve_route::CHINA_PRESET_RULES`（rule.rs）的前两条，第三条 MATCH
   * 收尾行的目标依赖 `global-outbound`，运行时数据，因此在这里现算而不是
   * 写死。这些行**没有 `line` 号**，写不回 `config_save` 的定点改写，
   * 所以不渲染拖拽把手/启用开关——它们不是可编辑的真实配置行。
   */
  const chinaPresetRows = $derived([
    { type: 'geosite', value: 'cn', target: 'DIRECT' },
    { type: 'geoip', value: 'CN', target: 'DIRECT' },
    { type: 'match', value: '*', target: globalOutbound || '（尚未设置）' },
  ]);
</script>

<section class="view" aria-label="分流规则">
  <div class="preset-bar">
    <Segmented label="分流预设" options={PRESET_OPTIONS} value={preset} onchange={onpresetchange} />
  </div>

  <Probe {colorOf} result={probe} error={probeError} {ontest} />

  {#if saveError}
    <!--
      §12 的纪律落到这里：写回失败必须说清**是什么失败**，
      并明确「界面上的顺序还没进文件」。role=alert 而非 aria-live=polite ——
      顺序没落盘是需要立刻打断的事，等用户读完别的再说就晚了。
    -->
    <p class="save-err" role="alert">
      <span class="hd">顺序未保存</span>
      <span class="msg">{saveError.message}</span>
      <span class="tail">
        {#if saveError.kind === 'config-invalid'}
          配置文件在此期间被改过，界面上的行号已陈旧。刷新后重试 —— 照旧行号改下去会改到别的规则头上。
        {:else}
          文件里的顺序仍是改动前的那一份，分流按文件走。
        {/if}
      </span>
    </p>
  {/if}

  {#if preset === 'direct'}
    <p class="preset-note">
      当前处于<b>全局直连</b>模式：所有流量直接连接，不经过任何出站，也不咨询下面的规则。
    </p>
  {:else if preset === 'global'}
    <p class="preset-note">
      当前处于<b>全局代理</b>模式：所有流量都走
      {#if globalOutbound}
        <b>{globalOutbound}</b>
      {:else}
        <span class="warn">尚未在设置里指定全局出站，需要先配置它</span>
      {/if}
      ，不咨询下面的规则。
    </p>
  {:else if preset === 'china'}
    <p class="preset-note">
      当前处于<b>中国大陆</b>预设：完全无视配置文件里自带的规则，只用下面这三条内置规则。
    </p>
    <table class="china-table">
      <caption class="sr-only">中国大陆内置预设，共 3 条只读规则，不可编辑。</caption>
      <thead>
        <tr>
          <th scope="col">类型</th>
          <th scope="col">匹配值</th>
          <th scope="col">出站</th>
        </tr>
      </thead>
      <tbody>
        {#each chinaPresetRows as r (r.type + r.value)}
          <tr class="builtin">
            <td class="type mono">{r.type}</td>
            <td class="val mono">{r.value}</td>
            <td class="out">
              <span class="chip" style:background={chip(r.target)} aria-hidden="true"></span>{r.target}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {:else if !rules.length}
    <EmptyState
      title="还没有规则。"
      hint="规则决定流量往哪走，顺序即优先级 —— 首命中即返回。至少需要一条 MATCH 兜底，否则未命中的流量无处可去。"
      action="添加第一条规则"
      onaction={onadd} />
  {:else}
    <!-- 排序结果的播报区。视觉上不可见，但对键盘路径是唯一的反馈通道。 -->
    <p class="sr-only" aria-live="polite" role="status">{announcement}</p>

    <table class="rules-table">
      <caption class="sr-only">
        分流规则共 {rules.length} 条，按顺序匹配、首命中即返回。
        使用拖拽把手或 Alt 加上下方向键调整顺序。
      </caption>
      <thead>
        <tr>
          <th scope="col"><span class="sr-only">顺序</span></th>
          <th scope="col">类型</th>
          <th scope="col">匹配值</th>
          <th scope="col">出站</th>
          <th scope="col" class="r">命中</th>
          <th scope="col"><span class="sr-only">启用</span></th>
        </tr>
      </thead>
      <tbody bind:this={tbodyEl}>
        {#each rules as r, i (r.id)}
          <tr
            style:background={probing ? undefined : heatColor(r.hits, maxHits)}
            class:hit={i === hitIndex}
            class:dim={probing && i !== hitIndex}
            class:invalid={r.unknownOutbound}
            class:off={!r.enabled}
            class:dragging={dragFrom === i}
            class:over={dragOver === i && dragFrom !== null && dragFrom !== i}
            aria-current={i === hitIndex ? 'true' : undefined}
            ondragover={(e) => onDragOver(e, i)}
            ondrop={(e) => onDrop(e, i)}>

            <td class="mark">
              <button type="button" class="handle"
                      data-rule-id={r.id}
                      aria-label={`移动规则 ${ruleLabel(r)}，当前第 ${i + 1} 条，共 ${rules.length} 条。按 Alt 加上下方向键调整顺序`}
                      draggable="true"
                      ondragstart={(e) => onDragStart(e, i)}
                      ondragend={onDragEnd}
                      onkeydown={(e) => onHandleKey(e, i)}>
                <span aria-hidden="true">{i === hitIndex ? '▸' : '⠿'}</span>
              </button>
            </td>

            <!-- 类型：等宽小写缩写 + 统一低对比灰。不是 pill，不带颜色。 -->
            <td class="type mono">{r.type}</td>
            <td class="val mono" title={r.value}>{r.value}</td>

            <td class="out">
              <span class="chip" style:background={chip(r.target)} aria-hidden="true"></span>{r.target}
              {#if r.unknownOutbound}
                <span class="err" title="该出站不存在，此规则将被跳过">不存在</span>
              {/if}
            </td>

            <td class="hits mono r">{count(r.hits)}</td>

            <td class="sw-cell">
              <button type="button" role="switch" class="sw"
                      aria-checked={r.enabled}
                      aria-label={`启用规则 ${ruleLabel(r)}`}
                      class:off={!r.enabled}
                      onclick={() => ontoggle(r.id, !r.enabled)}></button>
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>

<style>
  .view { background: var(--surface-1); }

  /* 写回失败的告示。borders-only，不用左侧粗色边框（§11.4 明确拒绝）——
     一圈均匀的边框 + 状态色文字足以传达，且与整体气质一致。 */
  .save-err {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 8px;
    margin: 0;
    padding: 10px 16px;
    border-bottom: 1px solid var(--border);
    background: var(--surface-0);
    font-size: var(--fs-12);
    line-height: 1.6;
  }
  .save-err .hd {
    color: var(--state-fail);
    font-weight: var(--fw-semibold);
    flex: none;
  }
  .save-err .msg { color: var(--text-2); }
  .save-err .tail { color: var(--text-3); }

  table { width: 100%; border-collapse: collapse; table-layout: fixed; }

  /* 列宽取自 mockup：标记 / 类型 / 匹配值 / 出站 / 命中 / 开关。
     只对可排序主表生效——只读的 china-table 只有三列，语义完全不同。 */
  .rules-table th:nth-child(1), .rules-table td:nth-child(1) { width: 30px; }
  .rules-table th:nth-child(2), .rules-table td:nth-child(2) { width: 74px; }
  .rules-table th:nth-child(4), .rules-table td:nth-child(4) { width: 150px; }
  .rules-table th:nth-child(5), .rules-table td:nth-child(5) { width: 66px; }
  .rules-table th:nth-child(6), .rules-table td:nth-child(6) { width: 44px; }

  th {
    height: 28px;
    font-size: 10px;
    letter-spacing: .08em;
    text-transform: uppercase;
    color: var(--text-4);
    font-weight: var(--fw-semibold);
    text-align: left;
    padding: 0 8px;
    background: var(--surface-0);
    border-bottom: 1px solid var(--border);
  }
  th.r, td.r { text-align: right; }

  tbody tr {
    height: var(--row-rule);
    border-bottom: 1px solid rgba(255, 255, 255, .035);
  }
  tbody tr:last-child { border-bottom: none; }

  td {
    padding: 0 8px;
    font-size: 12.5px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .type { color: var(--text-3); font-size: var(--fs-12); }
  .val  { color: var(--text-1); }
  .out  { color: var(--text-2); }
  .hits { color: var(--text-3); font-size: var(--fs-12); font-variant-numeric: tabular-nums; }

  .chip {
    display: inline-block;
    width: 6px; height: 6px;
    border-radius: 2px;
    margin-right: 7px;
    vertical-align: middle;
  }

  /* 探针激活：其余降噪，命中行升亮（signature ②） */
  .dim .val, .dim .type, .dim .out, .dim .hits { color: var(--text-4); }
  .dim .chip { opacity: .32; }

  .hit { background: rgba(91, 143, 249, .11); }
  .hit .val { color: #fff; font-weight: var(--fw-medium); }
  .hit .mark { color: var(--outbound-1); }
  .hit .type, .hit .hits { color: var(--text-2); }
  .hit .out { color: var(--text-1); }

  /* 拖拽中：被拖的行淡出，落点行显示一条上边界。
     不做整行位移动画 —— 32px 的行高下那只会让人看不清落在哪。 */
  .dragging { opacity: .4; }
  .over { box-shadow: inset 0 2px 0 0 var(--outbound-1); }

  /* 引用了不存在的出站（spec §12）：标红但不阻断，该规则视为不匹配跳过 */
  .invalid .out { color: var(--state-fail); }
  .err {
    margin-left: 6px;
    font-size: var(--fs-11);
    color: var(--state-fail);
    opacity: .85;
  }

  .off .val, .off .type { opacity: .45; }

  .handle {
    all: unset;
    display: block;
    width: 100%;
    text-align: center;
    color: var(--text-4);
    font-size: var(--fs-11);
    cursor: grab;
  }
  .handle:hover { color: var(--text-3); }
  .handle:focus-visible { outline: 2px solid var(--outbound-1); outline-offset: 1px; }

  .sw-cell { text-align: right; }
  .sw {
    all: unset;
    display: inline-block;
    width: 26px; height: 15px;
    border-radius: 8px;
    background: var(--state-live);
    position: relative;
    cursor: pointer;
    vertical-align: middle;
  }
  .sw::after {
    content: '';
    position: absolute;
    right: 2px; top: 2px;
    width: 11px; height: 11px;
    border-radius: 50%;
    background: #fff;
  }
  .sw.off { background: rgba(255, 255, 255, .13); }
  .sw.off::after { right: auto; left: 2px; background: var(--text-3); }
  .sw:focus-visible { outline: 2px solid var(--outbound-1); outline-offset: 2px; }

  .preset-bar {
    padding: 10px 16px;
    border-bottom: 1px solid var(--border);
    background: var(--surface-0);
  }

  /* 内置预设（direct/global/china）的说明文案与只读表 —— 与可编辑的
     .rules-table 视觉上刻意不同：没有拖拽把手、没有开关，读者不该以为
     这里能编辑。 */
  .preset-note {
    margin: 0;
    padding: 12px 16px;
    font-size: var(--fs-12);
    color: var(--text-2);
    line-height: 1.7;
  }
  .preset-note b { color: var(--text-1); font-weight: var(--fw-medium); }
  .preset-note .warn { color: var(--state-warn); }

  .china-table tbody tr.builtin { color: var(--text-2); }
</style>
