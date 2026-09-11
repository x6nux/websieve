# 服务端同时支持 HTTP/1.1、HTTP/2、HTTP/3 —— 设计文档

**日期**：2026-09-11
**状态**：设计待评审
**动机**：服务端当前只跑 HTTP/1.1，且**连 ALPN 都不协商**。这既是伪装上的显著异常特征，也让我们无从选择协议——而协议版本恰恰决定了整条数据面的连接数模型，进而决定条带（stripe）能拿到多少收益。

---

## 1. 为什么这件事不只是"加个协议支持"

一个常识性的判断是「多支持几种协议总是好的」。在本项目里不成立，因为**HTTP 版本直接决定 WebKit 会开几条 TCP/QUIC 连接**，而那正是条带并行度的物理来源。

先把三层的连接模型摆清楚（这是全文所有结论的基础）：

| 协议 | WebKit 每 origin 的连接数 | 并行性来源 | 丢包影响 |
|---|---|---|---|
| HTTP/1.1 | **6 条 TCP**（连接池） | 6 个独立拥塞窗口 | 每条独立降窗，互不牵连 |
| HTTP/2 | **1 条 TCP** | stream 多路复用 | **队头阻塞**：一次丢包拖住全部 stream |
| HTTP/3 | **1 条 QUIC** | stream 多路复用 | 无队头阻塞：stream 间真正独立 |

所以「开 h2」这个动作的真实含义是：**把 6 条独立 TCP 换成 1 条**。这不是中性的升级。

---

## 2. 现状核实（全部为 2026-09-11 实测，非推断）

### 2.1 服务端其实已经会 h2，只是没宣告

证据链：

- `crates/wsieve-server/Cargo.toml`：`hyper = { version = "1", features = ["server", "http1", "http2"] }` —— http2 feature 是开的
- `crates/wsieve-server/src/tls.rs:142`：用的是 `hyper_util::server::conn::auto::Builder`，其注释自己写着「h1/h2 自适应」。该 builder 会嗅探连接首字节，是 h2 preface 就走 h2
- `rg alpn crates/` → **生产代码零处命中**，只有 `examples/webkit_tcp_probe.rs` 里出现过

实测验证（对 `https://websieve.833337.xyz/`）：

```
curl --http2-prior-knowledge  →  HTTP/2 200     # 绕过 ALPN 直发 h2 preface，服务端正常处理
curl --http2                  →  HTTP/1.1 200   # 走正常 ALPN 协商，回落 h1
openssl s_client -alpn h2,http/1.1  →  No ALPN negotiated
```

**结论**：h2 的处理能力完整存在，唯一缺失的是 `tls.rs:67` 的 `rustls_config()` 里没有设 `alpn_protocols`。

### 2.2 现在那 6 条 TCP，是「没有 h2」换来的

在跨境节点（217.217.32.206，RTT 45.6ms）跑大流量时，于服务端侧实测：

```
ss -tn state established '( sport = :443 )' | wc -l
→ 6        # 恒为 6，即使客户端 lane 开到 16 也还是 6
```

6 正是 HTTP/1.1 连接池每 origin 的经典上限。**条带的多 lane 之所以有效，是因为多个并发 HTTP 请求被 WebKit 分配到了这 6 条独立 TCP 上**——收益并非来自 mux 层本身。

配套的吞吐实测（64MiB 单流下载，每档三轮取中位数）：

| lane 数 | 吞吐中位数 |
|---|---|
| 1 | 10.2 MiB/s |
| 2 | 19.4 MiB/s |
| 4 | **24.7 MiB/s** |
| 6 | 11.1 MiB/s（撞上链路差窗口，见下） |
| 8 | 22.7 MiB/s |

对照基准：同链路**单条 TCP** 的裸吞吐（SSH 实测）为 **20.1 MiB/s**。

两点说明，避免过度解读：
- **可信的部分**：lane 1→4 的上升趋势在两次独立测量中方向一致（另一轮为 14.9 → 21.0）。lane 超过 4 无稳定收益。
- **不可信的部分**：并发场景的数据被链路抖动完全淹没（同配置三轮实测出现过 8.29 / 30.53 / 37.53 MiB/s）。lane=6 那档的 11.1 属于撞上了链路状况差的几分钟，不构成"6 比 4 差"的证据。

