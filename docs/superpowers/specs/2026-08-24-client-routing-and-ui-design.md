# websieve 设计文档：分流路由、多出站与控制界面

- 状态：设计定稿，待实现
- 日期：2026-08-24
- 前置文档：`docs/superpowers/specs/2026-08-23-webview-https-proxy-design.md`（下称**传输 spec**）

---

## 1. 概要

传输 spec 解决了「一条流量如何伪装成正常网页浏览穿过去」。本文档解决其后的全部问题：**流量该往哪条路走，以及人怎么控制它**。

具体覆盖六件事：多出站服务器的配置与并存、Clash 语法的分流规则引擎、GEO 数据、DNS 子系统、混合入口（SOCKS5 / HTTP / 系统代理 / TUN），以及一个从零建起的控制界面。

实现顺序是「先内核后界面」，但架构与数据模型在本文档中**一次性定死**，避免分期实施造成返工。

---

## 2. 目标与非目标

### 目标

- 多个出站服务器**同时保持连接**，规则可将不同目标分发到不同落地
- 规则语法**兼容 Clash**，网上现成规则集可直接粘贴使用
- 支持域名匹配、IP/CIDR 匹配、GEOSITE / GEOIP、端口匹配
- 混合端口（SOCKS5 + HTTP 同口）、自动系统代理、TUN 全局模式
- DNS 子系统：内部解析器（服务于规则判决）+ fake-ip（服务于 TUN）
- 完整的图形控制界面，全部功能可视可控
- 恢复连接的路径上，能优化的全部优化

### 非目标

- **策略组**（`proxy-groups`：select / url-test / fallback / load-balance）。首版规则直接引用出站名。配置格式天然预留此位置——将来规则目标可以是组名，语法一字不改
- **订阅**（从 URL 拉取节点列表）。websieve 的出站需要成对密钥，不存在公开订阅生态
- **移动端界面**。传输 spec §3.3 已排除 iOS；Android 界面另议
- **进程级规则**（`PROCESS-NAME`）。需平台特定实现，收益不足以进首版

---

## 3. 既有约束与前提

### 3.1 从现有代码读出的事实

设计必须建立在仓库当下的真实状态上，而非文档描述上：

| 事实 | 出处 |
|---|---|
| 客户端**没有任何用户界面**。唯一的 WebView 是传输载体，默认 `visible(false)` | `src-tauri/src/main.rs:71` |
| 配置**全部来自环境变量**，启动读一次，不落盘 | `src-tauri/src/bootstrap.rs:34` |
| **单服务器**：一份 `server_pub` / `client_priv` / `session_bases` | `src-tauri/src/proxy.rs:26` |
| **无分流**：SOCKS5 收到什么就无条件递给隧道 | `src-tauri/src/proxy.rs:57` |
| `AddrPort` 已含 `Domain / V4 / V6` 三态，是分流判决的天然输入 | `crates/wsieve-proto/src/addr.rs:5` |
| `WebViewTransport::with_base()` 已支持给会话指定不同 origin | `src-tauri/src/bridge.rs:269` |
| `HostsFile::set_managed()` 签名已收**域名数组**，天然支持多域名托管 | `src-tauri/src/hosts.rs` |
| 转发器是**懒连接**：入站到达后才 `TcpStream::connect(upstream)` | `src-tauri/src/shard.rs:78` |
| 会话死亡时**无条件** `window.location.reload()` | `src-tauri/src/proxy.rs:221` |
| capability 仅授权 `main` 窗口，且开放 `remote.urls` 给任意 https | `src-tauri/capabilities/default.json` |

### 3.2 传输 spec 与实现的偏差（必须记录）

**传输 spec §6.2 写明「sid 在 cookie」，但实现从未如此。** 实际是：

```
client.rs:133   format!("/api/sync?n=0&sid={}", sid_b64)
client.rs:205   format!("/api/events?sid={}", sid_b64)
server/lib.rs:222   query_param(&uri, "sid")
```

sid 全程走 **query**，服务端不读 cookie。

这个偏差对本设计**具有决定性意义**：Safari 13.1 起默认阻止全部第三方 cookie，iOS/iPadOS 14 起所有第三方 WKWebView 实例一律套用 ITP，且无任何受支持的方式恢复。若 sid 真的走 cookie，则一个 WebView 绝无可能承载跨域名的第二个服务器。**因为它走 query，这条路是通的**——这正是 §9.1 单 WebView 多出站方案成立的前提。

> **行动项**（三处，一并处理）：
> 1. 修正传输 spec §6.2 的 `Cookie: sid=...` 描述，或在实现中改回 cookie。二者必须一致。本设计按**实现现状（query）**推进；若将来改回 cookie，§9.1 必须同步改为「每出站独立 WebView」
>    —— **代码侧已复核确认走 query**（`wsieve-xhttp/src/client.rs:133`／`:205`／`:453`，服务端 `wsieve-server/src/lib.rs:228`／`:250`；全仓 `crates/` 与 `src-tauri/src/` 对 cookie 零命中）。传输 spec §6.2 的散文仍待订正。
> 2. ~~§9.1 放宽 CORS 后，传输 spec **§6.7 第 3 条**（「CORS 响应头…只对与 `Host` 同域名的 `Origin`」）随之失效，需同步修订~~ —— **已完成**（提交 `dbb300d`）
> 3. ~~`crates/wsieve-server/src/lib.rs:269` 的 `cors_origin` 文档注释同样描述了该限制，需一并更新~~ —— **已完成**（提交 `dbb300d`）

### 3.3 承载方式的可行性边界

已核实、不可绕过的约束：

- **iframe 嵌多服务器不可行**。注入这一关能过（wry / Tauri 2 有 `initialization_script_for_all_frames`，`webview/mod.rs:997`），但跨站 iframe 是 ITP 的头号打击目标；若将来 sid 改回 cookie，此路彻底封死
- **单 WebView 轮流导航不可行**。导航即销毁上一页面的 JS 上下文与全部在途 fetch
- **一窗多 webview 可行但无收益**。`Window::add_child`（`window/mod.rs:1142`）门控在 `feature = "unstable"`；它省的是窗口而非 WKWebView 实例，而传输窗口本就隐藏

---

## 4. 架构

### 4.1 分层

```
控制层    控制窗口 "control"（Svelte）  ←→  IPC 命令面 / 事件流
              ↓
入口层    混合端口(SOCKS5+HTTP) · 系统代理 · TUN
          三者统一产出 (AddrPort, 双向流)
              ↓
路由层    规则引擎（纯函数） + GEO 库 + DNS Resolver
          判决 = Outbound(名) | Direct | Block
              ↓
出站层    出站管理器（每出站一份会话循环，共享 WebView 承载）
          Direct（TcpStream） · Block（立即拒绝）
              ↓
        既有且不动：mux · noise · xhttp · emitter
        要改：shard 转发器（预建 TCP + 多域名托管）
```

### 4.2 三条边界纪律

**① 路由层是纯函数，DNS 解析靠「两阶段求值」留在层外。**

引擎不碰网络、不读配置文件、非 async。但 IP 类规则遇到域名目标时确实需要解析结果——解决办法不是让引擎变 async，而是让它**把解析需求作为返回值抛出**：

```rust
pub enum Decision { Outbound(String), Direct, Block }

pub enum Verdict {
    Decided(Decision),
    /// 扫到一条 IP 类规则、目标是域名、且该规则未带 no-resolve。
    /// 调用方解析后带着结果重新调用一次即可。
    NeedResolve { domain: String },
}

pub fn evaluate(
    target: &AddrPort,
    resolved: Option<&[IpAddr]>,   // 第一轮传 None
    rules: &Rules,
    geo: &GeoDb,
) -> Verdict
```

