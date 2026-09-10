// websieve emitter —— 传输层 JS 半边（spec §4/§6.5/§6.7）。
//
// 承载页决策（spec §6.7，2026-09-10 改）：WebView 加载的是本机 http 壳
// （`http://127.0.0.1:{随机端口}/`），不是服务端首页，所有代理 fetch 都是
// 跨源——Cookie/Sec-Fetch-Site/Referer 靠服务端 CORS 放宽成立，不再是
// same-origin 天然免配置。承载页加不进 <script>，唯一注入点是
// Rust 侧的 initialization_script（data: URL 加载本文件，见
// src-tauri/src/bootstrap.rs）。本文件是单一事实源，build.rs 把它嵌进二进制。
//
// IPC 纪律（spec §3.2 已按下述实测结果修订）：**承载页若是 https origin，
// raw body 快路径在这里根本不可用**——Tauri 的 custom protocol IPC 走
// `fetch('ipc://localhost/…')`，而 WKWebView 禁止 https 页面访问 custom
// scheme，请求发都发不出去；Tauri 只 console.warn 一句就静默回退到
// postMessage，那条路把 Uint8Array 交给 JSON.stringify，replacer 里一句
// `Array.from(val)` 把每字节变成 "123," 约 4 个字符。
//
// 2026-09-10 补测确证：决定因素是 **scheme** 而非 origin 是否本地
// （四格对照见 scripts/spike-ipc-origin.sh）。承载页因此改为本机的
// http://127.0.0.1:{随机端口}，raw 快路径恢复可用。
//
// 也就是说「绝不退化成数字数组」这条原则在生产形态下**从来没有成立过**，
// 只是 Rust 侧拒收 JSON（`raw body required`）所以表现为彻底失败而非变慢。
// 2026-09-09 逐一实测排除了全部绕过方案：ipc:// 三种请求形态、
// http://127.0.0.1（被 mixed content 拦，本地探针 server 零日志）、
// 本地 origin 的 iframe 桥（连已知存在的 index.html 都不 onload）、
// gzip（加密数据 1.001x，反而变大）。唯一通道就是 postMessage 的 JSON。
//
// 因此二进制改走 base64 字符串：3.57x → 1.33x，JavaScriptCore 实测吞吐
// 75 → 148 MB/s。这是**修复**而非妥协。标量（requestId/status）仍编进
// 16 字节帧头，与 payload 一起作为一个帧编码后传给 Rust。
// 帧格式见 src-tauri/src/main.rs 的 handle_frame 注释。
//
// 下行取消的双杠杆顺序是 spec §6.5 钉死的（Xray dialer.html 血泪注记）：
//   await reader.cancel();   // 必须先
//   controller.abort();      // 后
// 只调 abort() 会永久卡在 reader.read() 里。

