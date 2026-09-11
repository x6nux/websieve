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

// 上面那组只钉住了判据（raw 该是 true 还是 false），没钉住后果：
// 真正与 Rust 侧 decode_body 构成契约的是「invoke 实际收到的 body 长什么样」。
// 判据算对了、但 sendFrame 没跟着切分支，一样会复现 2026-09-09 那次故障
// （心跳正常、传输永久挂起、零错误日志）——这组测试就是防这个。
//
// 触发点选 `window.__wsieve.post`：它是 emitter 对外暴露的三个函数之一，
// 内部会 fetch 再回帧调用 invoke。fetch 用 vi.stubGlobal 挡掉，不碰真实网络。
describe('IPC 通路契约：invoke 实际收到的 body 形状', () => {
  it('RAW=true 时 invoke 收到 Uint8Array（custom protocol 快路径）', async () => {
    var emitter = loadEmitter('http:', '127.0.0.1');
    expect(emitter.raw).toBe(true);
    vi.stubGlobal('fetch', vi.fn(() => Promise.resolve({
      status: 200,
      arrayBuffer: () => Promise.resolve(new Uint8Array([9, 8, 7]).buffer),
    })));
    await emitter.post(1, '/x', btoa('abc'));
    var call = window.__TAURI__.core.invoke.mock.calls.find(
      function (c) { return c[0] === 'wsieve_raw_post'; }
    );
    expect(call[1]).toBeInstanceOf(Uint8Array);
  });

  it('RAW=false 时 invoke 收到 { f: string }（base64 回退路径）', async () => {
    var emitter = loadEmitter('https:', 'a.example');
    expect(emitter.raw).toBe(false);
    vi.stubGlobal('fetch', vi.fn(() => Promise.resolve({
      status: 200,
      arrayBuffer: () => Promise.resolve(new Uint8Array([9, 8, 7]).buffer),
    })));
    await emitter.post(1, '/x', btoa('abc'));
    var call = window.__TAURI__.core.invoke.mock.calls.find(
      function (c) { return c[0] === 'wsieve_raw_post'; }
    );
    expect(call[1]).not.toBeInstanceOf(Uint8Array);
    expect(typeof call[1].f).toBe('string');
  });

  it('暴露 proto()，读不到时返回空串而不是 undefined', () => {
    // 协议由 WebKit 单方面决定，我们只能观测。观测不到时必须给出可区分的
    // 空串——"读不到"（多半是 Timing-Allow-Origin 没生效）与"未知协议"是
    // 两回事，undefined 会把这个区别抹掉。
    var emitter = loadEmitter('http:', '127.0.0.1');
    expect(typeof emitter.proto).toBe('function');
    expect(emitter.proto()).toBe('');
  });
});
