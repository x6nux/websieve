// emitter 的 IPC 通路判据测试。
//
// emitter.js 是个 IIFE，不导出任何东西，所以这里用「读源码 + new Function
// 重新执行」的方式加载：每次都是全新的一份，绕开 `if (window.__wsieve) return`
// 那道重复注入防护。
//
// `?raw` 是 Vite 原生的「按字符串读进来」。不用 fs.readFileSync —— jsdom
// 环境下 import.meta.url 不是 file: scheme，它会直接抛
// `TypeError: The URL must be of scheme file`。
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import SRC from './emitter.js?raw';

/** 用指定 origin 加载一份全新的 emitter，返回它的导出对象。 */
function loadEmitter(protocol, hostname) {
  delete window.__wsieve;
  delete window.__wsieveTune;
  vi.stubGlobal('location', { protocol, hostname, origin: `${protocol}//${hostname}` });
  // emitter 启动时就会起心跳并 invoke，缺了这个会同步抛 TypeError
  // （`.catch` 接不住同步异常）。
  window.__TAURI__ = { core: { invoke: vi.fn(() => Promise.resolve()) } };
  new Function(SRC)();
  return window.__wsieve;
}

beforeEach(() => {
  // emitter 里有 setInterval 心跳，不接管定时器会漏到别的测试里。
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  delete window.__wsieve;
  delete window.__wsieveTune;
  delete window.__TAURI__;
});

describe('IPC 通路判据', () => {
  it('http 承载页走 raw 快路径', () => {
    expect(loadEmitter('http:', '127.0.0.1').raw).toBe(true);
  });

  it('https 承载页必须退回 base64', () => {
    // 2026-09-09 那次故障的回归测试：判成能走 raw 却发不出去，
    // 表现为心跳正常、传输永久挂起、零错误日志。
    expect(loadEmitter('https:', 'a.example').raw).toBe(false);
  });

  it('判据看的是 scheme 而不是 hostname', () => {
    // 这两格正是旧判据会判反的：非回环的 http 能走 raw，回环的 https 不能。
    // 实测见 scripts/spike-ipc-origin.sh。
    expect(loadEmitter('http:', 'localtest.me').raw).toBe(true);
    expect(loadEmitter('https:', '127.0.0.1').raw).toBe(false);
  });

  it('__wsieveTune.raw 仍可显式覆盖（排障用）', () => {
    delete window.__wsieve;
    vi.stubGlobal('location', { protocol: 'https:', hostname: 'a.example' });
    window.__TAURI__ = { core: { invoke: vi.fn(() => Promise.resolve()) } };
    window.__wsieveTune = { raw: 1 };
    new Function(SRC)();
    expect(window.__wsieve.raw).toBe(true);
  });
});
