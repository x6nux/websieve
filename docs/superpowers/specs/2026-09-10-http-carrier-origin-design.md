# websieve 设计文档：承载页改用 http origin，取回 IPC raw 快路径

- 日期：2026-09-10
- 状态：待评审
- 前置：`2026-08-23-webview-https-proxy-design.md` §3.2（及其 2026-09-09 修订）
- 实证：`scripts/spike-ipc-origin.{swift,sh}`（退出码 0/3/4 可复现）

---

## 1. 问题

2026-09-09 的修订（`d8a6d21`）把二进制回帧从 raw body 改为 base64 字符串，
理由是「承载页是远程 https origin，WKWebView 禁止 https 页面访问 custom
scheme，Tauri 的 raw body 快路径不可用」。该修订是正确的——它把一个
「握手永久挂起、零错误日志」的故障改成了可用状态。

但它留下一笔实打实的开销（`ui/emitter.js:241` 记录的实测）：

| 通路 | 吞吐 | CPU |
|---|---|---|
| base64 | 48.8 MB/s | 86% |
| raw | 114.9 MB/s | 34% |

**2.4 倍吞吐、四成 CPU。** 本次要解决的用户痛点是 CPU 占用。

---

## 2. 实证：决定因素是 scheme，不是 host

`ui/emitter.js:236` 原有一句判断——「本地 origin 一定能走 raw」——它是
**推断**，成立于 Tauri 自己的 asset protocol 页（`tauri://localhost`，其
本身就是 custom scheme）。它没有回答「一个普通的 `http://…` 页面呢」。

2026-09-10 用 `scripts/spike-ipc-origin.swift` 做了四格对照。探针复现
Tauri 在 macOS 上的机制：注册一个 `WKURLSchemeHandler` 处理 `ipc` scheme
（wry 的做法），页面用与 emitter 相同的形态去 `fetch('ipc://localhost/…')`
（POST + `application/octet-stream` + `Uint8Array`）。四格用的是**同一段
注入脚本**，唯一变量是承载页 origin。

| 承载页 origin | 结果 |
|---|---|
| `http://127.0.0.1:18099` | ✅ ok，`httpBody=8B` 完整到达 handler |
| `http://localtest.me:18099` | ✅ ok，`httpBody=8B` |
| `https://127.0.0.1:18443`（自签） | ❌ `TypeError: Load failed` |
| `https://example.com` | ❌ `TypeError: Load failed` |

> 本地 https 那一格需要一张自签证书，探针为此实现了接受任意 serverTrust 的
> `didReceive challenge`。该放宽**只存在于 spike**，生产代码里没有、也绝不
> 该有。它只影响「证书校验」，不影响被测的 custom scheme 判定。

**结论：WKWebView 封的是「https 页面（secure context）访问 custom
scheme」，与 origin 是否本地无关。**

两条独立的交叉验证：

- `localtest.me` 的 hostname 不是 `localhost`/`127.0.0.1`，因此它**不是**
  W3C 意义上的 trustworthy origin，却通过了 → 排除「trustworthy 决定」。
- `https://127.0.0.1` 是最典型的本地 origin，却失败了 → 同样排除。

### 2.1 第二问：http 承载页还能不能 fetch https 端点？

这是本方案的另一个成败点，且方向相反——数据面**必须**留在 https，否则
TLS 指纹就没了，那正是 §3 非目标里要守住的东西。mixed content 理应只拦
「https 页面加载 http 资源」这个降级方向，但没测就是推断。

同一个探针加测一发：从 `http://127.0.0.1:18099` 去 `fetch` 一个已知回
`Access-Control-Allow-Origin: *` 的 https 端点（用它把 CORS 这个变量排除
掉，单看 scheme 升级方向）。

```
PROBE_MIXED ok status=200
```

**通过。** scheme 升级方向不被拦，承载页降到 http 不会连累数据面。

判定已固化进 `scripts/spike-ipc-origin.sh`：两问都通过才退 0；若出现
「raw 可用但 fetch 不了 https」，脚本会明确报方案作废而不是含糊放行。