调用约定：

1. 第一轮 `evaluate(target, None, ..)`。多数流量在域名类规则处就命中，**根本不会触发解析**
2. 若返回 `NeedResolve`，调用方（async 上下文）解析，然后 `evaluate(target, Some(&ips), ..)` 再来一轮。解析失败或超时传 `Some(&[])`
3. 第二轮**不再返回 `NeedResolve`**——已解析过，IP 规则直接用 `resolved` 匹配，空切片即视为不匹配

第二轮从头重扫而非从断点续扫：规则只有几十条，开销可忽略，换来的是**函数完全幂等**、无需维护游标状态。

这条纪律买到三样东西：路由逻辑可被穷举单测；**最多解析一次、且按需解析**（不需要时零 DNS 泄漏）；以及 UI 的「规则试算」可直接复用同一份代码——探针可选择只跑第一轮（快、不发 DNS）或跑完两轮（准），两种模式天然由同一签名支持，且结果与真实判决**永远一致**。试算与实际不一致的排查工具比没有更糟。

**② 入口层与出站层互不认识。** 入口只产出 `(目标地址, 双向流)`，出站只消费。TUN 因此是纯增量——加一个入口，不动其余任何一层。

**③ WebView 承载方式对出站层透明。** 出站只声明「我要一个 transport」，由承载器决定共享还是独占。`shared` / `isolated` 两种模式因此不污染业务逻辑。

---

## 5. 配置模型

### 5.1 格式：Clash 风格 YAML

规则语法既已兼容 Clash，配置就必须是 YAML——JSON 无法写注释，而规则文件是最需要注释的地方。

**依赖选择需要注意生态现状**：`serde_yaml` 已于 2024-03 废弃；其最流行的 fork `serde_yml` 也已废弃，且有安全公告 **RUSTSEC-2025-0068**（≤0.0.12 全部 unsound，`Serializer.emitter` 可能段错误，根源是 C-FFI libyaml）；`serde_norway` / `serde-yaml-bw` 可用但底层仍是 `unsafe-libyaml`。

**选用 `serde-saphyr`**：纯 Rust、内存安全、在维护。注意 YAML 1.2 严格性（"Norway 问题"，`no` 解析为字符串而非布尔）——配置一律使用 `true` / `false`，不受影响。

写回路径另有约束，见 §5.6 的实现约束小节：**结构化写不走 serde 序列化**，改用它 re-export 的 `granit-parser` 做事件流定点改写。

### 5.2 完整 schema

```yaml
# <app_config_dir>/config.yaml   权限 0600

# ── 入口 ────────────────────────────────────────
mixed-port: 7890            # SOCKS5 + HTTP 同口，首字节嗅探
bind-address: 127.0.0.1
allow-lan: false
mode: rule                  # rule | global | direct
global-outbound: ''         # mode: global 时的落地；空则取 MATCH 规则的目标
log-level: info
system-proxy: false         # 连接时写入系统设置，退出时恢复

# ── 出站 ────────────────────────────────────────
proxies:
  - name: "日本节点"          # 规则中引用的就是它
    type: websieve           # 遇到 ss/vmess 等一律明确报错，不静默忽略
    url: https://example.com/
    server-pub: "<hex32>"
    client-priv: "<hex32>"   # 明文，靠 0600 保护
    extra-sessions: 3        # 多端口条带的额外会话数
    mux-prefs: [0, 1, 2, 3, 4]

# ── 规则（有序，首命中即返回）────────────────────
rules:
  - DOMAIN-SUFFIX,google.com,日本节点
  - GEOSITE,category-ads,REJECT
  - GEOSITE,cn,DIRECT
  - IP-CIDR,192.168.0.0/16,DIRECT,no-resolve
  - GEOIP,CN,DIRECT
  - MATCH,日本节点            # 必需，见 §5.5

# ── DNS ─────────────────────────────────────────
dns:
  enable: true
  listen: ''                      # 空 = 不对外监听（仅第一层 Resolver）
  enhanced-mode: fake-ip          # TUN 阶段才生效
  fake-ip-range: 198.18.0.0/15
  fake-ip-filter: []              # 出站服务器域名自动加入
  nameserver:
    - https://1.1.1.1/dns-query   # 必须 IP 字面量，见 §7.2
  proxy-server-nameserver:
    - system                      # 专解出站服务器域名
  timeout-ms: 2000
  cache: { max: 4096, negative-ttl-s: 30 }

# ── GEO 数据 ────────────────────────────────────
geo-auto-update: true
geo-update-interval: 24
geox-url:
  geoip:   "https://…/geoip.dat"
  geosite: "https://…/geosite.dat"

# ── TUN（阶段 6）─────────────────────────────────
tun:
  enable: false
  stack: smoltcp
  auto-route: true

# ── websieve 专有 ───────────────────────────────
carrier: shared             # shared = 单 WebView | isolated = 每出站独立
carrier-host: ''            # 页面宿主出站名；空 = 取第一个启用的
shard-base-port: 18443
```

### 5.3 四个刻意取舍

**① 内置出站名用大写 `DIRECT` / `REJECT`。** Clash 的约定，只有照办才能真正粘贴现成规则集。

**② 规则引用出站用「名字」而非 id。** 为了规则文本可读、可粘贴。代价是改名需联动重写引用——由 UI 承担，用户无感。内部仍保留稳定 id 用于状态追踪。

**③ 端口不让用户手工管。** 只配 `shard-base-port` 起始值，各出站按 `extra-sessions + 1` 自动分段。手工管端口属于必然出错的那类事。

**④ `proxy-server-nameserver` 沿用 Clash 字段名，不自创。** 其语义恰好是「专门用来解析代理服务器域名」，与本设计需要的 bootstrap 解析器完全对应。

### 5.4 私钥存储

`client-priv` 明文存于配置文件，靠 **0600 权限**保护。写入时必须以 0600 创建（而非先创建再 chmod，避免竞态窗口）。

UI 在导出 / 分享配置的入口处必须显式警告：**该文件含私钥**。

> 已评估并放弃系统钥匙串方案：多一个依赖、跨平台行为不一、且备份迁移变复杂。当前取舍以可迁移性优先。

### 5.5 兜底出站：`MATCH` 是必需的，没有隐式默认

本设计**不引入独立的 `final:` 字段**。兜底就是 Clash 的 `MATCH` 规则，与现成规则集保持一致。文档其余部分凡提及「兜底」，一律指 `MATCH` 规则的目标。

**加载时校验：规则列表缺少 `MATCH` 即报错，拒绝启动。**

不设隐式默认（既不默认 `DIRECT` 也不默认 `REJECT`）的理由与 §6.4 同源：隐式走 `DIRECT` 就是静默裸奔，隐式走 `REJECT` 则表现为「莫名其妙全断网」。两者都是让用户在不知情的状态下承担后果。**兜底必须是显式声明的。**

`mode: global` 需要一个落地目标，而策略组已被排除（§2），因此用 `global-outbound` 字段承担：

| `global-outbound` | 行为 |
|---|---|
| 指定了出站名 | 全局模式一律走该出站 |
| 空字符串（默认） | 取 `MATCH` 规则的目标 |

UI 在 `mode: global` 旁提供出站选择器，写入的就是这个字段。

