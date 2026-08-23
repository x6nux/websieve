# websieve 设计文档：基于 WebView 的完全仿真 HTTPS 代理

- 日期：2026-08-23
- 状态：已评审（第 4 轮通过）/ 待规划
- 传输设计参考：Xray-core XHTTP（`transport/internet/splithttp/`）
- 协议版本：1

---

## 1. 概要

websieve 是一个把网络传输层放进 WebView 执行的代理客户端。代理流量由浏览器内核通过 `fetch` 真实发出，因此 TLS 指纹（JA3/JA4）、HTTP/2 指纹（SETTINGS 帧顺序、HPACK 行为）、以及请求头的生成与排序**全部由浏览器内核本体产生**，不是模仿。

与 uTLS 一类方案的根本差异：uTLS 是**复刻**一个 ClientHello，websieve 是**让浏览器真的去握手**。前者永远在追赶浏览器版本，后者天然与用户机器上的浏览器同版本。

服务端是一个**真实网站**，代理端点是它的一个 API 路径。WebView 加载该网站首页后，所有代理请求都是这个页面发出的**同源 XHR**。

---

## 2. 目标与非目标

### 目标

- 代理流量的 TLS 与 HTTP 指纹与宿主浏览器内核完全一致
- 网页浏览 / API / IM 场景下的低延迟体验
- macOS、Windows、Android 三平台共用同一份传输核心
- 客户端与服务端协商 mux 线格式，**5 种全部实现**并暴露给用户
- 部署形态可配置：直连（有效证书）或 CDN 前置（源站可无证书/自签证书）
- 伪装站点默认为 nginx 默认页副本，可配置反向代理任意上游站点
- 握手延迟最小化：结构性连接复用保底，机会性 TLS 0-RTT / HTTP/3 叠加（§6.8）
- 抗主动探测：探测者得到的是一个正常网站的正常响应

### 非目标

- **iOS**（见 §3.3，三重硬阻断）
- 高吞吐场景（视频、大文件下载）。架构不阻止，但不为此优化
- 与现有 Xray/sing-box 服务端互通（预留接口边界，不在首版实现）
- 会话恢复 / 断点续传（见 §11）

---

## 3. 平台范围与已核实的技术约束

本节所有结论均经过核实并附来源，**不是推测**。设计中的多处取舍直接由这些约束决定。

### 3.1 WebView 能力矩阵

| 能力 | macOS WKWebView | Windows WebView2 | Android WebView |
|---|---|---|---|
| fetch 流式**上传** | ❌ | ✅ Chromium 105+ | ✅ |
| fetch 流式**下载** | ✅ iOS/macOS 10.3+ | ✅ | ✅ |
| 后台 JS 存活 | ✅ | ✅ | ⚠️ timer 节流 |
| 引擎更新策略 | 随 OS，不可独立更新 | Evergreen 自动更新 | Google Play 独立更新，约 6 周 |

来源：
- Chrome 流式上传起自 Chromium 105：https://developer.chrome.com/docs/capabilities/web-apis/fetch-streaming-requests
- Safari 至 26.6 仍不支持；STP 250（2026-08-13）才首次落地初始支持：https://webkit.org/blog/18191/release-notes-for-safari-technology-preview-250/
- Interop 2026 已将其列为焦点项：https://webkit.org/blog/17818/announcing-interop-2026/
- 响应流 Baseline 自 2019-01 起 widely available：https://developer.mozilla.org/en-US/docs/Web/API/Response/body
- WebView 引擎版本策略：https://v2.tauri.app/reference/webview-versions/

### 3.2 由约束直接导出的设计决定

| 约束 | 导出的决定 |
|---|---|
| WKWebView 不支持流式上传，且引擎随 OS 不可独立更新 → 存量设备数年内无法使用 | **上行只用 packet-up（多个独立 POST）**。`stream-up` 不实现 |
| 流式上传强制 HTTP/2 或 HTTP/3 + HTTPS，h1.1 直接 reject（`net::ERR_H2_OR_QUIC_REQUIRED`），不降级 | 即便未来启用 stream-up，也需服务端强制 h2/h3 |
| Android Chromium timer 节流：后台每秒 1 次 → 10 秒后 0.01s/s 预算 → 5 分钟后每分钟 1 次 | 见 §9.3。**WebSocket/WebRTC 长连接被官方豁免节流**，是可用的规避路径 |
| Tauri IPC 的 JSON 路径对大数据是官方承认的瓶颈 | 必须走二进制快路径：`Channel<&[u8]>` / 顶层 `Uint8Array`。**切勿把二进制嵌在 object 里**，会退化成数字数组 |

Tauri IPC 官方说明：https://v2.tauri.app/develop/calling-rust/
Chromium 节流：https://developer.chrome.com/blog/timer-throttling-in-chrome-88

### 3.3 iOS 被排除的理由

三重**互相独立**的硬阻断，任一条都足以否决：

1. App 进后台后 WKWebView 的 WebContent 进程被挂起，JS 全停。Cordova CB-10657 官方结案 **Won't Fix**，标签 `wkwebview-baked-in`：https://issues.apache.org/jira/browse/CB-10657
2. App Extension 明文禁止长期后台任务（*"an app extension cannot: Perform long-running background tasks"*），且 NEPacketTunnelProvider 内存上限约 50MiB，塞不下 Web 引擎
3. Tauri 移动端创建不了额外 WebView（#10012、#11794 至今 open），绕道 wry `build_as_child` 会丢失 `window.__TAURI__` 桥

附带问题：per-app VPN 下 WKWebView 流量不走 NE（Apple 已知问题 FB9891393），且 WKWebView 无 API 可编程设置代理。

**未来重评的前提**：Safari 27 释出流式上传 + 存量设备形成基线（数年），且后台冻结那一条找到合规解法。后者是 OS 级行为，很可能永远无解。

### 3.4 一个被摘掉的误判

