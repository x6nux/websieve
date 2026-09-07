/**
 * 组装层的测试。
 *
 * 单组件的测试守各自的契约，这里守的是**只在接线之后才存在**的东西：
 *
 *   - 命令报 `not_ready` 时视图照常渲染，不白屏、不抛异常
 *   - 后端契约（snake_case 的事件字段、0-based 的 index、kebab-case 的配置键）
 *     被翻译成视图认的形状，且翻错时能被抓到
 *   - 私钥不因为「打开了一次设置」就进渲染层
 *   - 焦点从齿轮进覆盖层、Esc 之后回到齿轮 —— 这条只有装上齿轮按钮之后才测得了
 *
 * ## 为什么可以打 `window.__TAURI__` 这个桩
 *
 * 它是**浏览器环境提供的全局**，与 test-setup.js 里补的 `matchMedia` 同类，
 * 不是对被测代码的 mock：被测的是 App.svelte 如何解读命令的返回与事件的载荷，
 * 而桩给出的返回与载荷全部**逐字照抄 src-tauri 的真实定义**
 * （CmdError 的 kind 用 kebab-case、TrafficSample 用 snake_case、
 * RuleTestResult 的 index 是 0-based、ConfigView 分 config 与 rules 两列）。
 * 桩若与真实契约漂移，这些测试就在测一个不存在的后端 —— 所以每一处形状
 * 都在注释里指到了它的出处。
 */
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, within, waitFor } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import App from './App.svelte';

/** src-tauri/src/commands/mod.rs 的 `CmdError::NotReady`（serde tag = "kind"，kebab-case） */
const notReady = (what, dep) => ({ kind: 'not-ready', message: `${what} 尚未接入（${dep}）` });

/**
 * 一份最小但真实的 `config_get` 返回。
 *
 * 形状取自 `ConfigView`：`config` 是**已脱敏**的 JSON 投影（client-priv 换成
 * `***`）且**不含 rules 键**；`rules` 单独一列，每项 `{ value, line }` ——
 * 行号只在这一列里存在，因为 `Spanned<T>` 序列化时只吐出值。
 */
const CONFIG_VIEW = {
  config: {
    'mixed-port': 7890,
    'allow-lan': false,
    'system-proxy': false,
    tun: { enable: false },
    carrier: 'shared',
    'carrier-host': '',
    mode: 'rule',
    proxies: [
      { name: '日本节点', type: 'websieve', url: 'https://a.example/', 'server-pub': 'aa', 'client-priv': '***' },
      { name: '新加坡', type: 'websieve', url: 'https://b.example/', 'server-pub': 'bb', 'client-priv': '***' },
    ],
  },
  rules: [
    { value: 'GEOSITE,category-ads,REJECT', line: 12 },
    { value: 'DOMAIN-SUFFIX,googleapis.com,新加坡', line: 13 },
    { value: 'GEOSITE,cn,DIRECT', line: 15 },
    { value: 'MATCH,日本节点', line: 16 },
  ],
};

/** 当前挂着的事件监听。emit() 用它把事件推给 App。 */
let listeners;
/** 每条命令的实现。测试按需覆盖。 */
let handlers;
/** 每条命令被调用时的参数记录 —— 断言「发出去的是不是对的东西」 */
let calls;
/** 已经**返回**（而非仅被调用）的命令名。settled() 等的是这个。 */
let done;

function installTauri(overrides = {}) {
  listeners = new Map();
  calls = [];
  done = [];
  handlers = {
    // 完整实现的三条（见各自的 Rust 注释）
    config_get: async () => CONFIG_VIEW,
    config_get_raw: async () => RAW_YAML,
    config_save: async () => null,
    config_save_raw: async () => null,
    traffic_snapshot: async () => ({ up_bytes: 0, down_bytes: 0, up_rate: 0, down_rate: 0, active: 3 }),
    // 真实返回 NotReady 的那几条（control.rs / probe.rs）
    rule_test: async () => {
      throw notReady('rule_test', '正在服役的 RuleSet 尚未进 managed state，见阶段 5');
    },
    outbound_enable: async () => {
      throw notReady('outbound_enable', '出站的启停开关在阶段 2 的出站管理器里，尚未接到命令面');
    },
    outbound_latency_probe: async () => {
      throw notReady('outbound_latency_probe', '出站管理器尚未接到命令面');
    },
    ...overrides,
  };

  window.__TAURI__ = {
    core: {
      invoke: async (cmd, args) => {
        calls.push([cmd, args]);
        const h = handlers[cmd];
        if (!h) throw new Error(`未注册的命令：${cmd}`);
        try {
          return await h(args);
        } finally {
          done.push(cmd);
        }
      },
    },
    event: {
      listen: async (name, fn) => {
        const set = listeners.get(name) ?? new Set();
        set.add(fn);
        listeners.set(name, set);
        return () => set.delete(fn);
      },
    },
  };
}

