import { describe, it, expect } from 'vitest';
import { bytes, count, pct, ms } from './format.js';

describe('bytes', () => {
  it('按二进制进位并保持三位有效数字', () => {
    expect(bytes(0)).toBe('0 B');
    expect(bytes(999)).toBe('999 B');
    expect(bytes(1024)).toBe('1.00 KB');
    expect(bytes(1536)).toBe('1.50 KB');
    expect(bytes(1024 * 1024)).toBe('1.00 MB');
    expect(bytes(748 * 1024 * 1024)).toBe('748 MB');
    expect(bytes(2.41 * 1024 ** 3)).toBe('2.41 GB');
  });

  it('大数不退化成科学计数法', () => {
    expect(bytes(1024 ** 5)).not.toMatch(/e\+/);
  });

  it('非法输入返回占位符而不是 NaN', () => {
    expect(bytes(NaN)).toBe('—');
    expect(bytes(-1)).toBe('—');
    expect(bytes(undefined)).toBe('—');
  });
});

describe('count', () => {
  it('加千分位 —— 命中数要能一眼看出量级', () => {
    expect(count(0)).toBe('0');
    expect(count(999)).toBe('999');
    expect(count(88120)).toBe('88,120');
  });
  it('非法输入返回占位符', () => {
    expect(count(NaN)).toBe('—');
  });
});

describe('pct', () => {
  it('整数百分比', () => {
    expect(pct(0.62)).toBe('62%');
    expect(pct(0)).toBe('0%');
    expect(pct(1)).toBe('100%');
  });
  it('极小占比不显示成 0% —— 那会让用户以为没有流量', () => {
    expect(pct(0.0004)).toBe('<1%');
  });
  it('分母为零时返回占位符', () => {
    expect(pct(NaN)).toBe('—');
  });
});

describe('ms', () => {
  it('延迟带单位', () => {
    expect(ms(38)).toBe('38 ms');
    expect(ms(1240)).toBe('1.24 s');
  });
  it('未测得时是占位符而非 0 —— 0ms 会被误读成「极快」', () => {
    expect(ms(null)).toBe('—');
    expect(ms(undefined)).toBe('—');
  });
});
