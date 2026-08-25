import { describe, it, expect } from 'vitest';
import { render, screen, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import TrafficView from './TrafficView.svelte';
import { WEIGHT_CONNS } from '../lib/flows.js';

const mk = (site, rule, outbound, w) => ({
  site,
  rule,
  outbound,
  bytes: 0,
  conns: w,
  weight: w,
  weightUnit: WEIGHT_CONNS,
});

const many = [
  mk('youtube.com', 'geosite google', '日本节点', 9),
  mk('github.com', 'geosite github', '新加坡', 5),
  mk('taobao.com', 'geosite cn', 'DIRECT', 7),
  mk('x.com', 'final MATCH', '日本节点', 4),
];

const colorOf = () => '#5b8ff9';

describe('图与表共享同一份数据', () => {
  it('切到表后合计读数不变 —— 变了就是聚合的守恒被破坏了', async () => {
    const u = userEvent.setup();
    const { container } = render(TrafficView, { flows: many, colorOf, connected: true });
    const readAll = () => container.querySelector('.total').textContent.trim();
    const before = readAll();
    expect(before).toMatch(/25 条连接/);
    await u.click(screen.getByRole('radio', { name: '表' }));
    expect(readAll()).toBe(before);
  });

  it('默认是图，不是表 —— 打开即见走向', () => {
    const { container } = render(TrafficView, { flows: many, colorOf, connected: true });
    expect(container.querySelector('svg[role="group"]')).not.toBeNull();
    expect(screen.queryByRole('table')).toBeNull();
  });

  it('合计带量词，不会被读成体积', () => {
    const { container } = render(TrafficView, { flows: many, colorOf, connected: true });
    const el = container.querySelector('.total');
    expect(el.textContent.trim()).toBe('25 条连接');
    // 1s 一次的数字不能做成活动区域：那会让屏幕阅读器每秒打断用户一次
    expect(el.getAttribute('aria-live')).toBeNull();
  });

  it('不提供「字节 / 连接数」切换 —— 那是数据源的事实，不是用户偏好', () => {
    render(TrafficView, { flows: many, colorOf, connected: true });
    const groups = screen.getAllByRole('radiogroup').map((g) => g.getAttribute('aria-label'));
    expect(groups).toEqual(['时间窗口', '显示形态']);
  });
});

describe('流数少于 3 时自动降级', () => {
  it('两条流时不画桑基图，改出列表并说明原因', () => {
    const { container } = render(TrafficView, {
      flows: [mk('a.com', 'r1', 'JP', 3), mk('b.com', 'r2', 'SG', 2)],
      colorOf,
      connected: true,
    });
    expect(container.querySelector('svg[role="group"]')).toBeNull();
    expect(screen.getByRole('table')).toBeInTheDocument();
    expect(screen.getByText(/流数少于 3 条/)).toBeInTheDocument();
  });

  it('三条流时恢复成图', () => {
    const { container } = render(TrafficView, {
      flows: [mk('a.com', 'r1', 'JP', 3), mk('b.com', 'r2', 'SG', 2), mk('c.com', 'r3', 'DIRECT', 1)],
      colorOf,
      connected: true,
    });
    expect(container.querySelector('svg[role="group"]')).not.toBeNull();
    expect(screen.queryByText(/流数少于 3 条/)).toBeNull();
  });
});

describe('空状态 —— 第一次打开时最重要的一屏', () => {
  it('不画空的坐标骨架，也不留空表头', () => {
    const { container } = render(TrafficView, { flows: [], colorOf, connected: true });
    expect(container.querySelector('svg')).toBeNull();
    expect(screen.queryByRole('table')).toBeNull();
    expect(screen.queryByRole('columnheader')).toBeNull();
  });

  it('说清这里将会出现什么 —— 不是一句「暂无数据」', () => {
    render(TrafficView, { flows: [], colorOf, connected: true, mixedPort: 7890 });
    expect(screen.getByText(/目标站点 → 命中规则 → 出站/)).toBeInTheDocument();
  });

  it('未连接时让用户启动代理，而不是让他干等', () => {
    render(TrafficView, { flows: [], colorOf, connected: false, onConnect: () => {} });
    expect(screen.getByText(/代理未运行/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: '启动代理' })).toBeInTheDocument();
  });

  it('已连接但无流量时，给出可照做的一步 —— 端口原样列出', () => {
    const { container } = render(TrafficView, { flows: [], colorOf, connected: true, mixedPort: 7890 });
    expect(screen.getByText(/代理已在运行/)).toBeInTheDocument();
    expect(container.querySelector('code').textContent).toBe('127.0.0.1:7890');
    expect(screen.getByText(/HTTP 与 SOCKS5 同口/)).toBeInTheDocument();
  });

  it('端口未知时不编一个 —— 说法退化，但不给假端口', () => {
    const { container } = render(TrafficView, { flows: [], colorOf, connected: true, mixedPort: null });
    expect(container.querySelector('code')).toBeNull();
    expect(screen.getByText(/本机的混合入口/)).toBeInTheDocument();
  });

  it('一个出站都没有时优先引导去加服务器', () => {
    render(TrafficView, {
      flows: [],
      colorOf,
      connected: false,
      outboundCount: 0,
      onAddOutbound: () => {},
      onConnect: () => {},
    });
    expect(screen.getByText(/还没有配置任何出站/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: '去添加服务器' })).toBeInTheDocument();
    // 出站都没有时先别催他启动
    expect(screen.queryByRole('button', { name: '启动代理' })).toBeNull();
  });

  it('无出站的说明点出「会拒绝而不是偷偷直连」', () => {
    render(TrafficView, { flows: [], colorOf, outboundCount: 0 });
    expect(screen.getByText(/拒绝连接而不是偷偷直连/)).toBeInTheDocument();
  });

  it('没给回调时不渲染一个点了没反应的按钮', () => {
    render(TrafficView, { flows: [], colorOf, connected: false });
    expect(screen.queryByRole('button')).toBeNull();
  });
});

describe('键盘可达', () => {
  it('时间窗口用方向键切换，且只有选中项参与 Tab 序', async () => {
    const u = userEvent.setup();
    let picked = null;
    render(TrafficView, {
      flows: many,
      colorOf,
      connected: true,
      window: '1h',
      onWindowChange: (v) => (picked = v),
    });
    const group = screen.getAllByRole('radiogroup')[0];
    const radios = within(group).getAllByRole('radio');
    expect(radios.filter((r) => r.getAttribute('tabindex') === '0')).toHaveLength(1);

    await u.tab();
    expect(within(group).getByRole('radio', { name: '1 小时' })).toHaveFocus();
    await u.keyboard('{ArrowRight}');
    expect(picked).toBe('run');
  });

  it('图/表切换可用键盘完成', async () => {
    const u = userEvent.setup();
    const { container } = render(TrafficView, { flows: many, colorOf, connected: true });
    const group = screen.getAllByRole('radiogroup')[1];
    within(group).getByRole('radio', { name: '图' }).focus();
    await u.keyboard('{ArrowRight}');
    expect(screen.getByRole('table')).toBeInTheDocument();
    expect(container.querySelector('svg[role="group"]')).toBeNull();
  });
});
