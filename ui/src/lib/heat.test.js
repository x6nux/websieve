import { describe, it, expect } from 'vitest';
import { heat, heatColor, HEAT_MAX, HEAT_MIN } from './heat.js';

describe('命中热度归一化', () => {
  it('零命中是纯透明 —— 死规则必须看得出来是死的', () => {
    expect(heat(0, 88120)).toBe(0);
  });

  it('榜首拿满值', () => {
    expect(heat(88120, 88120)).toBeCloseTo(HEAT_MAX, 6);
  });

  it('单调递增', () => {
    const v = [1, 10, 100, 1000, 10000].map((h) => heat(h, 10000));
    for (let i = 1; i < v.length; i++) expect(v[i]).toBeGreaterThan(v[i - 1]);
  });

  it('对数刻度：跨三个数量级仍分得开', () => {
    // mockup 的真实数据：211 / 1,033 / 42,663 / 88,120
    const max = 88120;
    const a = heat(211, max);
    const b = heat(1033, max);
    const c = heat(42663, max);
    expect(a).toBeGreaterThan(b * 0.5); // 不被压到零附近
    expect(b).toBeGreaterThan(a);
    expect(c).toBeGreaterThan(b);
    // 线性刻度下 211/88120 会得到 .00012，肉眼不可见。
    // 对数刻度必须显著高于它。
    expect(a).toBeGreaterThan((211 / max) * HEAT_MAX * 10);
  });

  it('最低的非零命中仍高于可见阈值', () => {
    // .014 是 mockup 里最暗的一档；低于它就等于没染色
    expect(heat(1, 88120)).toBeGreaterThanOrEqual(0);
    expect(heat(211, 88120)).toBeGreaterThan(0.008);
  });

  it('不超过上限 —— 过亮会盖过探针高亮', () => {
    expect(heat(999999, 100)).toBeLessThanOrEqual(HEAT_MAX);
  });

  it('全表只有一条规则时不除零', () => {
    expect(Number.isFinite(heat(1, 1))).toBe(true);
    expect(Number.isFinite(heat(5, 5))).toBe(true);
  });

  it('非法输入不产出 NaN —— NaN 进 CSS 会让整行没有背景', () => {
    expect(heat(NaN, 100)).toBe(0);
    expect(heat(10, NaN)).toBe(0);
    expect(heat(undefined, 100)).toBe(0);
  });

  // ── 以下两条是本实现相对计划的加固，理由写在 heat.js 的 HEAT_MIN 注释里 ──

  it('命中 1 次不与零命中同色 —— 「死了」和「很冷」是两回事', () => {
    // 纯 Math.log 下 log(1)=0，这一档会被压回透明，从而与死规则不可区分。
    expect(heat(1, 88120)).toBeGreaterThan(0);
    expect(heat(1, 88120)).toBeGreaterThanOrEqual(HEAT_MIN);
  });

  it('全部非零命中都落在 §11.4 定死的 .014 ~ .052 区间内', () => {
    for (const h of [1, 2, 37, 211, 1033, 42663, 88120]) {
      const v = heat(h, 88120);
      expect(v).toBeGreaterThanOrEqual(HEAT_MIN);
      expect(v).toBeLessThanOrEqual(HEAT_MAX);
    }
  });
});

describe('heatColor', () => {
  it('零命中给 transparent，不给 rgba(...,0)', () => {
    expect(heatColor(0, 100)).toBe('transparent');
    expect(heatColor(undefined, 100)).toBe('transparent');
  });

  it('非零命中给可直接进 style 的 rgba', () => {
    expect(heatColor(88120, 88120)).toMatch(/^rgba\(255,255,255,0\.05/);
  });

  it('输出里不含 NaN —— NaN 进 CSS 会让整行悄悄失去背景', () => {
    for (const args of [[NaN, 100], [10, NaN], [null, 100], [10, 0]]) {
      expect(heatColor(...args)).not.toMatch(/NaN/);
    }
  });
});