「Tauri 移动端不支持隐藏 WebView」曾被列为阻断，**实际不构成阻断**：我们不需要额外开一个 WebView，直接用 Tauri App 的**主 WebView**——UI 是它的 DOM，传输逻辑是它的后台 JS，同一个 WebView 承担两件事。桌面端收进托盘时窗口隐藏，用 `backgroundThrottling: "disabled"` 压制节流（macOS 14+ 支持）。

---

## 4. 架构

```
┌───────────────── Tauri App（单进程）─────────────────┐
│                                                      │
│   Rust                              WebView          │
│   ┌────────────┐                   ┌──────────────┐  │
│   │  socks5    │  明文 TCP          │  emitter.ts  │  │
│   │      ↓     │                   │              │  │
│   │  mux       │                   │  fetch POST ─┼──┼─→ 上行
│   │      ↓     │  Channel<&[u8]>   │  fetch GET  ←┼──┼─── 下行
│   │  noise     │  ═══════════════▶ │  (流式读)     │  │
│   │      ↓     │                   │              │  │
│   │  xhttp     │  invoke(Uint8Array)│             │  │
│   │      ↕     │  ◀═══════════════ │              │  │
│   │  transport │                   └──────────────┘  │
│   └────────────┘             document.location =     │
│                              https://your-site/      │
└──────────────────────────────────────────────────────┘
```

### 4.1 分层

```
SOCKS5 连接们（几十条并发）
      ↓
   mux            多路复用 + 流控背压      ← 5 种线格式，握手时协商
      ↓
   noise          Noise_IK 加密           ← snow crate
      ↓
   XhttpConn      TU 分帧 + seq + 合流     ← 唯一自研层
      ↓
   fetch          浏览器发真实 HTTPS
```

`XhttpConn` 对上暴露为一条普通的 `AsyncRead + AsyncWrite`，上面两层直接叠加：

```rust
let reply  = transport.post("/api/sync?n=0", tu(msg1)).await?;  // 握手
let hs     = client_finish(reply.body)?;                        // 解 msg2
if hs.fallback { warn!("服务端不支持 {:?}，已回退 yamux", prefs); }
let noise  = TransportCipher::new(hs);
let mux    = mux_factory(hs.mux_id, noise).await?;              // 见 §7
// 每条 SOCKS5 连接 → mux.open().await?
```

### 4.2 为什么必须有 mux

一条 XHTTP 会话只能有一条下行长 GET。没有 mux，几十条并发 TCP 就需要几十条会话、几十次 Noise 握手、几十条长 GET。

有 mux 后，全程只占用 **1 条长 GET + 最多 8 条在途 POST = 9 条 h2 流**，远低于任何并发上限。

---

## 5. 组件与边界

| 模块 | 职责 | 依赖 | 可测性 |
|---|---|---|---|
| `socks5` | 收 SOCKS5 连接，产出 `(目标地址, 双向字节流)` | 无 | 纯 TCP，直接测 |
| `noise` | Noise_IK 握手 + AEAD 帧封装/解封 | 无 | 纯函数 + 双端对跑 |
| `mux` | `Mux` trait + 5 个实现 + 协商 | 各 mux crate | 双端对跑 |
| `xhttp` | TU 分帧、会话 ID、seq、上行窗口 | `transport` | 靠 `ReqwestTransport` |
| `transport` | **trait**：发 POST / 开流式 GET | — | — |
| `bridge` | Tauri IPC 胶水，`transport` 的 WebView 实现 | Tauri | 需 WebView |
| `emitter.ts` | 收请求描述 → `fetch` → 回传字节 | — | 需 WebView |
| `server` | axum 服务，伪装站点 + `/api/*` 端点 | 共享上述 crate | 集成测试的对端 |

### 5.1 支点：`HttpTransport` trait

整个设计的可测试性压在这一个边界上：

```rust
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// 上行 POST。返回状态码与响应体（n=0 时响应体 = msg2）。
    async fn post(&self, path: &str, body: Bytes) -> Result<PostReply>;

    /// 下行：开一条流式 GET，返回字节流
    async fn get_stream(&self, path: &str) -> Result<BoxStream<'static, Result<Bytes>>>;
}

pub struct PostReply { pub status: u16, pub body: Bytes }
```

两个实现，**协议层对此完全无感**：

- `WebViewTransport` — 生产路径，经 Tauri IPC 交给 `emitter.ts`
- `ReqwestTransport` — 测试路径，直接用 reqwest 发**真实 HTTP 请求**

`ReqwestTransport` **不是 mock**。它是一个完整的、真实的 HTTP 传输实现，向真实服务端发真实请求。它与 `WebViewTransport` 的唯一区别是 TLS 握手由 rustls 而非浏览器内核完成。因此：

**`socks5 → mux → noise → xhttp → 真实服务端 → 真实目标站点` 的完整链路集成测试可以在 `cargo test` 里跑完**，不需要起 WebView、不需要点界面。WebView 只在最后一公里的端到端验收中出现。

这是选择「薄 JS」方案（协议逻辑全在 Rust）而非「厚 JS」的全部理由。

---

## 6. 协议规格

**全文字节序统一为大端（网络序），无例外。**

### 6.1 传输单元 TU

上下行统一用同一个单元，外层带**明文**长度前缀（外层必须明文，否则解密前无法切分流）：

```
TU（Transport Unit）
┌────────────┬────────────────────────┐
│ u16 len    │  Noise 密文（len 字节）  │
└────────────┴────────────────────────┘
```

Noise 密文解密后是明文帧：

```
┌─────────┬──────────────┬──────────┬─────────┐
│ u8 type │ u16 p_len    │ payload  │ padding │
└─────────┴──────────────┴──────────┴─────────┘
```

| 字段 | 说明 |
|---|---|
| `type` | `0x01` DATA ／ `0x02` PADDING（保活/空闲心跳，`p_len=0`）。其余保留 |
| `p_len` | payload 字节数 |
| `padding` | 长度 = 明文总长 − 3 − `p_len`，**无需显式编码**（解密后明文总长已知） |

**长度约束链**：