/** 含明文私钥的原文。导出与保存都从这里取 —— 它绝不该出现在 DOM 里。 */
const SECRET = 'aaaabbbbccccddddeeeeffff00001111';
const RAW_YAML = [
  '# 我手写的注释',
  'mixed-port: 7890  # 混合入口',
  'allow-lan: false',
  'system-proxy: false',
  'tun:',
  '  enable: false',
  '  stack: system  # 默认栈',
  'proxies:',
  '  - name: 日本节点',
  '    type: websieve',
  '    url: https://a.example/',
  '    server-pub: "aa"',
  `    client-priv: "${SECRET}"`,
  'rules:',
  '  # 广告一律拒绝',
  '  - GEOSITE,category-ads,REJECT',
  '  - MATCH,日本节点',
  '',
].join('\n');

/** 把一条事件推给 App，形状照抄 events.rs */
async function emit(name, payload) {
  for (const fn of listeners.get(name) ?? []) fn({ payload });
  // 等 Svelte 把这一轮状态变更冲刷进 DOM
  await vi.waitFor(() => {});
}

/**
 * 等首屏的 config_get **返回并落进 state**。
 *
 * 等「被调用」是不够的：调用发出去到 promise 兑现之间，config 还是 null，
 * 而所有从 config 投影出来的东西（混合端口、出站列表）那时都还不在。
 * 用「被调用」当判据会得到一个时而通过时而失败的测试，而失败时看起来
 * 像是产品 bug。
 */
const settled = () => vi.waitFor(() => expect(done).toContain('config_get'));

beforeEach(() => installTauri());
afterEach(() => {
  delete window.__TAURI__;
  vi.restoreAllMocks();
});

describe('组装：默认视图与状态条', () => {
  it('默认打开首页而不是流量视图', async () => {
    render(App);
    await waitFor(() => {
      expect(screen.getByRole('radiogroup', { name: /视图/ })).toBeInTheDocument();
    });
    const nav = screen.getByRole('radiogroup', { name: /视图/ });
    expect(within(nav).getByRole('radio', { name: '首页' })).toHaveAttribute('aria-checked', 'true');
  });

  it('切到流量视图后不显示日志（§11.3）', async () => {
    const u = userEvent.setup();
    render(App);
    await settled();
    await u.click(screen.getByRole('radio', { name: '流量' }));
    expect(screen.getByRole('region', { name: '流量走向' })).toBeInTheDocument();
    expect(screen.queryByRole('region', { name: '分流规则' })).toBeNull();
  });

  it('导航是 segmented control，不是左侧图标栏（§11.4 拒绝套路一）', async () => {
    const { container } = render(App);
    await settled();
    expect(screen.getByRole('radiogroup', { name: '视图' })).toBeInTheDocument();
    expect(container.querySelector('nav, aside, .sidebar')).toBeNull();
  });

  it('状态条**没有** aria-live —— 1s 一次的播报会让屏幕阅读器无法使用', async () => {
    const { container } = render(App);
    await settled();
    const bar = container.querySelector('.status');
    expect(bar).toBeTruthy();
    expect(bar.getAttribute('aria-live')).toBeNull();
  });

  it('未连接时状态点旁边有文字 —— 状态不能只靠颜色', async () => {
    render(App);
    await settled();
    expect(screen.getByText('未连接')).toBeInTheDocument();
  });

  it('出站活起来之后状态条改口说「已连接」', async () => {
    render(App);
    await settled();
    // events.rs 的 OutboundState：{ name, state, latency_ms }
    await emit('outbound-state', { name: '日本节点', state: 'live', latency_ms: 38 });
    expect(screen.getByText('已连接')).toBeInTheDocument();
  });

  it('连接状态取自出站是否活着，不是「有没有点过按钮」', async () => {
    render(App);
    await settled();
    // connect 命令目前返回 NotReady，按点击推断会让绿灯一直亮着而一条会话都没建起来
    await emit('outbound-state', { name: '日本节点', state: 'connecting', latency_ms: null });
    expect(screen.getByText('未连接')).toBeInTheDocument();
  });

  it('快照补齐活跃连接数，不必空等下一个 1s tick', async () => {
    render(App);
    await vi.waitFor(() => expect(screen.getByText(/3 活跃连接/)).toBeInTheDocument());
  });

  it('快照的 0 速率不被当成一次真实采样写进 sparkline', async () => {
    // TrafficSample 的 up_rate/down_rate 在快照里恒为 0（速率需要「上一次采样」）
    const { container } = render(App);
    await settled();
    expect(container.querySelectorAll('.spark i')).toHaveLength(0);
  });

  it('traffic 事件驱动速率与 sparkline', async () => {
    const { container } = render(App);
    await settled();
    await emit('traffic', { up_bytes: 1, down_bytes: 2, up_rate: 100, down_rate: 2048, active: 7 });
    // 首页现在是默认视图，它的「流量统计」卡片放大复用同一份 spark/rate，
    // 所以下面两条都按状态条（.status）范围取，避免与首页卡片撞名
    expect(within(container.querySelector('.status')).getByText(/2\.00 KB\/s/)).toBeInTheDocument();
    expect(screen.getByText(/7 活跃连接/)).toBeInTheDocument();
    expect(container.querySelectorAll('.status .spark i')).toHaveLength(1);
  });

  it('sparkline 对屏幕阅读器隐藏 —— 真实数值在旁边', async () => {
    const { container } = render(App);
    await settled();
    expect(container.querySelector('.spark').getAttribute('aria-hidden')).toBe('true');
  });
});