### 5.6 结构化保存必须保留规则注释

§5.1 选择 YAML 的**唯一理由**是能写注释。但 §11.2 同时提供 `config_save`（表单结构化写回）与 `config_save_raw`（原始文本）——若结构化保存走朴素的 serde 序列化，用户在 UI 里点一次「启用出站」，手写的规则注释就全没了。这会直接摧毁选择 YAML 的理由。

策略：

| 区域 | 保存行为 |
|---|---|
| **`rules` 数组** | **必须保留注释**。解析时把每条规则的**前导注释行与行尾注释**绑定到该规则，写回时一并输出。规则被删除时其绑定注释一并删除；规则被拖拽排序时注释跟随移动 |
| 其余配置区 | 写回时使用固定的分区注释模板（即 §5.2 中那些分区标题）。**用户在这些区域的自定义注释会丢失** —— 需在文档与 UI 的保存提示中明示 |
| `config_save_raw` | 逃生舱：完全手工编辑，不经过结构化层，一字不动 |

§13 增加**注释保留往返测试**：带注释的规则列表经「结构化读入 → 修改一条 → 写回」后，未受影响的注释必须逐字保留。仅断言「语义不变」覆盖不到这一点。

#### 实现约束（已实测，勿走弯路）

**不能靠 `serde-saphyr` 的 `Commented<T>` 实现上表第一行。** 它的定义是 `pub struct Commented<T>(pub T, pub String)`——**只有一个 String**，无法区分前导注释与行尾注释。实测把「两行前导 + 一行行尾」喂进去，三者被合并成单一字符串且丢失换行结构。

写回侧同样受限：`comment_position` 是 `SerializerOptions` 上的**全局**字段（枚举仅 `Inline` / `Above`），无法逐条选择。于是无论取哪个值都是有损的——`Inline` 把多条注释挤成一行行尾，`Above` 挤成一行前导并给序列项生成别扭排版。**第二轮读写才幂等，第一轮必然改写用户原文。**

**出路是官方给的**：`serde-saphyr` 的 README 明言「不捕获游离注释，请直接用 `granit-parser`」，且 `src/lib.rs:66` 有 `pub use granit_parser;`（`granit-parser` — "A YAML parser with comment and style support"）。

因此配置读写分两条路：

| 方向 | 实现 |
|---|---|
| **结构化读** | `serde-saphyr` 的 serde 反序列化。简单直接 |
| **结构化写** | **`granit-parser` 事件流层面的定点改写**：只重写被改动的那几行，其余字节**原样透传** |
| 原始读写 | 直接文件 IO，不经任何解析层 |

「定点改写而非整份重排」这一点，恰好也让上表第二行（其余配置区用固定模板）的取舍更自然——**未被触碰的区域根本不会被重新序列化**，也就谈不上丢注释。

> 同一约束波及 §12 的「YAML 语法错 → UI 显示**出错行号**」：能力存在（`serde-saphyr` 有 `Spanned<T>` 与 `location.rs`），但需显式接入，不是随手就有的东西。实现时一并处理。

---

## 6. 分流引擎

### 6.1 规则类型

| 类型 | 语义 | 目标为 IP 时 |
|---|---|---|
| `DOMAIN` | 精确匹配 | 跳过 |
| `DOMAIN-SUFFIX` | 后缀匹配 | 跳过 |
| `DOMAIN-KEYWORD` | 包含匹配 | 跳过 |
| `GEOSITE` | geosite 类别 | 跳过 |
| `IP-CIDR` / `IP-CIDR6` | 网段匹配 | 直接匹配 |
| `GEOIP` | geoip 国家 | 直接匹配 |
| `DST-PORT` | 目标端口 | 恒可判 |
| `MATCH` | 兜底，等价于 `final` | 恒命中 |

第四段参数 `no-resolve` 受支持：`IP-CIDR,192.168.0.0/16,DIRECT,no-resolve` 表示该规则遇到域名目标时不触发解析、直接跳过。**局域网段规则应默认带上它**。

### 6.2 判决流程

求值遵循 §4.2 纪律①的两阶段协议，引擎本身保持同步纯函数。

```
mode 短路（在引擎外，调用方处理）：
    global → global-outbound（空则取 MATCH 目标）
    direct → DIRECT
    rule   → 进入引擎

evaluate(target, resolved, rules, geo)：
  按配置顺序逐条试，首命中即返回
  ├─ 域名类规则（DOMAIN / -SUFFIX / -KEYWORD / GEOSITE）
  │     目标是 IP → 跳过
  │     目标是域名 → 匹配
  ├─ IP 类规则（IP-CIDR / IP-CIDR6 / GEOIP）
  │     目标是 IP        → 直接匹配
  │     目标是域名：
  │        带 no-resolve → 跳过
  │        resolved 为 None  → 返回 NeedResolve{domain}，交由调用方解析
  │        resolved 为 Some  → 用这批 IP 匹配（空切片即不匹配）
  ├─ DST-PORT → 恒可判
  └─ 全不中 → MATCH 规则的目标（加载时已校验其必然存在，见 §5.5）
```

调用方拿到 `NeedResolve` 后调用 Resolver（硬超时 2s，§7.3）；**超时或失败传 `Some(&[])` 而非放弃判决**——该 IP 规则视为不匹配，流程继续往下走，绝不因一次 DNS 故障阻断整条连接。

### 6.3 DNS 解析结果的使用边界（关键）

**解析结果只用于「判决」，绝不改写「传给出站的地址」。**

- 判决走代理 → 仍把**域名**递给出站，由服务端做**远程解析**。服务端离目标更近，且不受本地污染影响。协议 `AddrPort` 本就支持 `Domain`，这是现状，不能弄丢
- 判决走直连 → 复用刚解析出的 IP，省掉一次重复查询

一句话：**本地解析是为了决定「去哪」，不是「怎么去」。**

### 6.4 出站不可用时的处置（安全决策）

规则命中某出站，而该出站正在重连或已停止时，有三种可能做法：

| 做法 | 判定 |
|---|---|
| 回退直连 | **禁止**。用户以为在走代理、实际裸奔。这不是可用性折衷，是隐私事故 |
| 回退到 `MATCH` 兜底出站 | 不采用。没裸奔，但用户的分流意图被悄悄改写 |
| **拒绝连接 + UI 报警** | **采用**。应用层收到 RST 会自行重试，用户立刻知道哪个出站出了问题 |

唯一例外：出站处于**启动中**（预连未完成）时短暂排队等待，上限默认 5s，超时转为拒绝。**等待不是回退。**

### 6.5 性能与数据结构

| 层面 | 做法 |
|---|---|
| 规则列表 | **顺序线性扫描**。手写规则通常几十条，开销可忽略，且顺序语义天然正确。**不做任何合并索引的聪明事**——那会打乱首命中语义 |
| GEOSITE 类别 | 单类别可达数十万条域名 → 反转域名 **trie**（按 `.` 分段）。只加载规则真正引用到的类别，**懒加载** |
| GEOIP 国家 | 同样数十万条 CIDR → **前缀树**，复杂度 O(位数) 而非 O(条数) |

### 6.6 GEO 数据

采用 **v2ray 的 `geoip.dat` + `geosite.dat`**：中文圈事实标准，Loyalsoldier / MetaCubeX 等增强版每日更新，域名与 IP 两类数据一套格式覆盖。

