import { describe, it, expect } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import Sankey from './Sankey.svelte';
import { WEIGHT_BYTES, WEIGHT_CONNS } from '../lib/flows.js';

// 三条流，两个出站。bytes 全零 —— 这正是当前后端的真实形状：
// ConnectionDelta 没有字节字段，所以能画的只有连接数。
const connRows = [
  { site: 'a.com', rule: 'final *', outbound: 'JP', bytes: 0, conns: 3, weight: 3, weightUnit: WEIGHT_CONNS },
  { site: 'b.com', rule: 'final *', outbound: 'JP', bytes: 0, conns: 2, weight: 2, weightUnit: WEIGHT_CONNS },
  { site: 'c.com', rule: 'geosite cn', outbound: 'DIRECT', bytes: 0, conns: 1, weight: 1, weightUnit: WEIGHT_CONNS },
];

const byteRows = connRows.map((r, i) => ({
  ...r,
  bytes: (i + 1) * 1024,
  weight: (i + 1) * 1024,
  weightUnit: WEIGHT_BYTES,
}));

const colorOf = () => '#5b8ff9';

describe('粗细的含义写在图上，不靠注释', () => {
  it('按连接数画时，图上明说「不是吞吐量」', () => {
    render(Sankey, { rows: connRows, colorOf });
    // 桑基图这个形态本身在暗示流量，只标「连接数」仍会被读成吞吐量的近似
    expect(screen.getByText(/流带粗细 = 连接数，不是吞吐量/)).toBeInTheDocument();
  });

  it('图例可见 —— 不能藏进 sr-only 只给屏幕阅读器', () => {
    const { container } = render(Sankey, { rows: connRows, colorOf });
    const cap = container.querySelector('figcaption');
    expect(cap).not.toBeNull();
    expect(cap.className).not.toMatch(/sr-only/);
  });

  it('按连接数画时，没有任何数字被挂上字节单位', () => {
    const { container } = render(Sankey, { rows: connRows, colorOf });
    // 「字节」二字只许出现在解释原因的图例里（「后端尚未上报逐流字节」），
    // 绝不许出现在读数旁边 —— 那才是拿连接数假装吞吐量。
    const readouts = [...container.querySelectorAll('text.lbl-v')].map((t) => t.textContent);
    expect(readouts.length).toBeGreaterThan(0);
    for (const t of readouts) {
      expect(t).not.toMatch(/[\d.]\s*(B|KB|MB|GB|TB)\b/);
      expect(t).not.toMatch(/字节/);
    }
    for (const el of container.querySelectorAll('[aria-label]')) {
      const l = el.getAttribute('aria-label');
      expect(l).not.toMatch(/字节/);
      expect(l).not.toMatch(/[\d.]\s*(B|KB|MB|GB|TB)\b/);
    }
  });

  it('节点读数用千分位而非字节进位 —— 3 条连接不能读成 "3 B"', () => {
    const { container } = render(Sankey, { rows: connRows, colorOf });
    const labels = [...container.querySelectorAll('[aria-label]')].map((e) => e.getAttribute('aria-label'));
    expect(labels.some((l) => /出站 JP，5 连接数/.test(l))).toBe(true);
    expect(labels.every((l) => !/\d+ B[，,。\s]/.test(l))).toBe(true);
  });

  it('单位换成字节后，措辞跟着换 —— 措辞没有第二个来源', () => {
    render(Sankey, { rows: byteRows, colorOf });
    expect(screen.getByText(/流带粗细 = 字节数/)).toBeInTheDocument();
    expect(screen.queryByText(/不是吞吐量/)).toBeNull();
  });

  it('图的朗读摘要说清了粗细代表什么', () => {
    const { container } = render(Sankey, { rows: connRows, colorOf });
    const svg = container.querySelector('svg[role="img"]');
    expect(svg.getAttribute('aria-label')).toMatch(/流带粗细代表连接数/);
    expect(svg.getAttribute('aria-label')).toMatch(/表视图/);
  });
});