```
Noise 单消息密文上限          65535   （协议规定）
  − AEAD tag                    16
= 明文上限                    65519
  − 帧头 (type + p_len)          3
  − padding 最大值            1000
= payload 实际上限            64516 字节
```

**承载方式**：

```
上行  POST body   =  TU ‖ TU ‖ …   （Content-Length 界定整体，TU 前缀界定内部；
                                     一个 POST body 可含 1..N 个 TU）
下行  GET stream  =  TU TU TU …     （无尽流，全靠 TU 前缀切分）
```

下行读取逻辑是确定的：读 2 字节 → 得 `len` → 读 `len` 字节 → 解密 → 得一个明文帧。服务端重组 = 按 seq 排序后把各 POST body **首尾拼接**成上行字节流，再按同样规则解析 TU。

padding 长度范围 0–1000 字节，每帧独立随机（Xray 经验值为 100–1000，`xpadding.go:179-188`；TU 有外层随机长度的加成，允许 0）。

### 6.2 HTTP 端点与形态

```http
# 握手 + 上行：POST，seq 在 query，sid 在 cookie
POST /api/sync?n=17 HTTP/2   ← 浏览器↔CDN/源站为 h2 或 h3（§6.8）；CDN Flexible 模式下 CDN↔源站段可能是 h1.1，对客户端无影响
Cookie: sid=<128-bit 随机的 base64url>
→ 200（n=0 且 msg1 验证通过，body = TU(msg2)）
→ 204（n≥1 且会话有效，body 恒空）
→ 其余（垃圾/重放/无会话/版本不符）：**不产生任何专属响应**，请求原样转交伪装处理器（反代上游，或内嵌 nginx 页的 404）——见 §8

# 下行：一条长 GET
GET /api/events HTTP/2
Cookie: sid=<同上>
Sec-Fetch-Site: same-origin          ← 同源天然生成，无需伪造
→ 200（会话有效）
  Content-Type: text/event-stream
  Cache-Control: no-store
  X-Accel-Buffering: no
→ <TU 密文的持续字节流，永不结束>
→ 其余：同上转交伪装处理器
```

设计说明：

- **`sid` 是客户端生成的 128-bit 随机路由标签**，走 cookie。它**不是凭证、不是秘密**（Xray sessionId 同款模型）——服务端只对成功完成 Noise 握手的 sid 建立会话状态，仅持有 sid 得不到任何东西。「网站用 cookie 标识会话」也是最不需要解释的 HTTP 形态
- **`n`（seq）走 query**：像个普通的分页/序号参数
- **`Content-Type: text/event-stream` + `X-Accel-Buffering: no` + `Cache-Control: no-store`**：防止 CDN / 反代缓冲下行流（Xray 走 CDN 的同款手法，`hub.go:356-369`）。body 是**裸二进制 TU 流而非真实 SSE 帧**——内层 Noise 加密已保证载荷机密性，任何 TLS 终结点（CDN、未来任何中间层）看到的只是密文与时序/长度；真 SSE 帧的 base64 开销（约 +33%）买不到额外的仿真度
- **`X-Accel-Buffering: no` + `Cache-Control: no-store`**：防反代缓冲、防缓存 tee

### 6.3 启动时序

```
1. C  生成 sid（128-bit 随机）
2. C→S POST /api/sync?n=0, Cookie: sid, body = TU(Noise msg1)
      msg1 的 0-RTT payload = [u8 version][u64 ts_ms][u8 mux_count][mux id 列表]（§7.4）
3. S  尝试用静态私钥解密 msg1。以下任一情况 → **转交伪装处理器**（与普通请求同一出口，不落任何会话状态）：
      - 解密失败（垃圾/探测）
      - version 不符
      - ts_ms 超出 ±300s 窗口
      - msg1 的 ephemeral 公钥命中已见缓存（重放）
      - 解出的客户端静态公钥不在白名单
      成功 → 建会话（进入 30s attach 窗口）→ 200, body = TU(Noise msg2)
4. C  收 msg2 → 双方持有传输密钥。mux 协商在握手中完成，零额外 RTT
5. C→S GET /api/events, Cookie: sid
      sid 无会话或已被挂载 → **转交伪装处理器**；正常 → 200，挂载，开始下发 TU 流
6. 之后上行 POST n=1,2,3…（每 body 含 1..N 个 TU）
```

msg1 防重放（Noise IK 的 msg1 在协议层可重放，必须在应用层补）：

- **ts 窗口**：±300s（配置项），ts 在加密载荷内，无法篡改
- **已见 ephemeral 公钥缓存**：LRU 4096 条，条目寿命 = 窗口时长
- 命中任一 → 与解密失败**同一路径** → 转交伪装处理器，响应无差别
- **0-RTT 无前向安全** → msg1 载荷只放 version / ts / mux 偏好这类非秘密数据

### 6.4 上行：packet-up + 窗口 + 重试

**Xray 的 browser dialer 在这里是坏的，必须修正。**

`splithttp/dialer.go:563-565` 只在 `DefaultDialerClient` 分支等待上一个 POST 写完；`BrowserDialerClient` 不等待，所有 POST 并发 fire-and-forget → 到达乱序 → 服务端重组堆超过 `scMaxBufferedPosts`（默认 30）直接断连（`upload_queue.go:105-110`）。

**我们的规则**：

| 项 | 值 | 说明 |
|---|---|---|
| 在途 POST 数 | **硬上限 8** | 服务端重组缓冲 30，留 3.7 倍余量 |
| 单 POST body 最大 | 1 MB（≈ 最多 16 个 TU） | 对齐 Xray 默认（`config.go:139-148`） |
| 首包 / 空闲后首包 | **立即发** | 低延迟优先 |
| 连续有数据 | 攒 4ms 或攒满 **64000 字节**，先到先发 | 64000 < 64516，落在单 TU 上限内 |

**seq 按 POST 分配**（不按 TU）。窗口内 8 个 POST 并发发出，到达顺序不保证，服务端按 seq 重排——但窗口 8 远小于缓冲 30，永不触发断连。

