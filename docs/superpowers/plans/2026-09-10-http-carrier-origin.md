# 承载页改用 http origin，取回 IPC raw 快路径 —— 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把承载 WebView 加载的页面从远程 https 服务器首页换成本机的
`http://127.0.0.1:{随机端口}/`，让 Tauri 的 IPC raw body 快路径重新可用，
去掉 base64 回帧的 CPU 开销（实测 86% → 34%）。

**Architecture:** 新增一个只吐固定 HTML 的本地 HTTP server 作为承载页壳。
数据面完全不改道——每个出站仍用绝对 https URL 直接打真实服务端，TLS 由
WebKit 本体握手。改动的实质是把 `page_url` 这一个字段承担的两件事
（「窗口加载什么」与「数据面打哪儿」）拆开：前者归新的 `carrier_page`，
后者归 `shard_setup` 全权提供。

**Tech Stack:** Rust / tokio / Tauri 2 / Svelte + vitest

**Spec:** `docs/superpowers/specs/2026-09-10-http-carrier-origin-design.md`

## Global Constraints

- **绝不终结 TLS。** 数据面的每一个请求都必须由 WebView 直接发往真实服务端。
  任何让 Rust 代为发起对外 HTTPS 的改动都直接作废本项目的前提
  （`src-tauri/src/shard.rs` 模块注释、spec §3 非目标）。
- **承载页 URL 恒为 `http://127.0.0.1:{carrier_port}/`**，不分出站、不分
  承载模式、不依赖 hosts 劫持（spec §5.2）。
- **`capabilities/transport.json` 的 `urls` 一个字都不改**，现有
  `http://127.0.0.1:*` 已覆盖（spec §5.6）。只更新该文件的 `description` 文案。
- **base64 回帧通路全部保留**：`bridge.rs` 的 `bs64_decode`、emitter 的
  `bytesToBase64`、`decode_body` 的双分支（spec §5.7）。
- **不引入新的 Cargo / npm 依赖。**
- 注释一律中文，与既有代码一致；解释「为什么」而不是「做了什么」。
- 刻意留下的简化用 `ponytail:` 注释标明代价与升级路径（仓库既有惯例，
  见 `shard.rs` 的 `PREWARM_DEPTH`）。

## 文件结构

| 文件 | 职责 | 本计划中的动作 |
|---|---|---|
| `src-tauri/src/carrier_page.rs` | 本地承载页 server：监听回环随机端口，对任何请求返回同一张 HTML | **新建**（Task 1） |
| `ui/emitter.js` | 传输层 JS。RAW 判据由 host 改为 scheme | 修改（Task 2） |
| `ui/emitter.test.js` | emitter 的判据测试 | **新建**（Task 2） |
| `src-tauri/src/outbound/carrier.rs` | 承载计划。收敛为「只管窗口」，交出数据面基址职责 | 修改（Task 3、4） |
| `src-tauri/src/shard_setup.rs` | 条带编排。数据面基址全权提供者；承载页 URL 改为入参 | 修改（Task 3、4） |
| `src-tauri/src/runtime_state.rs` | 启动计划装配。不再用承载计划覆盖会话 0 基址 | 修改（Task 3、4） |
| `src-tauri/src/main.rs` | 启动编排。新增起承载页 server 一步并向下传递 URL | 修改（Task 4） |
| `src-tauri/capabilities/transport.json` | 传输窗口能力。仅描述文案更新 | 修改（Task 4） |

**任务顺序的关键**：Task 3 是**行为等价重构**（把数据面基址的决定权从
`CarrierPlan` 搬到 `shard_setup`，指向的地址完全不变），Task 4 才真正替换
承载页 URL。先解耦、再替换——这样每一步都能编译、能跑测试、能单独回滚。
反过来做则会出现一个中间状态：所有非宿主出站的数据面基址被算成本地承载
server，代理流量整个打到那张空 HTML 上。

---

### Task 1: 本地承载页 server

**Files:**
- Create: `src-tauri/src/carrier_page.rs`
- Modify: `src-tauri/src/main.rs`（只加一行 `mod carrier_page;`，第 25-43 行的模块声明区，按字母序插在 `mod bridge;` 之后）

**Interfaces:**
- Produces:
  - `pub async fn spawn() -> anyhow::Result<CarrierPage>`
  - `pub struct CarrierPage`，方法 `pub fn port(&self) -> u16`、`pub fn url(&self) -> String`（返回 `http://127.0.0.1:{port}/`）
  - `impl Drop for CarrierPage`：abort accept 任务，释放端口

- [ ] **Step 1: 写失败的测试**

新建 `src-tauri/src/carrier_page.rs`，先只放测试与最小骨架：

```rust
//! 本地承载页 server（设计文档 2026-09-10 §5.1）。

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn serves_html_on_an_os_assigned_port() {
        let page = spawn().await.unwrap();
        assert_ne!(page.port(), 0, "端口必须是 OS 真的分配出来的");
        assert_eq!(page.url(), format!("http://127.0.0.1:{}/", page.port()));

        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", page.port()))
            .await
            .unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("HTTP/1.1 200 OK"), "{text}");
        assert!(text.contains("text/html"), "{text}");
        assert!(text.contains("<!doctype html>"), "{text}");
    }

    #[tokio::test]
    async fn any_path_gets_the_same_page() {
        // 承载窗口只会请求 `/`，但 WebView 还会自己去要 /favicon.ico。
        // 那个请求若得不到应答就会挂着，白占一条连接。
        let page = spawn().await.unwrap();
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", page.port()))
            .await
            .unwrap();
        s.write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        assert!(
            String::from_utf8_lossy(&out).starts_with("HTTP/1.1 200 OK"),
            "任何路径都该拿到同一张页面"
        );
    }

    #[tokio::test]
    async fn dropping_the_handle_releases_the_port() {
        let page = spawn().await.unwrap();
        let port = page.port();
        drop(page);
        // abort 生效是异步的，轮询等它释放而不是睡一个拍脑袋的固定时长。
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            if TcpListener::bind(("127.0.0.1", port)).await.is_ok() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "句柄 drop 一秒后端口 {port} 仍未释放"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}
```

在 `src-tauri/src/main.rs` 的模块声明区（第 29 行 `mod bridge;` 之后）加一行：

