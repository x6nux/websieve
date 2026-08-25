<script>
  /**
   * 控制窗口外壳（spec §11.3）。
   *
   *   ┌─ 状态条（常驻）─ 状态点 · 出站数 · 活跃连接 · sparkline · 速率 ─┐
   *   ├──────────────────────────────────────────────────────────────┤
   *   │  [ 流量 ]   规则   出站                              ⚙        │
   *   ├──────────────────────────────────────────────────────────────┤
   *   │   当前视图                                                    │
   *   └──────────────────────────────────────────────────────────────┘
   *
   * 流量是默认视图（打开即见走向）；设置走覆盖层而非第四个标签。
   *
   * ## 这一层的职责：所有 IPC 都在这里，视图一个都不发
   *
   * 五个视图全是纯展示 + 回调，`window.__TAURI__` 只出现在 `lib/ipc.js`，
   * 而 `lib/ipc.js` 只被本文件引用。好处有二：视图能在 jsdom 里不打桩就测；
   * 以及「控制窗口调了哪些命令」可被 grep，与 capabilities/control.json 逐条对得上。
   *
   * ## 与阶段 4 的三处落差，都在这里收口
   *
   * ① `traffic` 事件只有总量，没有分流 → 逐流明细由 `FlowStore` 从
   *    `connection` 事件在前端聚合（见 lib/flows.js）
   * ② `ConnectionDelta` 没有 bytes 字段 → 权重如实降级为连接数，措辞一律
   *    由 `weightUnit` 推出（见 lib/weight.js），绝不拿连接数写成「字节」
   * ③ 一半命令返回真实的 `NotReady` → `call()` 把 CmdError 结构化地交给对应
   *    视图**显示出来**，而不是吞掉。视图照常渲染，不白屏、不抛异常
   *
   * ## 状态怎么来的：一律是后端说了算，前端不自己推断
   *
   * `connected` 取的是「出站里有没有活着的」，不是「有没有点过连接按钮」——
   * `connect` 命令目前返回 NotReady，按点击推断会让状态点一直亮着绿灯而
   * 实际一条会话都没建起来。§6.4 的纪律在 UI 上的对应物就是这一条：
   * **绝不显示一个比事实更乐观的状态。**
   */
  import Segmented from './lib/Segmented.svelte';
  import TrafficView from './views/TrafficView.svelte';
  import RulesView from './views/RulesView.svelte';
  import OutboundsView from './views/OutboundsView.svelte';
  import SettingsOverlay from './views/SettingsOverlay.svelte';
  import { FlowStore } from './lib/flows.js';
  import { makePalette } from './lib/palette.js';
  import { bytes, count } from './lib/format.js';
  import { reorderOps } from './lib/reorder.js';
  import {
    mergeConfigProxies,
    mergeOutbound,
    parseRuleLine,
    probeResultOf,
    setScalar,
    toCmdError,
    whereOf,
  } from './lib/config-map.js';
  import {
    configGet,
    configGetRaw,
    configSave,
    configSaveRaw,
    listen,
    outboundEnable,
    outboundLatencyProbe,
    ruleTest,
    trafficSnapshot,
  } from './lib/ipc.js';

  let view = $state('traffic');
  let settingsOpen = $state(false);
  let win = $state('1h');

  let status = $state({ active: 0, downRate: 0, upRate: 0 });
  let spark = $state([]);
  /** config_get 的 config 部分（已脱敏）。规则单独一份，见下。 */
  let config = $state(null);
  /** 规则行。每条都带 line 与 raw —— 定点改写的全部依据，任何一环丢了就写不回去 */
  let rules = $state([]);
  let outbounds = $state([]);
  let probe = $state(null);

  /** 各条命令最近一次的 CmdError。分开存是因为它们要显示在不同的视图里 */
  let probeError = $state(null);
  let saveError = $state(null);
  let toggleError = $state(null);
  let latencyError = $state(null);
  /** 配置读取失败 / 连接事件溢出这类全局故障（§12），显示在导航条下方 */
  let alerts = $state([]);

  /**
   * 逐流明细在前端聚合（见 lib/flows.js）。
   *
   * `FlowStore` 是普通类实例，改它内部**不会**触发 Svelte 的响应式 ——
   * 必须靠一个 `$state` 版本号显式驱动。写成 `$derived.by` 而不是
   * `$derived((flowVersion, store.rows()))` 的逗号表达式，是因为后者会被
   * 格式化工具与 linter 当成笔误改掉，而改掉之后依赖就断了，表现为
   * 「图永远停在第一帧」，且没有任何报错。
   */
  const store = new FlowStore();
  let flowVersion = $state(0);
  const flows = $derived.by(() => {
    void flowVersion;
    return store.rows();
  });

  const colorOf = $derived(makePalette(outbounds.map((o) => o.name)));

  /** 出站里有没有活着的。见文件头「状态怎么来的」。 */
  const connected = $derived(outbounds.some((o) => o.state === 'live'));
  const enabledCount = $derived(outbounds.filter((o) => o.enabled).length);

  /** 混合端口。空状态要把它原样给用户，照着填代理设置 */
  const mixedPort = $derived(config?.['mixed-port'] ?? null);

  /**
   * 设置覆盖层吃的草稿。
   *
   * 从**脱敏过的** config 投影出具名字段，而不是把整份 config 递进去：
   * 覆盖层只渲染具名字段，多递的部分不会被渲染，但少递一点就少一条
   * 私钥意外进入渲染层的路径。kebab → camel 的翻译放在这里，
   * 是因为落盘时要翻回去（见 saveSettings），两次翻译挨着写才不会漂移。
   */
  const settingsDraft = $derived({
    mixedPort: config?.['mixed-port'] ?? 7890,
    allowLan: config?.['allow-lan'] ?? false,
    systemProxy: config?.['system-proxy'] ?? false,
    carrier: config?.carrier ?? 'shared',
  });

  /** 全局故障只留最近 5 条，且同一条消息不重复堆积 */
  function pushAlert(message) {
    if (alerts.at(-1)?.message === message) return;
    alerts = [...alerts, { message }].slice(-5);
  }

  /**
   * 把 IPC 的失败变成一个**能被显示出来**的值，而不是一个未捕获的 rejection。
   *
   * 返回 `{ ok, value, error }` 而非直接返回 null：调用方要区分
   * 「成功且值为 null」与「失败」。`toCmdError` 保证 error 一定是结构化的
   * `{ kind, message }`（见 lib/config-map.js），kind 一路带到视图 ——
   * 视图对 `not-ready` 与 `io` 的措辞不同，压成一句话就分不开了。
   *
   * **不在这里过滤 not-ready。** 计划的草稿版把它当「预期状态」静默掉，
   * 但那恰恰是用户最需要知道的一条：「功能没接上」与「这个域名确实没命中」
   * 在界面上看起来一模一样，而两者要做的下一步完全相反。
   */
  async function call(fn, ...args) {
    try {
      return { ok: true, value: await fn(...args), error: null };
    } catch (e) {
      return { ok: false, value: null, error: toCmdError(e) };
    }
  }


  /**
   * 事件订阅。
   *
   * 事件已在 Rust 侧聚合节流（§11.2）：traffic 1s、connection 200ms 一批、
   * rule-hit 1s 增量。**前端不再二次节流** —— 那只会叠加延迟，
   * 而节流的正确位置在产生侧。
   */
  $effect(() => {
    let disposed = false;
    const unlisteners = [];
    const bind = (name, handler) =>
      listen(name, handler).then(
        (un) => {
          // 卸载竞态：await 期间组件已经没了，那就当场撤销，
          // 否则这个监听会活到进程结束并往已卸载的组件上写状态
          if (disposed) un();
          else unlisteners.push(un);
        },
        // 订阅失败要说出来：静默失败的表现是「界面永远不更新」，
        // 而那看起来跟「没有流量」一模一样
        (e) => pushAlert(`事件 ${name} 订阅失败：${toCmdError(e).message}`),
      );

    bind('traffic', (e) => {
      const t = e.payload ?? {};
      status = {
        active: t.active ?? 0,
        downRate: t.down_rate ?? 0,
        upRate: t.up_rate ?? 0,
      };
      // sparkline 只留最近 40 个采样点（40 秒）
      spark = [...spark, t.down_rate ?? 0].slice(-40);
    });

    bind('connection', (e) => {
      store.apply(e.payload?.items ?? []);
      flowVersion++;
      if (e.payload?.dropped) {
        // 溢出要说出来 —— 悄悄丢数据会让用户对着一张不准的图排查
        pushAlert('连接事件溢出，部分流量未计入统计。图与表显示的都是不完整的样本。');
      }
    });

    bind('rule-hit', (e) => {
      // 增量计数，键是**规则原文**：events.rs 的 hit_delta 取自 RuleHits 的键，
      // 而那个键是判决路径上的规则字符串。所以这里必须按 raw 去对 ——
      // 拿解析后的 type/value 再拼一份回去，只会永远对不上。
      const d = e.payload ?? {};
      rules = rules.map((r) => (d[r.raw] ? { ...r, hits: r.hits + d[r.raw] } : r));
    });

    // status 事件是低频的一句话（connect/disconnect 未就绪的告知等），照实显示
    bind('status', (e) => pushAlert(String(e.payload)));

    bind('outbound-state', (e) => {
      // 单条状态更新（events.rs 的 OutboundState：{ name, state, latency_ms }），
      // 不是全量列表。按名字就地更新，认不出的名字追加一行 ——
      // 丢掉它等于「出站起来了但列表里没有」。
      const s = e.payload;
      if (!s?.name) return;
      outbounds = mergeOutbound(outbounds, s);
    });

    return () => {
      disposed = true;
      unlisteners.forEach((un) => un());
    };
  });

  /**
   * 后端的出站状态词 → 视图的状态词，以及单条事件并进列表的规则，
   * 都在 lib/config-map.js 里（那两件事都是可穷举单测的纯函数）。
   */
  $effect(() => {
    loadConfig();
    // 窗口刚开时补齐历史，不必空等下一个 1s tick。
    // 快照的 up_rate/down_rate 恒为 0（速率需要「上一次采样」，而快照没有），
    // 所以这里只取 active —— 把那个 0 当成速率写进去会让 sparkline 起一个假谷底。
    call(trafficSnapshot).then((r) => {
      if (r.ok && r.value) status = { ...status, active: r.value.active ?? 0 };
      else if (!r.ok) pushAlert(`流量快照读取失败：${r.error.message}`);
    });
  });

  /**
   * 读配置。
   *
   * `config_get` 返回 `{ config, rules }` 两列：规则单独一列是因为
   * `Spanned<String>` 序列化时**只吐出值**，行号会在 JSON 化的路上丢掉，
   * 而行号是 `config_save` 定点改写的唯一定位依据。
   * **在前端任何一环把 line 丢了，保留注释的编辑就彻底做不到了。**
   */
  async function loadConfig() {
    const r = await call(configGet);
    if (!r.ok) {
      // §12：配置读不出来（语法错、文件损坏）必须说清，且带上行号。
      // 界面照常渲染空列表 —— 白屏不会告诉用户任何事。
      pushAlert(`配置读取失败${whereOf(r.error)}：${r.error.message}`);
      return;
    }
    const v = r.value ?? {};
    config = v.config ?? {};
    const names = new Set((config.proxies ?? []).map((p) => p.name));
    rules = (v.rules ?? []).map((r0, i) => parseRuleLine(r0, i, names));
    // 配置里的出站先摆上，等 outbound-state 事件把状态填进来。
    // 不摆的话「一个出站都没有」的空状态会在配置明明写了出站时误报。
    outbounds = mergeConfigProxies(config.proxies ?? [], outbounds, config['carrier-host'] ?? '');
  }

  async function runProbe(target, { resolve } = {}) {
    if (!target) {
      probe = null;
      probeError = null;
      return;
    }
    const r = await call(ruleTest, target, !!resolve);
    if (!r.ok) {
      // 试算失败时**把上一次的判决清掉**：留着它，用户会以为那是刚输入的
      // 这个域名的判决，而它其实是上一个域名的
      probe = null;
      probeError = r.error;
      return;
    }
    probeError = null;
    // index 的 0→1 换算与「要不要提示需解析」都在 lib/config-map.js 里
    probe = probeResultOf(r.value, !!resolve);
  }

  /**
   * 排序 → `config_save` 的定点改写。
   *
   * **不是**「写回整份规则数组」：`config_save` 收的是 ops
   * （`{ op:'replace-rule', line, expect, value }`），只动那几行，
   * 用户手写的注释一个字节都不碰。翻译在 lib/reorder.js 里，
   * 缺 line/raw 时它会抛错而不是静默降级 —— 静默降级会让排序
   * 「看着生效了」而配置文件纹丝不动。
   *
   * 乐观更新 + 失败回滚：先动列表，键盘排序才能连按（焦点要跟着移动后的行走）；
   * 命令被拒就把列表退回去，并把错误原样交给视图。只回滚不报错、
   * 或只报错不回滚，都会让界面顺序与文件顺序分叉 —— 而分流只认文件。
   */
  async function reorder(from, to) {
    const before = rules;
    let ops;
    try {
      ops = reorderOps(before, from, to);
    } catch (e) {
      // reorderOps 只在「line 或 raw 丢了」时抛，那意味着定点改写已不可能。
      // 吞掉它等于让排序静默失效。
      saveError = { kind: 'other', message: String(e?.message ?? e) };
      return;
    }
    if (!ops.length) return;

    const next = [...before];
    const [x] = next.splice(from, 1);
    next.splice(to, 0, x);
    rules = next;

    const r = await call(configSave, ops);
    if (!r.ok) {
      rules = before;
      saveError = r.error;
      return;
    }
    saveError = null;
    // 重读一次让 line/raw 与文件重新对齐。定点改写只换了值，行号没变，
    // 但每条规则现在落在**另一行**上 —— 不重读的话，第二次排序会拿第一次
    // 排序前的 expect 去比对，被 config_save 的并发校验当场拒掉。
    await loadConfig();
  }

  /**
   * 规则启停。
   *
   * 配置格式里**没有** enabled 字段（`Config.rules` 是 `Vec<Spanned<String>>`，
   * 一条规则要么在要么不在），而 `wsieve_config::edit` 提供的是
   * `replace_rule_line` / `delete_rule_line` —— 停用只能等价于删除，
   * 而删除之后这条规则就从列表里消失了，用户没法再把它打开。
   *
   * 因此这里**不做一个切了以后什么都不会发生的假开关**：如实说明这条路
   * 现在走不通。假开关的危害是具体的 —— 用户以为某条规则已经停用，
   * 据此排查「流量为什么还走代理」，而它其实一直在生效。
   */
  function toggleRule() {
    saveError = {
      kind: 'not-ready',
      message:
        '规则的启用/停用尚未接入：配置格式里规则没有 enabled 字段，' +
        '停用等价于删除该行，而删除后它就不在列表里、无法再打开。' +
        '目前请直接编辑配置文件把该行注释掉。',
    };
  }

  /** 出站启停。命令目前返回真实的 NotReady，错误交给出站视图显示。 */
  async function toggleOutbound(id, enabled) {
    // 乐观翻转：开关得有反馈。命令被拒时视图会打出「开关的视觉状态不代表实际」，
    // 因此这里**不回滚** —— 回滚会让开关自己弹回去，看上去像是点击没生效，
    // 用户会继续猛点。
    outbounds = outbounds.map((o) => (o.id === id ? { ...o, enabled } : o));
    const r = await call(outboundEnable, id, enabled);
    toggleError = r.ok ? null : r.error;
  }

  async function probeLatency(id) {
    const r = await call(outboundLatencyProbe, id);
    if (!r.ok) {
      latencyError = r.error;
      return;
    }
    latencyError = null;
    outbounds = outbounds.map((o) => (o.id === id ? { ...o, latency: r.value } : o));
  }

  /**
   * 保存设置。
   *
   * **只能走 `config_save_raw`（整份覆盖）。** `wsieve_config::edit` 只导出
   * `replace_rule_line` / `delete_rule_line` 两个**规则行**改写器，没有标量
   * 字段的定点改写器 —— 端口、allow-lan、carrier 这些字段没有 `config_save`
   * 的路可走。这正是 §5.6 那句「非规则区的注释会丢」字面为真的原因，
   * 覆盖层里的那条提示不是免责声明，是事实描述。
   *
   * 做法：取原文 → 只替换被改动的那几个顶层标量 → 整份写回。逐行改而非重新
   * 序列化，是为了让没被碰过的行（包括规则区的全部注释）原样留下。
   * 写前 Rust 侧会解析并校验，不合法会带着行列号被拒。
   */
  async function saveSettings(draft) {
    const raw = await call(configGetRaw);
    if (!raw.ok) {
      // 原文取不到就不写 —— 拿一份凭空拼出来的 YAML 去覆盖是灾难性的
      pushAlert(`保存设置失败（读不到配置原文）${whereOf(raw.error)}：${raw.error.message}`);
      return;
    }
    // ⚠️ raw.value 含**明文私钥**。它只在这个函数里以局部变量存在：
    // 不进 $state、不进 console、不进 pushAlert、不进 DOM，改完立刻交回 Rust。
    let text = raw.value;
    for (const [key, value] of [
      ['mixed-port', draft.mixedPort],
      ['allow-lan', draft.allowLan],
      ['system-proxy', draft.systemProxy],
      ['carrier', draft.carrier],
    ]) {
      if (value === undefined) continue;
      text = setScalar(text, key, value);
    }

    const w = await call(configSaveRaw, text);
    if (!w.ok) {
      pushAlert(`保存设置失败${whereOf(w.error)}：${w.error.message}`);
      return;
    }
    settingsOpen = false;
    await loadConfig();
  }

  /**
   * 导出配置。
   *
   * ⚠️ `config_get_raw` 的返回值**含明文私钥**。它在这里只做一件事：交给
   * 浏览器的下载通道落盘。**不进 console、不进 alerts、不进任何 $state、
   * 不塞进会被截图的 DOM。** 覆盖层已在按钮旁给出 §5.4 要求的警告。
   *
   * 用 Blob + a[download] 而非 dialog/fs 插件：控制窗口的 capability 里只有
   * 那 16 条 wsieve_* 与 core:default，为一次导出加一条文件系统权限，
   * 等于为它永久扩大攻击面。Blob URL 同源、不出进程，用完立刻 revoke。
   */
  async function exportConfig() {
    const r = await call(configGetRaw);
    if (!r.ok) {
      // 只报「读不出来」。CmdError 的 message 是路径与 IO 错误，不含文件内容 ——
      // 若哪天它开始回显内容，这一行就是私钥进 DOM 的入口，故此注释留着。
      pushAlert(`导出失败${whereOf(r.error)}：${r.error.message}`);
      return;
    }
    const url = URL.createObjectURL(new Blob([r.value], { type: 'application/yaml' }));
    try {
      const a = document.createElement('a');
      a.href = url;
      a.download = 'websieve-config.yaml';
      a.click();
    } finally {
      URL.revokeObjectURL(url);
    }
  }

  /** §11.6：点桑基图的出站节点 → 跳到规则视图 */
  function pickOutbound(name) {
    view = 'rules';
    // 带着出站名跳过去当探针输入是错的：探针吃的是**目标域名**，
    // 塞一个出站名进去只会得到一条对不上的判决。这里只切视图。
    void name;
  }