**反证有效性**：对照组 `https://example.com` 的报错是
`TypeError: Load failed`，与 2026-09-09 生产实测记录的字样完全一致。
即这个探针确实复现了 Tauri 遇到的那条限制，它能测出失败，因此
「实验组通过」这一结论可以采信。

> 由此，`emitter.js` 里 `isLocalOrigin()` 这个函数名与判据都是错的：
> 它判 hostname，而真正的判据是 `location.protocol`。见 §5.4。

---

## 3. 目标与非目标

### 目标

- 承载页改用 `http` origin，取回 raw 快路径，把 base64 的 CPU 开销去掉
- **代理流量的 TLS 与 HTTP 指纹仍然全部由 WebKit 本体产生**（不可退让）
- 无权限环境（写不了 hosts）下仍可用，只是伪装形态次一等

### 非目标

- **反向代理数据面。** 把 `fetch` 指向本地、由 Rust 转发到真实服务端，会让
  TLS 握手变成 rustls 发出的，JA3/JA4 与 HTTP/2 指纹全部退化为 Rust 客户端
  的指纹。那等于删掉本项目存在的理由（前置设计文档 §1）。这条纪律在
  `src-tauri/src/shard.rs` 的模块注释里已有成文表述：「**绝不终结 TLS**……
  一旦在这里终结 TLS，整个项目『用真实浏览器指纹』的前提当场作废」。
  本设计**只把承载页的那一张 HTML** 放到本地，数据面一个字节都不改道。
- 消除 `Origin` 请求头。承载页与数据面一旦不同源，浏览器必然附带
  `Origin`，JS 无法阻止。这是本设计明码标价的代价，见 §6。

---

## 4. 现状与改动落点

现状（条带启用时，`shard_setup.rs:281`）：

```
承载页    https://a.example:30000/        ← hosts 劫持到本地，shard 纯 TCP 转发到真实 :443
会话 0    相对路径（与承载页同源，无 Origin 头）
会话 1..N https://a.example:30001/ …      ← 同样经 shard 转发
```

改动后：

```
承载页    http://127.0.0.1:{carrier_port}/  ← 本地 HTTP server 直接响应，零网络流量
会话 0    https://a.example:30000/          ← 显式绝对 URL（关键，见 §5.3）
会话 1..N https://a.example:30001/ …        ← 完全不变
```

数据面的每一条会话基址仍指向 shard 转发器、仍是 https、仍由 WebKit 直接
完成 TLS 握手。改的只有「承载那张 HTML 的是谁」。

---

## 5. 设计

### 5.1 本地承载页 server（新增 `src-tauri/src/carrier_page.rs`）

一个只做一件事的 HTTP 监听：对任何请求返回同一张最小 HTML。

- `TcpListener::bind("127.0.0.1:0")` —— **端口由 OS 分配**。不复用
  `shard_base_port` 段：那一段的每个端口都是 TCP 转发器，拿 http 去打它
  等于把明文 HTTP 塞给真实服务端的 TLS 端口。也不用固定端口——固定端口是
  本机指纹，别的进程扫到就知道装了什么。
- 页面内容用最简 HTML 即可。**承载页内容对伪装零影响**：它由本地直接
  响应，根本不出网，网络上没有任何观察者能看到它。spike 也确证了 raw
  通路与页面内容无关（探针页面是一行空 body）。
- 生命周期与 `ShardGuard` 一致：句柄丢弃即停止监听。

不引新依赖：项目已有 tokio，一个 accept 循环 + 固定响应字符串足够。
`ponytail:` 不解析请求（读到 `\r\n\r\n` 即回），因为这个 server 永远只有
一个客户端、只有一个响应。要是哪天需要多路径，再上真正的路由。

**全进程只起一个**，所有出站、两种承载模式共用同一个端口。`isolated` 下
每出站一个窗口，但它们加载的是同一个 server 的同一个响应——`shared` /
`isolated` 的差别仍然只在「建几个窗口」，与承载页由谁提供无关，这维持了
`carrier.rs` 纪律③「承载方式对出站层透明」。

