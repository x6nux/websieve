// websieve emitter —— 传输层 JS 半边（spec §4/§6.5/§6.7）。
//
// 同源关键决策（spec §6.7）：WebView 直接加载服务端真实首页（nginx 页），
// 所有代理 fetch 天然 same-origin、Cookie/Sec-Fetch-Site/Referer 全部由
// 浏览器内核正确生成。首页是服务器的页面，加不进 <script>，唯一注入点是
// Rust 侧的 initialization_script（data: URL 加载本文件，见
// src-tauri/src/bootstrap.rs）。本文件是单一事实源，build.rs 把它嵌进二进制。
//
// IPC 纪律（spec §3.2）：二进制永远走 invoke 的顶层 Uint8Array（raw body
// 快路径），绝不嵌在 object 里退化成数字数组。标量（requestId/status）
// 编进 16 字节帧头，与 payload 一起作为唯一顶层参数一次性传给 Rust。
// 帧格式见 src-tauri/src/main.rs 的 handle_frame 注释。
//
// 下行取消的双杠杆顺序是 spec §6.5 钉死的（Xray dialer.html 血泪注记）：
//   await reader.cancel();   // 必须先
//   controller.abort();      // 后
// 只调 abort() 会永久卡在 reader.read() 里。

(function () {
  'use strict';
  if (window.__wsieve) return; // 重复注入防护

  var CHUNK_TARGET = 64 * 1024; // 攒够 64KB 再 invoke，减少 IPC 次数
  var CHUNK_FLUSH_MS = 20;      // 或首字节后 20ms 强制 flush（保低延迟）
  var HEARTBEAT_MS = 5000;
  var HEADER = 16;

  var streams = new Map(); // requestId -> { controller, reader, cancelled }

  function invoke(cmd, args) {
    return window.__TAURI__.core.invoke(cmd, args);
  }

  // ---- 帧编码 ----------------------------------------------------------
  // magic "WSIE" | kind u8 | requestId u32 | status u16 | reserved u32
  function frame(kind, requestId, status, payload) {
    var n = payload ? payload.length : 0;
    var f = new Uint8Array(HEADER + n);
    f[0] = 0x57; f[1] = 0x53; f[2] = 0x49; f[3] = 0x45; // "WSIE"
    f[4] = kind;
    f[5] = (requestId >>> 24) & 0xff;
    f[6] = (requestId >>> 16) & 0xff;
    f[7] = (requestId >>> 8) & 0xff;
    f[8] = requestId & 0xff;
    f[9] = (status >>> 8) & 0xff;
    f[10] = status & 0xff;
    if (n) f.set(payload, HEADER);
    return f; // 顶层 Uint8Array → raw IPC（spec §3.2）
  }

  // ---- 上行 POST -------------------------------------------------------
  // Rust 经 eval 调入（base64 上行，每 POST 一次、≤1MB，可接受）；
  // 结果经 raw frame invoke 回填 pending 表。
  async function post(requestId, path, bodyB64) {
    try {
      var body = base64ToBytes(bodyB64);
      var resp = await fetch(path, {
        method: 'POST',
        body: body,
        credentials: 'include',
        // text/plain 是 CORS 简单请求的 Content-Type 白名单之一，
        // application/octet-stream 不是 —— 后者会让每个新 origin 先发一次
        // OPTIONS 预检。多端口条带下会话分散在多个 origin，预检开销与
        // 「服务端要不要特殊响应 OPTIONS」的指纹面都不划算。body 仍是二进制，
        // 服务端只读原始字节、不看此头。下行 GET 无自定义头，本就不触发预检。
        headers: { 'Content-Type': 'text/plain' },
      });
      var buf = new Uint8Array(await resp.arrayBuffer());
      await invoke('wsieve_raw_post', frame(1, requestId, resp.status, buf));
    } catch (e) {
      try { await invoke('wsieve_raw_post', frame(2, requestId, 0, null)); } catch (_) {}
    }
  }

  // ---- 下行长 GET -------------------------------------------------------
  async function openStream(requestId, path) {
    var controller = new AbortController();
    var entry = { controller: controller, reader: null, cancelled: false };
    streams.set(requestId, entry);
    try {
      var resp = await fetch(path, {
        credentials: 'include',
        signal: controller.signal,
      });
      if (!resp.ok) {
        await invoke('wsieve_raw_stream', frame(5, requestId, resp.status, null));
        streams.delete(requestId);
        return;
      }
      var reader = resp.body.getReader();
      entry.reader = reader;

      // 攒批：≥64KB 或首字节后 20ms 才 invoke 一次（文档化折衷）。
      var pending = [];
      var pendingLen = 0;
      var firstAt = 0;

      async function flush() {
        if (pendingLen === 0) return;
        var merged = new Uint8Array(pendingLen);
        var off = 0;
        for (var i = 0; i < pending.length; i++) {
          merged.set(pending[i], off);
          off += pending[i].length;
        }
        pending = [];
        pendingLen = 0;
        firstAt = 0;
        await invoke('wsieve_raw_stream', frame(3, requestId, 0, merged));
      }

      while (true) {
        var r = await reader.read();
        if (r.done) break;
        if (entry.cancelled) break;
        if (pendingLen === 0) firstAt = Date.now();
        pending.push(r.value);
        pendingLen += r.value.length;
        if (pendingLen >= CHUNK_TARGET || Date.now() - firstAt >= CHUNK_FLUSH_MS) {
          await flush();
        }
      }
      await flush(); // 收尾残余
      await invoke('wsieve_raw_stream', frame(4, requestId, 0, null));
    } catch (e) {
      if (!entry.cancelled) {
        try { await invoke('wsieve_raw_stream', frame(5, requestId, 0, null)); } catch (_) {}
      }
      // cancelled 时静默：Rust 侧已主动结束该流
    } finally {
      streams.delete(requestId);
    }
  }

  // Rust 侧取消（会话死亡/重启）。顺序钉死（spec §6.5）：
  // reader.cancel() 先、controller.abort() 后。
  async function cancelStream(requestId) {
    var entry = streams.get(requestId);
    if (!entry) return;
    entry.cancelled = true;
    try {
      if (entry.reader) await entry.reader.cancel(); // 必须先
    } catch (_) {}
    entry.controller.abort(); // 后
    streams.delete(requestId);
  }

  // ---- base64 解码（上行 body）----------------------------------------
  var B64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
  var B64_REV = (function () {
    var m = new Int8Array(128).fill(-1);
    for (var i = 0; i < B64.length; i++) m[B64.charCodeAt(i)] = i;
    return m;
  })();

  function base64ToBytes(b64) {
    var clean = b64.replace(/=+$/, '');
    var len = (clean.length * 3) >> 2;
    var out = new Uint8Array(len);
    var o = 0, acc = 0, bits = 0;
    for (var i = 0; i < clean.length; i++) {
      var v = B64_REV[clean.charCodeAt(i)];
      if (v < 0) throw new Error('bad base64');
      acc = (acc << 6) | v;
      bits += 6;
      if (bits >= 8) {
        bits -= 8;
        if (o < len) out[o++] = (acc >> bits) & 0xff;
      }
    }
    return out;
  }

  // ---- 心跳（健康检测，spec §9.1：5s 一跳，Rust 侧 >15s 判死）--------
  setInterval(function () {
    invoke('wsieve_heartbeat').catch(function () {});
  }, HEARTBEAT_MS);

  window.__wsieve = {
    post: post,
    openStream: openStream,
    cancelStream: cancelStream,
  };
})();