**解析器手写，不引入 `prost`。** 两个文件的 protobuf 结构极简（枚举 + 字符串 + bytes + uint32，三层嵌套），手写 varint 解析约 150 行、零依赖、无需 build.rs。这与项目既有取舍一致——`bridge.rs:285` 的注释原话是「纯 Rust base64，避免只为上行引入依赖」。

> 判断标准的一致性：**结构极简且固定** → 手写（geo dat）；**格式复杂坑多**（DNS 报文的域名压缩指针、EDNS）→ 用成熟库（§7.3）。同一把尺子，结论相反。

更新失败时保留旧文件并提示，不阻断运行。

---

## 7. DNS 子系统

### 7.1 两层切分

| 层 | 内容 | 阶段 |
|---|---|---|
| **第一层 · Resolver** | 纯内部解析器，**不监听任何端口、不需要 root**。唯一消费者是路由引擎 | 阶段 3 |
| **第二层 · DNS 服务器 + fake-ip** | 对外拦截系统查询，返回保留段假 IP 并记录映射 | 阶段 6（随 TUN） |

之所以能这么拆：SOCKS5 与 HTTP CONNECT **本就把域名原样递过来**，代理模式下客户端无需解析即可路由转发。解析只在一处被需要——让 `GEOIP` 这类规则对域名目标生效。那是内部查询，不是对外服务。

第二层是 TUN 的刚需：TUN 只能看到 IP 包，必须靠反查 fake-ip 才能拿回域名做路由。

### 7.2 三条纪律

**① 出站服务器域名绝不走本系统的 DNS。**

`shard.rs` 的 `resolve_upstream` 必须拿到真实 IP，且**必须早于写 hosts**（代码中已有该防线）。一旦 fake-ip 生效，它会拿到 `198.18.x.x`，转发器连向虚空。

纪律：服务器域名走**独立 bootstrap 解析器**（系统 DNS，绕过一切劫持与 fake-ip），且这些域名永久列入 `fake-ip-filter`。

> 已落地（阶段 3 Task 6）：`resolve_upstream` 的签名收 `&wsieve_dns::TokioResolver`（bootstrap 那一族）而**不是** `DnsResolver`。两者类型不同，判决用的解析器根本递不进来 —— 纪律①由此从「注释里的约定」变成编译器把关的事实。

**② DoH 上游一律用 IP 字面量配置。** 否则 DoH 服务器自身的域名由谁解析？用 `https://1.1.1.1/dns-query` 这类形式，不给自己留解析需求。

**③ 死锁不成立。** 「DoH 走代理，但建代理需要解析」的循环，因纪律①而自然解开——出站服务器域名本就不依赖 DoH。其余查询默认走代理，代理未就绪时排队等待。

**④ 路由解析器必须禁用 hosts 文件读取。**（2026-08-25 实测补充）

纪律①防的是 fake-ip 污染，但**污染源不止一个**：`hosts` 文件也是。我们自己往 `/etc/hosts` 写了 `127.0.0.1 <出站域名> # wsieve-managed`（`shard.rs` 的条带劫持），而多数 DNS 库**默认会读 hosts**（`hickory-resolver` 的 `use_hosts_file` 默认为 `Auto`，即读）。

后果是判决**静默反转**：规则判决路径上解析某个出站域名 → 拿到 `127.0.0.1` → 命中 `IP-CIDR,127.0.0.0/8,DIRECT` 之类的局域网规则 → 本该走代理的流量被判直连。没有任何报错。

因此路由用的解析器必须显式关闭 hosts 读取。**注意这与纪律①是两件事**：①管的是「服务器域名用哪个解析器」，④管的是「解析器本身会不会被我们写进系统的条目骗到」。两条都要有。

> 实测对照：同一个 hosts 条目下，`use_hosts_file: Auto` 返回被劫持的 IP，`Never` 返回真实 IP。

### 7.3 依赖与细节

采用 **`hickory-resolver`**（原 trust-dns，Rust 生态事实标准），自带 DoH / DoT / UDP、缓存与 EDNS。DNS 报文有域名压缩指针等坑，**不手写**。

- **缓存**：遵循 TTL 的 LRU；解析失败必须**负缓存**（短 TTL），否则不存在的域名会被反复查询
- **超时**：判决路径上的解析硬超时 2s，超时即视为该 IP 规则不匹配继续往下，**绝不阻塞整条连接**

  ⚠️ **不能依赖 DNS 库自带的 timeout 选项**（2026-08-25 实测补充）：`hickory-resolver` 的 `ResolverOpts::timeout` **不是 wall-clock 上界**——它的连接池只在**轮次之间**检查 deadline，而 TCP/TLS connect 卡在轮内。实测设 `timeout=500ms` 打一个黑洞上游，单次 `lookup_ip` 耗时 **15.03 秒**，超出 30 倍。

  必须在外面再包一层 `tokio::time::timeout` 才能兑现「2s 硬超时」这个承诺，并为此写一条回归测试。

---

## 8. 入口层

三种入口统一产出 `(AddrPort, 双向流)`，下游对入口类型无感。

### 8.1 混合端口

用 `peek()` 而非 `read()` 窥探首字节，不消耗数据：

```
0x05        → SOCKS5（复用现有 wsieve-socks5）
ASCII 字母  → HTTP：CONNECT 走隧道；其他 method 走普通转发（请求行重写为 origin-form）
```

### 8.2 系统代理

- macOS：`networksetup`，需先 `-listallnetworkservices` 枚举再逐个设置
- Windows：`HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings`

崩溃残留与 hosts 是同一类问题——进程崩了而系统代理仍指向已停止的端口，用户整机断网。处置见 §10。

### 8.3 TUN（阶段 6）

技术选型：**`netstack-smoltcp` + `tun-rs`**。前者专做「TUN 包 ⇄ TcpStream / UdpSocket」转换，暴露 `TcpListener` 让上层像收普通连接一样收 TUN 流量，覆盖 Linux / macOS / Windows / Android / iOS；`tun-rs` 在 Linux 上靠 GSO/GRO 批量收包，实测比 `rust-tun` 快约 4×。`ipstack` 更轻但官方自述仍不稳定，不采用。

netstack 把流量转成 `TcpStream` 后，**路由层与出站层完全复用**，TUN 本质只是又一个入口。

#### 8.3.1 环路陷阱（必须处理）

TUN 捕获全部流量，**包括转发器连向真实服务器 IP 的那条**：

```
WebView → 127.0.0.1:18443（环回，不过 TUN ✓）
  → 转发器 → 真实服务器 IP:443
      → 被 TUN 捕获 → 路由层判「走代理」
          → 出站 → WebView → 127.0.0.1:18443
              ↺ 死循环
```

解法是标配的「服务器地址绕行」：真实服务器 IP 加入 TUN bypass 路由、直连物理网卡。websieve 的特殊之处在于该 IP 是**运行时解析**得到的（`shard.rs:90`），因此 bypass 列表必须动态维护，并随出站增删而增删。

#### 8.3.2 启动顺序（钉死）

```
解析真实 IP → 写 bypass 路由 → 起转发器 → 写 hosts → 最后拉起 TUN
```

任何一步顺序颠倒都会**静默失败**：TUN 的 DNS 劫持若先生效，`resolve_upstream` 拿到的就是 fake-ip。

---

## 9. 出站管理

### 9.1 单 WebView 承载多出站

默认 `carrier: shared`：一个 WebView 加载**宿主出站**的页面，其余出站使用绝对 URL（`WebViewTransport::with_base()`，既有能力）。

**成立前提**是 §3.2——sid 走 query 而非 cookie，因此跨域名请求不受 ITP 第三方 cookie 拦截影响。