### 5.2 承载页 URL：恒为回环地址

```
page_url = http://127.0.0.1:{carrier_port}/
```

不分劫持成功与否，不分出站，没有第二种形态。

曾经考虑过让它用真实域名（`http://a.example:{carrier_port}`，靠 hosts 劫持
落到本地），以为泄漏出去的 `Origin` 指向自己的域名会更像。**判断反了**，
两条理由：

1. **伪装上更差。** `http://127.0.0.1:53119` 是全世界前端每天都在产生的
   形态（本地开发环境调线上 API）。而 `http://a.example:53119` ——一个跑在
   自家域名随机高端口上的 http 页面——是真实世界里根本不存在的形态，服务端
   一眼就知道自己没开那个端口。
2. **安全上更差。** 域名形态落在本地**靠的是 hosts 劫持**；劫持若在运行期
   被外部清掉，又赶上承载窗口 reload（`main.rs:836` 有这条路径），该 URL 就
   会真的连去远程，而中间人对一个明文 http 承载页可以注入任意 JS。回环地址
   则是**结构保证**：它不可能出本机。

选回环还顺带消掉了两样东西：不需要为「承载页是不是真的本地」加校验，
`capabilities/transport.json` 现有的 `http://127.0.0.1:*` 也已经覆盖，
不必放宽到 `http://**:*`（见 §5.6）。

本设计因此**不依赖 hosts 劫持、不依赖管理员权限**。`extra_sessions == 0`
（当前默认）同样生效，不要求用户先开条带。

### 5.3 拆开「窗口加载什么」与「数据面打哪儿」

这是本设计最实质的一处改动，也是最容易做错的一处。

现状里 `page_url` **一个字段同时承担两件事**，因为原设计下它们恰好相同
（承载页就是服务器首页，所以「窗口加载的 URL」与「数据面的 origin」是同
一个）。改动后二者不再相同，必须拆开。

三条现存的耦合：

1. `shard_setup.rs:281`：`session_bases[0] = None`，语义「用相对路径」，
   成立前提是承载页恰好是会话 0 的 origin（共用 `ports[0]`）。
2. `carrier.rs:118`：非宿主出站的基址是 `Some(origin_of(url)?)`，其中 `url`
   是**那个出站自己的 `page_url`**。
3. `runtime_state.rs:474`：`session_bases[0]` 会被
   `carrier.base_for(&p.name)` **覆盖**——也就是说会话 0 的基址实际由
   `CarrierPlan` 决定，不是 `shard_setup`。

若只改 `page_url` 而不动这三处，后果是：所有出站的 `page_url` 都变成同一个
`http://127.0.0.1:{carrier_port}/`，于是第 2 条会把**每个非宿主出站的数据面
基址都算成本地承载 server**，代理流量整个打到那张空 HTML 上。

#### 职责重新划线

| 职责 | 归属 |
|---|---|
| 每个出站的数据面基址（各自的 https） | `shard_setup`，`session_bases` 每项都是绝对 URL，**含会话 0** |
| 承载页 URL（全局唯一） | `carrier_page` server；`CarrierPlan` 只拿它当窗口 URL |
| 「不认识的出站必须拒绝」（§6.4 防线） | `runtime_state` 改用 `window_label` 做存在性校验 |

`shard_setup` 侧：

```rust
// 改动前：会话 0 靠「与承载页同源」这个已不成立的前提吃相对路径
let mut session_bases = vec![None];
session_bases.extend(ports.iter().skip(1).map(|p| Some(origin(*p))));
let page_url = format!("{}/", origin(ports[0]));

// 改动后：数据面全部显式绝对 URL；`page_url` 字段整个删掉
let session_bases: Vec<Option<String>> =
    ports.iter().map(|p| Some(data_origin(*p))).collect();
```

原来的 `origin` 闭包从 `split_url` 拿 scheme。改动后**数据面的 scheme 与
承载页的 scheme 是两件独立的事**，闭包改名 `data_origin` 并且只服务数据面
——共用一个名字，下一次有人改承载页 scheme 就会把数据面一起带歪。

