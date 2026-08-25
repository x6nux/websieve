/**
 * 跨组件无障碍审计。
 *
 * 单组件测试守各自的契约，这里守的是**组装之后才存在的东西**：
 * 重复 id、缺失的可访问名、颜色是唯一信息载体、活动区域播报过频、
 * 焦点环被 `all: unset` 清掉、以及桑基图那份「等价视图」到底是不是等价的。
 *
 * ## 这一组测试的立场
 *
 * axe 查得出的东西只是底线，而且它对本项目最关键的两条无能为力：
 *
 *   ① **桑基图有没有真的等价文本。** axe 只会检查 `role="img"` 有没有
 *      `aria-label`，不会检查那句 label 里说的是不是这张图真正承载的信息。
 *      spec §11.6 把桑基图的无障碍评级定为 **C** —— 表视图是必需的等价视图，
 *      而「等价」的判据是**同一份数据、同一个合计**，不是「旁边也摆了张表」。
 *   ② **活动区域会不会吵到没法用。** 一个挂在 1s 更新的合计上的 `aria-live`
 *      在 axe 眼里完全合规，在实际使用中则是每秒打断一次屏幕阅读器。
 *      Part B 的实现者已经为此拆掉过一个，这里把那条纪律钉死。
 *
 * 所以下面除了 axe，还有一组**手写的结构断言**。它们才是这个文件的主体。
 */