**重试与去重（exactly-once）**：

- 窗口槽持有该 POST 的**完整字节**直到收到响应
- 重试**原样重发同一 seq 同一字节，不重新加密** → Noise nonce 序列不被打乱（nonce 顺序 = TU 加密顺序 = seq 顺序）
- 服务端去重：`seq < next_seq`（已消费）或已在重组堆中 → 丢弃该 body，**仍回 204**（对所有上行一律 204，响应形态统一）。去重判定以 **POST（seq）为单位、整 body 原子丢弃**——不存在「body 内多 TU 部分消费」的语义

**post() 状态码处置**（服务端对无效会话**没有专属响应**，客户端以「是否为我方约定的成功码」判活）：

| 结果 | 含义 | 动作 |
|---|---|---|
| 传输层错误 / 超时 / 5xx | 不知是否送达 | 同 seq 同字节重试 ≤2 次，指数退避，仍败 → 断会话 |
| 200（仅 n=0）且 body 可解出合法 msg2 | 握手成功 | 进入传输阶段 |
| 204（仅 n≥1，**响应体恒空**——这是与伪装处理器响应的判别点：上游/静态页响应几乎必带 body） | 送达（或被去重，等价） | 释放窗口槽 |
| **其余一切**（上游的 404/405/301、nginx 404 等） | 会话已失效（服务端 GC / 被转交伪装处理器） | **断会话，不重试** |

「其余一切」含一个理论缝隙：上游恰好对 POST 回 204 时，死会话的包会被误判为送达。**误判后果有界**——下行长 GET 同时失效并触发断会话（§9.1），客户端最多多发包直到下行超时；n=0 的判别（200 + 可解密 msg2）则是密码学严密的。接受此缝隙，不加机制。

### 6.5 下行：一条长 GET + 保活

服务端每 20–80 秒随机往下行流插一个 **PADDING TU**（`type=0x02, p_len=0`），防中间盒掐断长连接。Xray 也做这件事（`hub.go:218-229`），但它只能写裸 `X` 字符；我们有帧格式，插的是合法空载荷帧。

**CDN 部署时的硬约束**：保活间隔上限必须低于 CDN 的空闲超时（Cloudflare 代理约 100 秒无字节即断流）。默认 80s 上限满足；该值与保活范围一起留作配置项，改小不改大。

客户端空闲（无上行数据）超过 60s 时，同样发 PADDING TU 作上行心跳（配合服务端空闲 GC，§9.4）。心跳 POST 占用 seq 与窗口槽，**复用 §6.4 完整重试逻辑，无旁路**。

测试补充（§10.1）： ephemeral 缓存满 4096 后的旧条目驱逐须与 ts 窗口过期行为一致（驱逐不影响仍在窗口内的判定）。

**中断流式读必须用双杠杆**——从 Xray 的血泪注释抄来（`dialer.html:117-127`）：

```js
await reader.cancel();      // 必须先
controller.abort();         // 后
```

只调 `abort()` 会永久卡在 `reader.read()` 里，直到服务端关闭、页面刷新或网络断开，流和内存一起泄漏。

### 6.6 加密：Noise_IK + 自定义套件

`snow` crate 的 `Noise_IK` 模式，**通过自定义 `CryptoResolver` 替换全部组件**：

| Noise 组件 | 实现 | 理由 |
|---|---|---|
| DH | X25519（`x25519-dalek`） | 标准，无争议 |
| Cipher (AEAD) | **AES-256-GCM**（`aes-gcm` crate） | 硬件 AES-NI / ARM64 Crypto Extensions；Android 移动端天然受益 |
| Hash | **BLAKE3**（`blake3` crate） | NEON/SIMD 加速，快于 BLAKE2s；`blake3` crate 生态成熟 |

协议名 `Noise_IK_25519_AESGCM_BLAKE3`。**注意**：这是私有命名——Noise 规范的哈希名字需正式注册，我们自定义组合无法注册。内部使用无碍，但要在协议常量里写死，双端一致即可。

**nonce 衔接**：snow 的 `Cipher` trait 以 `u64` nonce 驱动（Noise 规范内部拼 96 位 = 32 位零前缀 + 64 位计数器）。自定义 resolver 将其嵌入 AES-GCM 的 12 字节 nonce（4 字节零前缀 ‖ u64 计数器），**写死，不留给实现者发挥**。

为什么内层加密是必需的：外层 TLS 由浏览器保证，但任何 TLS 终结点（CDN 前置、企业 MITM box）都能看到明文。内层加密同时承担认证职责。

为什么选 IK：握手 1-RTT、服务端静态公钥预知（抗 MITM）、客户端静态公钥加密传输（**服务端白名单认证**）、有前向安全（握手后）。

**查证注记**：调研确认「blake3-aes」并无官方定义——BLAKE3 团队从未发布 AES 变体（其加速路线是 SIMD）；名字相近的 `oconnor663/blake3_aead` 是 hazmat 实验品（不含 AES、无审计、nonce 重用即灾难），`aes-eme2-blake3` 历史下载 24 次。故采用成熟组件组合 AES-GCM + BLAKE3，经 snow resolver 注入，不引入任何实验性原语。

IK 的 msg1 可携带 0-RTT payload（无前向安全），我们只用它放 version / ts / mux 偏好。

**协议版本**：`u8`，当前 `1`，位于 msg1 payload 首字节。不匹配 → **转交伪装处理器**（与垃圾请求同路径，不给探测者版本探测面）。

### 6.7 同源带来的收益

WebView 加载的是服务端首页而非 `about:blank`，因此所有代理请求都是同源的：

| | 加载 about:blank（Xray browser dialer 的处境） | 加载服务端真实页面 |
|---|---|---|
| CORS 预检 | 每个带自定义头的 POST 前多一次 OPTIONS RTT | **消失** |
| `Sec-Fetch-Site` | `cross-site` | **`same-origin`** |
| `Origin` / `Referer` | 缺失或需走私伪造 | 天然正确 |
| Cookie | 需手工 `document.cookie` 注入再清除（`dialer.html:40-62`） | 天然携带 |
| 流量语义 | 一个空页面在向陌生域名 POST | 一个用户在使用这个网站 |