describe('组装：未就绪的命令不该让界面垮掉', () => {
  it('rule_test 报 not-ready 时规则视图照常渲染并说明原因', async () => {
    const u = userEvent.setup();
    render(App);
    await settled();
    await u.click(within(screen.getByRole('radiogroup', { name: '视图' })).getByRole('radio', { name: '规则' }));

    await u.type(screen.getByLabelText(/试算/), 'x.com');
    await vi.waitFor(() => expect(screen.getByText(/试算不可用/)).toBeInTheDocument());
    // 列表还在，且一行都没少
    expect(screen.getAllByRole('row').slice(1)).toHaveLength(CONFIG_VIEW.rules.length);
    expect(screen.getByText(/尚未接入/)).toBeInTheDocument();
  });

  it('not-ready **不被吞掉** —— 它与「这个域名确实没命中」要做的下一步相反', async () => {
    const u = userEvent.setup();
    render(App);
    await settled();
    await u.click(within(screen.getByRole('radiogroup', { name: '视图' })).getByRole('radio', { name: '规则' }));
    await u.type(screen.getByLabelText(/试算/), 'x.com');
    await vi.waitFor(() => expect(screen.getByText(/试算不可用/)).toBeInTheDocument());
  });

  it('outbound_enable 报 not-ready 时说清「开关的视觉状态不代表实际」', async () => {
    const u = userEvent.setup();
    render(App);
    await settled();
    await u.click(screen.getByRole('radio', { name: '出站' }));
    await u.click(screen.getAllByRole('switch')[0]);
    await vi.waitFor(() => expect(screen.getByText(/启停未生效/)).toBeInTheDocument());
    expect(screen.getByText(/开关的视觉状态不代表实际/)).toBeInTheDocument();
  });

  it('测速报 not-ready 时列表与延迟列照常', async () => {
    const u = userEvent.setup();
    render(App);
    await settled();
    await emit('outbound-state', { name: '日本节点', state: 'live', latency_ms: 38 });
    await u.click(screen.getByRole('radio', { name: '出站' }));
    await u.click(screen.getAllByRole('button', { name: /测试延迟/ })[0]);
    await vi.waitFor(() => expect(screen.getByText(/测速不可用/)).toBeInTheDocument());
    expect(screen.getByText('38 ms')).toBeInTheDocument();
  });

  it('config_get 失败时不白屏，且带上出错行号（§12）', async () => {
    installTauri({
      config_get: async () => {
        // CmdError::ConfigSyntax { message, line, column }
        throw { kind: 'config-syntax', message: '第 13 行缩进不对', line: 13, column: 5 };
      },
    });
    render(App);
    await vi.waitFor(() => expect(screen.getByRole('alert')).toBeInTheDocument());
    expect(screen.getByText(/第 13 行第 5 列/)).toBeInTheDocument();
    // 界面还在
    expect(screen.getByRole('radiogroup', { name: '视图' })).toBeInTheDocument();
  });

  it('IPC 本身抛（不是 CmdError）也不产出「失败：undefined」', async () => {
    installTauri({
      config_get: async () => {
        throw new Error('control 窗口没有这条命令的权限');
      },
    });
    render(App);
    await vi.waitFor(() => expect(screen.getByRole('alert')).toBeInTheDocument());
    expect(screen.getByRole('alert').textContent).not.toMatch(/undefined/);
    expect(screen.getByText(/没有这条命令的权限/)).toBeInTheDocument();
  });

  it('连接事件溢出要说出来 —— 悄悄丢数据会让用户对着一张不准的图排查', async () => {
    render(App);
    await settled();
    await emit('connection', { items: [], dropped: true });
    expect(screen.getByText(/连接事件溢出/)).toBeInTheDocument();
  });
});

