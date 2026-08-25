/**
 * 令牌契约测试。
 *
 * 设计令牌的唯一定义处是阶段 4 的 ui/src/tokens.css，本阶段只消费。
 * 这个测试把「阶段 5 用到的变量名」钉死：阶段 4 若改名或删除，
 * 这里立刻失败，而不是让某个视图在浏览器里静默失去样式 ——
 * CSS 变量未定义时 `color: var(--gone)` 不报错、不回退，只是当作没写，
 * 后者是最难排查的一类问题。
 *
 * @vitest-environment node
 *
 * 这一行是必需的，不是随手加的优化：全局环境是 jsdom，而 jsdom 下
 * `import.meta.url` 是 http: 而非 file:，fileURLToPath 会抛
 * 「The URL must be of scheme file」。本文件只读文件、不碰 DOM，
 * 单独切回 node 环境比在测试里绕路拼路径干净。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const css = readFileSync(fileURLToPath(new URL('./tokens.css', import.meta.url)), 'utf8');

const REQUIRED = [
  '--surface-0', '--surface-1', '--surface-2',
  '--border', '--border-strong',
  '--text-1', '--text-2', '--text-3', '--text-4',
  '--state-live', '--state-warn', '--state-fail', '--state-direct',
  '--heat-min', '--heat-max',
  '--font-sans', '--font-mono',
  '--fs-11', '--fs-12', '--fs-13', '--fs-14', '--fs-18', '--fs-22',
  '--fw-regular', '--fw-medium', '--fw-semibold',
  '--space-1', '--space-2', '--space-3', '--space-4',
  '--row-rule', '--row-outbound',
  '--shadow-overlay', '--radius',
];

describe('设计令牌契约（定义在阶段 4）', () => {
  for (const name of REQUIRED) {
    it(`${name} 存在`, () => {
      expect(css).toMatch(new RegExp(`^\\s*${name}\\s*:`, 'm'));
    });
  }

  it('出站色码有 8 个 —— 轮转的取模基数依赖它', () => {
    for (let i = 1; i <= 8; i++) {
      expect(css).toMatch(new RegExp(`^\\s*--outbound-${i}\\s*:`, 'm'));
    }
  });

  it('.sr-only 存在 —— 无障碍兜底依赖它', () => {
    expect(css).toMatch(/\.sr-only\s*\{/);
  });

  it('字体走 @fontsource 而非外部 CDN —— 代理工具不该自己联网', () => {
    expect(css).toMatch(/@fontsource/);
    expect(css).not.toMatch(/fonts\.googleapis\.com|fonts\.gstatic\.com/);
  });
});
