# Spike：单 WebView 能否承载多出站（`carrier: shared`）

> 状态：**进行中**。本文件遵循「先落盘、再完善」——每确证一条就写一条，
> 未验证的部分显式标注「待验证」与验证方法，绝不用推测填充。
>
> 对应：阶段 2 计划 Part A Task 2 / 设计文档 §14 / §15 待实测项 #3

## 结论摘要

| 问题 | 结论 | 证据强度 |
|---|---|---|
| sid 走 query 还是 cookie？ | **query** | 已确证（代码） |
| 服务端 CORS 是否放行跨域名？ | **是**（本阶段 Task 1 改） | 已确证（测试） |
| 真实 WKWebView 能否在本会话跑起来？ | **能**（headless 亦可） | 已确证（实跑） |
| 单 WebView 能否对两个不同 origin 同时建会话并跑流量？ | 见下「实测结果」 | 见下 |

---

## 1. sid 走 query，不走 cookie（已确证）

这是 `carrier: shared` 成立与否的**全部押注**：若 sid 在 cookie 里，
WKWebView 的 ITP（Intelligent Tracking Prevention）会拦掉跨站第三方
cookie，跨域名握手必然失败。

**代码事实（代码为准，spec 散文有误）：**

- `crates/wsieve-xhttp/src/client.rs:133` — 握手上行
  `let path = format!("/api/sync?n=0&sid={}", sid_b64);`
- `crates/wsieve-xhttp/src/client.rs:205` — 下行长 GET
  `let downlink_path = format!("/api/events?sid={}", shared.lock().await.sid_b64);`
- `crates/wsieve-xhttp/src/client.rs:453` — 后续上行
  `let path = format!("/api/sync?n={}&sid={}", seq, sid_b64);`
- `crates/wsieve-server/src/lib.rs:228` 与 `:250` — 服务端两处均从 query 取：
  `query_param(&uri, "sid")`

**反向确证**：全仓（排除 `target/`）对 `cookie` 的大小写不敏感搜索，在
`crates/` 与 `src-tauri/src/` 下**零命中**。sid 与 cookie 无任何关联。

> `emitter.js` 里的 `credentials:'include'` 只是让 fetch 带上凭据（若有），
> 并**不意味着** sid 依赖 cookie。它对 sid 的传递没有作用。

**因此 §3.2 那条推论在代码层面成立。** 这也纠正了 spec §6.2 散文中
「sid 在 cookie」的错误表述——该表述曾导致一次被公开撤回的错误架构结论。

## 2. 服务端 CORS 已放行跨域名（已确证）

提交 `dbb300d`。`cors_origin` 由「Origin 与 Host 同域名」放宽为「回显任意
形态合法的 Origin」。

放宽的**不是**防线：`apply_cors` 只加在**认证成功**的响应上；未认证请求
一律走伪装处理器，那条路径根本不经过 `cors_origin`
（`crates/wsieve-server/src/lib.rs` 的 `fallback`）。探测者发不出合法 msg1
就永远看不到任何 CORS 痕迹。

原「同域名不同端口」的多端口条带场景是新判据的一个特例，仍被覆盖
（测试 `same_domain_different_port_still_allowed`）。

## 3. 真实 WKWebView 可在本会话运行（已确证）

这是能否做**真实**端到端确证的前提，先单独验证以免把时间压在不可行的路径上。

- `swiftc` 可用（Apple Swift 6.2.4），macOS SDK 就位。
- 最小 WKWebView 探针：`setActivationPolicy(.prohibited)`（无 Dock 图标、
  无窗口）下仍能完成导航并执行 JS。实测输出 `PROBE_EXIT done=true ok=true`。
- 结论：**无需 GUI 窗口**即可驱动真实 WebKit 网络栈。Safari 与 WKWebView
  共用同一网络栈，故此探针对 Tauri 的 WKWebView 有代表性。

## 4. 与计划书的偏差（已确证，已规避）

计划 Task 2 Step 2 要求 `sudo` 改 `/etc/hosts` 加 `wsieve-a.test` /
`wsieve-b.test`。**本环境 `sudo` 需要密码，不可用。**

替代方案（更优，且无副作用）：使用公共泛解析回环域名——
`localtest.me` 与 `lvh.me`，二者均解析到 `127.0.0.1`（已用
`dscacheutil`/`ping` 经**系统解析器**确认，不只是 `dig`）。

优于 hosts 方案的两点：
1. **无需 sudo、不改系统状态**，因此不存在「spike 结束忘记清理」的残留风险。
2. 二者是**不同的 eTLD+1**，即真正的 *cross-site*（而非仅 cross-origin）。
   ITP 的第三方 cookie 拦截正是按 eTLD+1 划界的，故这是**比计划书的
   `.test` 双域名更严格**的测试条件。

> 计划书原文的 `.test` 方案本身没错（RFC 6761 保留域），只是它依赖 sudo。
> 本环境下不可行，故换用等价且更严格的方案。

## 5. 实测结果（已确证）

**判定：`carrier: shared` 成立。** 单个真实 WKWebView 同时承载了两个
*cross-site* 出站的完整握手与数据面。

复现：`scripts/spike-cross-origin.sh`（退出码 0 = 成立，3 = 不成立）。

### 5.1 三条判据

| # | 判据 | 结果 |
|---|---|---|
| 1 | 同源握手（页面在 A → 请求发往 A）—— 基线 | ✅ 成功，mux = Yamux |
| 2 | **跨域名握手（页面在 A → 请求发往 B）** | ✅ **成功，mux = Yamux** |
| 3 | 两个出站各跑真实数据往返 | ✅ A 28B、B 39B，逐字节一致 |