describe('单位冲突时不画图', () => {
  const mixed = [
    { site: 'a.com', rule: 'r1', outbound: 'JP', weight: 3, weightUnit: WEIGHT_CONNS },
    { site: 'b.com', rule: 'r2', outbound: 'SG', weight: 1024, weightUnit: WEIGHT_BYTES },
  ];

  it('混合单位时整幅图消失，换成一段说明', () => {
    const { container } = render(Sankey, { rows: mixed, colorOf });
    // 一半字节一半连接数的桑基图没有含义，画出来只会误导
    expect(container.querySelector('svg')).toBeNull();
    expect(screen.getByRole('alert')).toBeInTheDocument();
  });

  it('说明讲清成因，而不是一句「出错了」', () => {
    render(Sankey, { rows: mixed, colorOf });
    const t = screen.getByRole('alert').textContent;
    expect(t).toMatch(/字节/);
    expect(t).toMatch(/连接数/);
    expect(t).toMatch(/不作绘制/);
  });

  it('aggregate 的 mergeUnit 传下 null 时同样拦住', () => {
    // prepareFlows 折叠两条单位不同的行时，weightUnit 会变成 null
    const merged = [{ site: 'x', rule: 'r', outbound: 'JP', weight: 5, weightUnit: null }];
    const { container } = render(Sankey, { rows: merged, colorOf });
    expect(container.querySelector('svg')).toBeNull();
    expect(screen.getByRole('alert')).toBeInTheDocument();
  });
});

describe('几何与着色', () => {
  it('只有出站节点满色，左中两列中性灰', () => {
    const { container } = render(Sankey, { rows: connRows, colorOf });
    const rects = [...container.querySelectorAll('rect.node')];
    const colored = rects.filter((r) => r.getAttribute('fill') === '#5b8ff9');
    // 两个出站节点：JP 与 DIRECT
    expect(colored).toHaveLength(2);
    for (const r of rects) {
      if (r.getAttribute('fill') !== '#5b8ff9') {
        expect(r.getAttribute('fill')).toMatch(/rgba\(255,255,255/);
      }
    }
  });

  it('中文出站名的渐变 id 不含 % —— 含 % 时流带静默透明', () => {
    const cn = connRows.map((r) => ({ ...r, outbound: '日本节点' }));
    const { container } = render(Sankey, { rows: cn, colorOf });
    for (const p of container.querySelectorAll('path')) {
      expect(p.getAttribute('stroke')).not.toMatch(/%/);
    }
    const ids = [...container.querySelectorAll('linearGradient')].map((g) => g.id);
    expect(ids.length).toBeGreaterThan(0);
    for (const id of ids) expect(id).not.toMatch(/%/);
  });

  it('没有 NaN 进入 SVG 属性 —— NaN 不报错，只是什么都不画', () => {
    const { container } = render(Sankey, { rows: connRows, colorOf });
    for (const p of container.querySelectorAll('path')) {
      expect(p.getAttribute('d')).not.toMatch(/NaN/);
      expect(p.getAttribute('stroke-width')).not.toMatch(/NaN/);
    }
    for (const r of container.querySelectorAll('rect')) {
      for (const a of ['x', 'y', 'width', 'height']) {
        expect(r.getAttribute(a)).not.toMatch(/NaN/);
      }
    }
  });

  it('每个节点都可 Tab 到达并有可读的名字', () => {
    const { container } = render(Sankey, { rows: connRows, colorOf });
    const rects = [...container.querySelectorAll('rect.node')];
    expect(rects.length).toBe(7); // 3 站点 + 2 规则 + 2 出站
    for (const r of rects) {
      expect(r.getAttribute('tabindex')).toBe('0');
      expect(r.getAttribute('aria-label')).toBeTruthy();
    }
  });

  it('空数据不残留坐标骨架', () => {
    const { container } = render(Sankey, { rows: [], colorOf });
    expect(container.querySelector('svg')).toBeNull();
    expect(container.querySelector('figcaption')).toBeNull();
  });
});