### 6.8 TLS 会话与 0-RTT 策略

浏览器侧 TLS 策略不受客户端直接控制（fetch 无「使用 early data」的开关，票据与连接池由浏览器网络栈自主管理）。因此不追求「开 0-RTT」，而是**铺三层路，让浏览器自己往快处走**：

| 层 | 机制 | 保障程度 |
|---|---|---|
| 1. 连接复用 | h2 多路复用：1 条长 GET + ≤8 在途 POST = ≤9 条流共享**同一条 h2 连接**；长 GET 永不结束 → 连接永不回收 | **稳态零握手，结构性保证** |
| 2. TLS 1.3 恢复 + early data | 服务端开启 session ticket 与 early data（`max_early_data_size > 0`）；浏览器对新建连接自动恢复，可能携带 0-RTT | 机会性，浏览器自行决定 |
| 3. HTTP/3（QUIC） | 广播 Alt-Svc 后，QUIC 会话恢复给新连接真 0-RTT | 可选：CDN 模式 = CDN 控制台开关；direct 模式需额外 h3 监听 |

**事实边界（不粉饰）**：

- 每次 App 启动的首次页面加载必然付一次 1-RTT（WebView 会话票据缓存不跨进程持久化）；此后整个会话生命周期内不再握手
- 0-RTT 的已知弱点是重放。**我们承担得起**：msg1 防重放（§6.3）+ seq 去重（§6.4）恰好覆盖「重放一个 POST」的攻击面——服务端对重放 POST 去重丢弃、回 204，无副作用；页面静态内容的重放无害。重放的 msg1 无论会话是否已 GC，300s 内命中 ephemeral 缓存、超窗后命中 ts 窗口，两条路都汇入伪装处理器。**TLS 层放弃的重放防护，内层已全额补齐，「舍弃部分安全性」的实际代价 ≈ 0**
- CDN 模式下 0-RTT 与 h3 在 CDN 边缘终结（控制台开启），源站无感

---

## 7. mux 层

### 7.1 两个必须分清的维度

`yamux` 和 `tokio-yamux` 是**同一线格式的两个实现**（都实现 HashiCorp yamux 规范，可跨语言互通）。

| 维度 | 是什么 | 决定 | 是否协商 |
|---|---|---|---|
| **线格式** | yamux / smux / muxado / picomux / h2mux | 能不能互通 | ✅ 握手时协商 |
| **实现** | `yamux` crate vs `tokio-yamux` crate | 跑多快 | ❌ 纯本地选择 |

协商的是线格式。客户端用哪个 crate 实现 yamux，服务端不知道也不需要知道。

### 7.2 `Mux` trait

因为 mux 是运行时协商出来的，trait **必须 object-safe**，不能用泛型 associated type：

```rust
pub type MuxStream = Box<dyn AsyncReadWrite + Send + Unpin>;

#[async_trait]
pub trait Mux: Send + Sync {
    async fn open(&self) -> Result<MuxStream>;      // 客户端
    async fn accept(&self) -> Result<MuxStream>;    // 服务端
}
```

代价：每条流一次 `Box` + 一次动态分发。**每条流一次，不是每字节一次**，可忽略。

### 7.3 五种线格式及其产品实现

**产品代码每种线格式一个实现**；yamux 线格式的产品实现选 `tokio-yamux`（原生 tokio + `Control` 句柄可 clone 解决并发开流）。paritytech `yamux` 只出现在 mux-bench example 里，与 `tokio-yamux` 构成同规范 A/B 对照，量化 compat 层 + poll API 封装的代价。

| 线格式 | mux id | 产品实现 crate | 流控 | 适配工作 |
|---|---|---|---|---|
| yamux | `0x01` | `tokio-yamux` 0.3.20 | 窗口 256KiB + 连接级 1GiB 上限 | 原生 tokio，直接实现 |
| smux | `0x02` | `smux`(iberryful) 0.2.0 | 滑动窗口 v2（UPD 帧） | 原生 tokio，直接实现 |
| muxado | `0x03` | `muxado` 0.5.4 | 窗口信用（WndInc 帧） | 原生 tokio，直接实现 |
| picomux | `0x04` | `picomux` 0.2.1 | **BDP 自适应窗口** | 原生 tokio，直接实现 |
| h2mux | `0x05` | `h2` 0.4.18 | 连接级 + 流级双层窗口 | **需适配**，见下 |

**h2mux 的适配层**（唯一需要写的）：

| | 内容 | 估算 |
|---|---|---|
| `SendStream`/`RecvStream` 是 `Bytes` 分块 + `reserve_capacity()`/`send_data()`/`release_capacity()`，不实现 `AsyncRead`/`AsyncWrite` | 需适配层 | ~150 行 |
| 每条流必须带 HTTP 请求/响应头 | 构造合成 header | 含于上 |

**排除项与理由**：

| | 原因 |
|---|---|
| `libp2p-mplex` | **无流控**（源码确认只有 32 帧缓冲上限，非窗口机制），libp2p 官方已弃用并建议改用 yamux。一条慢流会拖垮整条连接 |
| `quinn` 脱离 UDP | 技术上可行（`AsyncUdpSocket` trait 公开），但 TCP 之上跑 QUIC = 双层拥塞控制 + 双层重传 + 底层 HOL 阻塞复活，QUIC 优势全部抵消 |
| `tokio_smux` | `Stream` 刻意不实现 `AsyncRead`/`AsyncWrite`（只有 `send_message`/`recv_message`），API 形状与字节流场景不匹配 |
| `async_smux` | 2024-11 后停滞，crates.io 无 repository 字段，文档覆盖 2.5% |
| mux.cool | 协议规范层面**完全无流控**；唯一可用实现 `meow-proxy` 的 mux.cool 模块仅数天历史且编解码器为 `pub(crate)`。Xray 官方文档亦称「使用 Mux 看视频、下载或者测速通常都有反效果」 |