```rust
mod carrier_page;
```

- [ ] **Step 2: 跑测试，确认它因为「函数不存在」而失败**

Run: `cd src-tauri && cargo test --lib carrier_page 2>&1 | tail -20`

预期：编译失败，`cannot find function 'spawn' in this scope`。

> 若报的是别的错（比如 `mod carrier_page` 找不到文件），先修那个——
> 我们要的是「因为还没实现而失败」，不是「因为接错线而失败」。

- [ ] **Step 3: 写实现**

把下面的内容加到 `src-tauri/src/carrier_page.rs` 的 `#[cfg(test)] mod tests` **之前**：

```rust
//! 本地承载页 server（设计文档 2026-09-10 §5.1）。
//!
//! 存在的唯一理由是 origin 的 **scheme**：WKWebView 禁止 https 页面访问
//! custom scheme，Tauri 的 IPC raw body 快路径因此在远程承载页上根本发不
//! 出去（四格实测对照见 `scripts/spike-ipc-origin.sh`）。承载页换成
//! `http://127.0.0.1:{port}` 就能拿回 raw，省掉 base64 的 1.333 倍膨胀与
//! 一次全量编码——实测 48.8 MB/s @CPU 86% → 114.9 MB/s @CPU 34%。
//!
//! **它不碰数据面。** 页面加载完这个 server 就下班了：代理的每一个请求都由
//! WebView 用绝对 URL 直接发往真实服务端，TLS 仍由 WebKit 本体握手。一旦让
//! 数据面走进这里，握手就变成 rustls 发的，整个项目「用真实浏览器指纹」的
//! 前提当场作废——与 `crate::shard` 那条「绝不终结 TLS」同源。

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 承载页的 HTML。
///
/// **内容对伪装零影响**：它由本地直接响应，从不出网，网络上没有任何观察者
/// 能看到它。emitter 由 `initialization_script` 注入（见 `bootstrap::loader_js`），
/// 所以这里一个 `<script>` 都不需要。
const PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>websieve</title>";

/// 承载页 server 句柄：drop 即停（accept 任务随 abort 结束）。
pub struct CarrierPage {
    port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl CarrierPage {
    /// 实际监听到的端口。
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 承载 WebView 应加载的 URL。
    ///
    /// 带尾斜杠：拼路径的调用方不需要再判断要不要补，而 `origin_of` 一类的
    /// 解析都会把它吃掉。
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }
}

impl Drop for CarrierPage {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// 在 `127.0.0.1` 上起承载页 server，**端口由 OS 分配**。
///
/// 端口既不能固定也不能复用 `shard_base_port` 段：
/// - 固定端口是本机指纹，别的进程扫到就知道装了什么；
/// - 条带段的每个端口都是 TCP 转发器，拿明文 HTTP 去打它等于把 HTTP 请求
///   塞给真实服务端的 TLS 端口。
pub async fn spawn() -> anyhow::Result<CarrierPage> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| anyhow::anyhow!("承载页 server 监听 127.0.0.1:0 失败: {e}"))?;
    let port = listener.local_addr()?.port();
    let task = tokio::spawn(accept_loop(listener));
    tracing::info!("承载页 server 就绪: http://127.0.0.1:{port}/");
    Ok(CarrierPage { port, task })
}

async fn accept_loop(listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                tokio::spawn(serve_one(stream));
            }
            Err(e) => {
                // accept 失败通常是 fd 耗尽一类的瞬时问题。让出一次再继续，
                // **绝不 return**：退出循环会让承载页从此再也加载不了，表现
                // 为「应用起来了但永远连不上」，而且没有一条能解释原因的日志。
                tracing::warn!("承载页 server accept 失败（继续监听）: {e}");
                tokio::task::yield_now().await;
            }
        }
    }
}

/// 读完请求头再回同一张页面。
///
/// 必须先读完：不读就写，客户端可能在写完请求之前收到 FIN/RST，
/// WebView 那边表现为导航失败。
///
/// `ponytail:` 不解析请求——任何路径都回同一张页面，解析出来的东西没有
/// 一处会被用到。`windows(4)` 的重复扫描是 O(n²)，对 8KB 上限无所谓。
/// 升级路径：真需要按路径分流时再上正经的 HTTP 处理。
async fn serve_one(mut stream: TcpStream) {
    let mut buf = [0u8; 1024];
    let mut seen: Vec<u8> = Vec::new();
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => return,
            Ok(n) => {
                seen.extend_from_slice(&buf[..n]);
                if seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if seen.len() > 8192 {
                    // 请求头超过 8KB：不是浏览器发的正常请求，断开了事。
                    return;
                }
            }
            Err(_) => return,
        }
    }
    let resp = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        PAGE.len(),
        PAGE
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.flush().await;
}
```

- [ ] **Step 4: 跑测试，确认全绿**

Run: `cd src-tauri && cargo test --lib carrier_page 2>&1 | tail -20`

预期：3 个测试全部 PASS，零警告。

- [ ] **Step 5: 确认没有引入编译警告**

Run: `cd src-tauri && cargo build 2>&1 | grep -E "^warning" | head`

预期：无输出。

> `CarrierPage::port()` 此刻还没有生产调用方（Task 4 才接线）。若 clippy
> 报 dead_code，**逐项加 `#[allow(dead_code)]` 并注明「Task 4 接线」**，
> 不要整模块开——整模块 allow 会连真正的死代码一起盖住（仓库既有纪律，
> 见 `carrier.rs` 的 `page_url_for`）。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/carrier_page.rs src-tauri/src/main.rs
git commit -m "feat(carrier): 新增本地承载页 server，为取回 IPC raw 快路径铺路"
```

---

### Task 2: emitter 的 IPC 通路判据改看 scheme

与 Task 1 无依赖，可并行。

**Files:**
- Modify: `ui/emitter.js:41-46`（判据函数）、`:71`（`CHUNK_MAX`）、`:243`（`RAW`）、`:492-496`（导出对象）
- Test: `ui/emitter.test.js`（新建）

**Interfaces:**
- Produces: `window.__wsieve.raw`（布尔，诊断字段）——Task 5 的真机验收靠它
  在 console 里一眼确认走的是哪条通路

**背景（实现者必读）**：`emitter.js` 现在判 `location.hostname` 是不是回环。
spike 四格实测（`scripts/spike-ipc-origin.sh`）证明真正的决定因素是 **scheme**：

| origin | raw 可用 |
|---|---|
| `http://127.0.0.1:18099` | ✅ |
| `http://localtest.me:18099` | ✅ |
| `https://127.0.0.1:18443` | ❌ |
| `https://example.com` | ❌ |