(function () {
  'use strict';
  if (window.__wsieve) return; // 重复注入防护

  // 调优参数：宿主可在注入 emitter 之前设 window.__wsieveTune 覆盖。
  // 读不到就用这里的默认值，因此不设时行为与改动前一致。
  var TUNE = (window.__wsieveTune || {});

  // raw（custom protocol）通路可用性完全由**承载页的 scheme** 决定：
  // WKWebView 禁止 https 页面访问 custom scheme，与 origin 是不是本地无关。
  // 四格实测（scripts/spike-ipc-origin.sh）：
  //   http://127.0.0.1  ✅    http://localtest.me  ✅
  //   https://127.0.0.1 ❌    https://example.com  ❌
  //
  // 这里曾经判的是 hostname 是不是回环。那个判据只是**碰巧**与真判据重合，
  // 而它错的方向最危险：https://127.0.0.1 会被判成「可以走 raw」，于是开了
  // RAW 却发不出去 —— 症状是心跳正常、传输永久挂起、零错误日志。
  function canUseRawIpc() {
    return location.protocol === 'http:';
  }
  var FAST_IPC = canUseRawIpc();

  // 攒够多少再 invoke，减少 IPC 次数。最优值随 IPC 通路而变，实测（局域网
  // 千兆、单流下载）：
  //   base64 通路：64KB 72.5 MB/s ，256KB 64.6 MB/s  → 取 64KB
  //   raw 通路   ：64KB 114.2 MB/s @CPU 51%，256KB 114.4 MB/s @CPU 36% → 取 256KB
  // 吞吐在 raw 下两者持平，差在 CPU：分块越大，每字节摊到的 IPC 固定开销越小。
  // base64 反过来是因为编码本身随块长变贵，攒太大反而拖慢首字节。
  // 攒批目标按**时间**定，不按字节数定。
  //
  // 固定字节数的毛病是延迟随链路速率漂移：256 KiB 在千兆上要攒 2.2ms，在
  // 十兆上就是 200ms。而攒批真正要控制的本来就是"数据最多在手里压多久"。
  //
  // 实测这条权衡曲线很陡（千兆单流下行）：
  //   64KB → 攒满 0.5ms，CPU 68.4%
  //  128KB → 攒满 1.1ms，CPU 45.3%
  //  256KB → 攒满 2.2ms，CPU 36.3%
  // 块越小，每字节摊到的 IPC 固定开销越大。按时间定就能把这条曲线钉在一个
  // 点上：目标 1ms，块大小交给速率去决定，快链路自然用大块、慢链路用小块。
  // 攒批时长不写死，按系统忙闲自己调：闲的时候压到下限保延迟，忙的时候放大
  // 摊薄每次 IPC 的固定开销。见下面 loopLag 的注释。
  var CHUNK_MS_MIN = 1;
  var CHUNK_MS_MAX = 4;
  var CHUNK_MS = TUNE.chunkMs || CHUNK_MS_MIN;
  var CHUNK_MIN = 32 * 1024;
  var CHUNK_MAX = FAST_IPC ? 256 * 1024 : 64 * 1024;
  var CHUNK_TARGET = TUNE.chunkTarget || CHUNK_MIN;

  // 攒批的两条截止线，缺一不可：
  //
  //   IDLE —— 距**上一个分片**这么久没有新数据，就把手里的送走。
  //   CHUNK_FLUSH_MS —— 距**首个分片**的硬上限，防止分片以恰好小于 IDLE 的
  //                     间隔连绵到达时批次被无限推迟。
  //
  // 早先只有后者，于是每个攒不满 CHUNK_TARGET 的响应都要干等满这个时限。大包
  // 看不出来（分片连绵，永远是攒满先触发），小包则是每个请求都结结实实挨一发：
  // 实测 1 KB 响应的 P50 延迟随它线性走——flush 1/5/20ms 对应 11.1/16.2/32.9ms，
  // RPS 87/61/29。
  //
  // 改成"流一停就送"之后两边都对：大包的分片间隔远小于 IDLE（千兆下 256 KiB
  // 只要 2 毫秒出头），计时器一直被新数据顶掉，仍然靠攒满触发、IPC 次数不变；
  // 小包的响应一结束就没有后续分片，IDLE 立刻到期。
  var IDLE_FLUSH_MS = TUNE.idleMs !== undefined ? TUNE.idleMs : 2;

  // 批次小于这个量时，用零延迟探测代替 IDLE 定时器等待。
  //
  // 判据用"批次已攒多少"而不是"探测过几次"：一个小响应未必只有一个分片
  // （HTTP 头和 body 常常分两次交付），按次数给额度会在第二个分片上就退回
  // 定时器，等于没优化——实测 P50 卡在 4.5ms 下不来。
  //
  // 阈值本身则**随流量形态自适应**，因为固定值两头不讨好（实测：阈值
  // 32/64/128KB 下，64KB 响应的 P50 是 4.8/3.0/2.6ms，而大包下行 CPU 是
  // 35.7/62.7/72.9%）。原因是探测让出一轮只有微秒级，而满速下行的分片间隔
  // 有半毫秒——探测几乎必然扑空，批次被切碎，IPC 次数翻几倍。
  //
  // 但"分片会不会马上再来"恰恰就是这两种形态的分界：批量传输时分片连绵
  // 不断，请求-响应时一个响应发完就真的停了。所以按**下行速率**选阈值：
  // 忙的时候老实攒批，闲的时候激进送出。
  var PROBE_BUSY = TUNE.probeBelow || 32 * 1024;
  var PROBE_IDLE = 256 * 1024;
  var BUSY_KB_PER_MS = 50;   // ≈50 MB/s 以上算批量传输
  var RATE_WINDOW_MS = 200;

  // ---- 忙闲反馈：用 IPC 占空比当负载信号 --------------------------------
  //
  // 想要的是"Rust 侧忙不忙"，而 emitter 跑在 WebContent 进程里——那边的事件
  // 循环延迟只反映页面自己的调度压力，跟 Rust 侧的 CPU 毫无关系（实测大包满速
  // 时 Rust 侧 48%，页面侧 0.1%）。所以信号得从跨进程调用上取。
  //
  // 用 `await invoke(...)` 的阻塞时长：它一头连着 Rust 的处理，Rust 忙它就慢。
  // 控制量取**占空比**（IPC 阻塞时间 ÷ 墙钟时间）而不是单次耗时，因为后者会
  // 正反馈发散——块变大 → 单次 IPC 变慢 → 判定更忙 → 块再变大。占空比则天然
  // 是负反馈：块变大虽然单次变慢，但次数按比例减少，摊薄了每次调用的固定开销，
  // 总占空比反而下降。
  var ipcBusy = 0;
  var dutySince = 0;
  var DUTY_HI = 0.40; // 高于此说明 IPC 是瓶颈，攒大一点
  var DUTY_LO = 0.20; // 低于此说明在等数据，压小一点换延迟

  function noteIpc(ms, now) {
    ipcBusy += ms;
    if (dutySince === 0) {
      dutySince = now;
      return;
    }
    var span = now - dutySince;
    if (span < 100) return; // 窗口太短，样本不够
    var duty = ipcBusy / span;
    if (!TUNE.chunkMs) {
      if (duty > DUTY_HI) CHUNK_MS = Math.min(CHUNK_MS_MAX, CHUNK_MS * 1.5);
      else if (duty < DUTY_LO) CHUNK_MS = Math.max(CHUNK_MS_MIN, CHUNK_MS / 1.5);
    }
    ipcBusy = 0;
    dutySince = now;
  }

  var rateBytes = 0;
  var rateSince = 0;
  // 粗估最近的下行速率，返回当前该用的探测阈值。
  function probeLimit(now, n) {
    if (rateSince === 0 || now - rateSince > RATE_WINDOW_MS) {
      rateSince = now;
      rateBytes = 0;
    }
    rateBytes += n;
    var dt = now - rateSince;
    // 窗口刚开头时样本太少，先按"闲"处理：这时候多半正是一个新请求的首包。
    var busy = dt >= 2 && rateBytes / dt >= BUSY_KB_PER_MS * 1024 / 1000;
    // 顺带按当前速率把攒批目标钉在 CHUNK_MS 毫秒的量上。
    if (!TUNE.chunkTarget && dt >= 2) {
      var perMs = rateBytes / dt;
      CHUNK_TARGET = Math.max(CHUNK_MIN, Math.min(CHUNK_MAX, Math.round(perMs * CHUNK_MS)));
    }
    return busy ? PROBE_BUSY : PROBE_IDLE;
  }
  var probeBelow = PROBE_IDLE;
  var CHUNK_FLUSH_MS = TUNE.flushMs || 20;

  // 让出一轮事件循环，用来"零延迟地问一句：还有下一个分片吗"。
  //
  // 不能用 setTimeout(…, 0) 代替：HTML 的嵌套定时器规则会把它钳到最小 4ms，
  // 实测比 setTimeout(…, 1) 还慢（P50 4.4ms vs 3.2ms）。MessageChannel 是
  // 不受钳制的宏任务，一轮开销在微秒级。
  //
  // 只在批次还很小的时候用它，理由见 openStream 里的注释。
  // 返回 null 表示环境不支持，调用方退回 setTimeout。
  var yieldTick = (function () {
    if (typeof MessageChannel === 'undefined') return null;
    var ch = new MessageChannel();
    var queue = [];
    ch.port1.onmessage = function () {
      var fn = queue.shift();
      if (fn) fn();
    };
    return function (fn) {
      queue.push(fn);
      ch.port2.postMessage(0);
    };
  })();
  // 注意：**回帧目前不能并发发出**。帧头 16 字节里只有 magic/kind/requestId/
  // status，没有序号，Rust 侧按到达顺序拼接数据帧。想让多个 invoke 同时在途
  // （用 IPC 往返的等待时间去重叠下一批的 base64），必须先在帧头空出的
  // 第 11..15 字节里加序号并在 Rust 侧重排——那是另一件事，不能顺手做。
  // 允许同时在途的下行 invoke 数。1 = 严格串行（改动前的行为）。
  // >1 需要帧头序号 + Rust 侧重排，两者已就位。
  var PIPELINE = TUNE.pipeline || 1;
  var HEARTBEAT_MS = 5000;
  var HEADER = 16;

  var streams = new Map(); // requestId -> { controller, reader, cancelled }

  function invoke(cmd, args) {
    return window.__TAURI__.core.invoke(cmd, args);
  }

  // ---- 帧编码 ----------------------------------------------------------
  // magic "WSIE" | kind u8 | requestId u32 | status u16 | reserved u32
  function frame(kind, requestId, status, payload, seq) {
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
    // 第 11..14：下行分片序号。多个 invoke 同时在途时 Tauri 不保证完成
    // 顺序，而下行是字节流——错一个位置整条 TLS 连接就废了，所以顺序
    // 由这个序号在 Rust 侧还原，不依赖到达次序。
    seq = seq >>> 0;
    f[11] = (seq >>> 24) & 0xff;
    f[12] = (seq >>> 16) & 0xff;
    f[13] = (seq >>> 8) & 0xff;
    f[14] = seq & 0xff;
    if (n) f.set(payload, HEADER);
    return f;
  }

  // 帧 → base64 → invoke。六个回帧点统一走这里，避免有人漏掉编码那一步
  // 而悄悄退回数字数组（那条路 Rust 侧会直接拒收，但排查起来极难：
  // 症状是「心跳正常、传输永远挂着」，且全程零错误日志）。
  // RAW 模式：直接把 Uint8Array 交给 invoke，走 Tauri 的 custom protocol
  // 快路径，省掉 base64 的 CPU 与 1.333 倍膨胀。
  //
  // **只在 http origin 下成立**。承载页是 https origin 时，custom protocol
  // 请求会被 WKWebView 整体拦下，Tauri 静默回退到 postMessage，而那条路的
  // body 只能是 JSON——Uint8Array 会被 replacer 摊成数字数组（实测约 3.5 倍
  // 膨胀），Rust 侧则直接拒收 `raw body required`。
  //
  // 可用性完全由 scheme 决定，那就直接按 scheme 判，不必让宿主记得去开：http
  // origin 一定能走 raw，https origin 一定不能。这不是偏好而是能力检测，所以
  // 默认就该是自动的。宿主仍可用 `__wsieveTune.raw` 显式覆盖（排障用）。
  //
  // 差距很大，值得自动化：局域网千兆单流下载，base64 48.8 MB/s @CPU 86%，
  // raw 114.9 MB/s @CPU 34% —— 2.4 倍吞吐、四成 CPU。
  var RAW = TUNE.raw !== undefined ? !!TUNE.raw : FAST_IPC;
  function sendFrame(cmd, kind, requestId, status, payload, seq) {
    var f = frame(kind, requestId, status, payload, seq || 0);
    return RAW ? invoke(cmd, f) : invoke(cmd, { f: bytesToBase64(f) });
  }

  // 分块 btoa —— 比手写 base64 循环快 2.6 倍（JavaScriptCore 实测
  // 344 vs 132 MB/s）。分块是必须的：String.fromCharCode.apply 对整个
  // 大数组会爆栈（参数个数上限），0x8000 是公认安全的块大小。
  function bytesToBase64(u8) {
    var s = '';
    for (var i = 0; i < u8.length; i += 0x8000) {
      s += String.fromCharCode.apply(null, u8.subarray(i, i + 0x8000));
    }
    return btoa(s);
  }

  // ---- 上行 POST -------------------------------------------------------
  // Rust 经 eval 调入（base64 上行，每 POST 一次、≤1MB，可接受）；
  // 结果经 raw frame invoke 回填 pending 表。
  //
  // 这一次 POST 的往返实测 1.44ms，而同一条网络路径 curl 只要 0.4ms。差出来的
  // 一毫秒**不在 JS 这一侧**，已经逐项量过：
  //   - Rust eval → JS 执行 → IPC 回帧，整个往返只有 0.19–0.38ms；
  //   - 换成 XMLHttpRequest 与 fetch 完全持平（都是 P50 2.0ms）；
  //   - `cache: 'no-store'` 无可测差异。
  // 剩下的就是 WKWebView 自己的多进程网络栈：请求要从 WebContent 进程经 IPC
  // 交给 NetworkProcess 才能落到 socket 上，回来再走一遍。换任何 JS API 都绕
  // 不开这条路——它正是"流量由真实浏览器内核发出"这个前提的代价。
  async function post(requestId, path, bodyB64) {
    try {
      var body = base64ToBytes(bodyB64);
      var resp = await fetch(path, {
        method: 'POST',
        body: body,
        credentials: 'include',
        // 这条路上的响应从定义上就不可缓存（每个 POST 都是一次性的 TU 批次）。
        // 明说了 no-store，浏览器就不必去查一遍 HTTP 缓存、也不必考虑写回。
        cache: 'no-store',
        // text/plain 是 CORS 简单请求的 Content-Type 白名单之一，
        // application/octet-stream 不是 —— 后者会让每个新 origin 先发一次
        // OPTIONS 预检。多端口条带下会话分散在多个 origin，预检开销与
        // 「服务端要不要特殊响应 OPTIONS」的指纹面都不划算。body 仍是二进制，
        // 服务端只读原始字节、不看此头。下行 GET 无自定义头，本就不触发预检。
        headers: { 'Content-Type': 'text/plain' },
      });
      var buf = new Uint8Array(await resp.arrayBuffer());
      await sendFrame('wsieve_raw_post', 1, requestId, resp.status, buf);
    } catch (e) {
      try { await sendFrame('wsieve_raw_post', 2, requestId, 0, null); } catch (_) {}
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
        cache: 'no-store',
        signal: controller.signal,
      });
      if (!resp.ok) {
        await sendFrame('wsieve_raw_stream', 5, requestId, resp.status, null);
        streams.delete(requestId);
        return;
      }
      var reader = resp.body.getReader();
      entry.reader = reader;

      // 攒批：≥64KB 或首字节后 20ms 才 invoke 一次（文档化折衷）。
      var pending = [];
      var pendingLen = 0;
      var firstAt = 0;   // 本批第一个分片的到达时刻（硬上限用）
      var lastAt = 0;    // 本批最近一个分片的到达时刻（空闲线用）


      // 已发出但尚未完成的 invoke。PIPELINE=1 时行为与串行完全一致。
      var seq = 0;
      var inflight = [];

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
        lastAt = 0;
        var mySeq = seq++;
        var ipcT0 = (typeof performance !== 'undefined' ? performance.now() : Date.now());
        var p = sendFrame('wsieve_raw_stream', 3, requestId, 0, merged, mySeq);
        if (PIPELINE <= 1) {
          await p;
          var ipcNow = (typeof performance !== 'undefined' ? performance.now() : Date.now());
          noteIpc(ipcNow - ipcT0, ipcNow);
          return;
        }
        // 记账并封顶：不封顶的话内存与乱序跨度都会随下行速率无界增长。
        var entry = { p: p };
        inflight.push(entry);
        p.then(function () { drop(entry); }, function () { drop(entry); });
        while (inflight.length >= PIPELINE) {
          await Promise.race(inflight.map(function (e) { return e.p; }));
        }
      }

      function drop(entry) {
        var i = inflight.indexOf(entry);
        if (i >= 0) inflight.splice(i, 1);
      }

      // 收尾必须等**全部**在途完成：提前发结束帧的话，Rust 侧会在还有分片
      // 没到时就关掉流，末尾数据静默丢失。
      async function drainInflight() {
        while (inflight.length) {
          await Promise.race(inflight.map(function (e) { return e.p; }));
        }
      }

      // 截止时间**必须由定时器推动**，不能只在「新分片到达时」顺带检查。
      //
      // 曾经这里是 `await reader.read()` 之后才判 `Date.now() - firstAt >=
      // CHUNK_FLUSH_MS`，于是一个没攒满 64KB 的批次会被**扣押到下一个分片
      // 到达为止**——而下一个分片什么时候来是对端说了算。实测后果：批量
      // 下载正常（分片连绵不断，互相把前一批顶出去），但 TLS 握手这种严格
      // 一来一回的流量必然在最后一个记录上卡住，实测停顿精确落在服务端
      // 心跳的 10 秒栅格上。
      //
      // `readPromise` 要跨轮保留：Streams API 不允许在前一次 read() 未完成时
      // 再调一次，超时后丢弃它会直接把流读坏。
      var readPromise = null;
      var TIMED_OUT = {};
      while (true) {
        if (readPromise === null) readPromise = reader.read();
        var r;
        if (pendingLen > 0) {
          // 两条线取先到的那个：流停下来 IDLE 毫秒，或距首片满 CHUNK_FLUSH_MS。
          var now = Date.now();
          var wait = Math.max(
            0,
            Math.min(IDLE_FLUSH_MS - (now - lastAt), CHUNK_FLUSH_MS - (now - firstAt))
          );
          var timer = null;
          var timeoutP = new Promise(function (res) {
            // 已经到期（IDLE=0 的常见情形）就只让出一轮事件循环：这一轮里若
            // 有新分片立即可读，`readPromise` 会先 settle，批次继续攒；没有的话
            // 就说明流确实停了，立刻送走。用定时器做这件事会平白多等几毫秒。
            // 批次还小：零延迟问一句还有没有下一个。没有就说明这是个小响应，
            // 立刻送走（省下整整一个 IDLE 的等待）；有的话说明流还在继续，
            // 攒过阈值后就交回定时器。
            //
            // **不能对每个分片都这么做**。让出一轮之后 WebKit 往往还没来得及
            // 交付下一个分片（交付本身也走事件循环），于是大包会被切碎：
            // 让出一轮之后 WebKit 往往还没来得及交付下一个分片（交付本身也走
            // 事件循环），于是批次在几十 KB 上就被送走，IPC 次数翻几倍——实测
            // 下行 CPU 从 35% 涨到 80%，吞吐还是 95%，纯粹白烧。
            if (pendingLen < probeBelow && yieldTick) {
              yieldTick(function () { res(TIMED_OUT); });
            } else {
              timer = setTimeout(function () { res(TIMED_OUT); }, wait);
            }
          });
          var winner = await Promise.race([readPromise, timeoutP]);
          if (timer !== null) clearTimeout(timer);
          if (winner === TIMED_OUT) {
            // 到点了：把残余批次送走，未完成的 read 留到下一轮继续等。
            await flush();
            continue;
          }
          r = winner;
        } else {
          r = await readPromise;
        }
        readPromise = null;
        if (r.done) break;
        if (entry.cancelled) break;
        if (pendingLen === 0) firstAt = Date.now();
        lastAt = Date.now();
        probeBelow = probeLimit(lastAt, r.value.length);
        pending.push(r.value);
        pendingLen += r.value.length;
        if (pendingLen >= CHUNK_TARGET) {
          await flush();
        }
      }
      await flush(); // 收尾残余
      await drainInflight(); // 结束帧必须排在全部数据帧之后
      await sendFrame('wsieve_raw_stream', 4, requestId, 0, null);
    } catch (e) {
      if (!entry.cancelled) {
        try { await sendFrame('wsieve_raw_stream', 5, requestId, 0, null); } catch (_) {}
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
    // 当前用的是哪条 IPC 通路。排障时在 console 里一眼可见；也是这个判据
    // 唯一的观察点 —— emitter 是 IIFE，不暴露就没法测。
    raw: RAW,
  };
})();
