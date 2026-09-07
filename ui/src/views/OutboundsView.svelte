<script>
  /**
   * 出站列表（spec §11.4）。
   *
   * 行式而非卡片网格：§11.4 明确拒绝「服务器卡片网格 + 圆形延迟指示」。
   * 节点多了以后卡片网格既浪费空间又难扫读，而这个视图唯一需要的能力
   * 就是纵向比较 —— 哪个快、哪个断了、哪个在扛流量。
   *
   * 每行只留 色码 · 名字 · 延迟 · 会话数 · 开关，延迟与会话数用
   * tabular-nums 对齐成列，一扫就能比较。
   *
   * ## 延迟的定义（spec §11.2）—— 要如实呈现，不能含糊
   *
   * | 场景 | 取值 |
   * |---|---|
   * | 稳态（有流量） | 最近 N 次上行 `POST /api/sync` 响应耗时的**中位数**（零额外流量） |
   * | 刚建立、尚无流量 | 握手 RTT |
   * | 手动点「测速」 | 发一个 PADDING TU 的 POST 并计时 |
   *
   * 三者的量纲不同，用户拿它跟 ping 比会得出「程序在撒谎」的结论。
   * 所以表头下面那句出处说明不是装饰，是这一列可被正确解读的前提。
   * **不另开探测子流** —— 复用既有流量路径，既省实现也少一个可探测面。
   *
   * ## §6.4 的安全决策要在 UI 上真的做出来
   *
   * 出站不可用时是「拒绝连接 + UI 报警」，绝不静默回退。所以
   * reconnecting / failed 两个状态必须显眼 —— 一个安静的灰字等于没报警。
   *
   * ## 两条命令当前是真的未就绪
   *
   * `outbound_enable` 与 `outbound_latency_probe` 在 src-tauri 里返回
   * `CmdError::NotReady`（出站管理器还在 `run_stack` 的局部作用域里，
   * 没暴露到命令面）。这不是占位，是这个视图上线第一天就会走到的分支。
   * 处置照抄规则视图的先例：`role="alert"` 点名缺的是什么 + 说清什么还能用。
   *
   * 尤其是启停：开关是**乐观**翻转的（父组件先改本地状态再发命令），
   * 命令被拒时视觉上的「开」与实际的「没生效」会分叉，
   * 而分叉的方向恰好是最危险的那个 —— 用户以为流量在走这个出站。
   * 所以报警里必须写明「开关的视觉状态不代表实际」。
   */
  import Switch from '../lib/Switch.svelte';
  import EmptyState from './EmptyState.svelte';
  import { ms, count } from '../lib/format.js';

  let {
    outbounds = [],
    colorOf,
    /**
     * 上一次 `outbound_enable` 的 CmdError：`{ kind, message }`。
     * 目前恒为 `kind: 'not-ready'`。
     */
    toggleError = null,
    /** 上一次 `outbound_latency_probe` 的 CmdError，目前恒为 not-ready */
    probeError = null,
    ontoggle = () => {},
    onprobe = () => {},
    onadd = () => {},
    ondelete = () => {},
  } = $props();

  /** 待二次确认删除的出站 id；null 表示没有任何一行处在确认态 */
  let confirmingDelete = $state(null);
  function askDelete(id) {
    confirmingDelete = id;
  }
  function confirmDelete(name) {
    confirmingDelete = null;
    ondelete(name);
  }
  function cancelDelete() {
    confirmingDelete = null;
  }

  /**
   * 状态文案与色。
   *
   * live 刻意**没有文字** —— 正常态占着一格「已连接」只会稀释注意力，
   * 而这一列的价值全在异常上。它的延迟列显示真实数字，那本身就是「活着」
   * 的证据。异常态则一律给文字：§11.4 的无障碍底线是状态不能只靠颜色。
   */
  const STATE = {
    live: { text: '', color: 'var(--text-3)' },
    starting: { text: '连接中', color: 'var(--state-warn)' },
    reconnecting: { text: '重连中', color: 'var(--state-warn)' },
    failed: { text: '已断开', color: 'var(--state-fail)' },
    stopped: { text: '已停止', color: 'var(--text-4)' },
  };

  /** 整行的屏幕阅读器播报。逐列读过去会丢掉列与列的关系，一句话读完才有上下文。 */
  const desc = (o) => {
    const s = STATE[o.state] ?? STATE.stopped;
    const parts = [o.name];
    if (o.host) parts.push('宿主');
    parts.push(s.text || '已连接');
    if (o.latency != null) parts.push(`延迟 ${ms(o.latency)}`);
    parts.push(`${count(o.sessions)} 会话`);
    if (!o.enabled) parts.push('已停用');
    return parts.join('，');
  };
</script>