### 7.4 协商：折进 Noise 握手，零额外 RTT

mux 偏好在 msg1 的 0-RTT payload 里，服务端的选择在 msg2 payload 里回来：

```
msg1 payload:
  u8 version = 1
  u64 ts_ms                      // unix 毫秒
  u8 mux_count                   // 1..=5
  [mux_count × u8 mux_id]        // 按客户端偏好降序

msg2 payload:
  u8 chosen_mux_id
  u8 fallback                    // 0 = 命中偏好；1 = 无交集，回退基线
```

规则：

1. 服务端按**客户端的偏好顺序**取第一个自己也支持的
2. 无交集 → 选 **yamux 基线**（`0x01`），置 `fallback=1`
3. 客户端见 `fallback=1` → `WARN`：`服务端不支持 <偏好列表>，已回退 yamux（性能可能下降）`
4. **任何情况下连接都要建起来**，绝不因 mux 不匹配而失败

### 7.5 回退基线为什么是 yamux 而非自研极简 mux

1. **无流控的极简 mux 会内存爆炸**——一条慢流拖垮整条连接。`libp2p-mplex` 正是因此被官方弃用
2. **只在异常路径跑的自研代码是最容易腐烂的代码**。回退路径平时不跑，一跑就是已经出问题的时候，那时最不该面对一段没人测过的 mux
3. yamux 是唯一**跨语言线兼容**的，未来做 Xray 兼容层时留有退路

`ponytail:` 基线 = yamux，双端强制实现，不可关闭。

### 7.6 Benchmark：本地跑一次，选定即弃

**不进 CI，不做持续基准。** 一个一次性的 example 二进制：

```
cargo run --release --example mux-bench
```

**延迟与丢包在 mock transport 层注入，不碰系统网络配置**（`tc` / `pfctl` 需要 root 且不可重复）。注意此处的 mock 仅模拟**网络条件**，被测的 mux 实现和协议栈都是真实的。

四组场景——**只测局域网低延迟会得出误导性结论，流控差异根本显现不出来**：

| 场景 | RTT | 丢包 | 并发流 | 用途 |
|---|---|---|---|---|
| 本地 | 1ms | 0 | 8 | 理想情况基线 |
| 常规跨境 | 80ms | 0 | 32 | 日常工况 |
| **弱网跨境** | 250ms | 1% | 32 | **真实工况，拉开差距处** |
| 高并发小包 | 80ms | 0 | 128 | 网页浏览典型形态 |

指标三项：**首字节延迟**、**并发流下的总吞吐**、**慢流是否饿死快流**。

跑完把结果表格补进 §7.7，配置里写死默认值，脚本留在仓库但不再定期运行。

### 7.7 Benchmark 结果

实测于 2026-08-24（macOS，release 构建，`cargo run --release -p wsieve-mux --example mux-bench`，
总耗时约 20 分钟）。工作负载：每条流上传 256KB 确定性图案 → echo 回来 → 逐字节校验。
首字节延迟 = open 完成到 echo 首字节；吞吐 = 双向聚合字节 / makespan；公平性 = 各流完成时间极差。
fail 列 = 未在超时内完成完整性校验的流数（muxado/h2mux 高并发下停滞，见下）。

| 场景 | 实现 | P50 ttfb | P99 ttfb | makespan | 极差 | MB/s | fail |
|---|---|---|---|---|---|---|---|
| local (1ms/0%/8流) | tokio-yamux | 223.7ms | 317.4ms | 355ms | 153ms | 11.3 | 0 |
| | smux | **46.8ms** | **82.7ms** | **225ms** | 64ms | **17.8** | 0 |
| | muxado | — | — | — | — | — | 8（停滞） |
| | picomux | 37.5ms | 50.3ms | 380ms | 187ms | 10.5 | 0 |
| | h2mux | 30.4ms | 54.5ms | 658ms | 81ms | 3.8 | 3 |
| cross (80ms/0%/32流) | tokio-yamux | 13408.5ms | 23438.7ms | 24929ms | 18661ms | 0.6 | 0 |
| | smux | **11647.5ms** | 22875.7ms | **24070ms** | 18902ms | **0.7** | 0 |
| | muxado | — | — | — | — | — | 32（停滞） |
| | picomux | **2889.0ms** | **4844.7ms** | 25999ms | **8056ms** | 0.6 | 0 |
| | h2mux | 2348.1ms | 4095.7ms | 68637ms | 17303ms | 0.2 | 6 |
| weak (250ms/1%/32流) | tokio-yamux | 41693.3ms | 77402.1ms | 80852ms | 61339ms | 0.2 | 0 |
| | smux | **35019.2ms** | **67519.9ms** | **77256ms** | 57985ms | 0.2 | 0 |
| | muxado | — | — | — | — | — | 32（停滞） |
| | picomux | **9060.5ms** | **15939.4ms** | 80577ms | 29704ms | 0.2 | 0 |
| | h2mux | — | — | — | — | — | 32（停滞） |
| burst (80ms/0%/128流) | tokio-yamux | 45396.9ms | 88911.1ms | 91429ms | 84650ms | 0.7 | 0 |
| | smux | **43022.7ms** | 85723.0ms | **87053ms** | 81174ms | 0.7 | 0 |
| | muxado | — | — | — | — | — | 128（停滞） |
| | picomux | **7381.3ms** | **8916.5ms** | 93138ms | **40043ms** | 0.7 | 0 |
| | h2mux | — | — | — | — | — | 128（停滞） |

**实测暴露的实现缺陷**：

- **muxado 0.5.4**：8 流并发上传即死锁。开流在服务端 accept 侧被串行化
  （本仓库适配器为懒 SYN 写 sentinel + accept 端 `read_exact`，持会话锁），
  且流控窗口依赖对端回读，多流高压下大部分流的 echo 数据永远回不来。
  任何 ≥8 流场景全部超时。
