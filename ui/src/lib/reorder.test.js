import { describe, it, expect } from 'vitest';
import { move, keyboardMove, positionAnnouncement } from './reorder.js';

const L = ['a', 'b', 'c', 'd'];

describe('move', () => {
  it('下移', () => expect(move(L, 0, 2)).toEqual(['b', 'c', 'a', 'd']));
  it('上移', () => expect(move(L, 3, 1)).toEqual(['a', 'd', 'b', 'c']));
  it('相邻交换', () => expect(move(L, 1, 2)).toEqual(['a', 'c', 'b', 'd']));
  it('原地不动返回原数组引用（省一次无谓渲染）', () => expect(move(L, 1, 1)).toBe(L));

  it('越界不抛错也不损坏数据', () => {
    expect(move(L, -1, 2)).toBe(L);
    expect(move(L, 0, 99)).toBe(L);
    expect(move(L, 99, 0)).toBe(L);
  });

  it('非整数下标被拒 —— 浮点下标进 splice 会静默截断到别的位置', () => {
    expect(move(L, 1.5, 2)).toBe(L);
    expect(move(L, 1, NaN)).toBe(L);
  });

  it('不改写输入数组', () => {
    const copy = [...L];
    move(L, 0, 3);
    expect(L).toEqual(copy);
  });

  it('元素不丢不重', () => {
    const out = move(L, 0, 3);
    expect(out).toHaveLength(L.length);
    expect([...out].sort()).toEqual([...L].sort());
  });

  it('空表与单元素表不抛错', () => {
    expect(move([], 0, 0)).toEqual([]);
    expect(move(['x'], 0, 0)).toEqual(['x']);
    expect(move(['x'], 0, 1)).toEqual(['x']);
  });
});

describe('keyboardMove —— 拖拽的键盘等价物', () => {
  it('Alt+↑ 上移一位并跟随焦点', () => {
    const r = keyboardMove(L, 2, 'ArrowUp');
    expect(r.list).toEqual(['a', 'c', 'b', 'd']);
    expect(r.index).toBe(1);
  });

  it('Alt+↓ 下移一位并跟随焦点', () => {
    const r = keyboardMove(L, 1, 'ArrowDown');
    expect(r.list).toEqual(['a', 'c', 'b', 'd']);
    expect(r.index).toBe(2);
  });

  it('首项再上移不越界', () => {
    const r = keyboardMove(L, 0, 'ArrowUp');
    expect(r.index).toBe(0);
    expect(r.list).toEqual(L);
  });

  it('末项再下移不越界', () => {
    const r = keyboardMove(L, 3, 'ArrowDown');
    expect(r.index).toBe(3);
    expect(r.list).toEqual(L);
  });

  it('其他按键原样返回', () => {
    const r = keyboardMove(L, 1, 'Enter');
    expect(r.list).toBe(L);
    expect(r.index).toBe(1);
  });

  it('与 move 等价 —— 键盘路径与拖拽路径必须是同一个结果', () => {
    // 这条守的是「键盘能做的事和鼠标一模一样」：两条路径各算各的
    // 迟早会分叉，而分叉的那一天没人会发现，因为没人同时用两种方式排一次序。
    expect(keyboardMove(L, 2, 'ArrowUp').list).toEqual(move(L, 2, 1));
    expect(keyboardMove(L, 1, 'ArrowDown').list).toEqual(move(L, 1, 2));
  });
});

describe('positionAnnouncement —— 键盘排序后的位置播报', () => {
  it('报出名次与总数，不只说「已移动」', () => {
    const t = positionAnnouncement(2, 5, 'geosite cn');
    expect(t).toMatch(/第 3 条/);
    expect(t).toMatch(/共 5 条/);
    expect(t).toMatch(/geosite cn/);
  });

  it('移到首位时说明它会被最先匹配 —— 顺序即语义', () => {
    expect(positionAnnouncement(0, 5, 'x')).toMatch(/最先匹配/);
  });

  it('移到末位时说明它在最后', () => {
    expect(positionAnnouncement(4, 5, 'x')).toMatch(/最后/);
  });

  it('中间位置提示首命中即返回', () => {
    expect(positionAnnouncement(2, 5, 'x')).toMatch(/首命中即返回/);
  });

  it('不带标签也能播报', () => {
    expect(positionAnnouncement(1, 3)).toMatch(/第 2 条/);
  });

  it('非法输入给空串而不是「第 NaN 条」', () => {
    expect(positionAnnouncement(0, 0, 'x')).toBe('');
    expect(positionAnnouncement(-1, 3, 'x')).toBe('');
    expect(positionAnnouncement(5, 3, 'x')).toBe('');
    expect(positionAnnouncement(NaN, 3, 'x')).toBe('');
  });
});