**`ShardPlanEntry.page_url` 字段删除**，而不是改成承载页 URL 再传下去：
承载页 URL 全局唯一，每个 entry 存一份完全相同的值是冗余；更重要的是
「条带不再关心承载页」正是本节这条职责划线本身。删字段还能让编译器替我们
找出所有仍在依赖旧耦合的地方。

降级路径 `ShardPlanEntry::degraded` 同样删掉 `page_url` 的赋值，
`session_bases[0]` 显式填服务端原始 URL 的 origin。

`CarrierPlan` 侧：入参从 `&[(出站名, page_url)]` 变成 `&[出站名]` 加一个
全局 `page_url`；`Slot.base` 字段与 `base_for` 方法**整个删除**——推导数据面
基址的职责已经不在它身上了。

`runtime_state` 侧：不再用 `base_for` 覆盖 `session_bases[0]`，改为只做存在
性校验：

```rust
// §6.4 的防线原样保留，只是换了个不涉及基址推导的方法来守：
// 认不出的出站必须报错，绝不能套一个默认基址把流量发去别处。
carrier
    .as_ref()
    .expect("出站非空时承载计划必然已构建")
    .window_label(&p.name)
    .ok_or_else(|| anyhow::anyhow!("承载计划里没有出站「{}」", p.name))?;
```

> 顺带澄清一个曾被误判的点：即便漏改上述任何一处，**也不会明文出网**。
> 错误的基址指向的是回环上的承载 server（`carrier_port` 由 OS 从 ephemeral
> 段分配，与 `shard_base_port` 段不重叠；即便重叠，那个端口已被转发器占用、
> bind 会直接失败）。失败形态是 Noise 握手拿到一段 HTML 后解析失败——
> **会报错，不是静默的**，属功能 bug 而非安全问题。

### 5.4 emitter 判据修正（`ui/emitter.js`）

`isLocalOrigin()` 判 hostname 是不是 `127.0.0.1`/`localhost`/`::1`。承载页
恒为 `http://127.0.0.1:{port}`（§5.2），所以这个函数**碰巧会返回正确结果**，
不改也能跑。

仍然要改，因为它错的方向是最危险的那一种。§2 证明真判据是 scheme，而这个
函数判的是 host，两者只在当前形态下恰好重合。一旦承载页变成
`https://127.0.0.1`（有人给本地 server 配了 TLS、或换成别的本地形态），它
会返回 true、开启 RAW，而 RAW 实际不可用——症状就是 2026-09-09 那次：
**心跳正常、传输永久挂起、零错误日志**。判据与被判之物必须是同一件事，
否则下一次重合被打破时没有任何东西会报警。

改为按 spike 确证的真判据：

```js
// raw（custom protocol）通路可用性完全由**承载页的 scheme** 决定：
// WKWebView 禁止 https 页面访问 custom scheme，与 origin 是否本地无关。
// 四格对照见 scripts/spike-ipc-origin.sh。
function canUseRawIpc() { return location.protocol === 'http:'; }
```

`LOCAL` 的另一个用途 `CHUNK_MAX`（`emitter.js:71`）跟着同一判据走：它的
理由是「IPC 通路便宜就多攒一点」，与 raw 是否可用同源。

### 5.5 承载计划收敛为「只管窗口」

按 §5.3 的划线，`CarrierPlan` 交出数据面基址的职责后只剩两件事：**哪个出站
落在哪个窗口、那个窗口加载什么 URL**。而后者现在全局只有一个值。

于是两种模式的差别收敛到只剩「建几个窗口」：

- `shared`：一个窗口（`main`），全部出站挂在上面
- `isolated`：每出站一个窗口（`wsieve-transport-{名}`），但它们加载的是
  **同一个** `carrier_page` server 的同一张 HTML

原先 `shared` 里「宿主用相对路径、其余用绝对 URL」的区分随 `Slot.base` 一起
消失——承载页挪到回环之后没有任何出站与它同源，那个区分失去了对象。宿主
（`carrier_host`）这个概念本身仍然保留：它决定 `shared` 模式下窗口的归属
语义与错误信息，只是不再影响任何基址。