- **h2mux**：h2 默认连接级流控窗口 64KB 由全部子流共享，128 流 burst 直接
  停滞；32 流场景也有 6 条流超时；即使 8 流 local 也有 3 条失败。
- **picomux 0.2.1**：任何一条流发 FIN 会终结**整个会话**（`received remote FIN`
  → bail），无半关闭。基准被迫对其单独禁用写半关闭。吞吐和首字节延迟数据
  很好，但该语义与代理场景（流频繁开关）根本冲突——流一关连接就断。

**默认选定：smux（`0x02`），常量 `wsieve_xhttp::DEFAULT_MUX`。**

推翻了 §7.6 之前预押的 tokio-yamux。数据依据：smux 在全部 4 个场景的首字节
延迟 P50/P99 都低于 tokio-yamux（local 46.8ms vs 223.7ms、cross 11.6s vs 13.4s、
weak 35.0s vs 41.7s、burst 43.0s vs 45.4s），吞吐持平或更好（local 17.8 vs 11.3
MB/s），完成时间极差（公平性代理）更小，四场景零失败。picomux 虽然在
cross/weak/burst 的 ttfb 上优势明显，但其「单流 FIN 杀全会话」的语义缺陷使其
不可选为主默认。tokio-yamux 全面第二，继续担任 **无交集协商回退基线**（§7.5，
标准 yamux 线格式、跨语言退路）——默认偏好首位与协议回退基线是两个不同角色，
smux 胜出改变前者、不动后者。muxado/h2mux 在本工作负载下存在停滞性缺陷，
置于偏好列表末位。

---

## 8. 抗主动探测

**核心原则：服务器对不持有密钥的请求者而言就是一台纯反代/静态站，代理功能零可见。**

处理规则只有一条：**任何未通过认证的请求——无论打到哪个路径——原样转交伪装处理器**。不存在「代理端口被探测到后回 401」这种承认自身存在的响应；`/api/sync` 被打垃圾数据、`/api/events` 被无会话 GET，服务端的行为与扫描随机路径**完全一致**：转给上游（或回内嵌 nginx 页的 404）。

| 请求 | 响应 |
|---|---|
| `GET /` | 上游站点页面（或内嵌 nginx 默认页） |
| `POST /api/sync` 垃圾 body / 重放 msg1 / 版本不符 | **原样转交伪装处理器**（反代上游；上游对 POST 的响应是什么就回什么） |
| `GET /api/events` 无有效会话 | 同上 |
| 扫描随机路径 | 同上 |

Noise_IK 的 msg1 对不持有服务端静态私钥的观察者是**不可区分的随机字节**；重放的 msg1 被 ts 窗口 + ephemeral 缓存拦截（§6.3），拦截路径与解密失败、与普通请求**三者同响应**。主动探测者拿不到任何确认信号——服务端不回任何「我认识这个协议」的信号。

**伪装站点**：

- **默认 = 内嵌的 nginx 默认页副本**（静态 HTML + nginx 风格 404 页 + `Server: nginx` 响应头，版本串可配置）。零配置，且「全网最常见最不起眼的页面」比任何精心制作的假站都更不显眼
- **可选 `disguise.upstream`**：配置后，所有未认证请求（含 `/api/*` 路径上的无效请求）反向代理到该上游站点（透传 body 与常规头）。上游页面里指向第三方域名的绝对 URL 资源照常跨域加载，不影响 `/api` 的同源性——WebView 只需要初始文档来自本源，代理请求自然同源。`ponytail:` 上游反代响应的缓存策略由 CDN 侧配置负责，服务端不代管
- 认证判定**先于**路径路由：请求要么是「会话有效/握手成功」走代理路径，要么整体转伪装处理器——不存在中间态

**部署模式（服务端配置 `deployment`）**：

| 模式 | 浏览器侧 TLS | 源站证书 | 说明 |
|---|---|---|---|
| `direct` | 源站直接终结 | **必须有效**（证书文件配置） | WebView 的浏览器内核拒绝加载无效 TLS 页面，此模式别无选择 |
| `cdn` | CDN 终结（有效证书） | 有效（CDN Full strict） | 源站 IP 隐藏于 CDN 后 |
| `cdn` | 同上 | **自签**（CDN Full） | 源站零证书采购 |
| `cdn` | 同上 | **无，明文 HTTP 监听**（CDN Flexible） | 最省；CDN↔源站明文段上跑的仍是 Noise 密文，机密性不受损，只额外暴露时序/长度给该段观察者 |

客户端对部署模式**完全无感**：它只连一个域名（CDN 域名或源站域名），其余差异全在服务端配置里。CDN 前置的缓冲问题由 §6.2 的 SSE 三头 + §6.5 的保活间隔（上限 80s < Cloudflare ~100s 空闲超时）处理，Xray XHTTP 走 CDN 已验证此路径可行。

---

## 9. 错误处理

### 9.1 故障与处置

| 故障 | 处置 |
|---|---|
| 上行 POST 传输错误/超时/5xx | 同 seq 同字节重试 ≤2 次（指数退避），仍败 → 断会话 |
| 上行 POST 收到非约定响应（上游 404/405/301、nginx 404 等） | **断会话，不重试**（会话已失效，服务端已把它当普通请求转伪装处理器） |
| 下行 GET 收到非约定响应或流断开 | 断会话 |
| 断会话 | mux 关闭所有子流 → SOCKS5 回 RST → 上层应用自行重连 |
| Noise 握手未得到合法 msg2 | 不重试，报错给用户（配置/密钥/版本问题，重试无意义） |
| mux 协商无交集 | **不失败**，回退 yamux + WARN 日志（§7.4） |
| WebView 崩溃 / 页面导航离开（全平台） | pending IPC 全部失败/超时 → transport 标记死亡 → 断会话 + UI 提示 + 自动重载页面重建会话 |

### 9.2 seq 空洞

seq 严格递增，一个包永久丢失会让服务端一直等待（空洞），最终堆积超限断连。因此上行重试是**必需的**，不是可选优化。客户端在 2 次重试失败后主动断会话，空洞存活时间不超过 ~2 RTT；且在途窗口 8 < 服务端缓冲 30，即便突发也不会溢出。