describe('组装：诚实的计量口径', () => {
  /** ConnectionDelta：{ id, target, outbound, state }，**没有 bytes** */
  const deltas = [
    { id: 1, target: 'a.com:443', outbound: '日本节点', state: 'open' },
    { id: 2, target: 'b.com:443', outbound: '新加坡', state: 'open' },
    { id: 3, target: 'c.com:443', outbound: 'DIRECT', state: 'open' },
  ];

  /** 首页现在是默认视图，这一组测的是流量视图本身，故先切过去 */
  const goTraffic = async (u) => {
    await settled();
    await u.click(screen.getByRole('radio', { name: '流量' }));
  };

  it('没有逐流字节时，合计带的量词是「条连接」而非字节单位', async () => {
    const u = userEvent.setup();
    const { container } = render(App);
    await goTraffic(u);
    await emit('connection', { items: deltas, dropped: false });
    // 工具条上的合计。桑基图的文字摘要里也有同一个数（那是等价视图的一部分），
    // 所以这里按位置取，不用 getByText —— 否则会撞上两处
    expect(container.querySelector('.total').textContent).toMatch(/3 条连接/);
  });

  it('绝不拿连接数假装成字节 —— 图上不出现 KB / MB / GB', async () => {
    const u = userEvent.setup();
    const { container } = render(App);
    await goTraffic(u);
    await emit('connection', { items: deltas, dropped: false });
    const view = container.querySelector('[aria-label="流量走向"]');
    expect(view.textContent).not.toMatch(/\d\s*(KB|MB|GB|TB)\b/);
  });

  it('流带粗细的图例明说「不是吞吐量」', async () => {
    const u = userEvent.setup();
    render(App);
    await goTraffic(u);
    await emit('connection', { items: deltas, dropped: false });
    expect(screen.getByText(/不是吞吐量/)).toBeInTheDocument();
  });

  it('后端补上 bytes 之后自动改口说字节，前端一个字不用改', async () => {
    const u = userEvent.setup();
    const { container } = render(App);
    await goTraffic(u);
    await emit('connection', {
      items: deltas.map((d, i) => ({ ...d, bytes: (i + 1) * 1024 * 1024 })),
      dropped: false,
    });
    // 工具条上的合计 —— 单位一翻，它是第一个跟着翻的
    expect(container.querySelector('.total').textContent).toMatch(/\d+(\.\d+)?\s*MB/);
    // 图例也得跟着翻，不能一个说字节一个说连接数
    expect(screen.getByText(/流带粗细 = 字节数/)).toBeInTheDocument();
    expect(screen.queryByText(/不是吞吐量/)).toBeNull();
  });
});

describe('组装：规则视图与热度', () => {
  const goRules = async (u) => {
    await settled();
    await u.click(within(screen.getByRole('radiogroup', { name: '视图' })).getByRole('radio', { name: '规则' }));
  };

  it('规则从 config_get 的 rules 列读，不从 config.rules', async () => {
    const u = userEvent.setup();
    render(App);
    await goRules(u);
    expect(screen.getAllByRole('row').slice(1)).toHaveLength(4);
    expect(screen.getByText('googleapis.com')).toBeInTheDocument();
  });

  it('首次打开热度全灰 —— hits 靠 rule-hit 事件累加，这是预期不是 bug', async () => {
    const u = userEvent.setup();
    const { container } = render(App);
    await goRules(u);
    for (const tr of container.querySelectorAll('tbody tr')) {
      const style = tr.getAttribute('style') ?? '';
      expect(style).toMatch(/transparent|^$|rgba\(255,\s*255,\s*255,\s*0\)/);
    }
  });

  it('rule-hit 的键是**规则原文** —— 按解析后的字段去对会永远对不上', async () => {
    const u = userEvent.setup();
    const { container } = render(App);
    await goRules(u);
    // events.rs 的 hit_delta：HashMap<规则字符串, 增量>
    await emit('rule-hit', { 'MATCH,日本节点': 8000, 'GEOSITE,cn,DIRECT': 40 });
    expect(screen.getByText('8,000')).toBeInTheDocument();
    // 热的那行背景要比冷的那行亮（signature ①）
    const alpha = (el) => {
      const m = (el.getAttribute('style') ?? '').match(/rgba\(255,\s*255,\s*255,\s*([\d.]+)\)/);
      return m ? parseFloat(m[1]) : 0;
    };
    const rows = [...container.querySelectorAll('tbody tr')];
    expect(alpha(rows[3])).toBeGreaterThan(alpha(rows[2]));
  });

  it('rule-hit 是增量，连推两次要累加而不是覆盖', async () => {
    const u = userEvent.setup();
    render(App);
    await goRules(u);
    await emit('rule-hit', { 'GEOSITE,cn,DIRECT': 10 });
    await emit('rule-hit', { 'GEOSITE,cn,DIRECT': 5 });
    expect(screen.getByText('15')).toBeInTheDocument();
  });

  it('引用了不存在的出站的规则被标出来（§12）', async () => {
    installTauri({
      config_get: async () => ({
        ...CONFIG_VIEW,
        rules: [{ value: 'DOMAIN,x.com,幽灵节点', line: 9 }],
      }),
    });
    const u = userEvent.setup();
    render(App);
    await goRules(u);
    expect(screen.getByText('不存在')).toBeInTheDocument();
  });

  it('规则的启停如实报「这条路走不通」，不做一个切不动的假开关', async () => {
    const u = userEvent.setup();
    render(App);
    await goRules(u);
    await u.click(screen.getAllByRole('switch')[0]);
    await vi.waitFor(() => expect(screen.getByText(/顺序未保存/)).toBeInTheDocument());
    expect(screen.getByText(/没有 enabled 字段/)).toBeInTheDocument();
    // 而且**没有**去写文件
    expect(calls.some(([c]) => c === 'config_save')).toBe(false);
  });
});

