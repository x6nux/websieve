import { describe, it, expect } from 'vitest';
import { render, screen, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import FlowTable from './FlowTable.svelte';
import { WEIGHT_BYTES, WEIGHT_CONNS } from '../lib/flows.js';

// 与桑基图共享同一份形状。bytes 全零 —— 当前后端的真实情况。
const rows = [
  { site: 'a.com', rule: 'final *', outbound: 'JP', bytes: 0, conns: 3, weight: 3, weightUnit: WEIGHT_CONNS },
  { site: 'b.com', rule: 'geosite cn', outbound: 'DIRECT', bytes: 0, conns: 9, weight: 9, weightUnit: WEIGHT_CONNS },
  { site: 'c.com', rule: 'keyword x', outbound: 'SG', bytes: 0, conns: 7, weight: 7, weightUnit: WEIGHT_CONNS },
];

const byteRows = [
  { site: 'a.com', rule: 'final *', outbound: 'JP', bytes: 10, conns: 3, weight: 10, weightUnit: WEIGHT_BYTES },
  { site: 'b.com', rule: 'geosite cn', outbound: 'DIRECT', bytes: 90, conns: 1, weight: 90, weightUnit: WEIGHT_BYTES },
  { site: 'c.com', rule: 'keyword x', outbound: 'SG', bytes: 50, conns: 7, weight: 50, weightUnit: WEIGHT_BYTES },
];

const colorOf = () => '#5b8ff9';

const body = () => within(screen.getAllByRole('rowgroup')[1]);
const firstCol = () =>
  body().getAllByRole('row').map((r) => within(r).getAllByRole('cell')[0].textContent.trim());

describe('表视图 —— 桑基图的无障碍等价视图', () => {
  it('是真正的 table，有 caption 供屏幕阅读器定位', () => {
    render(FlowTable, { rows, colorOf });
    expect(screen.getByRole('table')).toHaveAccessibleName(/流量/);
  });

  it('五个列头都用 th + scope=col', () => {
    render(FlowTable, { rows, colorOf });
    const ths = screen.getAllByRole('columnheader');
    // 站点 / 规则 / 出站 / 连接 / 占比 —— 无字节数据时不摆一列全是 0 的假数据
    expect(ths).toHaveLength(5);
    for (const th of ths) expect(th).toHaveAttribute('scope', 'col');
  });

  it('每个列头都暴露 aria-sort，当前排序列不是 none', () => {
    render(FlowTable, { rows, colorOf });
    const ths = screen.getAllByRole('columnheader');
    for (const th of ths) expect(th).toHaveAttribute('aria-sort');
    expect(ths.filter((th) => th.getAttribute('aria-sort') !== 'none')).toHaveLength(1);
  });

  it('默认按流量大小降序 —— 打开就看到最大的那条', () => {
    render(FlowTable, { rows, colorOf });
    expect(firstCol()).toEqual(['b.com', 'c.com', 'a.com']);
  });

  it('点列头切换排序，再点一次反向', async () => {
    const u = userEvent.setup();
    render(FlowTable, { rows, colorOf });
    // 连接数就是当前的默认排序列，点它是「反向」而非「换列」
    await u.click(screen.getByRole('button', { name: /连接数/ }));
    expect(firstCol()).toEqual(['a.com', 'c.com', 'b.com']);
    await u.click(screen.getByRole('button', { name: /连接数/ }));
    expect(firstCol()).toEqual(['b.com', 'c.com', 'a.com']);
  });

  it('点一个新的数值列默认降序 —— 先看大的', async () => {
    const u = userEvent.setup();
    render(FlowTable, { rows, colorOf });
    await u.click(screen.getByRole('button', { name: /目标站点/ }));
    expect(firstCol()).toEqual(['a.com', 'b.com', 'c.com']);
    await u.click(screen.getByRole('button', { name: /占比/ }));
    expect(firstCol()).toEqual(['b.com', 'c.com', 'a.com']);
  });

  it('文本列按字典序排', async () => {
    const u = userEvent.setup();
    render(FlowTable, { rows, colorOf });
    await u.click(screen.getByRole('button', { name: /目标站点/ }));
    expect(firstCol()).toEqual(['a.com', 'b.com', 'c.com']);
  });

  it('列头可用键盘 Tab 到达并 Enter 触发', async () => {
    const u = userEvent.setup();
    render(FlowTable, { rows, colorOf });
    await u.tab();
    expect(screen.getByRole('button', { name: /目标站点/ })).toHaveFocus();
    await u.keyboard('{Enter}');
    expect(screen.getAllByRole('columnheader')[0].getAttribute('aria-sort')).not.toBe('none');
  });

  it('出站不只靠色块传达 —— 必须有文字', () => {
    render(FlowTable, { rows, colorOf });
    // 表体里的出站格。出站名在小计区还会出现一次，那不算数据行
    const cells = body()
      .getAllByRole('row')
      .map((r) => within(r).getAllByRole('cell')[2].textContent.trim());
    expect(cells).toContain('DIRECT');
    expect(cells).toContain('JP');
    expect(cells).toContain('SG');
  });

  it('数值列用 tabular-nums 类，能纵向对齐扫读', () => {
    render(FlowTable, { rows, colorOf });
    const cells = within(body().getAllByRole('row')[0]).getAllByRole('cell');
    expect(cells[3].className).toMatch(/\bn\b/);
    expect(cells[4].className).toMatch(/\bn\b/);
  });

  it('空数据时不渲染空表骨架', () => {
    render(FlowTable, { rows: [], colorOf });
    expect(screen.queryByRole('table')).toBeNull();
  });

  it('排序后行数不变 —— 排序不能吞行', async () => {
    const u = userEvent.setup();
    render(FlowTable, { rows, colorOf });
    await u.click(screen.getByRole('button', { name: /连接/ }));
    expect(body().getAllByRole('row')).toHaveLength(3);
  });
});

describe('等价性 —— 只看表的人不能比只看图的人少知道任何事', () => {
  it('列头如实说明数值的含义，不写「字节」', () => {
    render(FlowTable, { rows, colorOf });
    // 桑基图上写的是「流带粗细 = 连接数」，表上必须是同一个量
    expect(screen.getByRole('button', { name: /连接数/ })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /字节/ })).toBeNull();
  });

  it('有占比列 —— 它是流带粗细在表里的对应物', () => {
    render(FlowTable, { rows, colorOf });
    expect(screen.getByRole('button', { name: /占比/ })).toBeInTheDocument();
    // 9 / 19 ≈ 47%
    expect(within(body().getAllByRole('row')[0]).getAllByRole('cell')[4].textContent.trim()).toBe('47%');
  });

  it('出站小计对屏幕阅读器可得 —— 图上那是出站节点的高度', () => {
    render(FlowTable, { rows, colorOf });
    const cap = screen.getByRole('table').getAttribute('aria-label') ?? '';
    const text = screen.getByRole('table').textContent;
    const all = cap + text;
    expect(all).toMatch(/DIRECT\s*9/);
    expect(all).toMatch(/SG\s*7/);
    expect(all).toMatch(/JP\s*3/);
  });

  it('规则小计对屏幕阅读器可得 —— 图上那是中间层节点的高度', () => {
    const two = [
      ...rows,
      { site: 'd.com', rule: 'final *', outbound: 'JP', bytes: 0, conns: 5, weight: 5, weightUnit: WEIGHT_CONNS },
    ];
    render(FlowTable, { rows: two, colorOf });
    // a.com(3) + d.com(5) 同命中 final *，图上是一个高度为 8 的节点
    expect(screen.getByRole('table').textContent).toMatch(/final \*\s*8/);
  });

  it('合计与图上的合计是同一个数', () => {
    render(FlowTable, { rows, colorOf });
    expect(screen.getByRole('table').textContent).toMatch(/19/);
  });

  it('聚合行标注了它是聚合行，而不是一个真实站点', () => {
    const agg = [
      ...rows,
      { site: '其他 14 个站点', rule: 'final *', outbound: 'JP', bytes: 0, conns: 4, weight: 4, weightUnit: WEIGHT_CONNS, aggregated: true },
    ];
    render(FlowTable, { rows: agg, colorOf });
    const row = body().getAllByRole('row').find((r) => r.textContent.includes('其他 14 个站点'));
    // 只靠颜色变暗传达「这不是一个站点」，屏幕阅读器一点也拿不到
    expect(within(row).getByText(/聚合/)).toBeInTheDocument();
  });

  it('单位换成字节后多出字节列，占比与连接都还在', () => {
    render(FlowTable, { rows: byteRows, colorOf });
    const ths = screen.getAllByRole('columnheader').map((t) => t.textContent);
    expect(ths.some((t) => /字节/.test(t))).toBe(true);
    expect(ths.some((t) => /连接/.test(t))).toBe(true);
    expect(ths.some((t) => /占比/.test(t))).toBe(true);
  });
});

describe('单位冲突时表比图更耐受', () => {
  const mixed = [
    { site: 'a.com', rule: 'r1', outbound: 'JP', conns: 3, weight: 3, weightUnit: WEIGHT_CONNS },
    { site: 'b.com', rule: 'r2', outbound: 'SG', conns: 1, weight: 1024, weightUnit: WEIGHT_BYTES },
  ];

  it('报出冲突，但站点/规则/出站/连接这些逐行事实照样给', () => {
    render(FlowTable, { rows: mixed, colorOf });
    expect(screen.getByRole('alert')).toBeInTheDocument();
    // 这几列不依赖共同单位，没有理由跟着一起消失
    expect(screen.getByRole('table')).toBeInTheDocument();
    expect(screen.getByText('a.com')).toBeInTheDocument();
    expect(screen.getByText('SG')).toBeInTheDocument();
  });

  it('占比列消失 —— 分母说不清时算不出占比', () => {
    render(FlowTable, { rows: mixed, colorOf });
    expect(screen.queryByRole('button', { name: /占比/ })).toBeNull();
  });
});