### 9.3 Android renderer 崩溃与节流

- 监听 `onRenderProcessGone` → 重建 WebView → 重新加载页面 → 重建会话。renderer 死后 WebView 实例不可复用，必须移除重建
- `setRendererPriorityPolicy(RENDERER_PRIORITY_IMPORTANT, false)`（API 26+）降低被 OOM kill 概率。**第二个参数必须为 `false`**，传 `true` 会在 WebView 不可见时把优先级降为 `WAIVED`，反而成为首要 OOM 目标
- 这只影响「是否被杀」，不影响「是否被节流」

**Android 后台节流的应对**：Chromium 的 timer 节流**官方豁免 WebSocket 和 WebRTC 实时连接**（避免连接超时）。我们的下行是一条持续的 fetch 流而非 timer 驱动，且节流只作用于 timer 任务、loading 任务不受限——因此实际影响可能远小于预期。

`ponytail:` 此项**必须实测验证**，不预先为它设计规避方案。若实测确认有影响，再评估将下行改为 WebSocket 承载。

### 9.4 会话生命周期（服务端）

| 事件 | 规则 |
|---|---|
| 会话建立 | msg1 验证通过（§6.3 步骤 3） |
| attach 窗口 | 30s 内 GET 未挂载 → GC（客户端握手后立刻开 GET，正常 <1s） |
| 下行 GET 断开 | **立即 GC**（无会话恢复，客户端会用新握手重建） |
| 上行空闲 GC | 180s 无任何上行 POST/PADDING → GC（客户端心跳 60s，见 §6.5，余量 3 倍） |
| 重组缓冲上限 | 30 个 POST（对齐 Xray `scMaxBufferedPosts`），超限断会话 |
| 重复 seq | 去重丢弃，回 204（§6.4），**不是错误** |

---

## 10. 测试策略

| 层次 | 内容 | 是否需要 WebView |
|---|---|---|
| 1. Noise 双端对跑 | 握手 + 加解密往返 + **重放拒绝**（ts 窗 + ephemeral 缓存） | 否 |
| 2. TU / `XhttpConn` 单元测试 | 分帧边界（64516 临界）、seq 连续性、去重、窗口不越界、padding 长度分布 | 否 |
| 3. mux 双端对跑 | 5 种线格式各自的开流/并发/关闭；协商命中与回退路径 | 否 |
| 4. **全链路集成测试** ★ | `ReqwestTransport` + 真实服务端进程 + 真实目标站点 | 否 |
| 5. **探测等价性测试** | 同一路径分别发：垃圾 POST / 重放捕获的 msg1 / 随机路径扫描；断言三者响应（状态码、响应体、头）与「普通未认证请求」完全一致 | 否 |
| 6. 端到端验收 | 起 Tauri App，`curl --socks5` 验证 | 是 |

第 4 项是方案 A 的兑现点：**协议正确性由 `cargo test` 保证，WebView 只在第 6 步出现一次**。

第 3 项要特别覆盖回退路径，第 5 项钉住 §8 的承诺——回退与探测都是平时不跑的代码，最容易腐烂。

---

## 11. 已知限制与刻意未做的事

| 项 | 状态 | 何时再考虑 |
|---|---|---|
| iOS 支持 | 不做 | Safari 27 流式上传形成基线 **且** 后台冻结有解（后者可能永远无解） |
| 会话恢复 / 断点续传 | 不做 | Xray 也没有，上层应用自行重连。为此写状态机不划算 |
| `stream-up` / `stream-one` 模式 | 不做 | WKWebView 不支持流式上传，实现了也只有部分平台能用 |
| REALITY | **无法做** | 浏览器不给 ClientHello 控制权，架构上不可能 |
| XMUX 连接池控制 | **无法做** | 浏览器自己管 h2 连接池，`maxConnections` / `hMaxRequestTimes` 的抗指纹价值在 WebView 中丧失 |
| Xray XHTTP 服务端互通 | 预留接口边界，不实现 | 需在 Rust 中完整实现 VLESS 内层协议 + XHTTP 帧格式 |
| h2mux 与 sing-box 互通 | 不做 | 需完整实现 sing-box mux 协议帧（固定 `sp.mux.sing-box.arpa:444` 等约定），不只是「把 h2 当 mux」 |
| 高吞吐优化 | 不做 | 目标场景是低延迟。架构不阻止，但不为此设计 |

### 11.1 架构固有的取舍

选择 WebView 承载传输，换来指纹的完全真实，代价是**永久失去对 TLS 层和 h2 连接池的控制权**。REALITY 和 XMUX 不是「暂未实现」，是架构上不可能。这是这条路线的定价，接受它才能拿到指纹真实性。

---

## 12. 参考

### 代码

- Xray-core（本地克隆于 `.research/Xray-core`，commit `2323273e`）
  - `transport/internet/splithttp/` — XHTTP 传输
  - `transport/internet/browser_dialer/` — 浏览器拨号器与其内嵌的 `dialer.html`
  - `transport/internet/splithttp/browser_client.go:26` — 上游明示 browser dialer 的双向流未实现

### 规范与文档

- Chrome 流式请求：https://developer.chrome.com/docs/capabilities/web-apis/fetch-streaming-requests
- Safari TP 250 发布说明：https://webkit.org/blog/18191/release-notes-for-safari-technology-preview-250/
- Interop 2026：https://webkit.org/blog/17818/announcing-interop-2026/
- Tauri IPC：https://v2.tauri.app/develop/calling-rust/
- Tauri WebView 版本：https://v2.tauri.app/reference/webview-versions/
- Chromium timer 节流：https://developer.chrome.com/blog/timer-throttling-in-chrome-88
- Cordova CB-10657（iOS 后台 JS 冻结，Won't Fix）：https://issues.apache.org/jira/browse/CB-10657
- Mux.Cool 协议：https://xtls.github.io/development/protocols/muxcool.html