> **已实测确证**（2026-08-25，`scripts/spike-cross-origin.sh`，结论见
> `docs/superpowers/spikes/2026-08-25-cross-origin-carrier-spike.md`）：
> 单个真实 WKWebView 加载自域名 A 的页面，与 A、B 两个**不同 eTLD+1**
> （真正的 cross-site）的服务端各完成一次完整 Noise 握手，两个出站的数据面
> 均跑通、逐字节一致。**`carrier: shared` 成立。**
> 含反证对照：把 CORS 改回同域名限制则跨域名握手立即被 WebKit 拦掉
> （`TypeError: Load failed`），确证该 spike 确实在测跨域名能力，
> 且 CORS 放宽是其必要条件。

**服务端需要的唯一改动**：现有 CORS 逻辑限制「Origin 必须与 Host 同域名」（`server/lib.rs:270`），需放宽为接受任意 Origin。**已完成**（提交 `dbb300d`）。

**安全性基本不变**：真正的防线是「**只有认证成功的响应才带 CORS 头**」，该条保持不动。探测者发不出合法 msg1，永远看不到任何 CORS 痕迹。

**宿主选择**：默认取第一个启用的出站，可由 `carrier-host` 指定。宿主出站下线**不影响**其他出站——页面已加载完毕，JS 继续运行。

**降级路径**：`carrier: isolated` 为每个出站建独立隐藏 WebView，换来故障隔离。若将来 sid 改回 cookie，此模式将成为唯一可行方案。

> **已实测**（2026-08-25）：isolated 的内存代价**不是**「随出站数线性增长的 N×」。
> 同一进程内的多个 WKWebView 共用 WebContent 进程池，实测为**首个约 128 MB，
> 其后每个约 27 MB**（1/2/4 个 WebView 分别为 +128 MB / +155 MB / +211 MB）。
> isolated 比 shared 贵，但远没有 N× 那么贵——将来若因故障隔离等原因需要退回
> isolated，不应被旧的「N×」说法吓阻。shared 形态下增开出站**不增开 WebView**，
> 内存不随出站数增长。

### 9.2 hosts 与端口

- **hosts**：多出站是多个不同域名，写入 hosts 的不同行，互不冲突。`set_managed()` 签名已收域名数组，直接用。管理员权限仍只需一次
- **端口**：出站 i 从 `shard-base-port + Σ(前序出站会话数)` 起占 `extra-sessions + 1` 个

### 9.3 连接生命周期

```
出站 enabled ──▶ 立即预连（不等首个请求）
   ├─ 宿主 WebView 就绪（首个出站负责加载页面）
   ├─ 解析真实 IP → 写 hosts → 起转发器（预建 TCP）
   ├─ Noise 握手 → mux 就绪
   └─ 状态推 UI

会话死亡 ──▶ 必须区分两种情况
   ├─ 心跳停摆 / WebView 崩溃 ──▶ reload 页面 ──▶ 全部出站重建
   └─ 仅该出站会话死         ──▶ 复用现有页面，只重建这一个
```

### 9.4 恢复路径优化

恢复一个出站需要五步，成本极不均衡：

| 步骤 | 成本 |
|---|---|
| 建 WKWebView 实例 | ~100ms |
| **导航到伪装页** | **1–3 RTT + 页面字节** ← 大头 |
| emitter 注入 + 首个心跳 | ~ms |
| Noise 握手 | 1 RTT |
| `GET /api/events` 挂载 | 1 RTT |

前两步跳不过去——emitter 只能活在页面上下文中，页面必须真加载。**优化 Noise 握手只能省掉五分之一。**

采用的优化：

**① 会话死亡不无条件 reload 页面。** `proxy.rs:221` 当前无条件执行 `window.location.reload()`。但服务端 GC 会话、下行流断、握手失败等情况下 emitter 仍然健在（有 `if (window.__wsieve) return` 防重注入，且 post / openStream 均无状态）。白白 reload 一次即白白重付整套页面加载——恢复路径上最贵的一段。**仅在心跳停摆或 WebView 崩溃时 reload。**

**② 转发器预建 TCP。** `shard.rs:78` 当前为懒连接。改为预先备好少量到上游的 TCP，入站到达即取用，用掉即补。省下 TCP 握手 1 RTT（跨国链路可达 200ms+）。

**这不违反 `shard.rs` 的核心不变量**——该模块禁止的是「多条入站汇聚到一条出站」的**复用**，预建仍严格保持一进一出。

注意事项：闲置 TCP 可能被中间设备 RST 或被服务端超时回收，需设有效期并在取用时做健康检查；预备数量应少（1–2 条），过多反而像端口扫描。

**③ TLS 票据跨 WebView 复用（已免费生效）。** wry 默认使用 `WKWebsiteDataStore::defaultDataStore`（`wkwebview/mod.rs:244`），所有 WebView 共享票据与缓存，销毁重建也能命中 TLS 1.3 恢复。无需改动。

**④ 并行拉起多个出站**，而非串行。

**⑤ 启用即预连。** 出站在 UI 中被标为启用的瞬间开始建连，不等第一个请求。

**⑥ 服务端开启 TLS early data**（传输 spec §6.8 已规划，`max_early_data_size > 0`）。机会性收益，浏览器自行决定是否使用。

### 9.5 不做的优化及理由

| 优化 | 理由 |
|---|---|
| **Noise 会话恢复（0-RTT 复活）** | 传输 spec §9.4 明文规定「下行 GET 断开 → 立即 GC，无会话恢复」。推翻它需付三笔账：sid 会从「路由标签」退化为「凭证」（spec 明确声明 sid 不是秘密，若可凭 sid 复活会话，任何见过它的中间环节都能劫持）；断开瞬间在途 TU 丢失将使 Noise nonce 永久错位；多一条恢复路径即多一个可探测面。换回的只有 1 RTT |
| **GET 与 POST msg1 并发 attach** | 需服务端把未认证的 GET 挂起等待，等于给探测者一个可测量的时延指纹。换回 1 RTT |

**根本理由**：空闲但活着的出站，运行开销仅为一条长 GET 挂着 + 每 60s 一个 PADDING 心跳（CPU ≈ 0，带宽 ≈ 0，服务端上行空闲 GC 阈值 180s，余量 3 倍）。**保持连接活着，"恢复"这件事就不存在**——直接开一条 mux 子流即可，连 0 RTT 都不需要，是 0 步骤。与其优化恢复，不如不断开。

---

## 10. 外部状态托管

hosts、系统代理、TUN 路由三者共性明确：**修改了系统全局状态，进程崩溃后必须能恢复**。项目已在 `hosts.rs` 解决过一次（启动时 `clear_managed` 兜底 + `Drop` 中显式恢复）。

抽象为统一接口，三处共用一套纪律与一套测试：

```rust
trait ManagedSystemState {
    fn apply(&self) -> Result<()>;
    fn revert(&self) -> Result<()>;
    fn clear_stale() -> Result<()>;   // 启动时兜底，处理上次崩溃残留
}
```

要求：`apply` / `revert` 幂等；`clear_stale` 在任何 `apply` 之前调用；托管条目需可识别（如 hosts 现用的 `# wsieve-managed` 标记）。

---

## 11. 控制界面

### 11.1 IPC 契约与安全隔离（最高优先级）

**`main` 窗口加载的是远端服务器的页面。** 若该服务器被攻破，页面 JS 即可调用它被授权的每一个 Tauri 命令——而配置中存有 `client-priv` 私钥。

