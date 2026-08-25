import { describe, it, expect } from 'vitest';
import { toGraph, layout, gradientId, applyFrozenOrder } from './sankey-layout.js';
import { WEIGHT_BYTES, WEIGHT_CONNS } from './flows.js';

const rows = [
  { site: 'a.com', rule: 'final *', outbound: 'JP', bytes: 100, conns: 3 },
  { site: 'b.com', rule: 'final *', outbound: 'JP', bytes: 50, conns: 2 },
  { site: 'c.com', rule: 'geosite cn', outbound: 'DIRECT', bytes: 30, conns: 1 },
];

describe('toGraph', () => {
  it('把重复的 rule→outbound 边聚合成一条', () => {
    // a 与 b 都命中 final * 并走 JP，这条边在原始行里出现两次。
    // 不聚合的话 Svelte 的 keyed each 会直接抛 each_key_duplicate
    // 且整个组件渲染不出来（实测）。
    const g = toGraph(rows);
    const keys = g.links.map((l) => l.key);
    expect(new Set(keys).size).toBe(keys.length);
    const e = g.links.find((l) => l.key === 'r:final *>o:JP');
    expect(e.value).toBe(150);
  });

  it('三层各自建节点，不串层', () => {
    const g = toGraph(rows);
    expect(g.nodes.filter((n) => n.layer === 0)).toHaveLength(3);
    expect(g.nodes.filter((n) => n.layer === 1)).toHaveLength(2);
    expect(g.nodes.filter((n) => n.layer === 2)).toHaveLength(2);
  });

  it('只有出站节点带 dest（颜色语义只属于出站）', () => {
    const g = toGraph(rows);
    for (const n of g.nodes) {
      if (n.layer === 2) expect(n.dest).toBeTruthy();
      else expect(n.dest).toBeNull();
    }
  });

  it('零字节的行不产生流带 —— d3 会因此产出 NaN', () => {
    expect(toGraph([{ site: 'x', rule: 'y', outbound: 'z', bytes: 0 }])).toBeNull();
  });

  it('空输入返回 null', () => {
    expect(toGraph([])).toBeNull();
    expect(toGraph(null)).toBeNull();
  });
});

describe('toGraph 与 flows.js 的 weight 契约', () => {
  // Part A 的契约：每行带 weight + weightUnit，画图用的量是 weight 而非 bytes。
  // 当前后端不上报逐流字节，bytes 恒为 0 —— 若这里读 bytes，桑基图会永远空着。
  const wrows = [
    { site: 'a.com', rule: 'final *', outbound: 'JP', bytes: 0, conns: 3, weight: 3, weightUnit: WEIGHT_CONNS },
    { site: 'b.com', rule: 'final *', outbound: 'JP', bytes: 0, conns: 2, weight: 2, weightUnit: WEIGHT_CONNS },
    { site: 'c.com', rule: 'geosite cn', outbound: 'DIRECT', bytes: 0, conns: 1, weight: 1, weightUnit: WEIGHT_CONNS },
  ];

  it('bytes 全零但 weight 非零时照样出图', () => {
    const g = toGraph(wrows);
    expect(g).not.toBeNull();
    expect(g.links.find((l) => l.key === 'r:final *>o:JP').value).toBe(5);
  });

  it('weight 优先于 bytes —— 两者都在时画的是 weight', () => {
    const g = toGraph([
      { site: 'a', rule: 'r', outbound: 'o', bytes: 999, conns: 7, weight: 7, weightUnit: WEIGHT_CONNS },
    ]);
    expect(g.links.find((l) => l.key === 's:a>r:r').value).toBe(7);
  });

  it('没有 weight 字段时退回 bytes —— 阶段 2 补上字节后无需改这里', () => {
    const g = toGraph([{ site: 'a', rule: 'r', outbound: 'o', bytes: 42 }]);
    expect(g.links.find((l) => l.key === 's:a>r:r').value).toBe(42);
  });

  it('weight 全零时返回 null，走空状态而非画一张 NaN 图', () => {
    expect(
      toGraph([{ site: 'a', rule: 'r', outbound: 'o', bytes: 0, conns: 0, weight: 0, weightUnit: WEIGHT_BYTES }])
    ).toBeNull();
  });
});