</script>

<div class="app">
  <!-- 状态条常驻。
       **刻意不加 aria-live** —— traffic 事件 1s 一次，活动区域会让屏幕阅读器
       每秒打断一次用户正在读的内容，而这些播报还会排队堆积。那不是把信息
       给他，是让他没法用。role="status" 让它在辅助技术的地标列表里能被找到，
       需要时导航过来读即可。 -->
  <div class="status" role="status">
    <span class="dot" class:off={!connected} aria-hidden="true"></span>
    <span class="st-label">{connected ? '已连接' : '未连接'}</span>
    <span class="st-meta mono">
      {count(enabledCount)} 出站 · {count(status.active)} 活跃连接
    </span>

    <!-- sparkline 是趋势提示，真实数值在右侧 —— 对屏幕阅读器隐藏。
         对数压缩：速率跨几个数量级，线性画法平时就是贴地的一条直线。 -->
    <div class="spark" aria-hidden="true">
      {#each spark as v, i (i)}
        <i style:height="{Math.max(1, Math.min(22, Math.log10(v + 1) * 3.4))}px"></i>
      {/each}
    </div>
    <span class="rate mono">↓ {bytes(status.downRate)}/s</span>
  </div>

  <!-- segmented 导航，不是左侧图标栏（§11.4 明确拒绝套路一） -->
  <div class="nav">
    <Segmented
      label="视图"
      options={[
        { value: 'traffic', label: '流量' },
        { value: 'rules', label: '规则' },
        { value: 'outbounds', label: '出站' },
      ]}
      value={view}
      onchange={(v) => (view = v)} />
    <button
      type="button"
      class="gear"
      aria-label="打开设置"
      aria-haspopup="dialog"
      aria-expanded={settingsOpen}
      onclick={() => (settingsOpen = true)}>
      <!-- 图标对辅助技术隐藏，名字由 aria-label 给 —— 齿轮字符会被读成
           「齿轮」或干脆被跳过，两种都不是「打开设置」 -->
      <span aria-hidden="true">⚙</span>
    </button>
  </div>

  {#if alerts.length}
    <!-- §12：故障必须在界面上说出来。role="alert" 而非 polite —— 配置读不出来、
         事件丢数据都属于「你现在看到的东西不可信」，等用户读完别的再说就晚了。 -->
    <div class="alerts" role="alert">
      {#each alerts as a, i (i)}<p>{a.message}</p>{/each}
      <button type="button" class="dismiss" onclick={() => (alerts = [])}>清除全部提示</button>
    </div>
  {/if}

  <main>
    {#if view === 'traffic'}
      <TrafficView
        {flows}
        {colorOf}
        {connected}
        {mixedPort}
        outboundCount={outbounds.length}
        window={win}
        onWindowChange={(v) => (win = v)}
        onPickOutbound={pickOutbound}
        onAddOutbound={() => (settingsOpen = true)} />
    {:else if view === 'rules'}
      <RulesView
        {rules}
        {colorOf}
        {probe}
        {probeError}
        {saveError}
        ontest={runProbe}
        onreorder={reorder}
        ontoggle={toggleRule}
        onadd={() => (settingsOpen = true)} />
    {:else}
      <OutboundsView
        {outbounds}
        {colorOf}
        {toggleError}
        probeError={latencyError}
        ontoggle={toggleOutbound}
        onprobe={probeLatency}
        onadd={() => (settingsOpen = true)} />
    {/if}
  </main>

  <!-- 覆盖层只吃**具名的、已脱敏的**字段（见 settingsDraft）。导出走 onexport
       回调，由这一层在用户显式点击之后才去取原文 —— 私钥不会因为「打开过一次
       设置」就被复制进渲染进程的 JS 堆。 -->
  <SettingsOverlay
    open={settingsOpen}
    config={settingsDraft}
    onclose={() => (settingsOpen = false)}
    onsave={saveSettings}
    onexport={exportConfig} />
</div>

<style>
  .app {
    display: flex;
    flex-direction: column;
    height: 100vh;
  }

  .status {
    display: flex;
    align-items: center;
    gap: var(--space-5);
    padding: var(--space-3) var(--space-4);
    border-bottom: 1px solid var(--border);
    background: var(--surface-0);
    flex: none;
  }
  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--state-live);
    box-shadow: 0 0 0 3px rgba(63, 178, 127, 0.14);
    flex: none;
  }
  .dot.off {
    background: var(--text-4);
    box-shadow: 0 0 0 3px rgba(255, 255, 255, 0.05);
  }

  .st-label {
    font-size: var(--fs-13);
    font-weight: var(--fw-medium);
  }
  .st-meta {
    font-size: var(--fs-12);
    color: var(--text-3);
  }

  .spark {
    margin-left: auto;
    display: flex;
    align-items: flex-end;
    gap: 2px;
    height: 22px;
  }
  .spark i {
    width: 3px;
    background: var(--text-4);
    border-radius: 1px;
    display: block;
  }

  .rate {
    font-size: var(--fs-12);
    color: var(--text-2);
    min-width: 92px;
    text-align: right;
  }

  .nav {
    display: flex;
    align-items: center;
    padding: var(--space-2) var(--space-4);
    border-bottom: 1px solid var(--border);
    background: var(--surface-0);
    flex: none;
  }
  .gear {
    all: unset;
    margin-left: auto;
    padding: 2px var(--space-2);
    color: var(--text-3);
    cursor: pointer;
    font-size: var(--fs-14);
  }
  .gear:hover {
    color: var(--text-1);
  }
  /* all:unset 会把 outline 一并清掉，焦点环得自己加回来 */
  .gear:focus-visible {
    outline: 2px solid var(--outbound-1);
    outline-offset: 1px;
  }

  /* 告示用 borders-only，不用左侧粗色装饰边框（§11.4 明确拒绝） */
  .alerts {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-4);
    border-bottom: 1px solid var(--state-fail);
    background: var(--surface-0);
    flex: none;
  }
  .alerts p {
    margin: 0;
    width: 100%;
    font-size: var(--fs-12);
    color: var(--state-fail);
    line-height: 1.6;
  }
  .dismiss {
    all: unset;
    margin-left: auto;
    font-size: var(--fs-11);
    color: var(--text-3);
    cursor: pointer;
  }
  .dismiss:hover {
    color: var(--text-1);
  }
  .dismiss:focus-visible {
    outline: 2px solid var(--outbound-1);
    outline-offset: 1px;
  }

  main {
    flex: 1;
    overflow-y: auto;
  }
</style>