<section class="view" aria-label="出站服务器">
  {#if toggleError}
    <!--
      启停被拒。role="alert" 而非 aria-live="polite"：开关的视觉状态与实际
      分叉是要立刻打断的事 —— 用户正准备把流量交给这个出站。
    -->
    <p class="cmd-err" role="alert">
      <span class="hd">启停未生效</span>
      <span class="msg">{toggleError.message}</span>
      <span class="tail">
        {#if toggleError.kind === 'not-ready'}
          出站的启停开关还没接到命令面，<b>开关的视觉状态不代表实际</b>。
          实际启用哪些出站仍由配置文件决定，改配置后重启生效。
        {:else}
          这次改动没有落到出站上，<b>开关的视觉状态不代表实际</b>。
        {/if}
      </span>
    </p>
  {/if}

  {#if probeError}
    <p class="cmd-err" role="alert">
      <span class="hd">测速不可用</span>
      <span class="msg">{probeError.message}</span>
      <span class="tail">
        {#if probeError.kind === 'not-ready'}
          出站管理器还没接到命令面，手动测速暂时算不出结果。
          列表与状态仍然照常可读，稳态延迟接上后会自动出现在延迟列。
        {:else}
          这一次测速失败了，延迟列仍然显示上一次测得的值。
        {/if}
      </span>
    </p>
  {/if}

  {#if !outbounds.length}
    <!--
      首次运行的第一屏。空状态必须说清「这里将会出现什么」以及
      「怎样让它出现」，后者要具体到可照做的一步 —— 而 websieve 的出站
      需要一对密钥，用户不知道去哪弄就会卡在这里。
    -->
    <EmptyState
      title="还没有配置出站服务器。"
      hint="出站是流量真正离开本机的地方。websieve 的每个出站需要一对密钥（server-pub 与 client-priv），从你自建的服务端取得 —— 它不是公共节点列表，没有可以直接填的现成地址。"
      step={{
        text: '把服务端给出的地址与密钥填进配置文件的 proxies 段：',
        code: 'config.yaml',
        tail: '。填好后这里会出现对应的行。',
      }}
      action="添加第一个服务器"
      onaction={onadd} />
  {:else}
    <div class="toolbar">
      <button type="button" class="ghost" onclick={onadd}>+ 添加服务器</button>
    </div>

    <table>
      <caption class="sr-only">
        出站服务器共 {outbounds.length} 个。每行显示名称、状态、延迟、会话数与启用开关。
        延迟在稳态取最近若干次上行响应耗时的中位数，刚建立连接时取握手 RTT。
      </caption>
      <thead>
        <tr>
          <th scope="col"><span class="sr-only">色码</span></th>
          <th scope="col">名称</th>
          <th scope="col" class="r">延迟</th>
          <th scope="col" class="r">会话</th>
          <th scope="col"><span class="sr-only">操作</span></th>
          <th scope="col"><span class="sr-only">删除</span></th>
          <th scope="col"><span class="sr-only">启用</span></th>
        </tr>
      </thead>
      <tbody>
        {#each outbounds as o (o.id)}
          {@const s = STATE[o.state] ?? STATE.stopped}
          <tr aria-label={desc(o)} data-state={o.state} class="state-{o.state}" class:off={!o.enabled}>
            <td class="c">
              <span class="chip" style:background={colorOf(o.name)} aria-hidden="true"></span>
            </td>

            <td class="name">
              {o.name}
              {#if o.host}<span class="sub">· 宿主</span>{/if}
            </td>

            <!-- 状态有文字时占用延迟列：重连中的节点没有有意义的延迟，
                 强行显示上一次的数字会被当成当前值。 -->
            <td class="num r" style:color={s.text ? s.color : undefined}>
              {s.text || ms(o.latency)}
            </td>

            <td class="num r">{o.sessions > 0 ? count(o.sessions) : '—'}</td>

            <td class="c">
              <button
                type="button"
                class="probe"
                aria-label={`测试延迟：${o.name}`}
                disabled={o.state !== 'live'}
                onclick={() => onprobe(o.id)}>测速</button>
            </td>

            <td class="c">
              {#if confirmingDelete === o.id}
                <button type="button" class="mini danger" onclick={() => confirmDelete(o.name)}>确认删除</button>
                <button type="button" class="mini" aria-label="取消删除" onclick={cancelDelete}>取消</button>
              {:else}
                <button type="button" class="mini danger" aria-label={`删除节点 ${o.name}`} onclick={() => askDelete(o.id)}>删除</button>
              {/if}
            </td>

            <td class="c">
              <Switch
                checked={o.enabled}
                label={`启用出站 ${o.name}`}
                onchange={(v) => ontoggle(o.id, v)} />
            </td>
          </tr>
        {/each}
      </tbody>
    </table>

    <!-- 延迟的出处。放在表下方而非表头：表头只有 10px 的空间，
         而这句话读一次就够了，不需要每次扫读都撞见。 -->
    <p class="foot">
      延迟为最近若干次上行响应耗时的<b>中位数</b>（不额外发包）；
      连接刚建立、尚无流量时显示握手 RTT。点「测速」发一个填充包并计时。
    </p>
  {/if}
</section>

<style>
  .view {
    background: var(--surface-1);
  }

  /* 命令失败的告示。borders-only，不用左侧粗色装饰边框（§11.4 明确拒绝）——
     一圈均匀的边框 + 状态色文字足以传达。与规则视图的 .save-err 同构，
     两处措辞不同但形制一致，用户学一次就够。 */
  .cmd-err {
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
  .cmd-err .hd {
    color: var(--state-warn);
    font-weight: var(--fw-semibold);
    flex: none;
  }
  .cmd-err .msg {
    color: var(--text-2);
  }
  .cmd-err .tail {
    color: var(--text-3);
  }
  .cmd-err .tail b {
    color: var(--state-warn);
    font-weight: var(--fw-medium);
  }

  table {
    width: 100%;
    border-collapse: collapse;
    table-layout: fixed;
  }

  th:nth-child(1),
  td:nth-child(1) {
    width: 26px;
  }
  th:nth-child(3),
  td:nth-child(3) {
    width: 72px;
  }
  th:nth-child(4),
  td:nth-child(4) {
    width: 62px;
  }
  th:nth-child(5),
  td:nth-child(5) {
    width: 56px;
  }
  th:nth-child(6),
  td:nth-child(6) {
    width: 76px;
  }
  th:nth-child(7),
  td:nth-child(7) {
    width: 46px;
  }

  th {
    height: 28px;
    font-size: 10px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--text-4);
    font-weight: var(--fw-semibold);
    text-align: left;
    padding: 0 8px;
    background: var(--surface-0);
    border-bottom: 1px solid var(--border);
  }
  th.r,
  td.r {
    text-align: right;
  }

  tbody tr {
    height: var(--row-outbound);
    border-bottom: 1px solid rgba(255, 255, 255, 0.035);
  }
  tbody tr:last-child {
    border-bottom: none;
  }

  td {
    padding: 0 8px;
    font-size: var(--fs-13);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  td.c {
    text-align: center;
  }

  .name {
    font-weight: var(--fw-medium);
  }
  .sub {
    color: var(--text-4);
    font-weight: var(--fw-regular);
    font-size: var(--fs-12);
  }

  /* tabular-nums：延迟与会话数纵向对齐成列，一扫就能比较 */
  .num {
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
    font-size: var(--fs-12);
    color: var(--text-3);
  }

  .chip {
    display: inline-block;
    width: 6px;
    height: 6px;
    border-radius: 2px;
  }

  /* spec §6.4：出站不可用要报警，不能是个安静的灰字 */
  .state-failed .name {
    color: var(--state-fail);
  }
  .state-reconnecting .name,
  .state-stopped .name {
    color: var(--text-2);
  }

  /* 停用的行整体降噪，但色码保留 —— 它是这个出站的身份，不随启停变化 */
  .off .name,
  .off .num {
    opacity: 0.45;
  }

  .probe {
    background: transparent;
    color: var(--text-4);
    border: 1px solid var(--border);
    border-radius: 3px;
    padding: 2px 7px;
    font-size: var(--fs-11);
    font-family: inherit;
    cursor: pointer;
  }
  .probe:hover:not(:disabled) {
    color: var(--text-2);
    border-color: var(--border-strong);
  }
  .probe:disabled {
    opacity: 0.35;
    cursor: not-allowed;
  }

  .toolbar {
    display: flex;
    justify-content: flex-end;
    padding: 8px 16px;
    border-bottom: 1px solid var(--border);
    background: var(--surface-0);
  }
  .ghost {
    background: transparent;
    color: var(--text-2);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius);
    padding: 6px 14px;
    font-size: var(--fs-12);
    font-family: inherit;
    cursor: pointer;
  }
  .ghost:hover { color: var(--text-1); }

  .mini {
    background: transparent;
    color: var(--text-3);
    border: 1px solid var(--border);
    border-radius: 3px;
    padding: 2px 7px;
    font-size: var(--fs-11);
    font-family: inherit;
    cursor: pointer;
    margin-left: 4px;
  }
  .mini:hover { color: var(--text-1); border-color: var(--border-strong); }
  .mini.danger { color: var(--state-fail); border-color: rgba(255, 90, 90, .35); }

  .foot {
    margin: 0;
    padding: 10px 16px 14px;
    border-top: 1px solid var(--border);
    font-size: var(--fs-11);
    color: var(--text-4);
    line-height: 1.7;
  }
  .foot b {
    color: var(--text-3);
    font-weight: var(--fw-medium);
  }
</style>