describe('layout', () => {
  it('产出三列且全无 NaN', () => {
    const m = layout(toGraph(rows), 960, 452);
    const xs = [...new Set(m.nodes.map((n) => n.x0))].sort((a, b) => a - b);
    expect(xs).toHaveLength(3);
    for (const n of m.nodes) {
      expect(Number.isFinite(n.x0)).toBe(true);
      expect(Number.isFinite(n.y0)).toBe(true);
      expect(Number.isFinite(n.y1)).toBe(true);
    }
    for (const l of m.links) {
      expect(Number.isFinite(l.width)).toBe(true);
      expect(l.d).not.toMatch(/NaN/);
    }
  });

  it('不改写输入图 —— d3 会就地改写并塞进循环引用', () => {
    const g = toGraph(rows);
    const before = JSON.stringify(g);
    layout(g, 960, 452);
    expect(JSON.stringify(g)).toBe(before);
  });

  it('两次布局同一份图结果一致（幂等）', () => {
    const g = toGraph(rows);
    const a = layout(g, 960, 452);
    const b = layout(g, 960, 452);
    expect(a.nodes.map((n) => n.y0)).toEqual(b.nodes.map((n) => n.y0));
  });

  it('列内顺序严格等于输入数组顺序（节点顺序冻结的基础）', () => {
    const g = toGraph(rows);
    g.nodes.reverse();
    const m = layout(g, 960, 452);
    const got = m.nodes.filter((n) => n.layer === 0).sort((a, b) => a.y0 - b.y0).map((n) => n.id);
    const want = g.nodes.filter((n) => n.layer === 0).map((n) => n.id);
    expect(got).toEqual(want);
  });

  it('左右留出标签空间 —— 否则标签被 viewBox 裁掉', () => {
    const m = layout(toGraph(rows), 960, 452);
    const xs = m.nodes.map((n) => n.x0);
    expect(Math.min(...xs)).toBeGreaterThan(60);
    expect(Math.max(...xs)).toBeLessThan(960 - 60);
  });

  it('渐变端点覆盖第一列右缘到末列左缘', () => {
    const m = layout(toGraph(rows), 960, 452);
    const [x1, x2] = m.gradientX;
    expect(x1).toBeLessThan(x2);
    expect(x1).toBeCloseTo(Math.min(...m.nodes.map((n) => n.x1)), 5);
    expect(x2).toBeCloseTo(Math.max(...m.nodes.map((n) => n.x0)), 5);
  });

  it('节点带上自身的总量 —— 标签与朗读文本要用它', () => {
    const m = layout(toGraph(rows), 960, 452);
    const jp = m.nodes.find((n) => n.id === 'o:JP');
    expect(jp.value).toBe(150);
  });
});

describe('applyFrozenOrder', () => {
  it('按冻结顺序重排，新节点追加到末尾', () => {
    const g = toGraph(rows);
    const frozen = ['s:c.com', 's:a.com'];
    const out = applyFrozenOrder(g, frozen);
    const sites = out.nodes.filter((n) => n.layer === 0).map((n) => n.id);
    expect(sites.slice(0, 2)).toEqual(['s:c.com', 's:a.com']);
    expect(sites).toContain('s:b.com');
  });

  it('空的冻结顺序原样返回', () => {
    const g = toGraph(rows);
    expect(applyFrozenOrder(g, []).nodes).toBe(g.nodes);
  });
});

describe('gradientId', () => {
  it('中文出站名产出的 id 不含 % —— 含 % 的 id 在 url(#…) 里静默不上色', () => {
    const id = gradientId('日本节点');
    expect(id).not.toMatch(/%/);
    expect(id).toMatch(/^[A-Za-z][\w-]*$/);
  });
  it('不同名字产出不同 id', () => {
    expect(gradientId('日本节点')).not.toBe(gradientId('新加坡'));
  });
  it('同一名字稳定', () => {
    expect(gradientId('DIRECT')).toBe(gradientId('DIRECT'));
  });
});