现有 capability 授权 `main` 且开放 `remote.urls` 至任意 https。因此必须拆为两个 capability，**且绝不重叠**：

```jsonc
// capabilities/transport.json —— 传输窗口，远端 origin
{ "windows": ["main"],
  "permissions": ["allow-wsieve-heartbeat",
                  "allow-wsieve-raw-post",
                  "allow-wsieve-raw-stream"],
  "remote": { "urls": ["http://127.0.0.1:*", "https://**:*"] } }

// capabilities/control.json —— 控制窗口，本地 origin
{ "windows": ["control"],
  "permissions": ["allow-config-read", "allow-config-write",
                  "allow-connect", "allow-rule-test", …],
  "local": true }                    // 无 remote 字段
```

**纪律**：控制类命令永远不出现在 transport capability 中。此纪律**必须写成测试**——断言两个 capability 的 permission 集合交集为空（见 §13）。

> 迁移注意：`transport.json` 相比现状收紧了 `remote.urls`，去掉了 `http://**:*`，只保留 `http://127.0.0.1:*` 与 `https://**:*`。现有 `scripts/e2e.sh` 用 `http://127.0.0.1:$SRV_PORT/` 仍在覆盖范围内（IP 主机不触发条带劫持），但**以 http 域名部署的开发场景会被这条打到**。阶段 4 的迁移说明需点明此事。

### 11.2 命令与事件

命令（UI → Rust）：

```
config_get / config_save             结构化 CRUD（表单用）· client-priv 脱敏为 "***"
config_get_raw / config_save_raw     原始 YAML 文本（高级用户直接编辑）· 含明文私钥
connect / disconnect
set_mode(rule | global | direct)
outbound_enable(id, bool)
outbound_latency_probe(id)           语义见下
rule_test(target, resolve: bool) -> { index, decision, tried, resolved }
                                     ← 探针，复用 §4.2 的 evaluate()
geo_update / geo_status
traffic_snapshot(window)
```

`rule_test` 的 `resolve` 参数直接对应两阶段求值：`false` 只跑第一轮（快、不发 DNS），`true` 遇到 `NeedResolve` 时解析后跑第二轮（准）。UI 默认 `false`，命中的若是 IP 类规则则提示「需解析才能确定」并提供一键重测。

**延迟指标的定义**（websieve 出站没有独立的 ping 面，必须明确测什么）：

| 场景 | 取值 |
|---|---|
| 稳态（有流量） | 最近 N 次上行 `POST /api/sync` 的响应耗时**中位数**。该路径本就有 204 响应，天然可测，**零额外流量** |
| 刚建立、尚无流量 | 握手 RTT（`msg1` 发出到 `msg2` 收到），会话建立时记录 |
| 手动点「测试」 | 发一个 PADDING TU 的 POST 并计时，即触发一次上述稳态测量 |

不另开探测子流、不引入服务端探测端点——复用既有流量路径，既省实现也少一个可探测面。

事件（Rust → UI）——**必须在 Rust 侧聚合节流**，实时连接每秒可达数百条，逐条推送会直接卡死 WebView：

| 事件 | 节流 |
|---|---|
| `traffic` | 1s 一次采样 |
| `connection` | 200ms 批量一批 |
| `rule-hit` | 1s 推一次**增量**计数 |
| `status` / `outbound-state` | 变化时推，天然低频 |

**私钥在命令面上的可见性**（2026-08-25 补充）：`config_get`（表单用）把 `client-priv` **脱敏为 `"***"`**，只有 `config_get_raw`（用户主动进入原始编辑）才返回明文。理由是表单视图会被频繁调用、其响应会经过事件流与前端状态，而私钥在那里没有任何用途；原始编辑则是用户显式要求看全文，且 §5.4 已要求在该入口给出导出警告。

> 这是一处**实现期补充的解释**，spec 原本未指定。另一种做法（两个命令都返回明文，仅靠 UI 不显示）也站得住脚，但把安全性押在前端渲染上不如押在后端不发送上。选定的做法必须写进代码注释，不要留成隐含约定。

**命中计数持久化**：内存 `AtomicU64` 计数，退出时写 `stats.json`、启动时恢复。理由是 §11.5 的「命中热度」需要足够长的观察窗口才有意义——重启清零就看不出哪条规则是死的。用独立文件，不污染 `config.yaml`。

计数**以规则文本为键，不用索引**：删掉一条规则会让其后所有索引位移，恢复出来的热度数据会整体错位到相邻规则上——而热度恰恰是用来判断「哪条规则是死的」，错位等于给出反向结论。

### 11.3 信息架构

```
┌─ 状态条（常驻）─ 状态点 · 出站数 · 活跃连接 · sparkline · 速率 ─┐
├────────────────────────────────────────────────────────────┤
│  [ 流量 ]   规则   出站                              ⚙      │
├────────────────────────────────────────────────────────────┤
│   当前视图                                                  │
└────────────────────────────────────────────────────────────┘
```

- **流量**为默认视图（打开即见走向），内部有「图 ⇄ 表」切换、时间窗口（5m / 1h / 本次运行）、字节/连接数切换
- **规则**是排查主场，探针置于其顶部
- **设置**走覆盖层而非第四个标签——不常用，不该占据同级位置
- 用 **segmented control 而非左侧图标导航栏**（后者是同类产品的通用套路，且挤占宽度）
- **托盘常驻**：平时后台运行，托盘菜单可直接切模式 / 切出站，点图标显隐窗口
- 窗口默认 960×640、最小 720×480（规则表需要宽度）
- **空状态**：无任何出站时，让位给「添加第一个服务器」引导

### 11.4 视觉基线

技术栈：**Svelte + Vite**，纯手写 CSS，无组件库。

意图三问（设计的出发点，非装饰性描述）：

| | |
|---|---|
| **谁在用** | 自建服务器的技术用户，熟悉 Clash / sing-box 心智模型，看得懂 CIDR 与 GEOSITE。界面平时后台常驻，**只在网络出问题时被打开** |
| **要完成什么** | 排查「为什么这个域名没走代理」、切节点、加规则、看规则命中、盯掉线重连 |
| **该是什么感觉** | **密集像交易台，克制像 Proxyman**。参照 Proxyman / Little Snitch / Charles，**不是**漂亮的 SaaS 落地页 |

基线：

| 维度 | 决定 |
|---|---|
| 深度策略 | **borders-only**，只用这一种，不混用阴影（浮层除外） |
| 表面 | 单一色相只移明度：`#16181b` → `#1c1f23` → `#23272c`，每级 4–7% |
| 边框 | `rgba(255,255,255,.07)`，强调级 `.13` |
| 文本四级 | `#e6e8ea` / `#a4abb3` / `#6f777f` / `#4d545b` |
| 状态色 | live `#3fb27f` · warn `#d99a3f` · fail `#d1595c` · direct `#7d8894` |
| 出站色码 | `#5b8ff9` · `#61ddaa` · `#f6bd16` · 等，**界面中唯一允许的彩色** |
| 字体 | **IBM Plex Sans + IBM Plex Mono**。不用 Inter（默认选择）；Plex 有 IBM 技术文档的工程血统，同族搭配气质统一，开源 |
| 字阶 | 1.25 / 14px base：`11 · 12 · 13 · 14 · 16 · 18 · 22`。层级主要靠**字重 + 颜色**，不靠字号 |
| 密度 | 规则行 32px、出站行 38px、padding 12–16px |
| 间距 | 基数 4px |
| 颜色纪律 | 约 90% 屏幕为中性结构色；彩色只出现在出站色码与状态色上 |