旧判据在本计划改完后**碰巧仍返回正确结果**（承载页恒为
`http://127.0.0.1`）。仍然要改：它判的是 host、被判之物是 scheme，两者只在
当前形态下重合，而它错的方向是最危险的那种——`https://127.0.0.1` 会被判成
「可以走 raw」，于是开了 RAW 却发不出去，症状是 2026-09-09 那次的**心跳
正常、传输永久挂起、零错误日志**。

- [ ] **Step 1: 写失败的测试**

新建 `ui/emitter.test.js`：

```js
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
```

- [ ] **Step 2: 跑测试，确认它失败**

Run: `npx --prefix ui vitest run --root ui emitter.test.js 2>&1 | tail -20`

预期：4 条全部 FAIL，报 `expected undefined to be true`——因为 `window.__wsieve`
还没有 `raw` 字段。

> 若报的是 `Cannot read properties of undefined`，说明 emitter 加载就崩了，
> 先查 stub 是不是漏了 emitter 需要的全局，而不是急着往下走。

- [ ] **Step 3: 改判据**

`ui/emitter.js` 第 41-46 行，把整个 `isLocalOrigin` 函数与 `LOCAL` 变量替换为：

```js
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
```

- [ ] **Step 4: 把两处 `LOCAL` 的引用改过来**

第 71 行：

```js
  var CHUNK_MAX = FAST_IPC ? 256 * 1024 : 64 * 1024;
```

第 243 行：

```js
  var RAW = TUNE.raw !== undefined ? !!TUNE.raw : FAST_IPC;
```

第 229-242 行 `RAW` 上方那段长注释里「**只在本地 origin 下成立**」一句改为
「**只在 http origin 下成立**」，并把「本地 origin 一定能走 raw，远程 origin
一定不能」改为「http origin 一定能走 raw，https origin 一定不能」。

同时更新文件头第 9-21 行的 IPC 纪律段：把「承载页是远程 origin」改为
「承载页若是 https origin」，并补一句指向新 spike 的出处：

```js
// 2026-09-10 补测确证：决定因素是 **scheme** 而非 origin 是否本地
// （四格对照见 scripts/spike-ipc-origin.sh）。承载页因此改为本机的
// http://127.0.0.1:{随机端口}，raw 快路径恢复可用。
```

Run: `grep -n "LOCAL" ui/emitter.js`
预期：无输出（全部引用都已改名）。

- [ ] **Step 5: 暴露诊断字段**

第 492 行的导出对象加一个字段：

```js
  window.__wsieve = {
    post: post,
    openStream: openStream,
    cancelStream: cancelStream,
    // 当前用的是哪条 IPC 通路。排障时在 console 里一眼可见；也是这个判据
    // 唯一的观察点 —— emitter 是 IIFE，不暴露就没法测。
    raw: RAW,
  };
```

- [ ] **Step 6: 跑测试，确认全绿**

Run: `npx --prefix ui vitest run --root ui emitter.test.js 2>&1 | tail -20`
预期：4 条全部 PASS。

- [ ] **Step 7: 跑完整 UI 测试套件，确认没连累别的**

Run: `npm --prefix ui test 2>&1 | tail -15`
预期：全绿（改动前是 639 条）。

- [ ] **Step 8: 确认 Rust 侧仍能编译**

`build.rs` 把 `ui/emitter.js` 嵌进二进制，改了它要确认嵌入没坏。

Run: `cd src-tauri && cargo build 2>&1 | tail -5`
预期：编译成功，零警告。

- [ ] **Step 9: 提交**

```bash
git add ui/emitter.js ui/emitter.test.js
git commit -m "fix(emitter): IPC 通路判据从 hostname 改看 scheme，并暴露 raw 诊断字段"
```

---

### Task 3: 数据面基址改由条带全权提供（行为等价重构）

**这个任务不改变任何请求实际打到的地址**，只把「谁来决定会话 0 的基址」从
`CarrierPlan` 搬到 `shard_setup`。三个文件必须一起改——只改其一编译不过。

**Files:**
- Modify: `src-tauri/src/shard_setup.rs`（`ShardPlanEntry::degraded`、`plan_many` 内 `origin` 闭包与 `session_bases` 构造、新增 `origin_of`）
- Modify: `src-tauri/src/outbound/carrier.rs`（删 `Slot.base` 与 `base_for`，更新 `window_label` 文档）
- Modify: `src-tauri/src/runtime_state.rs:468-478`（不再覆盖 `session_bases[0]`）

**Interfaces:**
- Consumes: 无（不依赖 Task 1/2）
- Produces:
  - `ShardPlanEntry.session_bases` 的每一项都是 `Some(String)`，且是绝对
    `https://` URL（含会话 0）
  - `CarrierPlan` 不再有 `base_for`；存在性校验改用现有的
    `pub fn window_label(&self, name: &str) -> Option<String>`

**等价性推导（实现者必读，改完要能对上）：**

| 场景 | 改动前 | 改动后 |
|---|---|---|
| shared 宿主 / 劫持成功 | `None` + 页面 `https://a:30000/` ⇒ 打 `https://a:30000/api/*` | `Some("https://a:30000")` ⇒ 同 |
| shared 宿主 / 降级 | `None` + 页面 `https://a/` ⇒ 打 `https://a/api/*` | `Some("https://a")` ⇒ 同 |
| shared 非宿主 | `Some(origin_of(它的 page_url))` | 条带直接给同一个值 |
| isolated | `None` + 各自页面 | 条带直接给各自的绝对 URL |

- [ ] **Step 1: 先改 `shard_setup.rs` 的测试，表达新期望**

`src-tauri/src/shard_setup.rs` 的 `#[cfg(test)] mod tests` 里新增：

