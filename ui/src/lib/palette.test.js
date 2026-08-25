import { describe, it, expect } from 'vitest';
import { makePalette, PALETTE_SLOTS } from './palette.js';

describe('出站色码分配', () => {
  it('同一名字永远同色 —— 「颜色 = 去哪」的前提', () => {
    const c = makePalette(['JP', 'SG', 'DE']);
    expect(c('JP')).toBe(c('JP'));
  });

  it('不同名字不同色（在色码数以内）', () => {
    const c = makePalette(['JP', 'SG', 'DE']);
    expect(new Set([c('JP'), c('SG'), c('DE')]).size).toBe(3);
  });

  it('DIRECT 与 REJECT 用固定的状态色，不占出站色码', () => {
    const c = makePalette(['JP']);
    expect(c('DIRECT')).toBe('var(--state-direct)');
    expect(c('REJECT')).toBe('var(--state-fail)');
  });

  it('顺序无关 —— 出站列表重排不该让全屏换色', () => {
    const a = makePalette(['JP', 'SG', 'DE']);
    const b = makePalette(['DE', 'SG', 'JP']);
    expect(a('SG')).toBe(b('SG'));
  });

  it('未知名字有兜底色而非 undefined', () => {
    const c = makePalette(['JP']);
    expect(typeof c('从未见过')).toBe('string');
    expect(c('从未见过')).toBeTruthy();
  });

  it('8 个以内互不重色', () => {
    const names = Array.from({ length: 8 }, (_, i) => `out-${i}`);
    const c = makePalette(names);
    expect(new Set(names.map(c)).size).toBe(8);
  });

  it('超过色码数时循环复用而非产出无效值', () => {
    const names = Array.from({ length: 20 }, (_, i) => `out-${i}`);
    const c = makePalette(names);
    for (const n of names) expect(c(n)).toMatch(/^var\(--outbound-[1-8]\)$/);
  });

  it('轮转基数与 tokens.css 的 --outbound-1..8 对齐', () => {
    // 取模基数若与令牌数不符，会生成 var(--outbound-9) 这种**未定义**的变量名。
    // CSS 变量未定义时 background 静默失效 —— 色块变透明，而且不报任何错。
    expect(PALETTE_SLOTS).toBe(8);
  });

  it('兜底色也只落在既有的 8 个槽位里', () => {
    const c = makePalette([]);
    for (const n of ['日本节点', 'a', '', '很长很长的一个出站名字']) {
      expect(c(n)).toMatch(/^var\(--outbound-[1-8]\)$/);
    }
  });

  it('中文名字不塌成同一个槽 —— 哈希得真的散开', () => {
    // 出站名是用户自由输入的中文，若哈希只看 charCode 低位会大面积撞车。
    const names = ['日本节点', '新加坡', '德国备用', '香港', '美西', '英国'];
    const c = makePalette([]); // 走兜底哈希这条路
    expect(new Set(names.map(c)).size).toBeGreaterThanOrEqual(4);
  });

  it('重名只占一个槽 —— 配置里出站重名由 Rust 侧拒绝，这里不该再错位', () => {
    const c = makePalette(['JP', 'JP', 'SG']);
    expect(c('SG')).toBe(makePalette(['JP', 'SG'])('SG'));
  });

  it('列表里混进 DIRECT / REJECT 不挤占出站的槽位', () => {
    const withBuiltin = makePalette(['DIRECT', 'JP', 'REJECT', 'SG']);
    const without = makePalette(['JP', 'SG']);
    expect(withBuiltin('JP')).toBe(without('JP'));
    expect(withBuiltin('SG')).toBe(without('SG'));
  });

  it('非字符串名字不抛错也不产出 var(--outbound-NaN)', () => {
    const c = makePalette(['JP']);
    for (const n of [null, undefined, 42]) {
      expect(c(n)).toMatch(/^var\(--(outbound-[1-8]|state-\w+)\)$/);
    }
  });
});