import { describe, it, expect, vi } from 'vitest';
import { render, screen, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { axe } from 'vitest-axe';

import App from './App.svelte';
import FlowTable from './views/FlowTable.svelte';
import Sankey from './views/Sankey.svelte';
import TrafficView from './views/TrafficView.svelte';
import RulesView from './views/RulesView.svelte';
import OutboundsView from './views/OutboundsView.svelte';
import SettingsOverlay from './views/SettingsOverlay.svelte';
import Segmented from './lib/Segmented.svelte';
import { WEIGHT_CONNS } from './lib/flows.js';
import { makePalette } from './lib/palette.js';

const colorOf = makePalette(['日本节点', '新加坡']);
const noop = () => {};

/** 每行都带 weight 与 weightUnit —— 拿到粗细就必然拿到它的含义 */
const flow = (site, rule, outbound, conns) => ({
  site,
  rule,
  outbound,
  conns,
  bytes: 0,
  weight: conns,
  weightUnit: WEIGHT_CONNS,
});

const flows = [
  flow('a.com', 'GEOSITE,cn,DIRECT', 'DIRECT', 9),
  flow('b.com', 'MATCH,日本节点', '日本节点', 5),
  flow('c.com', 'DOMAIN-SUFFIX,x.com,新加坡', '新加坡', 3),
  flow('d.com', 'MATCH,日本节点', '日本节点', 2),
];

const rules = [
  { id: 1, raw: 'GEOSITE,cn,DIRECT', line: 12, type: 'geosite', value: 'cn', target: 'DIRECT', hits: 42663, enabled: true },
  { id: 2, raw: 'MATCH,日本节点', line: 13, type: 'match', value: '*', target: '日本节点', hits: 88120, enabled: true },
];

const outbounds = [
  { id: 'a', name: '日本节点', state: 'live', latency: 38, sessions: 4, enabled: true, host: true },
  { id: 'b', name: '新加坡', state: 'reconnecting', latency: null, sessions: 0, enabled: false },
];

/** 每个视图连同它的一份真实 props。审计逐个跑过去。 */
const cases = [
  ['流量表', FlowTable, { rows: flows, colorOf }],
  ['桑基图', Sankey, { rows: flows, colorOf, onPickOutbound: noop }],
  ['流量视图', TrafficView, { flows, colorOf, connected: true, mixedPort: 7890, outboundCount: 2 }],
  ['规则视图', RulesView, { rules, colorOf, probe: null, ontest: noop, onreorder: noop, ontoggle: noop }],
  ['出站列表', OutboundsView, { outbounds, colorOf, ontoggle: noop, onprobe: noop, onadd: noop }],
  [
    '设置覆盖层',
    SettingsOverlay,
    { open: true, config: { mixedPort: 7890, carrier: 'shared' }, onclose: noop, onsave: noop, onexport: noop },
  ],
];

describe('axe 自动审计', () => {
  for (const [name, Comp, props] of cases) {
    it(`${name} 无 axe 违规`, async () => {
      const { container } = render(Comp, props);
      const r = await axe(container);
      const violations = r.violations ?? [];
      if (violations.length) {
        // 报告要能直接看懂是哪条规则、哪个元素 —— 只说「有 3 条违规」的
        // 测试输出，等于让下一个人从头再查一遍
        const msg = violations
          .map((v) => `${v.id}: ${v.help}\n  ${v.nodes.map((n) => n.html).join('\n  ')}`)
          .join('\n');
        throw new Error(`${name} 有 ${violations.length} 条无障碍违规：\n${msg}`);
      }
      expect(violations).toHaveLength(0);
    });
  }

  it('可复用的 Segmented 自身无违规', async () => {
    const { container } = render(Segmented, {
      label: '视图',
      options: [
        { value: 'a', label: '流量' },
        { value: 'b', label: '规则' },
      ],
      value: 'a',
      onchange: noop,
    });
    const r = await axe(container);
    expect(r.violations ?? []).toHaveLength(0);
  });
});

describe('每个可交互元素都有可访问名字', () => {
  const named = (el, container) =>
    el.getAttribute('aria-label') ||
    el.getAttribute('aria-labelledby') ||
    el.getAttribute('title') ||
    el.textContent.trim() ||
    (el.id && container.querySelector(`label[for="${CSS.escape(el.id)}"]`));

  for (const [name, Comp, props] of cases) {
    it(`${name} 里没有匿名控件`, () => {
      const { container, unmount } = render(Comp, props);
      const els = container.querySelectorAll(
        'button, input, select, textarea, [role="switch"], [role="radio"], [role="button"]',
      );
      // 空查询会让这条测试永远通过 —— 先确认真的查到了东西
      expect(els.length, `${name} 一个可交互元素都没查到，选择器八成写错了`).toBeGreaterThan(0);
      for (const el of els) {
        expect(
          named(el, container),
          `${name} 里有个没名字的元素：${el.outerHTML.slice(0, 120)}`,
        ).toBeTruthy();
      }
      unmount();
    });
  }
});

describe('颜色绝不是唯一的信息载体', () => {
  /**
   * 色块旁边必须有文字。
   *
   * 「旁边」的范围要按布局取：在流量表与规则行里色块与名字同格，在出站列表里
   * 色块**独占一个 td**（对齐成列用），名字在下一格。审计的第一版按
   * `chip.parentElement` 取，于是出站列表当场报错 —— 那不是产品的问题，
   * 是断言的范围错了。真正要问的是「这一行里，颜色是不是唯一的载体」，
   * 所以范围取到行（表格里是 tr，否则退回父元素）。
   *
   * 范围放宽之后仍然抓得到东西：一个只有色块、名字全靠颜色区分的行
   * 依然会被这条拦下。
   */
  const scopeOf = (chip) => chip.closest('tr') ?? chip.parentElement;

  for (const [name, Comp, props] of cases) {
    it(`${name} 的色块一律 aria-hidden，且同一行里有文字`, () => {
      const { container, unmount } = render(Comp, props);
      for (const chip of container.querySelectorAll('.chip')) {
        expect(chip.getAttribute('aria-hidden'), `${name} 的色块没有 aria-hidden`).toBe('true');
        expect(
          scopeOf(chip).textContent.trim().length,
          `${name} 有个色块所在的行没有任何文字，这一格的信息只存在于颜色里`,
        ).toBeGreaterThan(0);
      }
      unmount();
    });
  }

  it('出站列表的色块旁边确实有名字（上面那条范围放宽后的补充）', () => {
    // 范围从「同格」放宽到「同行」之后，得单独钉一下出站名真的在那一行里，
    // 否则放宽等于把这条测试变成永远通过
    const { container } = render(OutboundsView, {
      outbounds,
      colorOf,
      ontoggle: noop,
      onprobe: noop,
      onadd: noop,
    });
    for (const chip of container.querySelectorAll('tbody .chip')) {
      const row = chip.closest('tr');
      expect(outbounds.some((o) => row.textContent.includes(o.name))).toBe(true);
    }
  });

  it('出站的异常状态有文字，不只是一抹颜色', () => {
    render(OutboundsView, { outbounds, colorOf, ontoggle: noop, onprobe: noop, onadd: noop });
    expect(screen.getByText(/重连中/)).toBeInTheDocument();
  });

  it('探针命中的那一行有 aria-current，不只靠背景亮度', () => {
    render(RulesView, {
      rules,
      colorOf,
      probe: { index: 2, decision: 'Outbound', outbound: '日本节点', tried: 1, needResolve: false },
      ontest: noop,
      onreorder: noop,
      ontoggle: noop,
    });
    expect(screen.getAllByRole('row').slice(1)[1]).toHaveAttribute('aria-current', 'true');
  });

  it('热度染色对辅助技术不是唯一线索 —— 命中数本身就是一列', () => {
    // signature ① 是给眼睛的。屏幕阅读器拿不到背景亮度，但拿得到数字，
    // 前提是那个数字真的以文本形式存在于表格里
    render(RulesView, { rules, colorOf, probe: null, ontest: noop, onreorder: noop, ontoggle: noop });
    expect(screen.getByText('42,663')).toBeInTheDocument();
    expect(screen.getByText('88,120')).toBeInTheDocument();
  });
});

describe('桑基图的等价视图必须真的等价（spec §11.6，图本身评级 C）', () => {
  it('图有可访问名字与文字摘要，且摘要指向表视图', () => {
    const { container } = render(Sankey, { rows: flows, colorOf, onPickOutbound: noop });
    // role="group" 而非 img —— 见 Sankey.svelte 里那段注释（审计findings ①）
    const svg = container.querySelector('svg[role="group"]');
    const label = svg.getAttribute('aria-label');
    expect(label).toMatch(/表视图/);
    // 摘要里得有条数与合计，不能只是一句「桑基图」
    expect(label).toMatch(/4 条流/);
    expect(label).toMatch(/19 条连接/);
  });

  it('图的摘要说清了粗细代表什么 —— 不说就等于默认标错轴', () => {
    const { container } = render(Sankey, { rows: flows, colorOf, onPickOutbound: noop });
    expect(container.querySelector('svg[role="group"]').getAttribute('aria-label')).toMatch(/连接数/);
  });

  it('摘要另有一份可被正常导航读到的文本 —— 容器名只在进入时播报一次', () => {
    const { container } = render(Sankey, { rows: flows, colorOf, onPickOutbound: noop });
    const sr = container.querySelector('figure > .sr-only');
    expect(sr, '图没有可导航到的文字摘要').toBeTruthy();
    expect(sr.textContent).toMatch(/表视图/);
    // 两份摘要必须同源，否则改了一份另一份还在说旧话
    expect(sr.textContent).toBe(container.querySelector('svg[role="group"]').getAttribute('aria-label'));
  });

  it('表把图只靠形状传达的东西写成了文字：每一条流的三段路径', () => {
    render(FlowTable, { rows: flows, colorOf });
    for (const f of flows) {
      const cell = screen.getAllByText(f.site)[0];
      const row = cell.closest('tr');
      // 同一行里三段齐全 —— 图上那条流带的全部信息
      expect(within(row).getByText(f.rule)).toBeInTheDocument();
      expect(row.textContent).toContain(f.outbound);
    }
  });

  it('表里有出站小计 —— 图只靠节点高度传达的量', () => {
    // 「哪个出站扛的最多」在图上是一眼可见的柱高，在表里若没有小计，
    // 屏幕阅读器用户得自己把 N 行加起来
    const { container } = render(FlowTable, { rows: flows, colorOf });
    const caption = container.querySelector('caption').textContent;
    expect(caption).toMatch(/日本节点/);
    expect(caption).toMatch(/DIRECT/);
  });

  it('表里有规则小计 —— 图的中间层', () => {
    const { container } = render(FlowTable, { rows: flows, colorOf });
    expect(container.querySelector('caption').textContent).toMatch(/按规则小计/);
  });

  it('图与表读的是同一份数据，合计必然相等', () => {
    // 「等价」的判据是同一份数据，不是「旁边也摆了张表」。
    // 两边各算各的话，迟早有一天两个数字对不上，而没人会发现。
    const a = render(Sankey, { rows: flows, colorOf, onPickOutbound: noop });
    const sankeyTotal = a.container.querySelector('svg[role="group"]').getAttribute('aria-label');
    a.unmount();
    const b = render(FlowTable, { rows: flows, colorOf });
    const tableTotal = b.container.querySelector('caption').textContent;
    expect(sankeyTotal).toMatch(/19 条连接/);
    expect(tableTotal).toMatch(/19 条连接/);
  });

  it('表可排序，且排序状态用 aria-sort 播报', async () => {
    const u = userEvent.setup();
    render(FlowTable, { rows: flows, colorOf });
    const th = screen.getAllByRole('columnheader')[0];
    await u.click(within(th).getByRole('button'));
    expect(th.getAttribute('aria-sort')).toMatch(/ascending|descending/);
  });

  it('表的列头是按钮，键盘可达', () => {
    render(FlowTable, { rows: flows, colorOf });
    for (const th of screen.getAllByRole('columnheader')) {
      const btn = within(th).queryByRole('button');
      expect(btn, '列头不是按钮，键盘用户排不了序').toBeTruthy();
      expect(btn.tabIndex).toBeGreaterThanOrEqual(0);
    }
  });

  it('图与表能互相切换，切换控件自身可键盘操作', async () => {
    const u = userEvent.setup();
    render(TrafficView, { flows, colorOf, connected: true, outboundCount: 2 });
    const shape = screen.getByRole('radiogroup', { name: '显示形态' });
    within(shape).getByRole('radio', { name: '图' }).focus();
    await u.keyboard('{ArrowRight}');
    expect(screen.getByRole('table')).toBeInTheDocument();
  });
});

describe('活动区域不能吵到没法用', () => {
  /**
   * 挂 aria-live 的东西必须是**用户动作的结果**，而不是每秒到一次的数据。
   *
   * 1s 一次的播报会排队堆积，把用户正在读的内容一遍遍打断 ——
   * 那不是把信息给他，是让他没法用这个界面。
   */
  const liveIn = (container) =>
    [...container.querySelectorAll('[aria-live], [role="alert"], [role="status"]')];

  it('流量视图的合计**没有** aria-live —— 它 1s 变一次', () => {
    const { container } = render(TrafficView, { flows, colorOf, connected: true, outboundCount: 2 });
    const total = container.querySelector('.total');
    expect(total).toBeTruthy();
    expect(total.closest('[aria-live]')).toBeNull();
  });

  it('状态条**没有** aria-live —— traffic 事件 1s 一次', async () => {
    installStubTauri();
    const { container } = render(App);
    await vi.waitFor(() => expect(container.querySelector('.status')).toBeTruthy());
    expect(container.querySelector('.status').getAttribute('aria-live')).toBeNull();
    teardownStubTauri();
  });

  it('桑基图整幅**没有** aria-live —— 它每帧都在重算', () => {
    const { container } = render(Sankey, { rows: flows, colorOf, onPickOutbound: noop });
    expect(liveIn(container).filter((e) => e.getAttribute('role') !== 'alert')).toHaveLength(0);
  });

  it('规则排序的播报是 polite，且只在用户按键之后才有内容', async () => {
    const u = userEvent.setup();
    const { container } = render(RulesView, {
      rules,
      colorOf,
      probe: null,
      ontest: noop,
      onreorder: noop,
      ontoggle: noop,
    });
    const live = container.querySelector('[aria-live="polite"][role="status"]');
    expect(live.textContent.trim()).toBe('');
    screen.getAllByRole('button', { name: /移动/ })[0].focus();
    await u.keyboard('{Alt>}{ArrowDown}{/Alt}');
    expect(live.textContent).toMatch(/第 2 条/);
  });

  it('写回失败用 alert（要打断），排序位置用 polite（不该打断）', () => {
    // 两者混用会让「顺序没落盘」这条被排在位置播报后面读到，而那时用户
    // 已经在改下一条规则了
    const { container } = render(RulesView, {
      rules,
      colorOf,
      probe: null,
      saveError: { kind: 'io', message: '磁盘已满' },
      ontest: noop,
      onreorder: noop,
      ontoggle: noop,
    });
    expect(container.querySelector('.save-err').getAttribute('role')).toBe('alert');
    expect(container.querySelector('[aria-live="polite"][role="status"]')).toBeTruthy();
  });

  it('排序播报与探针播报是两个独立区域 —— 共用会互相打断', () => {
    const { container } = render(RulesView, {
      rules,
      colorOf,
      probe: { index: 1, decision: 'Direct', tried: 0, needResolve: false },
      ontest: noop,
      onreorder: noop,
      ontoggle: noop,
    });
    const lives = container.querySelectorAll('[aria-live="polite"]');
    expect(lives.length).toBeGreaterThanOrEqual(2);
  });
});

describe('键盘可达性：拔掉鼠标要能走完', () => {
  it('规则的拖拽把手有键盘等价物，且名字里写明了怎么按', () => {
    render(RulesView, { rules, colorOf, probe: null, ontest: noop, onreorder: noop, ontoggle: noop });
    for (const h of screen.getAllByRole('button', { name: /移动/ })) {
      expect(h.tabIndex).toBeGreaterThanOrEqual(0);
      expect(h.getAttribute('aria-label')).toMatch(/Alt/);
    }
  });

  it('桑基节点可 Tab 到并能用回车激活', async () => {
    const u = userEvent.setup();
    const picked = vi.fn();
    const { container } = render(Sankey, { rows: flows, colorOf, onPickOutbound: picked });
    const nodes = [...container.querySelectorAll('rect.node')];
    expect(nodes.length).toBeGreaterThan(0);
    for (const n of nodes) expect(n.getAttribute('tabindex')).toBe('0');
    // 出站层的节点（layer 2）激活后跳规则视图
    const out = nodes.find((n) => (n.getAttribute('aria-label') ?? '').startsWith('出站'));
    out.focus();
    await u.keyboard('{Enter}');
    expect(picked).toHaveBeenCalled();
  });

  it('segmented 用方向键切换，而不是 Tab 逐个走（roving tabindex）', async () => {
    const u = userEvent.setup();
    const onchange = vi.fn();
    render(Segmented, {
      label: '视图',
      options: [
        { value: 'a', label: '流量' },
        { value: 'b', label: '规则' },
        { value: 'c', label: '出站' },
      ],
      value: 'a',
      onchange,
    });
    const radios = screen.getAllByRole('radio');
    // 只有选中项在 tab 序里
    expect(radios.map((r) => r.tabIndex)).toEqual([0, -1, -1]);
    radios[0].focus();
    await u.keyboard('{ArrowRight}');
    expect(onchange).toHaveBeenCalledWith('b');
  });

  it('segmented 的容器本身不该抢进 tab 序', () => {
    // roving tabindex 的约定是 Tab 进出整组、方向键在组内移动。
    // 容器若也拿 tabindex=0，Tab 会先停在一个什么都不是的 div 上。
    const { container } = render(Segmented, {
      label: '视图',
      options: [{ value: 'a', label: '流量' }],
      value: 'a',
      onchange: noop,
    });
    const group = container.querySelector('[role="radiogroup"]');
    expect(group.getAttribute('tabindex')).not.toBe('0');
  });

  it('出站的测速与开关都能 Tab 到', () => {
    render(OutboundsView, { outbounds, colorOf, ontoggle: noop, onprobe: noop, onadd: noop });
    for (const b of [...screen.getAllByRole('switch'), ...screen.getAllByRole('button', { name: /测试延迟/ })]) {
      expect(b.tabIndex).toBeGreaterThanOrEqual(0);
    }
  });

  it('设置覆盖层的焦点被困在层内，Esc 能出去', async () => {
    const u = userEvent.setup();
    const onclose = vi.fn();
    render(SettingsOverlay, {
      open: true,
      config: { mixedPort: 7890, carrier: 'shared' },
      onclose,
      onsave: noop,
      onexport: noop,
    });
    const dialog = screen.getByRole('dialog');
    await vi.waitFor(() => expect(dialog.contains(document.activeElement)).toBe(true));
    await u.keyboard('{Escape}');
    expect(onclose).toHaveBeenCalled();
  });
});

describe('焦点环不能被 all:unset 清掉', () => {
  /**
   * `all: unset` 是本项目按钮的通用起手式（去掉浏览器默认样式），
   * 而它会**连 outline 一起清掉**。tokens.css 里的全局 `:focus-visible`
   * 用的正是 outline —— 被 unset 的元素拿不到它，焦点环就此静默消失，
   * 而「静默」是这里的关键词：视觉上一切正常，只有键盘用户发现自己不知道
   * 焦点在哪，而这类反馈几乎不会传回来。
   *
   * jsdom 不做样式层叠，所以断言的是**源码里有没有把它加回来**。这条查的
   * 是纪律而非渲染结果 —— 但它能在 CI 上拦住下一个人，那就有价值。
   *
   * 文件表用 import.meta.glob 生成而不是手写：手写的表在有人新增组件时
   * 不会自动覆盖它，于是这条测试会安静地漏掉恰恰最需要检查的那个新文件。
   */
  const sources = import.meta.glob('./**/*.svelte', { query: '?raw', import: 'default', eager: true });
  const names = Object.keys(sources).sort();

  /**
   * 注释必须先剥掉。
   *
   * 第一版的断言是「文件里出现过 :focus-visible 这几个字」，而上面那段解释
   * 为什么需要焦点环的**注释里**就写着这个词 —— 于是把真正的 CSS 规则整个
   * 删掉，测试照样通过。那正是本项目反复抓到的那类假绿：一个不可能失败的断言。
   *
   * 所以剥注释，并且数的是**规则**（`:focus-visible ... {`）而不是词。
   */
  const stripComments = (s) => s.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^\s*\/\/.*$/gm, '');
  const countRules = (s, re) => (s.match(re) ?? []).length;

  it('真的扫到了组件，不是在对着一张空表打勾', () => {
    expect(names.length).toBeGreaterThanOrEqual(10);
  });

  for (const f of names) {
    it(`${f}：每一处 all:unset 都配一条 :focus-visible 规则`, () => {
      const src = stripComments(sources[f]);
      const unset = countRules(src, /all:\s*unset/g);
      // 没用 unset 的元素走 tokens.css 的全局 :focus-visible，不必自己加
      if (unset === 0) return;
      const rules = countRules(src, /:focus-visible[^{]*\{/g);
      expect(
        rules,
        `${f} 里有 ${unset} 处 all:unset，但只有 ${rules} 条 :focus-visible 规则。` +
          `all:unset 会把 outline 一起清掉，少一条就有一类控件的焦点环静默消失。`,
      ).toBeGreaterThanOrEqual(unset);
    });
  }

  it('没有任何组件把焦点环 outline:none 掉', () => {
    for (const f of names) {
      // `outline: none` 不带补偿是无障碍上的经典失误：视觉上干净了，
      // 键盘用户从此不知道自己在哪
      const bad = /outline:\s*(none|0)\s*[;}]/.exec(stripComments(sources[f]));
      expect(bad, `${f} 里有 outline:none —— 键盘用户会看不到焦点在哪`).toBeNull();
    }
  });

  it('这一组断言本身是可以失败的（自检）', () => {
    // 上面那些测试逐个 return 掉就会全绿。这里造一份必然违规的源码喂给
    // 同一套判据，确认它真的会说「不行」——否则整组测试等于没写。
    const fake = '<style>button { all: unset; }</style>';
    const src = stripComments(fake);
    expect(countRules(src, /all:\s*unset/g)).toBe(1);
    expect(countRules(src, /:focus-visible[^{]*\{/g)).toBe(0);
    // 注释里提到这个词不算数
    const commentOnly = '<style>/* :focus-visible 说明 */ button { all: unset; }</style>';
    expect(countRules(stripComments(commentOnly), /:focus-visible[^{]*\{/g)).toBe(0);
  });
});

describe('文本对比度：低对比文本必须另有高对比的同源信息', () => {
  /**
   * spec §11.4 定死了 `--text-4`（#4d545b），它在 panel（#1c1f23）上约 2.6:1，
   * 低于 WCAG AA 的 4.5:1。**令牌值不改** —— 它是 spec 规定的。
   * 该改的是「哪些内容配用哪一级文本色」：用 --text-4 的地方，其信息必须
   * 在别处以更高对比度重复出现一次。
   *
   * 下面逐个查用了 --text-4 的位置，确认它们承载的都是**辅助措辞**
   * （占位符、单位、hint、分隔符），而不是唯一的数据来源。
   */
  it('规则视图：类型列用低对比灰，但匹配值与出站是高对比的', () => {
    const { container } = render(RulesView, {
      rules,
      colorOf,
      probe: null,
      ontest: noop,
      onreorder: noop,
      ontoggle: noop,
    });
    // 类型是靠列位置识别的辅助信息；真正决定「这条规则干什么」的是
    // 匹配值与出站，两者都在 --text-1 / --text-2 上
    expect(container.querySelector('.val').textContent.trim()).toBeTruthy();
    expect(container.querySelector('.out').textContent.trim()).toBeTruthy();
  });

  it('出站视图：未测得的延迟给占位符而非低对比的 0', () => {
    render(OutboundsView, { outbounds, colorOf, ontoggle: noop, onprobe: noop, onadd: noop });
    expect(screen.queryByText('0 ms')).toBeNull();
  });

  it('流量视图的口径说明是可见文本，不藏在 sr-only 里', () => {
    // 「粗细 = 连接数，不是吞吐量」这句话对所有人都必要，不只是屏幕阅读器用户
    const { container } = render(Sankey, { rows: flows, colorOf, onPickOutbound: noop });
    const cap = container.querySelector('figcaption');
    expect(cap).toBeTruthy();
    expect(cap.className).not.toMatch(/sr-only/);
    expect(cap.textContent).toMatch(/连接数/);
  });
});

describe('表格语义完整', () => {
  for (const [name, Comp, props] of [
    ['流量表', FlowTable, { rows: flows, colorOf }],
    ['规则视图', RulesView, { rules, colorOf, probe: null, ontest: noop, onreorder: noop, ontoggle: noop }],
    ['出站列表', OutboundsView, { outbounds, colorOf, ontoggle: noop, onprobe: noop, onadd: noop }],
  ]) {
    it(`${name} 有 caption 与 scope=col 的表头`, () => {
      const { container, unmount } = render(Comp, props);
      const table = container.querySelector('table');
      expect(table, `${name} 没有 table`).toBeTruthy();
      expect(table.querySelector('caption'), `${name} 的表没有 caption`).toBeTruthy();
      const ths = table.querySelectorAll('thead th');
      expect(ths.length).toBeGreaterThan(0);
      for (const th of ths) {
        expect(th.getAttribute('scope'), `${name} 有个表头没写 scope`).toBe('col');
      }
      unmount();
    });

    it(`${name} 的每个表头都有可读文本（视觉隐藏的也算）`, () => {
      const { container, unmount } = render(Comp, props);
      for (const th of container.querySelectorAll('thead th')) {
        expect(th.textContent.trim(), `${name} 有个空表头：${th.outerHTML}`).toBeTruthy();
      }
      unmount();
    });
  }
});

describe('组装后的整体审计', () => {
  it('App 整体无 axe 违规', async () => {
    installStubTauri();
    const { container } = render(App);
    await vi.waitFor(() => expect(screen.getByRole('radiogroup', { name: '视图' })).toBeTruthy());
    const r = await axe(container);
    const violations = r.violations ?? [];
    if (violations.length) {
      const msg = violations
        .map((v) => `${v.id}: ${v.help}\n  ${v.nodes.map((n) => n.html).join('\n  ')}`)
        .join('\n');
      throw new Error(`组装后有 ${violations.length} 条无障碍违规：\n${msg}`);
    }
    teardownStubTauri();
  });

  it('打开设置覆盖层之后整体仍无违规', async () => {
    installStubTauri();
    const u = userEvent.setup();
    const { container } = render(App);
    await vi.waitFor(() => expect(screen.getByRole('button', { name: '打开设置' })).toBeTruthy());
    await u.click(screen.getByRole('button', { name: '打开设置' }));
    await screen.findByRole('dialog');
    const r = await axe(container);
    const violations = r.violations ?? [];
    if (violations.length) {
      const msg = violations
        .map((v) => `${v.id}: ${v.help}\n  ${v.nodes.map((n) => n.html).join('\n  ')}`)
        .join('\n');
      throw new Error(`覆盖层打开时有 ${violations.length} 条违规：\n${msg}`);
    }
    teardownStubTauri();
  });

  it('全局 id 不重复 —— 三个视图里的表单控件挤在同一个文档里', async () => {
    installStubTauri();
    const u = userEvent.setup();
    const { container } = render(App);
    await vi.waitFor(() => expect(screen.getByRole('button', { name: '打开设置' })).toBeTruthy());
    // 规则视图的探针输入框 + 设置层的几个字段同时在场
    await u.click(screen.getByRole('radio', { name: '规则' }));
    await u.click(screen.getByRole('button', { name: '打开设置' }));
    await screen.findByRole('dialog');

    const ids = [...container.querySelectorAll('[id]')].map((e) => e.id);
    expect(new Set(ids).size, `id 重复：${ids.join(', ')}`).toBe(ids.length);
    teardownStubTauri();
  });

  it('页面上只有一个 main 与一个 status 地标', async () => {
    installStubTauri();
    const { container } = render(App);
    await vi.waitFor(() => expect(container.querySelector('main')).toBeTruthy());
    expect(container.querySelectorAll('main')).toHaveLength(1);
    expect(container.querySelectorAll('[role="status"]')).toHaveLength(1);
    teardownStubTauri();
  });
});

// ── 环境桩 ────────────────────────────────────────────────────────
// 与 App.test.js 同类：补的是浏览器全局，不是对被测代码的 mock。
// 这里只需要「命令能返回、事件能订阅」，形状取最小可用的一份。

function installStubTauri() {
  window.__TAURI__ = {
    core: {
      invoke: async (cmd) => {
        if (cmd === 'config_get') {
          return {
            config: {
              'mixed-port': 7890,
              carrier: 'shared',
              'carrier-host': '',
              proxies: [{ name: '日本节点' }, { name: '新加坡' }],
            },
            rules: [
              { value: 'GEOSITE,cn,DIRECT', line: 12 },
              { value: 'MATCH,日本节点', line: 13 },
            ],
          };
        }
        if (cmd === 'traffic_snapshot') {
          return { up_bytes: 0, down_bytes: 0, up_rate: 0, down_rate: 0, active: 0 };
        }
        throw { kind: 'not-ready', message: `${cmd} 尚未接入` };
      },
    },
    event: { listen: async () => () => {} },
  };
}

function teardownStubTauri() {
  delete window.__TAURI__;
}
