import { describe, it, expect } from 'vitest';
import { tracePath } from './trace.js';

//  s:yt ──A──▶ r:final ──B──▶ o:jp
//  s:ot ──E──▶ r:final
//  s:tb ──C──▶ r:cn    ──D──▶ o:direct
const links = [
  { key: 'A', sourceId: 's:yt', targetId: 'r:final' },
  { key: 'B', sourceId: 'r:final', targetId: 'o:jp' },
  { key: 'C', sourceId: 's:tb', targetId: 'r:cn' },
  { key: 'D', sourceId: 'r:cn', targetId: 'o:direct' },
  { key: 'E', sourceId: 's:ot', targetId: 'r:final' },
];

describe('路径闭包', () => {
  it('hover 站点点亮两跳直到出站（不是只点亮第一跳）', () => {
    expect([...tracePath(links, 's:yt')].sort()).toEqual(['A', 'B']);
  });

  it('hover 规则同时点亮上下游', () => {
    expect([...tracePath(links, 'r:final')].sort()).toEqual(['A', 'B', 'E']);
  });

  it('hover 出站回溯到全部来源站点', () => {
    expect([...tracePath(links, 'o:jp')].sort()).toEqual(['A', 'B', 'E']);
  });

  it('不串到无关分支', () => {
    const hot = tracePath(links, 's:yt');
    expect(hot.has('C')).toBe(false);
    expect(hot.has('D')).toBe(false);
  });

  it('无 hover 时返回 null（表示「全部原样」，不是「全部变暗」）', () => {
    expect(tracePath(links, null)).toBeNull();
    expect(tracePath(links, undefined)).toBeNull();
  });

  it('孤立节点返回空集合而非报错', () => {
    expect(tracePath(links, 'o:nobody').size).toBe(0);
  });

  it('图中存在环时不死循环', () => {
    const cyc = [
      { key: 'X', sourceId: 'a', targetId: 'b' },
      { key: 'Y', sourceId: 'b', targetId: 'a' },
    ];
    expect([...tracePath(cyc, 'a')].sort()).toEqual(['X', 'Y']);
  });

  it('空图不报错', () => {
    expect(tracePath([], 's:yt').size).toBe(0);
  });
});
