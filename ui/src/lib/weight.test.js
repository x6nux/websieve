import { describe, it, expect } from 'vitest';
import {
  unitNoun,
  widthLegend,
  formatWeight,
  weightSummary,
  rowsUnit,
  UNIT_CONFLICT_MSG,
  WEIGHT_BYTES,
  WEIGHT_CONNS,
} from './weight.js';

describe('措辞随单位走', () => {
  it('连接数的图例把「不是吞吐量」明说出来', () => {
    const t = widthLegend(WEIGHT_CONNS);
    expect(t).toMatch(/连接数/);
    // 桑基图这个形态本身在暗示流量，只说「= 连接数」仍会被读成吞吐量的近似
    expect(t).toMatch(/不是吞吐量/);
  });

  it('字节的图例说字节，且不出现「连接数」', () => {
    const t = widthLegend(WEIGHT_BYTES);
    expect(t).toMatch(/字节/);
    expect(t).not.toMatch(/连接数/);
  });

  it('单位未知时一个字也不说 —— 不挑一个看起来对的', () => {
    expect(widthLegend(null)).toBeNull();
    expect(widthLegend(undefined)).toBeNull();
    expect(unitNoun(null)).toBeNull();
  });

  it('量的名词', () => {
    expect(unitNoun(WEIGHT_BYTES)).toBe('字节');
    expect(unitNoun(WEIGHT_CONNS)).toBe('连接数');
  });
});

describe('数值格式随单位走', () => {
  it('连接数走千分位而非字节进位 —— 3 条连接不能显示成 "3 B"', () => {
    expect(formatWeight(3, WEIGHT_CONNS)).toBe('3');
    expect(formatWeight(1500, WEIGHT_CONNS)).toBe('1,500');
  });

  it('字节走二进制进位', () => {
    expect(formatWeight(2048, WEIGHT_BYTES)).toBe('2.00 KB');
  });

  it('单位未知给占位符', () => {
    expect(formatWeight(42, null)).toBe('—');
  });

  it('合计读数带量词，连接数不会被误读成体积', () => {
    expect(weightSummary(12, WEIGHT_CONNS)).toBe('12 条连接');
    expect(weightSummary(2048, WEIGHT_BYTES)).toBe('2.00 KB');
    expect(weightSummary(1, null)).toBe('—');
  });
});

describe('rowsUnit', () => {
  it('全体一致时给出该单位', () => {
    expect(rowsUnit([{ weightUnit: WEIGHT_CONNS }, { weightUnit: WEIGHT_CONNS }])).toBe(WEIGHT_CONNS);
  });

  it('混了两种单位时返回 null —— 混合的图没有含义', () => {
    expect(rowsUnit([{ weightUnit: WEIGHT_CONNS }, { weightUnit: WEIGHT_BYTES }])).toBeNull();
  });

  it('aggregate.mergeUnit 传下来的 null 同样是冲突', () => {
    expect(rowsUnit([{ weightUnit: null }, { weightUnit: WEIGHT_BYTES }])).toBe(WEIGHT_BYTES);
    expect(rowsUnit([{ weightUnit: null }])).toBeNull();
  });

  it('没有任何一行声明单位时返回 null，不猜成字节', () => {
    expect(rowsUnit([{ site: 'a' }, { site: 'b' }])).toBeNull();
  });

  it('空输入返回 null', () => {
    expect(rowsUnit([])).toBeNull();
    expect(rowsUnit(null)).toBeNull();
  });
});

describe('冲突文案', () => {
  it('说清了成因与后果，而不是一句「出错了」', () => {
    expect(UNIT_CONFLICT_MSG).toMatch(/字节/);
    expect(UNIT_CONFLICT_MSG).toMatch(/连接数/);
    expect(UNIT_CONFLICT_MSG).toMatch(/不作绘制|没有意义/);
  });
});