§6.4 的防线不随 `base_for` 一起删，它挪到 `window_label` 上（见 §5.3 末尾的
代码），语义完全不变：认不出的出站必须报错，绝不套默认值。

### 5.6 capability：不需要改动

`src-tauri/capabilities/transport.json` 现有 remote urls 是
`["http://127.0.0.1:*", "https://**:*"]`——承载页恒为
`http://127.0.0.1:{carrier_port}`，第一条已经覆盖。

这是选回环形态（§5.2）顺带拿到的：若用真实域名，`http://a.example:53119`
一条都不匹配，就得放宽到 `http://**:*`，把授权面从「一个回环地址」扩大到
「任意 http 页面」。

`transport.json` 的描述里有一句需要更新——「传输窗口：加载**远端服务器的
伪装页**」已不再属实，改为本地承载页，并补上「页面 JS 不再由远端服务器
控制」这一条（见 §6 的攻击面收窄）。

### 5.7 base64 通路保留

`bridge.rs` 的 `bs64_decode` 与 emitter 的 `bytesToBase64` 全部保留。
`decode_body` 本来就两条都认（`main.rs:1171` 的注释已说明这是刻意的），
留着零成本，而它在三种情况下仍是唯一可用的路：

1. Windows / Android 的 WebView IPC 机制与 WKWebView 不同，未逐项验证
2. 承载页因任何理由退回 https origin
3. `__wsieveTune.raw=0` 显式排障

### 5.8 编排顺序（`main.rs`）

承载页 URL 是 `CarrierPlan` 的入参，而 `CarrierPlan` 在
`build_startup_plan` 里构造，因此承载页 server 必须先于它起来。插进现有的
顺序纪律里：

```
-1) 起承载页 server，拿到它的 URL         ← 新增
 0) 查 fake-ip 段归属
 1) 解析真实 IP          ┐
 2) 写 bypass 路由       ├ plan_many 内部（签名不变——条带不再碰承载页）
 3) 起转发器             │
 4) 写 hosts             ┘
 4.5) build_startup_plan（承载页 URL 在这里进 CarrierPlan）
 5) 建承载 WebView
 6) 最后拉起 TUN
```

放在 `-1` 而不是紧挨着 `4.5`：它 bind 的是 `127.0.0.1:0`，不解析域名、
不出网、不依赖 hosts 或 TUN 任何一步的结果。没有任何理由让它排在一串可能
失败的异步 IO 之后——越早失败越早报，而且报的时候还没有任何系统状态被托管
出去（hosts、bypass 路由都还没写）。

**它不需要 bypass 路由**（对比转发器必须有）：TUN 捕获的是出网流量，而这个
server 的连接两端都在回环上，从不离开本机。这条差异要写在代码注释里，否则
下一个读 `tun::bypass_hook` 的人会以为漏了一处。

失败处理：起不来就**整体拒绝启动**（`exit(2)`），不降级。没有承载页就没有
传输，静默降级只会变成「应用起来了但永远连不上，且没有一条能解释原因的
日志」——那正是 2026-09-09 那次故障的形态。

---

## 6. 代价（明码标价）

1. **`Origin: http://127.0.0.1:{port}` 必然出现。** 承载页与数据面不同源，
   浏览器强制附带，JS 无法阻止。中间盒看不到（它在 TLS 里），能看到的是
   服务端与 CDN。

   > 这是三种候选里最不显眼的一种：本地回环 + 随机高端口是全世界前端每天
   > 都在产生的形态（本地开发环境调线上 API）。真正扎眼的是自家域名跑在
   > 随机高端口上的 http 页面，那种形态真实世界里不存在（见 §5.2）。
   > 但「不显眼」不等于「没有」——改动前是同源、**根本没有这个头**。

2. **丢失「先加载首页、再持续 XHR」的自然流量形态。** 承载页不再出网，
   网络上看到的是凭空开始的 XHR。
3. 承载页从远程 https 变成本地 http，`Sec-Fetch-Site` 等元数据头随之改变。