```rust
    #[test]
    fn degraded_gives_session_zero_an_explicit_absolute_origin() {
        // 会话 0 曾经吃相对路径，前提是「承载页就是它的 origin」。
        // 承载页马上要挪到本机 http 壳上（下一个任务），这个前提消失，
        // 因此降级路径也必须显式写死数据面 origin。
        let e = ShardPlanEntry::degraded("https://a.example/some/path");
        assert_eq!(
            e.session_bases,
            vec![Some("https://a.example".to_string())],
            "降级路径的会话 0 必须是绝对 origin，且路径要被剥掉"
        );
    }

    #[test]
    fn degraded_keeps_an_explicit_port_verbatim() {
        // 不归一默认端口：https://a:443 与 https://a 对 fetch 等价，
        // 但保留原样能让日志里的基址和配置文件对得上。
        let e = ShardPlanEntry::degraded("https://a.example:8443/");
        assert_eq!(e.session_bases, vec![Some("https://a.example:8443".to_string())]);
    }

    #[tokio::test]
    async fn no_session_base_is_ever_relative() {
        // 钉死「没有任何一项是 None」——None 意味着相对路径，而承载页
        // 已经不是任何出站的 origin 了。
        //
        // 走降级路径：劫持路径要真实 DNS 解析 + 真的起转发器，单测里既慢
        // 又不确定（离线环境直接降级），那条腿交给 Task 5 的真机走查。
        // 不变量本身两条路径是同一条。
        let r = plan_many(
            vec![target("https://x.com/", 18443, 0)],
            "/nonexistent".into(),
        )
        .await;
        for (i, b) in r.entries[0].session_bases.iter().enumerate() {
            let b = b.as_ref().unwrap_or_else(|| panic!("会话 {i} 的基址是 None"));
            assert!(b.starts_with("https://"), "会话 {i} 的基址必须是 https：{b}");
        }
    }
```

> `target(url, base_port, extra_sessions)` 是该测试模块里已有的辅助函数
> （`shard_setup.rs:355`）。可写 hosts 的场景用 `temp_hosts(name, content)`，
> 不可写场景直接传 `"/nonexistent".into()` —— 都照现有测试的写法，**不要
> 新造辅助函数**。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test --lib shard_setup 2>&1 | tail -25`

预期：新增的三条 FAIL（`session_bases` 仍是 `vec![None]` / 首项是 `None`）。

- [ ] **Step 3: 实现 `shard_setup.rs` 的改动**

在 `split_url` 函数下方新增：

```rust
/// 从 URL 取出 origin（`scheme://authority`，无路径、无尾斜杠）。
///
/// 与 `split_url` 的区别是**不归一默认端口**：`https://a.com:443` 与
/// `https://a.com` 对 fetch 等价，但保留用户写的原样能让日志里的基址和配置
/// 文件对得上。数据面基址要给 WebView 直接拼路径用，所以走这一个。
fn origin_of(url: &str) -> anyhow::Result<String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| anyhow::anyhow!("URL 缺少 scheme: {url}"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        anyhow::bail!("URL 缺少主机: {url}");
    }
    Ok(format!("{scheme}://{authority}"))
}
```

把 `ShardPlanEntry::degraded` 替换为：

```rust
    /// 降级：不劫持、单会话、页面就是原始 URL。
    fn degraded(server_url: &str) -> Self {
        Self {
            page_url: server_url.to_string(),
            // 会话 0 也用显式绝对 origin。它曾经吃相对路径，前提是「承载页
            // 就是它的 origin」——承载页挪到本机 http 壳之后这个前提没了。
            //
            // 取不出 origin 时退回原样：走到这条路径的 URL 有一部分正是因为
            // `split_url` 失败才降级的，此处再制造一个失败点没有意义。坏 URL
            // 会在 fetch 时报错，那是能看见的失败。
            session_bases: vec![Some(
                origin_of(server_url).unwrap_or_else(|_| server_url.to_string()),
            )],
            upstream: None,
            bypass_error: None,
        }
    }
```

把 `plan_many` 里构造 `session_bases` 与 `page_url` 的那三行（现 `shard_setup.rs:279-282`）替换为：

```rust
        let ports = forwarder.ports().to_vec();
        // 数据面的 origin。**与承载页的 scheme 无关**：承载页是本机的 http
        // 壳，而数据面必须留在 https 才有真实 TLS 指纹。两者共用一个闭包，
        // 下一次有人改承载页 scheme 就会把数据面一起带歪。
        let data_origin = |p: u16| format!("{scheme}://{host}:{p}");
        // 每条会话都用显式绝对 URL，**含会话 0**——见 degraded 上的注释。
        let session_bases: Vec<Option<String>> =
            ports.iter().map(|p| Some(data_origin(*p))).collect();
        let page_url = format!("{}/", data_origin(ports[0]));
```

- [ ] **Step 4: 跑 `shard_setup` 测试**

Run: `cd src-tauri && cargo test --lib shard_setup 2>&1 | tail -25`

预期：全绿。现有测试里若有断言 `session_bases` 首项为 `None` 的，改成对应的
绝对 URL——**改断言值，不要改被测行为**。

- [ ] **Step 5: 改 `carrier.rs`：交出基址职责**

`Slot` 结构删掉 `base` 字段：

```rust
/// 单个出站在承载计划里的位置。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Slot {
    /// 承载它的窗口标签。
    window: String,
    /// 该窗口应加载的页面 URL。
    page_url: String,
}
```

`shared()` 里那段 `let base = if *name == host { None } else { Some(origin_of(url)?) };`
连同 `base` 字段一起删掉，循环体变成：

```rust
        let mut slots = BTreeMap::new();
        for (name, _url) in &entries {
            slots.insert(
                name.clone(),
                Slot {
                    window: SHARED_WINDOW.to_string(),
                    page_url: host_page.clone(),
                },
            );
        }
```

`isolated()` 里同样删掉 `base: None,` 那一行。

删掉整个 `base_for` 方法，并把防线的说明挪到 `window_label` 上：

```rust
    /// 该出站落在哪个窗口。未知出站返回 `None`。
    ///
    /// **`None` 必须当错误处理**：认不出这个出站，说明承载计划与出站表不
    /// 同步，悄悄放过去就等于让一个没有窗口承载的出站去发请求——与 §6.4
    /// 「绝不给一个默认值把流量发去别处」同源。这条防线原本立在 `base_for`
    /// 上，数据面基址的职责搬去条带之后挪到了这里。
    pub fn window_label(&self, name: &str) -> Option<String> {
        self.slots.get(name).map(|s| s.window.clone())
    }