### 2.3 WKWebView 的 h3 行为（查证结论）

- **Safari 16+ / iOS 16+ 默认启用 HTTP/3**；WKWebView 共享 WebKit 的 CFNetwork 网络栈，**继承该行为，无独立开关**。
- **Apple 的网络栈不会推测性尝试 QUIC**。必须先被告知，两种途径：
  1. `Alt-Svc` 响应头（如 `Alt-Svc: h3=":443"; ma=86400`）——**但这意味着首次连接必然走 h1/h2**，看到头之后记住，**下一次连接**才升级到 h3
  2. **DNS 的 HTTPS 资源记录**（含 `h3` ALPN 值）——优势是**第一次接触就能直接用 h3**
- `assumesHTTP3Capable` 是 `URLRequest` 的属性，WKWebView 的页面导航与 fetch 无法设置，对本项目不可用。
- **UDP 443 被阻断/限速时会静默回落**到 h2/h1（Private Relay、部分 VPN 与运营商 QoS 都会触发）。

来源：[Apple WWDC21 - Accelerate networking with HTTP/3 and QUIC](https://developer.apple.com/wwdc21/10094)、[Browser support for HTTP/3 QUIC](https://mybyways.com/blog/browser-support-for-http3-quic/)、[HTTP/3: Browser Support, Features, Known Issues](https://www.testmuai.com/learning-hub/http3-browser-support/)

### 2.4 协议无关性核实

emitter 侧（`ui/emitter.js:287,314`）用的是标准 `fetch(path, {method:'POST', body})`，**没有使用 duplex streaming 等 HTTP/1.1 特有语义**。因此 h2/h3 下 fetch 的请求-响应语义不变，xhttp 协议本身无需改动。

workspace 当前**零 QUIC 依赖**（`grep -riE "quinn|s2n-quic|^h3"` 无命中），h3 需要新引入整条栈。

---

## 3. 核心张力与解法

### 3.1 ~~张力~~ —— 该预测已被实测推翻（2026-09-11）

> **本节原先的论断是错的，保留原文以记录推理失误。**
>
> 原论断：「开启 h2 会把并行度从 6 条独立连接压成 1 条，单流吞吐将从
> ~24.7 MiB/s 回落到单连接水平，并让条带第 1、2 层的收益归零。」

**实测结果恰恰相反：h2 的中位吞吐是 h1 的 3.22 倍。**

对照实验（同一节点、同一链路、同一 curl 测法，唯一变量是 `cfg.alpn_protocols` 是否为空）：

| | 服务端侧 TCP 连接数 | 64MiB 单流吞吐（各 8 样本） |
|---|---|---|
| h1（ALPN 关闭） | 5 | 中位 **11.0** MB/s，范围 5.0–22.3 |
| h2（ALPN 开启） | 1 | 中位 **35.5** MB/s，范围 10.2–46.7 |

连接数确实如预测从 6 降为 1，**但吞吐不降反升**。

**推理错在哪：**把"并行度"等同于"连接数"。实际瓶颈是 **HTTP/1.1 的队头阻塞叠加长流占用**——

xhttp 的下行是 `GET /api/events` 长流，一条流会**永久占住一条 TCP 连接**。条带默认 4 条 lane，于是 6 条连接里有一大半被长流长期占据（实测 h1 下连接数稳定在 5，已逼近上限），剩下的连接要承载全部上行 POST，严重排队。h1 没有 pipelining，一条连接同一时刻只能有一个请求在飞。

h2 则把全部 lane 与上行请求复用进一条连接的并发 stream，不存在这个限制；而且流量集中让单条连接的拥塞窗口增长得比 6 条各自竞争的小窗口更充分——在 45ms RTT 上这一点尤其显著。

**推论修正：**

- 条带第 1、2 层（分片 + 多 lane）在 h2 下**不但不归零，反而更有效**，因为 lane 不再受"最多 6 条连接"的限制。
- `target_lanes` 的最优值需要在 h2 下重新标定（原先的 4 是 h1 的 6 连接上限逼出来的）。
- **h3 的吸引力因此上升**：它同样是单连接多路复用，且 QUIC 的 stream 之间无队头阻塞，理论上应优于 h2。

### 3.2 多 origin：从「必需品」降为「待验证的可选优化」

> **随 §3.1 一并修正。** 原先认为多 origin 是补偿 h2 连接数损失的必需手段；
> 既然 h2 本身就快 3.22 倍，补偿的前提不复存在。多 origin 仍可能有价值
> （更多独立拥塞窗口、丢包时各自恢复），但那是**待实测的增量优化**，
> 不再是 h2 上线的前置条件。下面的机制描述仍然准确。

WebKit 的连接池是按 **origin（scheme + host + port）** 分的。h2/h3 下每 origin 仍是一条独立连接，因此 N 个 origin = N 条独立连接。

这正是条带第 3 层 `extra-sessions` 已经实现的机制（`src-tauri/src/shard_setup.rs:320`）：

```rust
let data_origin = |p: u16| format!("{scheme}://{host}:{p}");
```

**同域名、多端口** —— 端口参与 origin 判定，所以能造出多个 origin；域名不变所以证书与 SNI 全部照旧有效。

~~结论：**h2/h3 与多 origin 必须成套交付**。单独开 h2 是净损失。~~

**修正后的结论**：h2 可以单独交付并立即获益（实测 3.22 倍）。多 origin 降级为后续的增量优化项，其价值需要在 h2 已生效的基线上重新测量——原先"补偿连接数损失"的立论已不成立。

### 3.3 h3 的补偿优势

h3 并非单纯的"连接数变少"。QUIC 的 stream 之间真正独立，**一次丢包只影响所属 stream**，而 HTTP/1.1 的 6 条 TCP 虽然独立，每条在丢包时都要各自降窗。

在高丢包的跨境链路上，「1 条 QUIC + N 个 origin」有可能优于「6 条 TCP × 1 个 origin」。这是需要实测回答的问题，不应在设计阶段拍板——§9 的验证计划为此保留了对照组。

---

## 4. 设计：ALPN 分层

两个监听面，各自宣告各自的协议集：

| 监听 | 协议 | ALPN |
|---|---|---|
| TCP 443 | HTTP/1.1 + HTTP/2 | `["h2", "http/1.1"]` |
| UDP 443 | HTTP/3 | `["h3"]` |

顺序有意义：ALPN 按服务端偏好选择，`h2` 在前表示优先 h2，客户端不支持时回落 `http/1.1`。

QUIC 强制要求 TLS 1.3——本项目的 `rustls_config()` 已经是 `builder_with_protocol_versions(&[&TLS13])`，天然满足，无需改动。

**证书可直接复用**：当前证书的 SAN 为 `DNS:websieve.833337.xyz, IP Address:217.217.32.206`（Let's Encrypt `shortlived` profile 混签），TCP 与 UDP 两个监听面共用同一份即可。

### 4.1 ⚠️ 两个监听面不能共用同一份 rustls config

这是一条会让人直接踩进去的硬约束：**QUIC 只接受 `max_early_data_size` 为 `0` 或 `u32::MAX`**，而当前 `tls.rs:75` 设的是：

```rust
cfg.max_early_data_size = 16_384;   // §6.8 第 2 层：让浏览器 0-RTT
```

这个值对 TCP 上的 TLS 完全合法，但拿去 `QuicServerConfig::try_from` 会失败。因此两个面必须各构建各的 config：

| | ALPN | max_early_data_size |
|---|---|---|
| TCP 443 | `["h2", "http/1.1"]` | `16_384`（保持现状不动） |
| UDP 443 | `["h3"]` | `u32::MAX`（要 0-RTT）或 `0`（不要） |

实现上应把 `rustls_config()` 参数化，而不是复制一份出来各改各的——复制会在将来某次只改了一处时产生静默分歧（比如加了 client auth 只加在 TCP 面）。证书与私钥仍然共用同一份，只有这两个字段分叉。

---

## 5. 伪装影响评估

这是本项目的第一前提（`crates/wsieve-server` 存在的理由就是让 TLS 指纹由真实浏览器产生），必须单独评估。

### 5.1 h2：纯粹的改善

当前状态是**负分**：一个 TLS 1.3 服务端，在客户端 ClientHello 提供了 `ALPN: h2, http/1.1` 的情况下不做任何选择，是可被动识别的异常特征——真实世界的 HTTPS 服务端几乎全部会协商 h2。

### 5.2 h3：加分，但引入新的服务端指纹面

- **加分**：现代网站普遍开 h3，UDP 443 开着 QUIC 是"正常网站"的特征而非异常。客户端侧的 QUIC 指纹（transport parameters + TLS ClientHello）由 WebKit 真实产生，与项目前提一致。
- **新风险**：服务端的 QUIC 实现指纹（quinn）与 nginx / Cloudflare 的 QUIC 指纹不同，transport parameters 的取值组合可能成为被动识别点。**服务端指纹的重要性低于客户端，但不为零。**
- **缓解**：quinn 的 transport config 可调（`max_idle_timeout`、`initial_max_data`、流数上限等），必要时可向主流实现的取值靠拢。这属于后续加固，不阻塞首版。

### 5.3 Alt-Svc 头本身也是特征

发 `Alt-Svc: h3=":443"; ma=86400` 是真实网站的普遍行为，**不发反而更异常**（既然 UDP 443 开着 QUIC 却不宣告）。因此这一项与 §5.2 是绑定的：要么都做，要么都不做。

---

## 6. 降级与容错

必须假定 UDP 443 在部分链路上不可用（跨境 QoS、企业防火墙、Private Relay）。

- **客户端侧无需处理**：WebKit 自己会在 QUIC 握手失败时回落到 TCP 上的 h2/h1，这是 CFNetwork 的既有行为。
- **服务端侧的硬约束**：TCP 443 **必须始终监听**。h3 是叠加能力，绝不能替代 TCP 面。
- **UDP 监听失败不得阻断启动**：与 `shard_setup` 里"条带禁用则降级到单会话"同源的纪律——绑定 UDP 443 失败（权限、端口占用）应打警告并继续以 h1/h2 提供服务，而不是拒绝启动。
- **`Alt-Svc` 的 `ma`（max-age）不宜过大**：客户端会记住该宣告。若 h3 面临时故障而客户端仍在 `ma` 窗口内，每次连接都要先尝试 QUIC 失败再回落，徒增延迟。建议首版取较小值（如 `ma=600`），稳定后再调大。

---

## 7. 实现要点

### 7.1 h2（改动极小）

`crates/wsieve-server/src/tls.rs` 的 `rustls_config()`（当前 67-77 行）增加：

```rust
cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
```

`serve()` 中的 `auto::Builder` 已能自适应处理两者，**无需其他改动**。

### 7.2 h3（新增一条完整栈）

新依赖：

```toml
quinn = "0.11"
h3 = "0.0.8"
h3-quinn = "0.0.10"
rustls = { version = "0.23", features = ["ring", "std", "tls12", "quic"] }  # 增加 quic feature
```

结构：

1. **QUIC endpoint**：`quinn::Endpoint::server(server_config, udp_addr)`，其中 `server_config` 由 `quinn::crypto::rustls::QuicServerConfig::try_from(rustls_cfg)` 构造（ALPN 设为 `["h3"]`）
2. **accept 循环**：每个 `quinn::Connection` 包进 `h3_quinn::Connection`，再交给 `h3::server::Connection`
3. **h3 → axum 适配层**：这是主要工作量。`h3` crate 的 API 是手动 `accept()` 取 `(Request, RequestStream)`，与 hyper/axum 的 `Service` 模型不同，需要一层转换：
   - 从 h3 取到 `http::Request<()>` 与请求流 → 读 body 组装成 axum 期望的 `Request<Body>`
   - 调用 `Router`（tower `Service`）
   - 把 `Response` 的 header 与 body 写回 `RequestStream`

   预计 100–200 行。**注意**：xhttp 的下行是流式的，适配层必须支持 **response body 流式写出**，不能先缓冲完再发——否则下行会退化成一次性返回，条带与低延迟全部失效。这是本项目对适配层的**硬要求**，也是最容易写错的地方。

4. **Alt-Svc 中间件**：在 axum Router 上挂一层，给所有响应加 `Alt-Svc: h3=":443"; ma=600`

### 7.3 DNS HTTPS 记录（可选，收益明确）

在 Cloudflare 为 `websieve.833337.xyz` 添加 HTTPS 记录（含 `alpn=h3`），可让 WebKit **首次连接就直接用 h3**，绕开"先 h2 再升级"的一轮。属于部署配置而非代码，可独立实施。

---

## 8. 对客户端的影响

预期**零代码改动**：

- `ui/emitter.js` 用标准 fetch，协议版本由 WebKit 自选（§2.4）
- `capabilities/transport.json` 的 URL glob 不涉及协议版本
- 条带的 lane/会话逻辑不感知 HTTP 版本

唯一需要重新标定的是**默认参数**：`StripeCfg::target_lanes = 4`（`crates/wsieve-mux/src/stripe_runtime.rs:87`）这个值是在 h1 的 6 条连接前提下测出来的最优点。h2/h3 下连接模型变了，该值需要重测，且 `extra_sessions` 的默认值可能需要从 0 改为非 0。

---

## 9. 验证计划

每一阶段都必须有**实测对照**，不接受"理论上更好"。

| 阶段 | 验证项 | 方法 | 通过标准 |
|---|---|---|---|
| h2 | ALPN 协商生效 | `openssl s_client -alpn h2,http/1.1` | 返回 `ALPN protocol: h2` |
| h2 | 连接数模型变化 | 大流量时于服务端 `ss -tn ... \| wc -l` | 从 6 降为 1（**确认张力真实存在**） |
| h2 | 吞吐代价量化 | 64MiB 单流，多轮中位数 | 记录数值，与 h1 基线对照 |
| h2+多origin | 补偿效果 | `extra-sessions=N`，同上 | 连接数 ≈ N，吞吐回到或超过 h1 基线 |
| h3 | 协议生效 | `curl --http3` 与 WKWebView 实测 | 返回 HTTP/3 |
| h3 | Alt-Svc 升级路径 | **连续两次**连接 | 第二次走 h3（首次走 h2 属预期，见 §2.3） |
| h3 | UDP 受阻时的回落 | 防火墙屏蔽 UDP 443 | 自动回落 h2，**服务不中断** |
| h3 | 下行流式性 | 大文件下载的 TTFB | TTFB 不随文件大小线性增长（否则适配层缓冲了整个 body） |
| 全部 | 条带参数重标定 | lane 数扫描 | 找出 h2/h3 下的新最优点 |

**测量纪律**（由本轮测试的教训得出）：跨境链路抖动可达 ±3 倍，任何单轮数字都不构成证据，必须多轮取中位数；且要警惕测量工具自身的天花板（本轮已踩过两次：目标端 Nagle 造成的 40ms 假延迟、`socketserver` 默认 backlog=5 造成的假并发上限）。

---

## 10. 分阶段交付

1. **阶段 1 — h2 宣告**：✅ **已完成（2026-09-11）**。一行 ALPN。实测连接数从 6→1，吞吐中位 11.0 → 35.5 MB/s（3.22 倍）。~~量化吞吐代价~~ —— 没有代价，是净收益，见 §3.1 的修正。
2. **阶段 2 — 多 origin**：~~补偿 h2 的连接数损失~~ → 降级为**增量优化**，在 h2 基线上重新测量其价值。打通 `extra-sessions` 需 hosts 写权限。**不再是阶段 1 的前置条件。**
3. **阶段 3 — h3 栈**：QUIC endpoint + h3→axum 适配层 + Alt-Svc 中间件 + 降级容错。
4. **阶段 4 — 参数重标定与加固**：重测 `target_lanes` / `extra_sessions` 默认值；按需调 quinn transport config 向主流实现靠拢（§5.2）。

---

## 11. 未决问题

1. **`extra-sessions` 的默认值该是多少？** 取决于阶段 2 的实测。过大则每个 origin 都要占一个本地转发端口与一条 hosts 记录，也更容易被关联分析。
2. **h3 下是否还需要多 origin？** QUIC 无队头阻塞，单连接的表现可能已足够（§3.3）。需阶段 3 的对照实测回答。
3. **hosts 劫持要求管理员权限**，这是 `extra-sessions` 的现实门槛（`shard_setup.rs` 先查 `writable()` 再降级，手动加 hosts 行绕不过）。是否值得为多 origin 引入权限提升，是产品决策而非技术决策。

---

## 12. h3 质量降级保护（2026-09-11 追加）

### 12.1 要解决的问题

跨境链路的 UDP 常被运营商 QoS 限速。此时 h3 **连得上但很慢**，而 WebKit
只在 QUIC **握手失败**时回落到 TCP —— **慢不会触发回落**。加上 Alt-Svc 的
`ma=86400`，客户端可能整整一天都卡在一条劣质 h3 连接上。

### 12.2 两个卡住方案的事实

**一、`Alt-Svc: clear` 拆不掉正在用的连接。** RFC 7838 明确：客户端对已建立
的替代服务连接，不必因缓存失效而停止使用；缓存只约束**新连接**的建立。
xhttp 的会话是长连接，只发 `clear` 等于什么都没做。（另：Safari 对 `clear`
的支持未见明确文档，不能想当然。）

**二、WKWebView 没有强制协议的 API。** 客户端侧无法否决 WebKit 的选择，
`assumesHTTP3Capable` 是 `URLRequest` 属性，页面内的 fetch 用不上。

### 12.3 解法：服务端自判 + 主动断连

quinn 的 `Connection::stats().path` 直接给出质量指标，**无需客户端配合**：

| 字段 | 用途 |
|---|---|
| `lost_packets` / `sent_packets` | 丢包率 |
| `congestion_events` | 拥塞事件频次 |
| `black_holes_detected` | 路径黑洞（UDP 被中途丢弃的强信号） |
| `rtt` / `cwnd` | 时延与窗口 |

判定为劣质后两步走，缺一不可：

1. **发 `Alt-Svc: clear`** —— 让客户端忘掉 h3，将来的新连接不再走 QUIC
2. **主动 `Connection::close()`** —— 逼客户端重连；因为第 1 步已清除记忆，
   重连会落到 TCP 上的 h2

单做第 1 步拦不住当前连接（§12.2），单做第 2 步则客户端重连时又会选 h3
（记忆还在）。**两步的顺序也不能反**：先清后断。

xhttp 本就有会话重建逻辑（实测日志可见「全部 mux 会话已死，重建」），
所以这次断连是可恢复的，代价是一次重连抖动。

### 12.4 必须有滞回

瞬时丢包不该触发降级，否则会在 h3/h2 之间反复横跳，每次都付一次断连代价。
要求：连续 N 个采样窗口都判劣才降级；降级后进入冷却期，期间**不再宣告 h3**。

### 12.5 ⚠️ 不要配置 DNS HTTPS/SVCB 记录

§7.3 曾建议用 DNS 的 HTTPS RR 宣告 h3，好处是首次连接就能用上 QUIC。
**该建议在本节成立后作废**：DNS 提示先于任何连接到达客户端，`Alt-Svc: clear`
对它无效——客户端会绕过我们刚清掉的缓存，继续尝试 QUIC。

**配了 HTTPS RR 就等于永久放弃降级能力。** 代价（首连慢一个往返）远小于
收益（劣质链路上能退回 TCP）。

### 12.6 分层交付

| 层 | 内容 |
|---|---|
| 1 · 配置化 | `--h3 on\|off` 控制 UDP 监听与宣告；`--alt-svc-ma N` 控制记忆时长 |
| 2 · 可观测 | 服务端记录 `PathStats`；响应加 `Timing-Allow-Origin` 让 emitter 能读 `nextHopProtocol`（WebKit 自 2022 起该字段受 TAO 保护，跨 origin 无此头一律返回空串） |
| 3 · 自动回退 | 按 §12.3 判定 + 两步降级，带 §12.4 的滞回 |

第 2 层是第 3 层的前提，也独立有价值：没有它，"现在到底跑在哪个协议上"
无从得知，阈值也就无从标定。