**明确拒绝的三个套路**：

| 套路 | 替代 |
|---|---|
| 左侧图标导航 + 主内容区 | 视图自身即主体，segmented 切换 |
| 服务器卡片网格 + 圆形延迟指示 | 紧凑行列表，延迟与会话数用 `tabular-nums` 对齐成列 |
| 规则类型做成彩色 pill 标签 | 类型用**等宽小写缩写 + 统一低对比灰**，靠列位置识别；**颜色全部让给出站** |

第三条最关键：彩色 pill 会把规则列表变成彩虹糖，恰好摧毁它唯一需要的能力——**扫读**。

同时禁止：卡片左侧粗色装饰边框、大面积渐变、无意义留白撑场面。

### 11.5 两个 signature

**① 命中热度染色（规则视图）。** 命中次数不靠读数字，靠**行背景的中性染色**（`rgba(255,255,255,.014 ~ .052)`，按命中数归一化）。最热的规则微亮，死规则几乎透明。滚过 200 条规则，哪几条在真正干活一眼可辨。

这解决的是分流工具独有的痛点——「我这堆规则里哪些是死的」。用中性色而非出站色染色，避免与出站色码语义冲突。

**② 探针即搜索框（规则视图）。** 顶部输入框不是过滤器，而是**试算探针**。输入域名 → 命中行升亮、其余整体降噪 → 显示「前 N 条已试未命中」。把核心排查动作压缩为一次输入。

后端复用路由层纯函数（§4.2 纪律①），保证试算结果与真实判决永远一致。

### 11.6 流量走向视图（桑基图）

三层桑基：**目标站点（Top 12）→ 命中规则（Top 8）→ 出站**，流带宽度即字节量。

| 议题 | 决定 |
|---|---|
| **着色** | 流带按**最终去向**着色，非按来源。整条路径共享一个 `userSpaceOnUse` 全局横向渐变，左端 opacity ≈0.08 → 右端 ≈0.52。视觉语义即「未分类的流量被逐层筛清、各归其类」。左层与中层节点保持中性灰，**只有出站节点满色** |
| **聚合** | 超出 Top N 的压成「其他 N 个」，用最暗的中性色。本场景约 24 条流，远低于 50 的阈值，**SVG 足够**，无需 Canvas |
| **标签** | 全部标签用 `paint-order: stroke` + 3.5px 画布色描边，压在任何流带上均可读。节点低于阈值时标签隐藏，hover 才出 |
| **交互** | hover 时**把噪音调暗**而非把目标点亮：目标路径保持原样，其余降至 16% 不透明度。点击锁定。**点击出站节点跳转到规则视图并筛出该出站的规则** |
| **实时更新** | 数据 1s 更新，但**绝不每秒重算布局**——图会不停跳动无法阅读。**节点顺序一旦确定即冻结**（除非出现新节点），仅对流带宽度做 200ms ease-out 插值。不做流带流动动画。`prefers-reduced-motion` 下直接跳变 |
| **集成** | 只引入 `d3-sankey` + `d3-shape`（约 10KB）。**d3 只做布局计算，不碰 DOM**；Svelte 用 `{#each}` 渲染 `<path>` / `<rect>`。不使用 d3 的 enter/exit/update——会与 Svelte 响应式冲突 |
| **空状态** | 无流量时**不画空的坐标骨架**，显示引导文案。流数少于 3 时桑基图本不适用，自动降级为流量列表 |

**表视图是必需的等价视图，不是可选装饰。** 桑基图的无障碍评级为 C——结构性流图无法只靠颜色传达，必须提供流量表格（源 → 规则 → 出站 → 字节 → 连接），可排序、可键盘遍历、屏幕阅读器友好。

因此「默认不显示日志」的准确落地是：**日志从独立标签降级为流量视图内部的表形态**，图/表一键切换，同一份数据。

---

## 12. 错误处理

| 故障 | 处置 |
|---|---|
| 规则命中的出站不可用 | 拒绝连接 + UI 报警，**绝不静默回退**（§6.4） |
| 出站正在启动中 | 排队等待，上限 5s，超时拒绝 |
| YAML 语法错 / 配置损坏 | 不启动代理，UI 显示**出错行号**，保留上次可用配置 |
| 规则引用了不存在的出站 | **加载期报错并指出行号，不加载该配置**，保留上次可用配置 |
| 配置中出现不支持的 proxy type | 明确报错并指出类型名，**不静默忽略** |
| 规则引用了不存在的 GEO 类别 | 加载成功，该规则视为不匹配跳过 + **告警并指出行号**，UI 标红该行 |
| GEO 文件缺失或损坏 | 涉 GEO 的规则跳过 + 警告（按库汇总，给出受影响条数），不阻断启动 |
| GEO 更新失败 | 保留旧文件，提示，不影响运行 |
| DNS 解析超时 | 视为该 IP 规则不匹配，继续往下 |
| hosts 不可写 | 降级单会话（现有行为）+ UI 明示 |
| 系统代理设置失败 | 提示用户手工设置，不阻断代理运行 |
| 混合端口被占用 | 启动失败并**指明冲突端口**（沿用 `shard.rs:214` 的「报错不静默跳过」纪律） |
| WebView 崩溃 | reload → 全部出站重建 |
| 控制窗口关闭 | 代理继续运行，托盘常驻 |

### 12.1 为什么「未知出站」阻断加载，而「未知 GEO 类别」只告警

两者看着对称，处置却相反，这是刻意的：

**未知出站 = 这条规则根本无法被兑现。** 用户写下 `DOMAIN-SUFFIX,x.com,日本节点`，
表达的是「这个域名必须走日本」。若该出站不存在而我们把规则跳过，这条规则就会
悄悄变成「走后面某条规则决定的任意去向」—— 通常是 `MATCH` 的兜底。用户的意图
被改写了，且毫无提示。这与 §6.4 拒绝在出站不可用时回退到另一个出站是同一条纪律：
**宁可不通，也不能在用户不知情的前提下把流量送去它不该去的地方。** 因此加载期
直接报错并指出行号，保留上次可用配置。

**未知 GEO 类别 = 这条规则降级为「不匹配」，且外部数据本就允许缺失。**
`GEOSITE,cnnn,DIRECT` 里的 `cnnn` 不存在，规则的语义退化成「什么都不命中」，
流量继续按后续规则判决 —— 用户的其余意图完好无损。更关键的是，GEO 是从网上
下载的外部数据：文件可能还没下载、正在更新、或版本较旧而暂缺某个新类别。
让一份**外部数据的时序状态**决定代理能不能启动，等于把「今天能不能上网」
交给一次下载的成败。所以这里是加载成功 + 告警，并把行号交给 UI 标红。

一句话概括这条分界：**配置里写错的东西阻断加载，外部数据缺失的东西只告警。**
前者改不改全在用户手上，后者不在。

> 实现见 `RuleSet::build`（返回 `BuildError::UnknownOutbound`）与
> `RuleSet::check_geo`（返回 `Vec<GeoWarning>`）。GEO 告警刻意与 build 分离：
> `build` 不带 GEO 文件也能用，且 `GeoDb` 惰性加载，不含 GEO 规则的配置
> 一个字节都不会读。告警自身再分两类 —— 类别不存在是**配置笔误**，
> 文件读不了是**环境问题**，修复方向不同，绝不混报。

---

## 13. 测试策略