describe('组装：排序翻译成 config_save 的定点改写', () => {
  const goRules = async (u) => {
    await settled();
    await u.click(within(screen.getByRole('radiogroup', { name: '视图' })).getByRole('radio', { name: '规则' }));
  };

  it('Alt+↓ 发出的是 replace-rule ops，不是整份规则数组', async () => {
    const u = userEvent.setup();
    render(App);
    await goRules(u);
    screen.getAllByRole('button', { name: /移动/ })[0].focus();
    await u.keyboard('{Alt>}{ArrowDown}{/Alt}');

    await vi.waitFor(() => expect(calls.some(([c]) => c === 'config_save')).toBe(true));
    const [, args] = calls.find(([c]) => c === 'config_save');
    expect(Array.isArray(args.ops)).toBe(true);
    for (const op of args.ops) {
      // RuleOp 是 #[serde(tag="op", rename_all="kebab-case", deny_unknown_fields)]
      expect(op.op).toBe('replace-rule');
      expect(Number.isInteger(op.line)).toBe(true);
      expect(typeof op.expect).toBe('string');
      expect(typeof op.value).toBe('string');
    }
  });

  it('line 取自 config_get 的快照，不是下标加一', async () => {
    const u = userEvent.setup();
    render(App);
    await goRules(u);
    screen.getAllByRole('button', { name: /移动/ })[0].focus();
    await u.keyboard('{Alt>}{ArrowDown}{/Alt}');
    await vi.waitFor(() => expect(calls.some(([c]) => c === 'config_save')).toBe(true));
    const [, args] = calls.find(([c]) => c === 'config_save');
    // 12/13 是那两行的真实行号（14 是注释，不该出现）
    expect(args.ops.map((o) => o.line).sort()).toEqual([12, 13]);
  });

  it('expect 是那一行的原值，不是排序后的新值', async () => {
    const u = userEvent.setup();
    render(App);
    await goRules(u);
    screen.getAllByRole('button', { name: /移动/ })[0].focus();
    await u.keyboard('{Alt>}{ArrowDown}{/Alt}');
    await vi.waitFor(() => expect(calls.some(([c]) => c === 'config_save')).toBe(true));
    const [, args] = calls.find(([c]) => c === 'config_save');
    const first = args.ops.find((o) => o.line === 12);
    expect(first.expect).toBe('GEOSITE,category-ads,REJECT');
    expect(first.value).toBe('DOMAIN-SUFFIX,googleapis.com,新加坡');
  });

  it('保存成功后重读配置 —— 不重读的话第二次排序会被并发校验拒掉', async () => {
    const u = userEvent.setup();
    render(App);
    await goRules(u);
    const before = calls.filter(([c]) => c === 'config_get').length;
    screen.getAllByRole('button', { name: /移动/ })[0].focus();
    await u.keyboard('{Alt>}{ArrowDown}{/Alt}');
    await vi.waitFor(() =>
      expect(calls.filter(([c]) => c === 'config_get').length).toBeGreaterThan(before),
    );
  });

  it('保存被拒时列表回滚并说清「文件里还是旧顺序」', async () => {
    installTauri({
      config_save: async () => {
        throw { kind: 'io', message: '写入 config.yaml 失败：磁盘已满' };
      },
    });
    const u = userEvent.setup();
    render(App);
    await goRules(u);
    const order = () => [...document.querySelectorAll('tbody .val')].map((e) => e.textContent);
    const before = order();

    screen.getAllByRole('button', { name: /移动/ })[0].focus();
    await u.keyboard('{Alt>}{ArrowDown}{/Alt}');

    await vi.waitFor(() => expect(screen.getByText(/顺序未保存/)).toBeInTheDocument());
    expect(screen.getByText(/磁盘已满/)).toBeInTheDocument();
    // 回滚：界面顺序与文件顺序不能分叉，而分流只认文件
    expect(order()).toEqual(before);
  });

  it('行号陈旧（config-invalid）时给出可照做的下一步', async () => {
    installTauri({
      config_save: async () => {
        throw { kind: 'config-invalid', message: '第 13 行现在是 "MATCH,DIRECT"' };
      },
    });
    const u = userEvent.setup();
    render(App);
    await goRules(u);
    screen.getAllByRole('button', { name: /移动/ })[0].focus();
    await u.keyboard('{Alt>}{ArrowDown}{/Alt}');
    await vi.waitFor(() => expect(screen.getByText(/刷新后重试/)).toBeInTheDocument());
  });
});