### 一项抵消：攻击面收窄

`transport.json` 的描述原文写着：「该页面的 JS **由服务器控制**，服务器被
攻破即等同于这些命令被攻破」。改动后承载页 JS 不再由远端服务器提供，这条
攻击面消失——页面来源从「信任远端服务器」变成「信任本机进程」。

承载页恒为回环地址（§5.2）让这条收窄是**结构性**的而非约定性的：那个 URL
不可能被中间人截获或劫持，因此不需要任何「这张页面是不是真的我们的」的
运行期校验。

---

## 7. 验收

### 7.1 自动化测试

- `shard_setup`：**`session_bases` 每一项都是 `Some(https://…)`**（钉死
  §5.3——`None` 会让会话 0 去请求承载页那张 HTML）。`ShardPlanEntry` 的
  `page_url` 字段随本设计**删除**：承载页 URL 全局唯一，每个 entry 存一份
  相同的值是冗余，而「条带不再关心承载页」正是 §5.3 那条职责划线本身
- `carrier`：`shared` / `isolated` 下 `windows()` 给出的每个窗口都加载同一个
  承载页 URL；**`window_label` 对未知出站仍返回 `None`**（§6.4 防线换了载体
  但不能失守）
- `runtime_state`：`build_startup_plan` 产出的 `session_bases` **每一项都是
  `Some(https://…)`**，且与 `shard_setup` 给的值逐字相同（钉死「不再被承载
  计划覆盖」）；未知出站仍然报错而不是拿到默认基址
- `carrier_page`：server 起得来、返回合法 HTML、句柄丢弃后端口释放
- `emitter`（vitest）：`canUseRawIpc()` 对 `http:` 返回 true、对 `https:`
  返回 false
- `capability_isolation`：现有测试继续绿（transport 与 control 交集仍为空，
  且 transport 的 urls **没有**被放宽）

### 7.2 真机端到端（不可省）

spike 用的是手写 `fetch`，而 Tauri 的 `__TAURI_INTERNALS__.invoke` 会多带
若干头（`Tauri-Callback` 等）。scheme 既然不被封，理应同样能通，但**这一条
必须在真实 app 上验证，不能拿探针的成功替它背书**。

判据：
1. 出站进入 `Connected`，curl 经代理返回服务端出口 IP
2. 日志中不出现 `JSON body 缺少字符串字段 f`（走了 base64 回退就会出现）
3. 承载页 origin 是 http（`WSIEVE_SHOW_WINDOW=1` 看得到）
4. **抓包确认离开本机的流量全部是 443 的 TLS**。承载页降到 http 之后，
   本项目的核心不变量（数据面 TLS 由 WebKit 直接握手）值得复核一次——
   不是因为 §5.3 会导致裸奔（它不会，见该节），而是因为这次动的正是
   scheme，改坏了它没有第二道防线
5. CPU 占用较改动前明显下降（痛点本身）

---

## 8. 风险与未决

1. **Windows / Android 未验证。** spike 只测了 macOS 的 WKWebView。WebView2
   的 IPC 走 `window.chrome.webview.postMessage`，机制不同，可能本来就没有
   这条限制。`canUseRawIpc()` 按 scheme 判在那两个平台上可能过于保守（该开
   没开）。*验证方法*：在各平台跑 §7.2 的判据 2。
2. **`Origin` 头的实际风险等级未量化。** 本设计按「服务端是自己的、CDN 日志
   分析是被动且非实时的」来接受它。若将来出现主动比对 `Origin` 的部署形态，
   需要重新评估。
3. **多会话仍然需要多端口。** 本设计不改变这一点，也改不了：h2 把同一
   origin 的请求复用到一条 TCP（`webkit_tcp_probe.rs` 实测 4 会话全挤 1
   条），而 origin 含端口，多端口是造出多条独立 TCP 的唯一结构手段。要在
   本地「统一管理连接」就必须由本地去建那些连接，那等于 §3 非目标里禁掉的
   反代数据面。浏览器侧也没有任何 API 能让 `fetch` 强制开新连接。