| 层 | 内容 |
|---|---|
| **路由引擎** | 纯函数穷举：每种规则类型的中/不中、域名目标遇 IP 规则的 `NeedResolve` 抛出、`no-resolve` 跳过、第二轮传空切片时的降级、首命中顺序语义、`MATCH` 兜底、引用不存在出站时**加载即报错**（§12.1）、引用不存在 GEO 类别时**加载成功但告警且带行号**（§12.1） |
| **两阶段求值幂等性** | 同一输入两轮求值结果一致；第二轮**永不**再返回 `NeedResolve`；缺 `MATCH` 时加载即报错 |
| **Clash 规则解析** | 取公开规则集真实片段解析，不得报错 |
| **GEO dat 解析** | 真实 `geoip.dat` / `geosite.dat` 小样本，结果与已知值逐条比对 |
| **混合端口嗅探** | 给定字节流正确判定 SOCKS5 / HTTP |
| **capability 隔离** 🔒 | 断言两个 capability 的 permission 集合**交集为空**。这是安全测试而非形式检查 |
| **配置往返** | YAML → 结构 → YAML 语义不变 |
| **规则注释保留** | 带注释的规则列表经「读入 → 改一条 → 写回」后，未受影响的注释**逐字保留**；删除规则时其绑定注释一并消失；排序时注释跟随（§5.6） |
| **外部状态托管** | `apply` / `revert` / `clear_stale` 幂等性 |
| **端口分段** | 多出站的端口分配无重叠、无越界 |
| **E2E** | 扩展 `scripts/e2e.sh`：起服务端 + 客户端，验证某域名走代理、某域名直连 |
| **TUN 环路回归** | 专项用例：确认转发器出网连接未被 TUN 捕获 |

---

## 14. 实现阶段

每阶段可独立验证，不存在「写完全部才能跑」的阶段。

| # | 阶段 | 内容 | 验证方式 |
|---|---|---|---|
| 1 | **配置与路由引擎** | YAML **读**（serde-saphyr）/ **写**（granit 事件流定点改写，见 §5.6）+ 语法错行号（`Spanned<T>`）· Clash 规则解析 · 路由引擎两阶段纯函数 · geo dat 解析 + trie | CLI 子命令 `websieve route <target>` 打印判决 |
| 2 | **入口层与出站管理** | **先做 spike**（见下）· 混合端口 · `proxy.rs` 参数化为多出站管理器 · 单 WebView 承载 · §9.4 优化①②④⑤ · 外部状态托管 | 扩展 `scripts/e2e.sh` |
| 3 | **DNS Resolver** | `hickory-resolver` 接入 · 缓存与负缓存 · 2s 超时 · bootstrap 隔离 | 解析注入 + 规则命中单测 |
| 4 | **控制窗口与 IPC** | Svelte + Vite 脚手架 · **双 capability 隔离** · 命令面 · 事件节流 · 托盘 | 双窗口共存冒烟 + capability 交集断言 |
| 5 | **五个视图** | 流量走向（桑基 + 表）· 规则视图与探针 · 出站列表 · 设置 · 空状态 | design-review + a11y 审计 |
| 6 | **TUN 与 fake-ip** | `netstack-smoltcp` + `tun-rs` · DNS 服务器与 fake-ip · bypass 路由 · 启动顺序纪律 | 环路回归专项 |

**阶段 2 的第一件事必须是 spike：跨域名 fetch 在 WKWebView 下的真实行为。**

`carrier: shared` 是默认值，而它成立与否全押在 §3.2（sid 走 query，故不受 ITP 第三方 cookie 拦截影响）这一条推论上。推论本身经代码核实无误，但**尚未有一次真实的跨域名会话建立来确证**。做法：放宽服务端 CORS 后，用两台不同域名的服务端跑通一次握手 + 一次数据往返即可。

失败的代价可控——§4.2 纪律③已让承载方式对出站层透明，退回 `isolated` 只是换个承载器实现。但**必须在出站管理器动工之前知道答案**，否则整个阶段 2 的结构建立在未验证的前提上。

服务端配套改动（§9.1 的 CORS 放宽）需在此 spike 之前完成。

---

## 15. 已知限制与刻意未做

- **IP 类规则对域名目标依赖 DNS 解析**，带来解析延迟与一次本地查询。`no-resolve` 提供逃生舱，`GEOSITE` + `GEOIP` 配合使用可在多数场景避开
- **`carrier: shared` 存在单点故障**：WebView 崩溃时全部出站同时断开。`isolated` 模式作为可配置的降级路径存在
- **私钥明文存储**，仅靠 0600 保护。取舍以可迁移性优先，已在 §5.4 说明
- **无策略组**：故障转移、按延迟择优等能力缺席。配置格式已预留，语法不需变更
- **无订阅**：websieve 出站需成对密钥，不存在公开订阅生态
- **TUN 需提权**：macOS 创建 utun 需 root，Windows 需 wintun，Linux 需 `CAP_NET_ADMIN`。与既有的 hosts 写入同属一类，但更重

### 待实测项

沿用传输 spec §9.3 的做法——**不为未实测的问题预先设计规避方案**：

1. ~~**单个隐藏 WKWebView 的常驻内存**~~ —— **已实测**（2026-08-25）：首个约 128 MB，其后每个约 27 MB（多 WebView 共用 WebContent 进程池，非线性）。见 §9.1 与 `docs/superpowers/spikes/2026-08-25-cross-origin-carrier-spike.md`
2. **预建 TCP 的安全闲置时长**。中间设备与服务端的超时回收行为需实测，决定有效期与预备数量
3. ~~**跨域名 fetch 在 WKWebView 下的实际行为**~~ —— **已实测**（2026-08-25）：单个真实 WKWebView 与两个 *cross-site* 域名（不同 eTLD+1）各完成完整 Noise 握手并跑通数据面，`carrier: shared` **成立**。含反证对照。复现：`scripts/spike-cross-origin.sh`

---

## 16. 参考

### 本仓库代码

- `src-tauri/src/proxy.rs` — 会话主循环，阶段 2 的重构对象
- `src-tauri/src/shard.rs` — 本地多端口转发器，「一进一出绝不池化」不变量
- `src-tauri/src/hosts.rs` — 外部状态托管的既有范本
- `src-tauri/src/bridge.rs` — `WebViewTransport`，`with_base()` 是多出站的基础
- `crates/wsieve-proto/src/addr.rs` — `AddrPort`，分流判决的输入
- `crates/wsieve-server/src/lib.rs:270` — CORS 同域名限制，§9.1 需放宽

### 外部

- [Full Third-Party Cookie Blocking and More — WebKit](https://webkit.org/blog/10218/full-third-party-cookie-blocking-and-more/)
- [netstack-smoltcp](https://crates.io/crates/netstack-smoltcp) · [ipstack](https://crates.io/crates/ipstack)
- [serde-saphyr](https://crates.io/crates/serde-saphyr) · [granit-parser](https://crates.io/crates/granit-parser)（后者由前者 `src/lib.rs:66` re-export，是注释保留的落点，见 §5.6）
- RUSTSEC-2025-0068 / GHSA-hhw4-xg65-fp2x — `serde_yml` is unsound and unmaintained（查询 ID 即可，勿引 RUSTSEC-2024-0320，那是 `yaml-rust` 的另一条公告）
- Tauri 2 多 webview：`crates/tauri/src/window/mod.rs:1142`（`feature = "unstable"`）
- wry 默认 data store：`src/wkwebview/mod.rs:244`