describe('组装：规则表单接线', () => {
  it('规则「添加」打开新增表单，提交后调用 config_save 的 insert-rule 并重新加载配置', async () => {
    const u = userEvent.setup();
    render(App);
    await waitFor(() => screen.getByRole('radiogroup', { name: /视图/ }));
    await u.click(within(screen.getByRole('radiogroup', { name: '视图' })).getByRole('radio', { name: '规则' }));
    // 用 /添加/ 而非 /添加规则/：默认 mock 配置里规则是否为空未知，命中的可能是
    // 工具栏的「+ 添加规则」，也可能是空状态的「添加第一条规则」——两种文案都含
    // 「添加」，测试不该绑定某一种具体状态。
    await u.click(await screen.findByRole('button', { name: /添加/ }));
    await u.selectOptions(screen.getByLabelText('类型'), 'MATCH');
    await u.selectOptions(screen.getByLabelText('出站'), 'DIRECT');
    await u.click(screen.getByRole('button', { name: '保存' }));
    await vi.waitFor(() => expect(calls.some(([c]) => c === 'config_save')).toBe(true));
    const [, args] = calls.find(([c]) => c === 'config_save');
    expect(args.ops).toEqual([
      expect.objectContaining({ op: 'insert-rule', value: 'MATCH,DIRECT' }),
    ]);
    // 提交成功后重新加载配置
    await vi.waitFor(() =>
      expect(calls.filter(([c]) => c === 'config_get').length).toBeGreaterThan(1),
    );
  });
});

describe('组装：设置覆盖层的焦点往返（Task 15 遗留的那一条）', () => {
  it('齿轮按钮有可访问名字，不是一个光秃秃的图标', async () => {
    render(App);
    await settled();
    expect(screen.getByRole('button', { name: '打开设置' })).toBeInTheDocument();
  });

  it('点齿轮打开覆盖层，焦点进入层内', async () => {
    const u = userEvent.setup();
    render(App);
    await settled();
    await u.click(screen.getByRole('button', { name: '打开设置' }));
    const dialog = await screen.findByRole('dialog');
    await vi.waitFor(() => expect(dialog.contains(document.activeElement)).toBe(true));
  });

  it('Esc 关闭覆盖层，焦点回到齿轮 —— 键盘用户不会掉到 body 上', async () => {
    const u = userEvent.setup();
    render(App);
    await settled();
    const gear = screen.getByRole('button', { name: '打开设置' });
    await u.click(gear);
    const dialog = await screen.findByRole('dialog');
    await vi.waitFor(() => expect(dialog.contains(document.activeElement)).toBe(true));

    await u.keyboard('{Escape}');
    await vi.waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    // 焦点掉回 body 的话，下一次 Tab 要从文档开头重新开始
    expect(document.activeElement).toBe(gear);
  });

  it('点关闭按钮同样把焦点还回齿轮', async () => {
    const u = userEvent.setup();
    render(App);
    await settled();
    const gear = screen.getByRole('button', { name: '打开设置' });
    await u.click(gear);
    await screen.findByRole('dialog');
    await u.click(screen.getByRole('button', { name: /关闭设置/ }));
    await vi.waitFor(() => expect(document.activeElement).toBe(gear));
  });

  it('齿轮用 aria-expanded 播报覆盖层的开合', async () => {
    const u = userEvent.setup();
    render(App);
    await settled();
    const gear = screen.getByRole('button', { name: '打开设置' });
    expect(gear).toHaveAttribute('aria-expanded', 'false');
    await u.click(gear);
    await screen.findByRole('dialog');
    expect(gear).toHaveAttribute('aria-expanded', 'true');
  });
});

describe('组装：私钥不进渲染层', () => {
  it('打开设置不会去取原文 —— 私钥不因为「看了一眼设置」就进 JS 堆', async () => {
    const u = userEvent.setup();
    render(App);
    await settled();
    await u.click(screen.getByRole('button', { name: '打开设置' }));
    await screen.findByRole('dialog');
    expect(calls.some(([c]) => c === 'config_get_raw')).toBe(false);
  });

  it('设置层里渲染的是脱敏过的具名字段，DOM 里找不到私钥', async () => {
    const u = userEvent.setup();
    const { container } = render(App);
    await settled();
    await u.click(screen.getByRole('button', { name: '打开设置' }));
    await screen.findByRole('dialog');
    expect(container.textContent).not.toContain(SECRET);
    for (const el of container.querySelectorAll('input, textarea, select')) {
      expect(String(el.value)).not.toContain(SECRET);
    }
  });

  it('导出是显式动作，且导出失败的提示里不回显文件内容', async () => {
    installTauri({
      config_get_raw: async () => {
        throw { kind: 'io', message: '读取 /x/config.yaml 失败：权限不足' };
      },
    });
    const u = userEvent.setup();
    render(App);
    await settled();
    await u.click(screen.getByRole('button', { name: '打开设置' }));
    await screen.findByRole('dialog');
    await u.click(screen.getByRole('button', { name: /导出/ }));
    await vi.waitFor(() => expect(screen.getByText(/导出失败/)).toBeInTheDocument());
    expect(document.body.textContent).not.toContain(SECRET);
  });

  it('导出走浏览器下载通道，不需要 fs / dialog 权限', async () => {
    const u = userEvent.setup();
    // jsdom 没实现这两个，补上环境而非打桩被测代码
    const created = [];
    window.URL.createObjectURL = (b) => {
      created.push(b);
      return 'blob:stub';
    };
    window.URL.revokeObjectURL = vi.fn();
    const click = vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(() => {});

    render(App);
    await settled();
    await u.click(screen.getByRole('button', { name: '打开设置' }));
    await screen.findByRole('dialog');
    await u.click(screen.getByRole('button', { name: /导出/ }));

    await vi.waitFor(() => expect(click).toHaveBeenCalled());
    expect(created).toHaveLength(1);
    // 用完立刻 revoke —— blob 里装着私钥，留着它就是留一个可访问的 URL
    expect(window.URL.revokeObjectURL).toHaveBeenCalledWith('blob:stub');
  });
});

