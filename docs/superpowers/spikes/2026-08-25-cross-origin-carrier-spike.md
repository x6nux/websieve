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

## 5. 实测结果

（下一节由实跑填充）