```

- [ ] **Step 6: 改 `carrier.rs` 的测试**

以下测试断言的是已删除的 `base_for`，逐个处理：

- `shared_carrier_gives_host_relative_path_and_others_absolute` → **删除**
  （它断言的区分已不存在）
- `host_going_down_does_not_change_anyone_elses_base` → **删除**，其意图
  （宿主下线不连累邻居）已由 `one_outbound_failing_does_not_kill_its_neighbour_on_a_shared_carrier` 覆盖
- `host_defaults_to_first_enabled_when_unspecified` → 去掉 `base_for` 那行
  断言，保留 `host_name()` 的断言
- `isolated_carrier_gives_every_outbound_its_own_window` → 去掉两行
  `base_for` 断言，保留 `window_label` 与 `page_url_for`
- `build_dispatches_on_mode` → 把两行 `base_for` 断言换成 `window_label`
- `unknown_outbound_yields_none_never_a_default_base` → 改名并改断言：

```rust
    #[test]
    fn unknown_outbound_yields_none_never_a_default_window() {
        // §6.4：认不出的出站必须让调用方拿到 None 去拒绝。这条防线原本
        // 立在 base_for 上，基址职责搬走之后由 window_label 承担。
        let c = CarrierPlan::shared("A", &[("A", "https://a.com/")]).unwrap();
        assert_eq!(c.window_label("不存在的节点"), None);
        assert_eq!(c.page_url_for("不存在的节点"), None);
    }
```

- `one_outbound_failing_does_not_kill_its_neighbour_on_a_shared_carrier` →
  该测试用 `plan.base_for("宿主").unwrap()` 构造 `session_bases`，改为直接
  写绝对 URL（这正是改动后条带会给出的形态）：

```rust
            session_bases: vec![Some("https://host.example:18443".to_string())],
```

  以及 `let b_base = plan.base_for("邻居").unwrap().unwrap();` 改为：

```rust
        let b_base = "https://peer.example".to_string();
```

  该测试末尾断言 `b_js` 含 `https://peer.example/api/sync`、不含
  `host.example` 的两条**原样保留**——它们才是这个测试的意义。

- [ ] **Step 7: 改 `runtime_state.rs`：不再覆盖会话 0**

把 `src-tauri/src/runtime_state.rs:468-478` 替换为：

```rust
        // 条带给出的会话基址：**每一项都是绝对 URL，含会话 0**。承载页已经
        // 不是任何出站的 origin（它是本机的 http 壳），因此没有任何一条会话
        // 还能吃相对路径。
        let session_bases = entry.session_bases.clone();
        if session_bases.is_empty() {
            anyhow::bail!("出站「{}」的条带结果没有任何会话基址", p.name);
        }
        // §6.4 的防线原样保留，只是换了个不涉及基址推导的方法来守：认不出
        // 的出站必须报错，绝不能套一个默认基址把流量发去另一台服务器。
        carrier
            .as_ref()
            .expect("出站非空时承载计划必然已构建")
            .window_label(&p.name)
            .ok_or_else(|| anyhow::anyhow!("承载计划里没有出站「{}」", p.name))?;
```

同时更新 `build_startup_plan` 上方第 424-427 行那段注释——「承载计划先算：
**会话 0 的基址由它决定**」已不成立：

```rust
    // 承载计划先算：它要的页面 URL 来自条带（劫持成功时是本地端口）。
    // **会话基址不再经过它**——每条会话的基址都由条带全权给出（含会话 0），
    // 承载计划只负责「谁落在哪个窗口、那个窗口加载什么」。
```

- [ ] **Step 8: 改 `runtime_state.rs` 的测试并加一条等价性守卫**

`a_non_host_outbound_gets_an_absolute_base_taken_from_its_shard_page` 与它
上方那条断言 `vec![None, Some(...)]` 的测试，都改为「条带给什么就是什么」。
另外新增：

```rust
    /// 会话基址必须**逐字**来自条带，一项都不能被承载计划改写。
    ///
    /// 改写回来的后果不是「基址不好看」：承载页是本机那张空 HTML，被改写
    /// 的会话会把 Noise 握手发给它，握手拿到一段 HTML 后解析失败。
    #[test]
    fn session_bases_come_from_the_shard_verbatim() {
        let cfg = config_with(vec![
            proxy("A", &valid_pub(), &valid_priv()),
            proxy("B", &valid_pub(), &valid_priv()),
        ]);
        let shard = vec![
            crate::shard_setup::ShardPlanEntry {
                page_url: "https://a.example:18443/".to_string(),
                session_bases: vec![
                    Some("https://a.example:18443".to_string()),
                    Some("https://a.example:18444".to_string()),
                ],
                upstream: None,
                bypass_error: None,
            },
            crate::shard_setup::ShardPlanEntry {
                page_url: "https://b.example:18450/".to_string(),
                session_bases: vec![Some("https://b.example:18450".to_string())],
                upstream: None,
                bypass_error: None,
            },
        ];
        let plan = build_startup_plan(&cfg, &shard).unwrap();
        assert_eq!(plan.outbound_cfgs[0].session_bases, shard[0].session_bases);
        assert_eq!(plan.outbound_cfgs[1].session_bases, shard[1].session_bases);
        for cfg in &plan.outbound_cfgs {
            for b in &cfg.session_bases {
                let b = b.as_ref().expect("没有任何会话还能吃相对路径");
                assert!(b.starts_with("https://"), "数据面必须留在 https：{b}");
            }
        }
    }
```

- [ ] **Step 9: 跑全部 Rust 测试**

Run: `cd src-tauri && cargo test 2>&1 | tail -25`

预期：全绿（改动前 263 条）。**任何一条红都不要跳过**——这是等价重构，
红了就说明等价性破了。

- [ ] **Step 10: 确认零警告**

Run: `cd src-tauri && cargo build 2>&1 | grep -E "^warning" | head`
预期：无输出。若 `origin_of`（carrier.rs 里那个）此时变成死代码，
**先不删**——Task 4 会连同 `validate` 的 URL 校验一起处理。加
`#[allow(dead_code)]` 并注明「Task 4 删除」。

- [ ] **Step 11: 提交**

