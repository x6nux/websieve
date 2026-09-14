import { describe, it, expect } from 'vitest';
import { topN, prepareFlows, OTHER_SITE, OTHER_RULE } from './aggregate.js';
import { WEIGHT_BYTES, WEIGHT_CONNS } from './flows.js';

const flows = (...specs) =>
  specs.map(([site, rule, outbound, b, c]) => ({
    site, rule, outbound, bytes: b, conns: c ?? 1,
  }));

describe('topN', () => {
  it('按值降序取前 N', () => {
    const m = new Map([['a', 10], ['b', 30], ['c', 20]]);
    expect(topN(m, 2)).toEqual(new Set(['b', 'c']));
  });
  it('总数不超过 N 时全留', () => {
    const m = new Map([['a', 10], ['b', 30]]);
    expect(topN(m, 5)).toEqual(new Set(['a', 'b']));
  });
  it('并列时结果稳定（不随 Map 插入顺序抖动）', () => {
    const m1 = new Map([['a', 10], ['b', 10], ['c', 10]]);
    const m2 = new Map([['c', 10], ['b', 10], ['a', 10]]);
    expect([...topN(m1, 2)].sort()).toEqual([...topN(m2, 2)].sort());
  });
});

describe('prepareFlows', () => {
  it('超出 Top N 的站点被压成「其他」', () => {
    const rows = prepareFlows(
      flows(['a', 'r', 'O', 100], ['b', 'r', 'O', 90], ['c', 'r', 'O', 5], ['d', 'r', 'O', 3]),
      { sites: 2, rules: 8 }
    );
    const sites = new Set(rows.map((r) => r.site));
    expect(sites.has('a')).toBe(true);
    expect(sites.has('b')).toBe(true);
    expect(sites.has('c')).toBe(false);
    expect([...sites].some((s) => s.startsWith(OTHER_SITE))).toBe(true);
  });

  it('「其他」的字节数是被折叠项之和', () => {
    const rows = prepareFlows(
      flows(['a', 'r', 'O', 100], ['c', 'r', 'O', 5], ['d', 'r', 'O', 3]),
      { sites: 1, rules: 8 }
    );
    const other = rows.find((r) => r.site.startsWith(OTHER_SITE));
    expect(other.bytes).toBe(8);
    expect(other.conns).toBe(2);
  });

  it('「其他」的标签写明被折叠了几个', () => {
    const rows = prepareFlows(
      flows(['a', 'r', 'O', 100], ['c', 'r', 'O', 5], ['d', 'r', 'O', 3]),
      { sites: 1, rules: 8 }
    );
    expect(rows.find((r) => r.site.startsWith(OTHER_SITE)).site).toBe('其他 2 个站点');
  });

  it('规则也按自己的 Top N 折叠', () => {
    const rows = prepareFlows(
      flows(['a', 'r1', 'O', 100], ['a', 'r2', 'O', 50], ['a', 'r3', 'O', 1]),
      { sites: 12, rules: 2 }
    );
    expect(rows.some((r) => r.rule.startsWith(OTHER_RULE))).toBe(true);
  });

  it('折叠后同 (站点,规则,出站) 的行会合并，不留重复', () => {
    const rows = prepareFlows(
      flows(['x', 'r', 'O', 5], ['y', 'r', 'O', 3], ['z', 'r', 'O', 2]),
      { sites: 0, rules: 8 }
    );
    expect(rows).toHaveLength(1);
    expect(rows[0].bytes).toBe(10);
  });

  it('出站**绝不**折叠 —— 它是颜色语义的载体', () => {
    const rows = prepareFlows(
      flows(['a', 'r', 'O1', 100], ['b', 'r', 'O2', 1], ['c', 'r', 'O3', 1], ['d', 'r', 'O4', 1]),
      { sites: 12, rules: 8 }
    );
    expect(new Set(rows.map((r) => r.outbound)).size).toBe(4);
  });

  it('总字节守恒 —— 折叠不能凭空吞掉流量', () => {
    const src = flows(['a', 'r1', 'O', 100], ['b', 'r2', 'O', 50], ['c', 'r3', 'O', 7],
                      ['d', 'r4', 'O', 3], ['e', 'r5', 'O', 1]);
    const before = src.reduce((s, r) => s + r.bytes, 0);
    const after = prepareFlows(src, { sites: 2, rules: 2 }).reduce((s, r) => s + r.bytes, 0);
    expect(after).toBe(before);
  });

  it('空输入返回空数组', () => {
    expect(prepareFlows([], { sites: 12, rules: 8 })).toEqual([]);
  });
});

