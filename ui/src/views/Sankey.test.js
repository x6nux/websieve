import { describe, it, expect, beforeAll } from 'vitest';
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
    const svg = container.querySelector('svg[role="group"]');
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

/**
 * 阶段 5 完成标准里的两条，此前只有实现没有测试。
 *
 * 「代码里写着 prefersReducedMotion.current」与「reduced-motion 下真的
 * 跳变」不是一回事 —— 本项目已经反复抓到过这类靠肉眼验收的条目
 * （托盘菜单从未挂上 builder，而清单上写着「五项俱全」）。
 */
describe('实时更新的两条纪律（完成标准的自动化）', () => {
  const grow = connRows.map((r) => ({ ...r, weight: r.weight * 4, conns: r.conns * 4 }));
  /** 各列内的节点 y 序，用来判断「顺序有没有变」 */
  const orderOf = (container) =>
    [...container.querySelectorAll('rect.node')]
      .map((r) => ({
        x: Number(r.getAttribute('x')),
        y: Number(r.getAttribute('y')),
        label: r.getAttribute('aria-label'),
      }))
      .sort((a, b) => a.x - b.x || a.y - b.y)
      .map((n) => n.label.split('，')[0]);

  it('数据连续变化时节点顺序不变，只有粗细动（§11.6）', async () => {
    const { container, rerender } = render(Sankey, { rows: connRows, colorOf });
    const before = orderOf(container);
    expect(before.length).toBe(7);

    // 同一批流带、量翻四倍：这正是 1s 一次的真实更新形态
    await rerender({ rows: grow, colorOf });
    expect(orderOf(container)).toEqual(before);
  });

  it('上游把行重排了，图上的节点位次**依然**不动', async () => {
    /*
     * 这一条才是 applyFrozenOrder 存在的理由，上面两条都不是。
     *
     * 布局侧的 nodeSort(null) 已经保证「列内顺序 = 输入数组顺序」，
     * 而 toGraph 按首次出现建节点 —— 所以只要行的**顺序**没变，
     * 图就不会重排，跟冻不冻结无关。等比放大与名次互换都属于这种情况，
     * 拿它们去测冻结，会得到一个删掉 frozen 也照样通过的断言。
     *
     * 真正会让位次跳动的是**行本身被重排**：aggregate.js 的 Top N 按量
     * 排序，量一变，行的先后就变了 —— 而这正是 1s 一次的更新里必然发生的事。
     * 冻结要挡住的就是它。
     */
    const { container, rerender } = render(Sankey, { rows: connRows, colorOf });
    const before = orderOf(container);
    // 同一批流，顺序倒过来（模拟 Top N 重新排序后的输出）
    await rerender({ rows: [...connRows].reverse(), colorOf });
    expect(orderOf(container)).toEqual(before);
  });

  it('新节点出现时追加在末尾，已有节点不被重排', async () => {
    const { container, rerender } = render(Sankey, { rows: connRows, colorOf });
    const before = orderOf(container);
    const withNew = [
      ...connRows,
      { site: 'z.com', rule: 'final *', outbound: 'JP', bytes: 0, conns: 1, weight: 1, weightUnit: WEIGHT_CONNS },
    ];
    await rerender({ rows: withNew, colorOf });
    const after = orderOf(container);
    // 已有的七个还在，且相对次序没变
    expect(after.filter((x) => before.includes(x))).toEqual(before);
    expect(after.length).toBe(8);
  });

  /**
   * 比例变化，而不是整体放大。
   *
   * d3-sankey 会把布局**归一化到给定高度**，因此把所有流量同时乘 4，
   * 算出来的宽度一模一样 —— 第一版就是这么写的，于是「跳没跳变」这件事
   * 根本没被测到。要让宽度真的动，必须改变流带之间的**比例**。
   */
  const skew = [
    { ...connRows[0], conns: 30, weight: 30 },
    connRows[1],
    connRows[2],
  ];

  /**
   * skew 那组数据的**终值**宽度：直接把它当初始数据渲染一次即得
   * （首帧没有可插值的前值，必然是终值）。不写死数字 —— 写死的话
   * 布局参数一改，这两条测试会以一个看不懂的方式失败。
   */
  let finalWidths;
  beforeAll(() => {
    const { container, unmount } = render(Sankey, { rows: skew, colorOf });
    finalWidths = widthsOf(container);
    unmount();
    // 三条流经两跳（站点→规则、规则→出站），聚合后是 5 条流带。
    // 这一句不是装饰：不确认拿到了真实宽度，下面两条断言可能都在跟空数组比，
    // 而跟空数组比的 not.toEqual 永远通过。
    expect(finalWidths).toHaveLength(5);
    expect(Math.max(...finalWidths)).toBeGreaterThan(0);
  });

  it('reduced-motion 下直接跳变，不留插值中间态', async () => {
    // 这一条是无障碍要求（§11.6 明确写了）。前庭功能障碍的用户开着这个
    // 系统偏好，而一张持续做宽度插值的图会让他们直接无法使用界面。
    const { setMediaMatches } = await import('../test-setup.js');
    setMediaMatches((q) => q.includes('prefers-reduced-motion'));
    try {
      const { container, rerender } = render(Sankey, { rows: connRows, colorOf });
      const before = widthsOf(container);
      await rerender({ rows: skew, colorOf });
      // 跳变 = 下一帧就是终值
      expect(widthsOf(container)).toEqual(finalWidths);
      expect(widthsOf(container)).not.toEqual(before);
    } finally {
      setMediaMatches(() => false);
    }
  });

  it('未开 reduced-motion 时确实在插值 —— 上一条才有对照', async () => {
    // 没有这条对照，「跳变」那条测的可能只是「渲染同步完成」，
    // 而它在两种设置下都会通过 —— 又一个不可能失败的断言。
    const { container, rerender } = render(Sankey, { rows: connRows, colorOf });
    await rerender({ rows: skew, colorOf });
    // 插值中：此刻还没走到终值
    expect(widthsOf(container)).not.toEqual(finalWidths);
  });
});

/** 当前各流带的宽度，排序后比较（顺序由布局决定，不是这条测试关心的） */
function widthsOf(container) {
  return [...container.querySelectorAll('.links path')]
    .map((p) => Number(p.getAttribute('stroke-width')))
    .sort((a, b) => a - b);
}