```bash
git add src-tauri/src/shard_setup.rs src-tauri/src/outbound/carrier.rs src-tauri/src/runtime_state.rs
git commit -m "refactor(carrier): 数据面基址改由条带全权提供，承载计划交出基址职责"
```

---

### Task 4: 承载页换成本机 http 壳

依赖 Task 1（`carrier_page::spawn`）与 Task 3（基址已解耦）。**顺序不能颠倒**：
在 Task 3 之前做这一步，会让所有非宿主出站的数据面基址被算成本地承载 server。

**Files:**
- Modify: `src-tauri/src/shard_setup.rs`（删 `ShardPlanEntry.page_url` 字段）
- Modify: `src-tauri/src/outbound/carrier.rs`（`shared`/`isolated`/`build` 签名，`validate` 简化，删 `origin_of`）
- Modify: `src-tauri/src/runtime_state.rs`（`build_startup_plan` 新增 `carrier_page_url` 入参）
- Modify: `src-tauri/src/main.rs`（起承载页 server 并向下传 URL）
- Modify: `src-tauri/capabilities/transport.json`（**仅** `description` 文案）

**Interfaces:**
- Consumes: `carrier_page::spawn() -> anyhow::Result<CarrierPage>`、`CarrierPage::url() -> String`（Task 1）
- Produces:
  - `CarrierPlan::build(mode: CarrierMode, host: &str, outbounds: &[&str], page_url: &str) -> anyhow::Result<CarrierPlan>`
  - `build_startup_plan(config: &Config, shard: &[ShardPlanEntry], carrier_page_url: &str) -> anyhow::Result<StartupPlan>`
  - `ShardPlanEntry` 不再有 `page_url` 字段

> **为什么删 `ShardPlanEntry.page_url` 而不是把 `carrier_page_url` 传进
> `plan_many`**：承载页 URL 现在全局唯一，每个 entry 存一份完全相同的值是
> 冗余；而且「条带不再关心承载页」正是 spec §5.3 那条职责划线本身。删掉
> 字段让编译器替我们找出所有还在依赖旧耦合的地方。

- [ ] **Step 1: 先改测试，表达新期望**

`src-tauri/src/outbound/carrier.rs` 的测试模块，把所有 `CarrierPlan::shared` /
`isolated` / `build` 的调用改成新签名，并新增：

```rust
    #[test]
    fn every_window_loads_the_one_carrier_page() {
        // 承载页全局唯一。两种模式的差别收敛到只剩「建几个窗口」。
        const PAGE: &str = "http://127.0.0.1:53119/";
        let s = CarrierPlan::shared("B", &["A", "B", "C"], PAGE).unwrap();
        assert_eq!(s.windows(), vec![("main".to_string(), PAGE.to_string())]);

        let i = CarrierPlan::isolated(&["A", "B", "C"], PAGE).unwrap();
        assert_eq!(i.windows().len(), 3);
        for (_, url) in i.windows() {
            assert_eq!(url, PAGE, "isolated 的每个窗口也加载同一张承载页");
        }
    }

    #[test]
    fn carrier_plan_no_longer_cares_about_server_urls() {
        // 出站 URL 不再进承载计划——它推导数据面基址的职责已经交给条带。
        // 这条钉死「别把 URL 校验又加回来」：那会让一个坏 URL 在两个地方
        // 各报一次，而修的时候只会想到其中一个。
        let c = CarrierPlan::shared("", &["只有名字"], "http://127.0.0.1:1/").unwrap();
        assert_eq!(c.window_label("只有名字").as_deref(), Some("main"));
    }
```

删除这两条已失去对象的测试：`malformed_url_is_an_error_not_an_empty_base`、
`origin_strips_path_and_keeps_explicit_port`。

`isolated_window_labels_are_actually_granted_by_the_capability_file` 保留，
只改调用签名——它守的是 ACL，与本次改动无关但绝不能失守。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test --lib carrier 2>&1 | tail -20`
预期：编译失败（参数个数不对）。

- [ ] **Step 3: 改 `carrier.rs` 的签名与实现**

`validate` 与 `resolve_host` 改为只认名字：

```rust
/// 校验出站表：非空、无重名、名字非空。
///
/// **URL 校验已随基址职责一起搬去条带**（`shard_setup` 的 `split_url` /
/// `origin_of`）。承载计划不再从出站 URL 推导任何东西，它只认名字——在这里
/// 再校验一次，只会让同一个坏 URL 在两处各报一次，而修的人只想得到其中一处。
fn validate(outbounds: &[&str]) -> anyhow::Result<Vec<String>> {
    if outbounds.is_empty() {
        anyhow::bail!("承载计划至少需要一个启用的出站");
    }
    let mut seen = BTreeMap::<&str, usize>::new();
    let mut out = Vec::with_capacity(outbounds.len());
    for (i, name) in outbounds.iter().enumerate() {
        if name.trim().is_empty() {
            anyhow::bail!("第 {} 个出站的名字为空", i + 1);
        }
        if let Some(prev) = seen.insert(name, i) {
            anyhow::bail!(
                "出站名重复：{name:?} 同时出现在第 {} 和第 {} 个",
                prev + 1,
                i + 1
            );
        }
        out.push(name.to_string());
    }
    Ok(out)
}