// ── 与 flows.js 的 weight/weightUnit 契约对接 ──────────────────
//
// 折叠是画图前的最后一道数据变换。若它在这里把单位丢掉，或者按一个
// 与绘图量不同的量排 Top N，前面在 flows.js 里守住的诚实就白守了。
describe('weight 与单位穿过折叠', () => {
  const withWeight = (unit) => (...specs) =>
    specs.map(([site, rule, outbound, w, c]) => ({
      site, rule, outbound,
      bytes: unit === WEIGHT_BYTES ? w : 0,
      conns: c ?? w,
      weight: w,
      weightUnit: unit,
    }));

  it('weightUnit 原样透传 —— 折叠不改变数值的含义', () => {
    const rows = prepareFlows(
      withWeight(WEIGHT_CONNS)(['a', 'r', 'O', 5], ['b', 'r', 'O', 3]),
      { sites: 12, rules: 8 }
    );
    for (const r of rows) expect(r.weightUnit).toBe(WEIGHT_CONNS);
  });

  it('被折进「其他」的行同样保留单位', () => {
    const rows = prepareFlows(
      withWeight(WEIGHT_CONNS)(['a', 'r', 'O', 9], ['c', 'r', 'O', 2], ['d', 'r', 'O', 1]),
      { sites: 1, rules: 8 }
    );
    const other = rows.find((r) => r.site.startsWith(OTHER_SITE));
    expect(other.weightUnit).toBe(WEIGHT_CONNS);
    expect(other.weight).toBe(3);
  });

  it('总 weight 守恒', () => {
    const src = withWeight(WEIGHT_CONNS)(
      ['a', 'r1', 'O', 9], ['b', 'r2', 'O', 4], ['c', 'r3', 'O', 2], ['d', 'r4', 'O', 1]
    );
    const before = src.reduce((s, r) => s + r.weight, 0);
    const after = prepareFlows(src, { sites: 2, rules: 2 }).reduce((s, r) => s + r.weight, 0);
    expect(after).toBe(before);
  });

  it('Top N 按 weight 排名，而不是按恒为 0 的 bytes', () => {
    // 这是无字节数据时的真实形态：bytes 全 0，连接数才是有信息量的那一列。
    // 若按 bytes 排名，全并列 —— 留下谁就只看键名字典序，图上会随机
    // 砍掉流量最大的站点，而用户完全看不出为什么。
    const rows = prepareFlows(
      withWeight(WEIGHT_CONNS)(['zzz', 'r', 'O', 99], ['aaa', 'r', 'O', 1]),
      { sites: 1, rules: 8 }
    );
    expect(rows.some((r) => r.site === 'zzz')).toBe(true);
  });

  it('没有 weight 字段时退回按 bytes —— 阶段 2 之前的旧形态仍可用', () => {
    const rows = prepareFlows(
      flows(['big', 'r', 'O', 100], ['small', 'r', 'O', 1]),
      { sites: 1, rules: 8 }
    );
    expect(rows.some((r) => r.site === 'big')).toBe(true);
  });

  it('混入无单位的行时不静默编造单位', () => {
    // 两种来源混在一起本身就是 bug，宁可让它显形也不要猜一个单位出来。
    const rows = prepareFlows(
      [
        { site: 'a', rule: 'r', outbound: 'O', bytes: 0, conns: 3, weight: 3, weightUnit: WEIGHT_CONNS },
        { site: 'a', rule: 'r', outbound: 'O', bytes: 5, conns: 1, weight: 5, weightUnit: WEIGHT_BYTES },
      ],
      { sites: 12, rules: 8 }
    );
    expect(rows[0].weightUnit).toBe(null);
  });
});