describe('组装：设置保存走 config_save_raw 且不毁注释', () => {
  const openSettings = async (u) => {
    await settled();
    await u.click(screen.getByRole('button', { name: '打开设置' }));
    return screen.findByRole('dialog');
  };

  it('保存走的是 config_save_raw —— 标量字段没有定点改写器可用', async () => {
    const u = userEvent.setup();
    render(App);
    const d = await openSettings(u);
    await u.click(within(d).getByRole('button', { name: '保存' }));
    await vi.waitFor(() => expect(calls.some(([c]) => c === 'config_save_raw')).toBe(true));
  });

  it('写回的原文里，规则区的注释一个字都没少（§5.6 的承诺）', async () => {
    const u = userEvent.setup();
    render(App);
    const d = await openSettings(u);
    const port = within(d).getByLabelText(/混合端口/);
    await u.clear(port);
    await u.type(port, '1080');
    await u.click(within(d).getByRole('button', { name: '保存' }));

    await vi.waitFor(() => expect(calls.some(([c]) => c === 'config_save_raw')).toBe(true));
    const [, args] = calls.find(([c]) => c === 'config_save_raw');
    expect(args.text).toContain('# 我手写的注释');
    expect(args.text).toContain('  # 广告一律拒绝');
    expect(args.text).toContain('  - GEOSITE,category-ads,REJECT');
    // 改的那一行确实改了，且行尾注释还在
    expect(args.text).toContain('mixed-port: 1080  # 混合入口');
  });

  it('私钥原样写回，不被脱敏后的 *** 覆盖掉', async () => {
    // 拿 config_get（已脱敏）那一份去写回，会把用户的私钥换成三个星号 ——
    // 表现为「保存了一下设置，然后再也连不上了」
    const u = userEvent.setup();
    render(App);
    const d = await openSettings(u);
    await u.click(within(d).getByRole('button', { name: '保存' }));
    await vi.waitFor(() => expect(calls.some(([c]) => c === 'config_save_raw')).toBe(true));
    const [, args] = calls.find(([c]) => c === 'config_save_raw');
    expect(args.text).toContain(SECRET);
    expect(args.text).not.toContain('client-priv: "***"');
  });

  it('保存被拒时不关闭覆盖层，并把行列号说出来', async () => {
    installTauri({
      config_save_raw: async () => {
        throw { kind: 'config-syntax', message: '解析失败', line: 4, column: 3 };
      },
    });
    const u = userEvent.setup();
    render(App);
    const d = await openSettings(u);
    await u.click(within(d).getByRole('button', { name: '保存' }));
    await vi.waitFor(() => expect(screen.getByText(/第 4 行第 3 列/)).toBeInTheDocument());
    expect(screen.getByRole('dialog')).toBeInTheDocument();
  });

  it('读不到原文就不写 —— 拿一份凭空拼的 YAML 去覆盖是灾难性的', async () => {
    installTauri({
      config_get_raw: async () => {
        throw { kind: 'io', message: '读取失败' };
      },
    });
    const u = userEvent.setup();
    render(App);
    const d = await openSettings(u);
    await u.click(within(d).getByRole('button', { name: '保存' }));
    await vi.waitFor(() => expect(screen.getByText(/读不到配置原文/)).toBeInTheDocument());
    expect(calls.some(([c]) => c === 'config_save_raw')).toBe(false);
  });
});