/// 宿主选择：空串取第一个启用的出站；指定了就必须存在。
fn resolve_host(host: &str, entries: &[String]) -> anyhow::Result<String> {
    let host = host.trim();
    if host.is_empty() {
        return Ok(entries[0].clone());
    }
    if entries.iter().any(|n| n == host) {
        Ok(host.to_string())
    } else {
        // 报错而非退回第一个：用户点名要拿某个节点当宿主是个明确意图，
        // 悄悄换一个等于把这个判断作废。
        anyhow::bail!(
            "carrier-host 指向不存在的出站：{host}（可选：{}）",
            entries.join("、")
        )
    }
}
```

三个构造函数：

```rust
    /// `shared`：一个 WebView 加载承载页，全部出站挂在上面。
    ///
    /// `host` 传空串表示「未指定」，取第一个启用的出站。**宿主这个概念在
    /// 基址职责搬走之后只剩语义归属与错误信息**——它不再影响任何一个出站的
    /// 请求发往何处（那些全部由条带给出的绝对 URL 决定）。
    pub fn shared(host: &str, outbounds: &[&str], page_url: &str) -> anyhow::Result<Self> {
        let entries = validate(outbounds)?;
        let host = resolve_host(host, &entries)?;
        let mut slots = BTreeMap::new();
        for name in &entries {
            slots.insert(
                name.clone(),
                Slot {
                    window: SHARED_WINDOW.to_string(),
                    page_url: page_url.to_string(),
                },
            );
        }
        Ok(Self {
            mode: CarrierMode::Shared,
            host,
            slots,
        })
    }

    /// `isolated`：每出站一个隐藏窗口。
    ///
    /// 它们加载的是**同一个**本机承载 server 的同一张 HTML —— 与 `shared`
    /// 的差别收敛到只剩「建几个窗口」。
    pub fn isolated(outbounds: &[&str], page_url: &str) -> anyhow::Result<Self> {
        let entries = validate(outbounds)?;
        let host = entries[0].clone();
        let mut slots = BTreeMap::new();
        for name in &entries {
            slots.insert(
                name.clone(),
                Slot {
                    window: format!("{ISOLATED_WINDOW_PREFIX}{name}"),
                    page_url: page_url.to_string(),
                },
            );
        }
        Ok(Self {
            mode: CarrierMode::Isolated,
            host,
            slots,
        })
    }

    /// 按模式构造。
    pub fn build(
        mode: CarrierMode,
        host: &str,
        outbounds: &[&str],
        page_url: &str,
    ) -> anyhow::Result<Self> {
        match mode {
            CarrierMode::Shared => Self::shared(host, outbounds, page_url),
            CarrierMode::Isolated => Self::isolated(outbounds, page_url),
        }
    }
```

删掉整个 `origin_of` 函数（连同 Task 3 给它加的 `#[allow(dead_code)]`）。

同时更新模块头注释：`shared` 那段「宿主用相对路径（完全同源），其余出站用
绝对 URL」已不成立，改为：

```rust
//! - **`shared`（默认）**：一个 WebView 加载本机承载页（`crate::carrier_page`），
//!   全部出站挂在上面，各自用**绝对 URL** 发请求。承载页是本机的 http 壳，
//!   与任何出站都不同源，因此不存在「谁能吃相对路径」的区分。
//!   跨域名可行已实测确证，见
//!   `docs/superpowers/spikes/2026-08-25-cross-origin-carrier-spike.md`。
```

- [ ] **Step 4: 删 `ShardPlanEntry.page_url`**

`src-tauri/src/shard_setup.rs`：

1. `ShardPlanEntry` 结构删掉 `page_url` 字段及其文档注释
2. `degraded` 删掉 `page_url: server_url.to_string(),` 那一行
3. `Prepared` 结构删掉 `page_url` 字段
4. `plan_many` 里删掉 `let page_url = format!("{}/", data_origin(ports[0]));`
   以及两处 `page_url` 的搬运（`Prepared { ... page_url, ... }` 与
   `ShardPlanEntry { page_url: p.page_url, ... }`）
5. 模块头 `ShardPlanEntry` 相关注释里提到「主 WebView 应加载的 URL」的地方
   一并删掉——承载页 URL 与条带**再无关系**，这正是这次改动的要点
6. 测试里所有 `page_url:` 字段初始化与断言一并删除

- [ ] **Step 5: 改 `runtime_state.rs`**

`build_startup_plan` 签名加一个入参，并更新构造 `CarrierPlan` 的那段：

```rust
pub fn build_startup_plan(
    config: &wsieve_config::Config,
    shard: &[crate::shard_setup::ShardPlanEntry],
    carrier_page_url: &str,
) -> anyhow::Result<StartupPlan> {
```

```rust
    // 承载计划只负责「谁落在哪个窗口、那个窗口加载什么」。窗口加载的永远是
    // 本机承载页（`carrier_page`），与条带、与出站 URL 都无关；每条会话的
    // 基址则由条带全权给出（含会话 0）。
    let carrier = if config.proxies.is_empty() {
        None
    } else {
        let mode = crate::outbound::carrier::CarrierMode::parse(&config.carrier)?;
        let names: Vec<&str> = config.proxies.iter().map(|p| p.name.as_str()).collect();
        Some(crate::outbound::carrier::CarrierPlan::build(
            mode,
            &config.carrier_host,
            &names,
            carrier_page_url,
        )?)
    };
```

更新 `StartupPlan` 上方第 288-292 行那段文档——「会话基址与承载页面 URL 都
取决于 hosts 劫持有没有成功」只剩前半句成立：

```rust
/// **本地条带的结果是输入而不是输出**：会话基址取决于 hosts 劫持有没有成功
/// （成功则各会话分到本地转发端口，失败则退回服务端原始 origin），而那是
/// 异步 IO，不属于"纯"的范围。承载页面 URL 则**与条带无关**，它恒为本机
/// `carrier_page` server 的地址，由调用方递进来。
```

测试辅助 `plan_of`（第 764 行）与所有测试里的 `build_startup_plan(...)` 调用
补上第三个参数，统一用 `"http://127.0.0.1:53119/"`。

`runtime_state.rs` 的测试里每一处构造 `ShardPlanEntry { ... }` 都要删掉
`page_url:` 那一行（字段已在 Step 4 删除，不删编译不过）——包括 Task 3
新增的那条测试。其中
`a_non_host_outbound_gets_an_absolute_base_taken_from_its_shard_page` 那条测试的
意图已被 Task 3 新增的 `session_bases_come_from_the_shard_verbatim` 完全覆盖，
**删除它**——它断言的「基址从各自的 page_url 推导」正是本次要消灭的耦合。

Run: `grep -n "page_url" src-tauri/src/runtime_state.rs`
预期：无输出。

- [ ] **Step 6: `main.rs` 接线**

在 `let sessions: Vec<(&str, usize)> = ...` **之前**（即 `plan_many` 与
`tun_prepare` 的全部准备工作之前）插入：

