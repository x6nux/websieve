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

describe('服务端链路观测回传', () => {
  /** 用一个受控的 fetch 响应跑一次 post()，返回 invoke 收到的那一帧。 */
  async function postWithHeader(serverTiming) {
    delete window.__wsieve;
    vi.stubGlobal('location', {
      protocol: 'http:', hostname: '127.0.0.1', origin: 'http://127.0.0.1',
    });
    const invoke = vi.fn(() => Promise.resolve());
    window.__TAURI__ = { core: { invoke } };
    const headers = new Map();
    if (serverTiming !== null) headers.set('server-timing', serverTiming);
    vi.stubGlobal('fetch', vi.fn(async () => ({
      status: 204,
      arrayBuffer: async () => new ArrayBuffer(0),
      headers: { get: (k) => headers.get(k) ?? null },
    })));
    new Function(SRC)();
    await window.__wsieve.post(7, '/api/sync?n=1&sid=x', '');
    // 第一帧可能是心跳，取 wsieve_raw_post 那次
    const call = invoke.mock.calls.find((c) => c[0] === 'wsieve_raw_post');
    return call ? call[1] : null;
  }

  // 这 4 个字节在 POST 结果帧里本来是空的（下行分片序号只在流帧上用）。
  // 自适应流控借它回传服务端观测，省掉一次 IPC 和一条新的 Tauri 命令授权。
  function decodeObs(f) {
    return ((f[11] << 24) | (f[12] << 16) | (f[13] << 8) | f[14]) >>> 0;
  }

  it('把 Server-Timing 的三个数编进回帧头的空闲字节', async () => {
    const f = await postWithHeader('edge;dur=0.3, q;desc="2-5"');
    expect(f).not.toBeNull();
    // srv_us = 300 → 存 301；gaps=2；dups=5
    expect(decodeObs(f)).toBe(((301 << 16) | (2 << 8) | 5) >>> 0);
  });

  // 缺头是常态而非故障（伪装响应不带它）。全零必须表示「没有数据」，
  // 而不能被 Rust 侧读成「服务端零耗时、零丢包」——那会让画像把链路
  // 判得比实际更好，且是静默的。
  it('没有 Server-Timing 时回传零（即“无数据”）', async () => {
    const f = await postWithHeader(null);
    expect(decodeObs(f)).toBe(0);
  });

  // `<< 16` 在 JS 里是 32 位**有符号**运算：us 超过 32767 时符号位被点亮，
  // 不做 `>>> 0` 就会变成负数，写进字节数组后 Rust 侧解出一个天文数字。
  it('大延迟不会因为有符号位移而翻成负数', async () => {
    const f = await postWithHeader('edge;dur=60.0, q;desc="0-0"');
    const v = decodeObs(f);
    expect(v).toBeGreaterThan(0);
    expect(v >>> 16).toBe(60001);
  });

  // u16 在 65 ms 处饱和。饱和本身就是「服务端非常慢」的信号，不丢信息；
  // 回绕则会把最慢读成最快。
  it('超过 u16 的服务端耗时饱和而不回绕', async () => {
    const f = await postWithHeader('edge;dur=5000.0, q;desc="0-0"');
    expect(decodeObs(f) >>> 16).toBe(65535);
  });
});