describe('组装：首页系统代理/TUN 开关的保存路径', () => {
  it('系统代理开关走顶层标量 setScalar，改到 system-proxy: true', async () => {
    const u = userEvent.setup();
    render(App);
    await settled();
    await u.click(screen.getByRole('switch', { name: '系统代理' }));
    await vi.waitFor(() => expect(calls.some(([c]) => c === 'config_save_raw')).toBe(true));
    const [, args] = calls.find(([c]) => c === 'config_save_raw');
    expect(args.text).toMatch(/^system-proxy: true$/m);
  });

  it('TUN 开关走 setNestedScalar，改到 tun 块里的 enable: true，不是编造的顶层 tun.enable', async () => {
    // 这是本次要修的 bug：旧实现用 setScalar(text, 'tun.enable', v) 去改一个
    // schema 里不存在的顶层键，找不到就追加到文件末尾，写出一行
    // `tun.enable: true`——Config 的 deny_unknown_fields 会把它整份拒收。
    const u = userEvent.setup();
    render(App);
    await settled();
    await u.click(screen.getByRole('switch', { name: '虚拟网卡（TUN）' }));
    await vi.waitFor(() => expect(calls.some(([c]) => c === 'config_save_raw')).toBe(true));
    const [, args] = calls.find(([c]) => c === 'config_save_raw');
    expect(args.text).toMatch(/^ {2}enable: true$/m);
    expect(args.text).not.toContain('tun.enable');
    // tun 块里的兄弟字段与它的行尾注释不受影响
    expect(args.text).toContain('  stack: system  # 默认栈');
  });

  it('TUN 开关保存失败（比如后端拒收）时如实报错，不假装成功', async () => {
    installTauri({
      config_save_raw: async () => {
        throw { kind: 'config-syntax', message: '解析失败', line: 5, column: 3 };
      },
    });
    const u = userEvent.setup();
    render(App);
    await settled();
    await u.click(screen.getByRole('switch', { name: '虚拟网卡（TUN）' }));
    await vi.waitFor(() => expect(screen.getByText(/切换失败/)).toBeInTheDocument());
  });
});

describe('组装：空状态按真实状态分支', () => {
  /** 首页现在是默认视图，这些空状态文案在流量视图里，故先切过去 */
  const goTraffic = async (u) => u.click(screen.getByRole('radio', { name: '流量' }));

  it('一个出站都没有时，先让用户去加服务器', async () => {
    installTauri({
      config_get: async () => ({ config: { ...CONFIG_VIEW.config, proxies: [] }, rules: [] }),
    });
    const u = userEvent.setup();
    render(App);
    await goTraffic(u);
    await vi.waitFor(() => expect(screen.getByText(/还没有配置任何出站/)).toBeInTheDocument());
  });

  it('有出站但没连上时，说的是「代理未运行」', async () => {
    const u = userEvent.setup();
    render(App);
    await goTraffic(u);
    await vi.waitFor(() => expect(screen.getByText(/代理未运行/)).toBeInTheDocument());
  });

  it('连上了还没有连接时，把混合端口原样给出去', async () => {
    const u = userEvent.setup();
    render(App);
    await settled();
    await goTraffic(u);
    await emit('outbound-state', { name: '日本节点', state: 'live', latency_ms: 38 });
    expect(screen.getByText(/代理已在运行/)).toBeInTheDocument();
    expect(screen.getByText('127.0.0.1:7890')).toBeInTheDocument();
  });

  it('空状态不画空的坐标骨架（§11.6）', async () => {
    const u = userEvent.setup();
    const { container } = render(App);
    await settled();
    await goTraffic(u);
    expect(container.querySelector('svg')).toBeNull();
  });
});

describe('组装：出站色码全局一致', () => {
  it('同一个出站在规则视图与出站视图是同一个颜色', async () => {
    const u = userEvent.setup();
    const { container } = render(App);
    await settled();

    await u.click(within(screen.getByRole('radiogroup', { name: '视图' })).getByRole('radio', { name: '规则' }));
    // 「MATCH,日本节点」那一行的色块
    const inRules = [...container.querySelectorAll('tbody tr')]
      .find((tr) => tr.textContent.includes('日本节点'))
      .querySelector('.chip')
      .getAttribute('style');

    await u.click(screen.getByRole('radio', { name: '出站' }));
    const inOutbounds = [...container.querySelectorAll('tbody tr')]
      .find((tr) => tr.textContent.includes('日本节点'))
      .querySelector('.chip')
      .getAttribute('style');

    expect(inRules).toBe(inOutbounds);
  });

  it('DIRECT 用状态色，不占出站色码', async () => {
    const u = userEvent.setup();
    const { container } = render(App);
    await settled();
    await u.click(within(screen.getByRole('radiogroup', { name: '视图' })).getByRole('radio', { name: '规则' }));
    const direct = [...container.querySelectorAll('tbody tr')]
      .find((tr) => tr.textContent.includes('DIRECT'))
      .querySelector('.chip')
      .getAttribute('style');
    expect(direct).toMatch(/--state-direct/);
  });

  it('色码用的是 tokens.css 里存在的变量名，不会是 --outbound-9', async () => {
    const u = userEvent.setup();
    const { container } = render(App);
    await settled();
    await u.click(screen.getByRole('radio', { name: '出站' }));
    for (const chip of container.querySelectorAll('.chip')) {
      const s = chip.getAttribute('style') ?? '';
      expect(s).toMatch(/var\(--(outbound-[1-8]|state-(direct|fail))\)/);
    }
  });
});