```rust
    // ── 起承载页 server（设计文档 2026-09-10 §5.8 的第 -1 步）────────────
    //
    // 排在最前面：它 bind 的是 127.0.0.1:0，不解析域名、不出网、不依赖
    // hosts 或 TUN 任何一步的结果，因此越早失败越早报。
    //
    // **它不需要 bypass 路由**（对比条带转发器必须有）：TUN 捕获的是出网
    // 流量，而这个 server 的连接两端都在回环上，从不离开本机。
    //
    // 失败即 exit(2)，不降级：没有承载页就没有传输，静默降级只会变成
    // 「应用起来了但永远连不上，且没有一条能解释原因的日志」。
    let carrier_page = match tauri::async_runtime::block_on(carrier_page::spawn()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("承载页 server 启动失败: {e:#}");
            std::process::exit(2);
        }
    };
    let carrier_page_url = carrier_page.url();
```

`build_startup_plan` 的调用（第 262 行）补上第三个参数：

```rust
    let plan = match runtime_state::build_startup_plan(&config, &shard.entries, &carrier_page_url) {
```

在 `let shard_guard = ...`（第 321 行）附近，把承载页句柄一并持有住：

```rust
    // 承载页 server 必须活到进程结束——句柄一 drop，监听就停，承载窗口
    // 刷新时会白屏。它不托管任何系统状态（不像 hosts / 系统代理），因此
    // **不进** RunEvent::Exit 的摘除序列，持有到 main 结束即可。
    let _carrier_page = carrier_page;
```

- [ ] **Step 7: 更新 capability 描述**

`src-tauri/capabilities/transport.json` 的 `description`，把开头
「传输窗口：加载远端服务器的伪装页，只授权 emitter 需要的二进制通道命令。
此处绝不能出现任何控制类命令——该页面的 JS 由服务器控制，服务器被攻破即
等同于这些命令被攻破。」替换为：

```
传输窗口：加载本机承载页 http://127.0.0.1:{随机端口}（crate::carrier_page），只授权 emitter 需要的二进制通道命令。此处绝不能出现任何控制类命令。页面 JS 不再由远端服务器提供（2026-09-10 起），但这条纪律不放松——授权面越小，将来任何一次承载形态变更的代价就越小。urls 里的 http://127.0.0.1:* 正是承载页那一条，不要放宽成 http://**:*。
```

**`windows`、`permissions`、`urls`、`local` 四个字段一个字都不改。**

验证——只有 `description` 变了，授权面零变化：

```bash
git diff --stat src-tauri/capabilities/transport.json
python3 -c "
import json, subprocess
new = json.load(open('src-tauri/capabilities/transport.json'))
old = json.loads(subprocess.check_output(['git','show','HEAD:src-tauri/capabilities/transport.json']))
for k in ('windows','permissions','urls','local','remote','identifier'):
    assert new.get(k) == old.get(k), f'{k} 变了：{old.get(k)} -> {new.get(k)}'
print('授权面零变化，只有 description 更新')
"
```

预期：打印「授权面零变化」。**断言失败就是把授权面改宽了**——本设计选回环
形态的一半理由就是不必放宽它（spec §5.6）。

- [ ] **Step 8: 全量测试**

Run: `cd src-tauri && cargo test 2>&1 | tail -25`
预期：全绿。

Run: `cd src-tauri && cargo build 2>&1 | grep -E "^warning" | head`
预期：无输出。

- [ ] **Step 9: 提交**

```bash
git add src-tauri/src/ src-tauri/capabilities/transport.json
git commit -m "feat(carrier): 承载页改用本机 http 壳，取回 IPC raw 快路径"
```

---

### Task 5: 真机端到端验收

**不可省。** spike 用的是手写 `fetch`，而 Tauri 的 `__TAURI_INTERNALS__.invoke`
会多带若干头（`Tauri-Callback` 等）。scheme 既然不被封，理应同样能通，但
**不能拿探针的成功替真实 app 背书**。

**Files:** 无代码改动。产出是走查记录。

- [ ] **Step 1: 起真实服务端并跑通基线**

按 `docs/superpowers/plans/2026-09-07-live-walkthrough-checklist.md` 的既有
流程部署服务端。

> **重启纪律**：协议或承载形态变更后，常驻的演示栈进程必须重启，否则会
> 撞上幽灵故障（旧进程还在按旧形态跑）。

- [ ] **Step 2: 带窗口跑一次，确认承载页 origin**

Run: `WSIEVE_SHOW_WINDOW=1 cargo run --manifest-path src-tauri/Cargo.toml`

判据：承载窗口地址栏 / devtools 显示 `http://127.0.0.1:{某个高端口}/`。

- [ ] **Step 3: 确认走的是 raw 而非 base64**

在承载窗口的 devtools console：

```js
window.__wsieve.raw
```

预期：`true`。

同时检查应用日志**不出现** `JSON body 缺少字符串字段 f` —— 那条只有走
base64 回退时才会出现。

- [ ] **Step 4: 数据面端到端**

判据：
1. 出站进入 `Connected`
2. `curl -x socks5h://127.0.0.1:{socks 端口} https://ifconfig.me` 返回**服务端
   出口 IP**，与直连对照不同

- [ ] **Step 5: 抓包确认没有明文出网**

Run: `sudo tcpdump -i any -n 'tcp port 80' -c 20`（跑代理流量的同时）

判据：**离开本机的流量全部是 443 的 TLS**。回环上的 `127.0.0.1:{carrier_port}`
出现是正常的（那就是承载页），但不能有任何**非回环**的 80 端口流量。

这一条不是因为 §5.3 会导致裸奔（它不会，见设计文档该节），而是这次动的
正是 scheme，改坏了它没有第二道防线。

- [ ] **Step 6: 确认 CPU 改善（痛点本身）**

跑一次大流量下载，用 Activity Monitor 或 `top` 观察进程 CPU。

判据：较改动前明显下降。设计文档记录的实测基线是 base64 48.8 MB/s @CPU 86%
→ raw 114.9 MB/s @CPU 34%。**如实记录实际数字**，与基线不符也照实写——
那说明还有别的瓶颈，是下一步的输入而不是要藏起来的东西。

- [ ] **Step 7: 记录走查结果**

把第 2-6 步的实际输出追加到
`docs/superpowers/plans/2026-09-07-live-walkthrough-checklist.md`，注明日期与
本次改动。

- [ ] **Step 8: 提交**

```bash
git add docs/superpowers/plans/2026-09-07-live-walkthrough-checklist.md
git commit -m "docs(carrier): 承载页 http 化的真机走查记录"
```