第 2 条是本 spike 的**全部意义**：B 与 A 不同 eTLD+1，是真正的 cross-site
请求，由真实 WebKit 网络栈裁决。它通过，意味着 ITP 与 CORS 都没有拦住
XHTTP 的握手与数据面。

数据面走的是完整真实链路：`XhttpConn`（真实 Noise_IK 握手）→ `mux_factory`
→ `StripeDialer` → echo 目标，逐字节比对一致。**没有任何 mock**：fetch 由
真实 WKWebView 发出，Rust 侧的 `HttpTransport` 实现只是把浏览器的结果接回
协议层，协议层完全不知道自己在跟浏览器说话。

### 5.2 反证：这个 spike 真的能测出失败

一个「跑通了」的 spike 若无法失败，什么也没证明。故做了**反证对照**：
把 `cors_origin` 临时改回改动前的「Origin 与 Host 同域名」判据，其余不变，
重跑同一脚本：

```
== [1/3] 同源握手 ==            ✅ A 握手成功，mux = Yamux
== [2/3] 跨域名握手 ==          ❌ B 跨域名握手失败: WebView fetch 失败: TypeError: Load failed
[bridge] WebView 侧错误: POST id=3 失败: TypeError: Load failed
=== 判定：carrier: shared 不成立 ===   （退出码 3）
```

即：**同源仍通、跨域名被浏览器拦掉**，失败信号来自 WebKit 自己
（`TypeError: Load failed` 是 CORS 拦截响应时 fetch 的标准报错）。
这确证了：
1. 该 spike 确实在测跨域名能力，不是在测别的；
2. Task 1 的 CORS 放宽**正是**跨域名成立的必要条件；
3. 失败路径会被如实报告，不会被静默吞掉。

反证后已恢复放宽实现（`git diff` 对该文件为空）。

### 5.3 内存实测

WKWebView 是多进程架构（WebContent / Networking / GPU 各为 XPC 进程），
且**同一进程内的多个 WKWebView 默认共用进程池**，故内存并非按 WebView
数线性增长。实测（同一进程建 N 个 WKWebView，各自加载页面，RSS 合计）：

| WebView 数 | WebKit 相关 RSS 合计 | 相对基线增量 | 每多一个的边际成本 |
|---|---|---|---|
| 基线（无本进程 WebView） | 213 072 KB | — | — |
| 1 | 341 072 KB | +128 000 KB | — |
| 2 | 368 480 KB | +155 408 KB | +27 408 KB |
| 4 | 423 888 KB | +210 816 KB | ≈ +27 400 KB／个 |

> 基线含本机其它 WebKit 使用者，绝对值仅供比较，看**增量**。

**对设计文档的一处修正**：§9.1 把 isolated 的代价记为「N×」。实测表明
它是 **1× + 约 27 MB/出站**（首个约 128 MB，其后每个约 27 MB），因为
进程池共用。isolated 比 shared 贵，但**没有 N× 那么贵**——这在将来
若因别的原因必须退回 isolated 时，是一条有利信息，不应被旧的 N× 说法吓阻。

shared 形态下（本 spike 实跑，1 个 WebView 承载 2 个出站）WebKit 相关
RSS 合计约 348 MB，与「1 个 WebView」量级一致 —— 即**增开出站不增开
WebView，内存不随出站数增长**。

## 6. 对 Task 3-13 的结论

- **`carrier: shared` 保持为默认值**，计划书 Part B–D 按原样推进，无需改写。
- 出站管理器可让全部出站共享同一个 WebView，各自用**绝对 URL**
  （`WebViewTransport::with_base` 已就绪，`src-tauri/src/bridge.rs:269`）。
- Task 10 的承载器仍需保留 `isolated` 作为可选实现（§4.2 纪律③：承载方式
  对出站层透明），但它不是默认路径。
- 服务端 CORS 的放宽（Task 1）是 shared 的**必要条件**，已由反证确认。

## 7. 仍未验证的部分（诚实标注）

以下不在本 spike 的射程内，Part B–D 若依赖它们需另行验证：

1. **HTTPS / 真实 CDN 场景**：本 spike 走 plain HTTP + 回环域名。真实部署是
   HTTPS 经 CDN。CORS 与 ITP 的行为不因 scheme 改变（二者都不是 scheme
   敏感的），但**证书与 CDN 边缘对 `Origin` 头的转发**未测。
   *验证方法*：把两个服务端换成 `--deployment` 的 TLS 模式、用真实证书域名重跑。
2. **出站数 > 2**：只测了 2 个。无理由认为 3+ 会不同（同一策略），但未实测。
   *验证方法*：给 spike 加 `SPIKE_OUTBOUNDS=N` 循环建会话。
3. **长时稳定性**：本 spike 是「建立 + 一次往返」，跑完即退。ITP 的某些
   限制是**时间/交互累积型**（如 7 天存储上限），对不依赖 cookie/storage 的
   sid-in-query 不适用，但长时行为未测。
   *验证方法*：让 spike 常驻数小时并周期性跑往返。
4. **Tauri 内的 WKWebView**：本 spike 用独立 Swift 宿主的 WKWebView。它与
   Tauri 用的是同一套 WebKit 与同一网络栈，但 Tauri 的
   `WKWebViewConfiguration`（如 `background_throttling`）未逐项对齐。
   *验证方法*：Part D 集成后用真实 app 跑一次 `--with-app` 式验收。

