# 阶段 3：DNS Resolver — 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 建立 `wsieve-dns` crate —— 接入 `hickory-resolver` 的内部解析器（不监听端口、不需要 root），把阶段 1 留下的「两阶段求值第二轮由谁供料」这道缝补上，并把设计文档 §7.2 的三条纪律固化成**编译期或加载期就会失败**的形状，而不是运行时才炸的注释。

**Architecture:** 三层职责分开。`upstream.rs` 只管「配置字符串 → hickory 上游配置」，纪律②（IP 字面量）在这里变成加载期硬错误；`resolver.rs` 持有解析器实例并施加硬超时，纪律③（失败即不匹配）在这里变成**签名里没有 Result**；`decide.rs` 是唯一允许调用解析器的判决入口，把 `Verdict::NeedResolve` 接住、解析、再来一轮。`bootstrap()` 是一个**完全独立的解析器实例**，纪律①（出站域名绕开自家 DNS）靠「压根是两个对象」保证，而不是靠某个 if 分支。

**Tech Stack:** Rust 2021 · `hickory-resolver` 0.26.1（features: `tokio` `system-config` `https-ring` `webpki-roots`，`default-features = false`）· 缓存与负缓存**用 hickory 内置的 `ResponseCache`，不自建** · 硬超时用 `tokio::time::timeout` 外包一层

**依据:** `docs/superpowers/specs/2026-08-24-client-routing-and-ui-design.md` §4.2 §6.2 §6.3 §7 §12 §14

---

## 本计划的验证状态（先读这个）

阶段 1 计划的评审者抓出 4 段编译不过的代码。为不重蹈覆辙，**本计划的每一段 Rust 代码都在一个临时 workspace 里真编译过、测试真跑过**：

| 项 | 状态 |
|---|---|
| `hickory-resolver` 版本 | **0.26.1**（crates.io 当前版本，实测拉取） |
| 全部代码块编译 | ✅ `cargo check` 通过 |
| 全部测试运行 | ✅ **34 个测试全绿**（upstream 12 · resolver 9 · routing 13） |
| clippy | ✅ `-D warnings` 无警告 |
| 与阶段 1 的接口对接 | ✅ 用阶段 1 计划里**原文摘出的** `rule.rs` / `engine.rs` 编译验证，非凭空假设签名 |

实测中发现了 **3 个与直觉相反的行为**，每一个都会导致静默故障，已分别写进对应 Task 的注释与测试里。见下一节。

---

## 三个实测发现（每个都会造成静默故障）

### ① `ResolverOpts::timeout` **不是**墙钟上限 —— 实测超出 30 倍

这是本阶段最重要的一条。直觉上 `opts.timeout = 500ms` 就该在 500ms 返回，实测**不是**：

```
上游 = https://192.0.2.1/dns-query（RFC 5737 文档段，保证黑洞）
opts.timeout = 500ms, opts.attempts = 2
单次 lookup_ip 实际耗时：15.03 秒          ← 30 倍
外层加 tokio::time::timeout(2s) 之后：2.00 秒   ← 稳定
```

原因在 `name_server_pool.rs:290`：deadline 只在名字服务器池的**轮与轮之间**检查，而 TCP/TLS 建连本身卡在系统 connect 超时里，**一轮都没走完**，deadline 就没有被检查的机会。

后果直接命中设计文档 §7.3 的红线：判决路径上一次 DNS 故障会把连接晾 15 秒。**必须外包一层 `tokio::time::timeout`**，Task 3 有专门的回归测试锁住这条。

### ② 默认会读系统 `/etc/hosts`，而我们自己往里写了劫持行

`ResolverOpts::use_hosts_file` 默认是 `Auto`，**会读系统 hosts 文件**。而 `shard.rs` / `hosts.rs` 正是往 hosts 里写 `127.0.0.1 <出站域名> # wsieve-managed`。

实测（用本机 hosts 里一条真实的劫持行 `81.69.97.154 api.deepseek.com`）：

```
use_hosts_file = Always : [81.69.97.154]   ← hosts 里的劫持值
use_hosts_file = Auto   : [81.69.97.154]   ← 默认值，同样被劫持
use_hosts_file = Never  : [3.173.21.63]    ← 真实 IP
```

若不显式设成 `Never`，路由判决会认为出站域名解析到 `127.0.0.1`，从而命中「内网直连」类规则 —— 判决与事实相反，且完全静默。两个解析器（判决用与 bootstrap 用）**都必须设 `Never`**。

### ③ `name_server::TokioConnectionProvider` 在 0.26 是**私有模块**

hickory 自己的文档注释（`src/lib.rs` 的 Usage 段、`tls.rs` 的测试）仍在用 `hickory_resolver::name_server::TokioConnectionProvider`，但 `lib.rs:190` 写的是 `mod name_server;`（私有），外部 crate 引用会直接报 `private module`。

正确的公开路径是 **`hickory_resolver::net::runtime::TokioRuntimeProvider`**。照抄官方文档会编译失败 —— 这正是「必须真编译」的价值所在。

---

## 前置阅读（实现者必读）

- **§7.1** — 两层切分。本阶段**只做第一层**（内部 Resolver，不监听端口、不需要 root）。fake-ip 与 DNS 服务器是阶段 6 的事，**不要在这里做**
- **§7.2 三条纪律** — 本阶段的全部意义所在，逐条落点见下表
- **§7.3** — 缓存 / 负缓存 / 2s 硬超时
- **§4.2 纪律①** — 两阶段求值协议。阶段 1 已实现 `evaluate`，本阶段实现**调用方**
- **§6.2 / §6.3** — 判决流程；以及「解析结果只用于判决，绝不改写传给出站的地址」
- **§12** — DNS 解析超时 → 视为该 IP 规则不匹配，继续往下

**三条纪律在代码里的落点（这是本阶段的骨架）：**

| 纪律（§7.2） | 落点 | 靠什么保证 |
|---|---|---|
| ① 出站服务器域名绝不走本系统 DNS | `resolver::bootstrap()` + Task 6 改写 `shard.rs` | **两个独立对象**，无共享缓存/上游/状态。不是 if 分支 |
| ② DoH 上游一律 IP 字面量 | `upstream::parse_nameserver` | **加载期硬错误**，坏配置建不出解析器 |
| ③ 超时/失败 = 不匹配，继续往下 | `DnsResolver::lookup_for_routing` | **签名里没有 `Result`**，调用方无从写出「失败就阻断」 |

**项目既有惯例（请遵守）：**

- 代码注释用**中文**，与仓库现有代码一致
- 刻意的简化用 `ponytail:` 注释标注并写明上限与升级路径
- 错误绝不静默跳过。参照 `src-tauri/src/shard.rs:214` 的 `port_conflict_is_reported_not_skipped`

---

## 文件结构

```
crates/wsieve-dns/                 新建
  Cargo.toml
  src/lib.rs                       公开门面
  src/upstream.rs                  配置字符串 → NameServerConfig（纪律②）
  src/resolver.rs                  DnsResolver + bootstrap（纪律①③）
  src/inject.rs                    RoutingResolver trait + StubResolver
  src/decide.rs                    两阶段求值驱动方
  tests/resolver.rs                构造校验 · 硬超时 · bootstrap 隔离
  tests/routing.rs                 解析注入 + 规则命中（§14 的验证方式）

Cargo.toml                         修改：workspace members 增加一项
src-tauri/src/shard.rs             修改：resolve_upstream 改走 bootstrap（Task 6）
src-tauri/src/shard_setup.rs       修改：传入 bootstrap 解析器（Task 6）
src-tauri/Cargo.toml               修改：依赖 wsieve-dns
```

**为什么单独一个 crate 而不是塞进 `wsieve-route`**：`wsieve-route` 的立身之本是「零 async、零网络、零 IO」（阶段 1 计划 Part B 的开篇纪律）。把解析器塞进去会让整个路由引擎变成 async 依赖树，穷举单测的前提当场作废。二者的边界正是设计文档 §4.2 纪律①要保护的那条缝。

---

## 关于「缓存要不要自己写」—— 已评估，不写

设计文档 §7.3 要求两件事：**遵循 TTL 的 LRU 缓存**，以及**解析失败必须负缓存（短 TTL）**。

结论是 **hickory 内置的 `ResponseCache` 已经同时满足这两条，不要自己再写一层。** 依据是读源码 + 实测：

| 要求 | hickory 的实现 | 实测 |
|---|---|---|
| 遵循 TTL 的缓存 | `cache.rs` 用 `moka::sync::Cache` + 自定义 `Expiry`，按记录 TTL 过期；容量由 `ResolverOpts::cache_size` 控制 | 正向查询：首次 47.6ms → 二次 155µs |
| 负缓存 | `ResponseCache::insert` 对 `NetError::Dns(DnsError::NoRecordsFound(_))` 单独走一条分支入缓存，TTL 由 `negative_min_ttl` / `negative_max_ttl` 夹取（`cache.rs:55-68`） | NXDOMAIN：首次 273ms → 二次 165µs |
| 负缓存 TTL 下限 | 服务端给的 NXDOMAIN TTL 可能是 0（等于没有负缓存）。`opts.negative_min_ttl` 正是为此存在，我们把配置的 `negative-ttl-s` 接到这里 | 见 Task 3 |

自建一层只会与内置缓存的 TTL 记账打架 —— 两套过期逻辑各记各的，出问题时无法判断是谁的缓存返回了旧值。**配置项 `dns.cache.{max, negative-ttl-s}` 直接映射到 `ResolverOpts::{cache_size, negative_min_ttl}`。**

> 唯一没有直接映射的是「LRU」这个词：moka 用的是 TinyLFU 而非严格 LRU。对本用途（限容 + 按 TTL 过期）二者等效，且 TinyLFU 的命中率通常更好。**不为这个词去替换实现。**

---

## Part A — `wsieve-dns`

### Task 1: 建立 crate 骨架

**Files:**
- Create: `crates/wsieve-dns/Cargo.toml`
- Create: `crates/wsieve-dns/src/lib.rs`
- Modify: `Cargo.toml`（workspace members）

- [ ] **Step 1: 创建 Cargo.toml**

```toml
[package]
name = "wsieve-dns"
version = "0.1.0"
edition = "2021"
description = "内部 DNS 解析器：服务于路由判决，含 bootstrap 隔离与硬超时"

[dependencies]
wsieve-proto = { path = "../wsieve-proto" }
wsieve-geo = { path = "../wsieve-geo" }
wsieve-route = { path = "../wsieve-route" }
thiserror = { workspace = true }
tokio = { workspace = true }
tracing = "0.1"
hickory-resolver = { version = "0.26", default-features = false, features = [
    "tokio",
    "system-config",
    "https-ring",
    "webpki-roots",
] }

[dev-dependencies]
```

**四个 feature 逐条说明**（`default-features = false` 是刻意的 —— 默认 features 是 `["system-config", "tokio"]`，不含 DoH，写全反而更清楚）：

| feature | 为什么需要 |
|---|---|
| `tokio` | 项目全栈 tokio。它拉起 `TokioRuntimeProvider` |
| `system-config` | **纪律①的前提**：`bootstrap()` 靠它读 `/etc/resolv.conf` / Windows 注册表 |
| `https-ring` | DoH（HTTP/2）。**选 ring 而非 aws-lc-rs**：仓库 `Cargo.lock` 里 `ring` 与 `aws-lc-rs` 都已存在（reqwest / rustls 链路带进来的），但 ring 是纯 Rust + 少量汇编、无 cmake 依赖，交叉编译更省事 |
| `webpki-roots` | DoH 要校验证书。**不用 `rustls-platform-verifier`**：它在 macOS 上走 Security.framework，而我们连的是 IP 字面量，平台校验器对 IP SAN 的处理各平台不一。webpki-roots 是编译进二进制的固定根证书集，跨平台行为一致 |

> **`https-ring` 与 `tls-ring` 的关系**：`https-ring` 自动打开 `__https` → `__tls`，因此 DoT（`tls://`）也一并可用，不需要再加 `tls-ring`。已实测 `ProtocolConfig::Tls` 可正常构造。

- [ ] **Step 2: 注册进 workspace**

在根 `Cargo.toml` 的 `members` 数组里，`"crates/wsieve-proto"` 之前插入一行：

```toml
    "crates/wsieve-dns",
```

- [ ] **Step 3: 写占位 lib.rs**

```rust
//! 内部 DNS 解析器：唯一的消费者是路由引擎。
//!
//! 本 crate 只实现设计文档 §7.1 的**第一层**：纯内部解析器，
//! **不监听任何端口、不需要 root**。第二层（对外的 DNS 服务器 + fake-ip）
//! 是阶段 6 的事，随 TUN 一起做 —— 不要在这里提前动手。
//!
//! 之所以能这么拆：SOCKS5 与 HTTP CONNECT 本就把域名原样递过来，代理模式下
//! 客户端无需解析即可路由转发。解析只在一处被需要 —— 让 GEOIP / IP-CIDR
//! 这类规则对域名目标生效。那是内部查询，不是对外服务。

pub mod decide;
pub mod inject;
pub mod resolver;
pub mod upstream;

pub use decide::{decide, Outcome};
pub use inject::{RoutingResolver, StubResolver};
pub use resolver::{bootstrap, bootstrap_with, DnsResolver, ResolverError};
pub use upstream::{parse_nameserver, UpstreamError};
```

- [ ] **Step 4: 验证能编译**

Run: `cargo check -p wsieve-dns`
Expected: 报 `file not found for module decide/inject/resolver/upstream` —— 这是预期的。先确认 workspace 注册生效（**不该**出现 "package not found"），且 hickory 及其依赖能拉下来编译。

首次编译会拉 140+ 个包（hickory-proto / hickory-net / rustls / h2 / ring …），耗时约 1–2 分钟，属正常。

- [ ] **Step 5: 提交**

```bash
git add Cargo.toml crates/wsieve-dns/
git commit -m "chore(dns): 建立 wsieve-dns crate 骨架"
```

---

### Task 2: 上游地址解析（纪律② —— IP 字面量）

**Files:**
- Create: `crates/wsieve-dns/src/upstream.rs`

设计文档 §7.2 纪律②：**DoH 上游一律用 IP 字面量配置**，否则「DoH 服务器自身的域名由谁解析」就成了先有鸡还是先有蛋。

本 Task 的关键取舍是**把这条纪律变成加载期的硬错误**，而不是文档里的一句提醒。写了域名的配置根本建不出解析器 —— 用户在启动时就看到可操作的报错，而不是运行几小时后遇到一次诡异的解析失败。

- [ ] **Step 1: 写失败的测试**

放在 `crates/wsieve-dns/src/upstream.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn ok(spec: &str) -> NameServerConfig {
        parse_nameserver(spec).unwrap_or_else(|e| panic!("解析 {spec:?} 失败：{e}"))
    }

    #[test]
    fn doh_ip_literal_is_accepted() {
        let ns = ok("https://1.1.1.1/dns-query");
        assert_eq!(ns.ip, "1.1.1.1".parse::<IpAddr>().unwrap());
        assert_eq!(ns.connections.len(), 1);
        assert_eq!(ns.connections[0].port, 443);
        match &ns.connections[0].protocol {
            ProtocolConfig::Https { server_name, path } => {
                assert_eq!(&**server_name, "1.1.1.1", "SNI 必须是 IP 本身");
                assert_eq!(&**path, "/dns-query");
            }
            other => panic!("应是 Https，实为 {other:?}"),
        }
    }

    #[test]
    fn doh_with_hostname_is_rejected_naming_the_host() {
        // 纪律②：这是本模块存在的理由，必须在加载期就炸
        let e = parse_nameserver("https://cloudflare-dns.com/dns-query")
            .unwrap_err()
            .to_string();
        assert!(e.contains("cloudflare-dns.com"), "要点名冒犯的主机：{e}");
        assert!(e.contains("IP"), "要说明为什么：{e}");
    }

    #[test]
    fn plain_ip_defaults_to_udp_plus_tcp() {
        let ns = ok("1.1.1.1");
        assert_eq!(ns.connections.len(), 2, "UDP 要配一条 TCP 兜截断应答");
        assert_eq!(ns.connections[0].port, 53);
        assert!(matches!(ns.connections[0].protocol, ProtocolConfig::Udp));
        assert!(matches!(ns.connections[1].protocol, ProtocolConfig::Tcp));
    }

    #[test]
    fn dot_uses_853() {
        let ns = ok("tls://9.9.9.9");
        assert_eq!(ns.connections[0].port, 853);
    }

    #[test]
    fn explicit_port_overrides_default() {
        let ns = ok("https://1.1.1.1:8443/dns-query");
        assert_eq!(ns.connections[0].port, 8443);
    }

    #[test]
    fn ipv6_literal_in_brackets() {
        let ns = ok("https://[2606:4700:4700::1111]/dns-query");
        assert_eq!(ns.ip, "2606:4700:4700::1111".parse::<IpAddr>().unwrap());
        assert_eq!(ns.connections[0].port, 443);
    }

    #[test]
    fn bare_ipv6_without_port_is_not_mistaken_for_host_colon_port() {
        // 裸 IPv6 有一堆冒号，不能把最后一段当端口
        let ns = ok("2606:4700:4700::1111");
        assert_eq!(ns.ip, "2606:4700:4700::1111".parse::<IpAddr>().unwrap());
        assert_eq!(ns.connections[0].port, 53);
    }

    #[test]
    fn missing_path_defaults_to_dns_query() {
        let ns = ok("https://8.8.8.8");
        match &ns.connections[0].protocol {
            ProtocolConfig::Https { path, .. } => assert_eq!(&**path, "/dns-query"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn custom_path_is_kept() {
        let ns = ok("https://8.8.8.8/resolve");
        match &ns.connections[0].protocol {
            ProtocolConfig::Https { path, .. } => assert_eq!(&**path, "/resolve"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_scheme_is_rejected() {
        assert!(parse_nameserver("quic://1.1.1.1").is_err());
        assert!(parse_nameserver("ftp://1.1.1.1").is_err());
    }

    #[test]
    fn empty_is_rejected() {
        assert!(parse_nameserver("   ").is_err());
    }

    #[test]
    fn bad_port_is_rejected_not_silently_defaulted() {
        assert!(parse_nameserver("https://1.1.1.1:99999/dns-query").is_err());
        assert!(parse_nameserver("https://1.1.1.1:abc/dns-query").is_err());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p wsieve-dns --lib upstream::`
Expected: 编译失败，`cannot find function parse_nameserver in this scope`

- [ ] **Step 3: 写实现**

放在测试模块**之前**：

```rust
//! 上游地址解析：把配置里的 nameserver 字符串变成 hickory 的 NameServerConfig。
//!
//! 纪律②（设计文档 §7.2）：**DoH 上游一律用 IP 字面量**。否则「DoH 服务器
//! 自己的域名由谁解析」就成了一个先有鸡还是先有蛋的问题。本模块把这条纪律
//! 变成**解析期的硬错误**——写了域名的配置根本加载不进来，而不是运行时才炸。

use std::net::IpAddr;
use std::sync::Arc;

use hickory_resolver::config::{ConnectionConfig, NameServerConfig, ProtocolConfig};

#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error("nameserver「{0}」为空")]
    Empty(String),
    #[error(
        "nameserver「{spec}」的主机部分「{host}」不是 IP 字面量。\
         DoH/DoT 上游必须写成 IP（如 https://1.1.1.1/dns-query），\
         否则解析这台 DNS 服务器的域名本身又需要一次 DNS 查询，\
         形成先有鸡还是先有蛋的死循环（设计文档 §7.2 纪律②）"
    )]
    NotAnIpLiteral { spec: String, host: String },
    #[error("nameserver「{spec}」的端口「{port}」非法")]
    BadPort { spec: String, port: String },
    #[error("nameserver「{spec}」使用了不支持的协议前缀。支持：https:// · tls:// · udp:// · tcp:// · 裸 IP")]
    UnknownScheme { spec: String },
}

/// 把一条配置字符串解析成 hickory 的上游配置。
///
/// 支持的写法：
/// - `https://1.1.1.1/dns-query`  → DoH（默认 443 端口）
/// - `tls://1.1.1.1`             → DoT（默认 853 端口）
/// - `udp://1.1.1.1` / `1.1.1.1` → 明文 UDP+TCP（默认 53 端口）
/// - `tcp://1.1.1.1`             → 明文 TCP
/// - 端口可显式覆盖：`https://1.1.1.1:8443/dns-query`
/// - IPv6 用方括号：`https://[2606:4700:4700::1111]/dns-query`
pub fn parse_nameserver(spec: &str) -> Result<NameServerConfig, UpstreamError> {
    let s = spec.trim();
    if s.is_empty() {
        return Err(UpstreamError::Empty(spec.to_string()));
    }

    let (scheme, rest) = match s.split_once("://") {
        Some((sc, r)) => (sc.to_ascii_lowercase(), r),
        // 裸 IP 视为 udp
        None => ("udp".to_string(), s),
    };

    // 先切掉路径，再切端口
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (rest, None),
    };
    let (host, port) = split_host_port(authority).map_err(|port| UpstreamError::BadPort {
        spec: spec.to_string(),
        port,
    })?;

    // 纪律②：主机必须是 IP 字面量
    let ip: IpAddr = host.parse().map_err(|_| UpstreamError::NotAnIpLiteral {
        spec: spec.to_string(),
        host: host.to_string(),
    })?;

    // server_name 就用 IP 的字符串形式：证书里 1.1.1.1 / 8.8.8.8 / 9.9.9.9
    // 都带有 IP SAN，实测可通过校验（见本 Task 的 Step 5）。
    let sni: Arc<str> = Arc::from(ip.to_string().as_str());

    let protocol = match scheme.as_str() {
        "https" | "h2" => ProtocolConfig::Https {
            server_name: sni,
            // 路径缺省用 /dns-query，与 RFC 8484 的惯例一致
            path: Arc::from(
                match path {
                    Some(p) if !p.is_empty() => format!("/{p}"),
                    _ => "/dns-query".to_string(),
                }
                .as_str(),
            ),
        },
        "tls" | "dot" => ProtocolConfig::Tls { server_name: sni },
        "udp" => ProtocolConfig::Udp,
        "tcp" => ProtocolConfig::Tcp,
        _ => {
            return Err(UpstreamError::UnknownScheme {
                spec: spec.to_string(),
            })
        }
    };

    let mut conn = ConnectionConfig::new(protocol);
    if let Some(p) = port {
        conn.port = p;
    }

    // udp 额外配一条 tcp：被截断的应答（TC 位）要能退回 TCP 重问。
    let connections = if scheme == "udp" {
        let mut tcp = ConnectionConfig::new(ProtocolConfig::Tcp);
        if let Some(p) = port {
            tcp.port = p;
        }
        vec![conn, tcp]
    } else {
        vec![conn]
    };

    Ok(NameServerConfig::new(ip, true, connections))
}

/// 拆 `host[:port]`，IPv6 用 `[...]` 包裹。
/// 返回 Err(端口原文) 表示端口段存在但解析失败。
fn split_host_port(authority: &str) -> Result<(&str, Option<u16>), String> {
    if let Some(rest) = authority.strip_prefix('[') {
        // IPv6 字面量
        let (host, tail) = rest.split_once(']').ok_or_else(|| authority.to_string())?;
        let port = match tail.strip_prefix(':') {
            Some(p) => Some(p.parse::<u16>().map_err(|_| p.to_string())?),
            None => None,
        };
        return Ok((host, port));
    }
    // IPv4 或裸 IPv6（无端口）。裸 IPv6 含多个冒号，不能当端口分隔符。
    match authority.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => {
            Ok((h, Some(p.parse::<u16>().map_err(|_| p.to_string())?)))
        }
        _ => Ok((authority, None)),
    }
}
```

**三处容易写错、已被测试锁住的地方：**

1. **`ConnectionConfig` 的 `port` 字段**。`ConnectionConfig::new(protocol)` 会按协议填默认端口（Https→443、Tls→853、Udp/Tcp→53），显式端口要**在 `new` 之后覆盖**。该结构体带 `#[non_exhaustive]`，但 `port` 是 `pub` 字段，**外部 crate 仍可赋值**（已实测）—— `#[non_exhaustive]` 只禁止字面量构造，不禁止字段写入
2. **裸 IPv6 的端口切分**。`2606:4700:4700::1111` 用 `rsplit_once(':')` 会把 `1111` 当端口。守卫是 `!h.contains(':')`
3. **`ProtocolConfig::Https` 的 `path` 是 `Arc<str>` 不是 `Option`**。`ConnectionConfig::https()` 才收 `Option` 并在内部补默认值；我们走 `ProtocolConfig::Https{..}` 字面量，得自己补

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p wsieve-dns --lib upstream::`
Expected: **12 个测试全部 PASS**

- [ ] **Step 5: 实证 IP 字面量的证书能过校验**

纪律②要求用 IP 字面量，但这只在「DoH 服务商给证书签了 IP SAN」时才成立。这不是可以假设的事 —— 写个临时 example 实证：

```bash
cat > /tmp/doh_probe.rs <<'EOF'
// 放进 crates/wsieve-dns/examples/doh_probe.rs 临时验证，验完删掉
use std::net::IpAddr;
use std::time::Duration;
use hickory_resolver::config::{ResolverConfig, ResolverOpts};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::Resolver;
use wsieve_dns::parse_nameserver;

#[tokio::main]
async fn main() {
    for spec in ["https://1.1.1.1/dns-query", "https://8.8.8.8/dns-query", "https://9.9.9.9/dns-query"] {
        let ns = parse_nameserver(spec).unwrap();
        let mut o = ResolverOpts::default();
        o.timeout = Duration::from_secs(3);
        o.attempts = 1;
        let r = Resolver::builder_with_config(
            ResolverConfig::from_parts(None, vec![], vec![ns]),
            TokioRuntimeProvider::default(),
        ).with_options(o).build().unwrap();
        match tokio::time::timeout(Duration::from_secs(6), r.lookup_ip("example.com.")).await {
            Ok(Ok(l)) => println!("{spec} OK {:?}", l.iter().collect::<Vec<IpAddr>>()),
            Ok(Err(e)) => println!("{spec} ERR {e}"),
            Err(_) => println!("{spec} TIMEOUT"),
        }
    }
}
EOF
mkdir -p crates/wsieve-dns/examples && cp /tmp/doh_probe.rs crates/wsieve-dns/examples/doh_probe.rs
cargo run -p wsieve-dns --example doh_probe
```

Expected: 三家全部 `OK`，各返回一批 example.com 的地址。

**计划评审时的实测结果**（供对照）：

```
https://1.1.1.1/dns-query OK [2606:4700:10::ac42:93f3, ..., 104.20.23.154]
https://8.8.8.8/dns-query OK [...]
https://9.9.9.9/dns-query OK [...]
```

Cloudflare / Google / Quad9 三家的 DoH 证书**都带 IP SAN**，纪律②在现实中站得住。若你的实测有某家失败，说明该服务商改了证书策略 —— 在配置默认值里换一家，不要把纪律降级成「尽量用 IP」。

验完删掉 example：

```bash
rm crates/wsieve-dns/examples/doh_probe.rs && rmdir crates/wsieve-dns/examples 2>/dev/null || true
```

- [ ] **Step 6: 提交**

```bash
git add crates/wsieve-dns/src/upstream.rs
git commit -m "feat(dns): 上游地址解析，IP 字面量纪律落为加载期硬错误"
```

---

### Task 3: `DnsResolver` 与硬超时（纪律③）

**Files:**
- Create: `crates/wsieve-dns/src/resolver.rs`
- Create: `crates/wsieve-dns/tests/resolver.rs`

本 Task 有两个核心决定，都是实测逼出来的：

**① 硬超时必须外包 `tokio::time::timeout`。** 见本计划开头「实测发现①」：只靠 `ResolverOpts::timeout` 实测超出 30 倍（500ms 设定 → 15.03s 实际）。

**② `lookup_for_routing` 的签名里没有 `Result`。** 这是纪律③在类型层面的表达 —— 解析失败不是一个需要调用方处理的错误，而是「这批 IP 是空的」这一普通事实。签名里根本没有失败这条路，调用方也就**无从写出**「解析失败就阻断连接」的代码。这比在文档里写一句「请不要阻断」可靠得多。

- [ ] **Step 1: 写失败的测试**

`crates/wsieve-dns/tests/resolver.rs`：

```rust
//! 解析器本体：构造校验、硬超时、bootstrap 隔离。
//!
//! 除标注「联网」的两个用例外，全部脱网。联网用例在无网环境下会自行跳过，
//! CI 不因外网抖动变红。

use std::time::{Duration, Instant};

use wsieve_dns::{bootstrap, bootstrap_with, DnsResolver};

fn ns(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn empty_nameserver_list_is_rejected_with_an_actionable_message() {
    let e = DnsResolver::new(&[], Duration::from_secs(2), 4096, Duration::from_secs(30))
        .unwrap_err()
        .to_string();
    assert!(e.contains("nameserver"), "{e}");
    assert!(e.contains("1.1.1.1"), "错误信息要给出可照抄的例子：{e}");
}

#[test]
fn hostname_upstream_is_rejected_at_construction_not_at_query_time() {
    // 纪律②必须在加载期生效：坏配置根本建不出解析器
    let e = DnsResolver::new(
        &ns(&["https://cloudflare-dns.com/dns-query"]),
        Duration::from_secs(2),
        4096,
        Duration::from_secs(30),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("cloudflare-dns.com"), "{e}");
}

#[test]
fn valid_config_builds() {
    assert!(DnsResolver::new(
        &ns(&["https://1.1.1.1/dns-query", "https://8.8.8.8/dns-query"]),
        Duration::from_secs(2),
        4096,
        Duration::from_secs(30),
    )
    .is_ok());
}

/// **本阶段最重要的一个测试。**
///
/// 证明硬超时确实是墙钟上限。上游指向 192.0.2.1（RFC 5737 文档段，保证黑洞），
/// 解析必须在超时附近返回空切片，而不是把连接晾在那里。
///
/// 计划评审时实测：仅靠 `ResolverOpts::timeout = 500ms` 时单次查询耗时
/// **15.03 秒**；加上外层 `tokio::time::timeout` 后稳定在设定值。
#[tokio::test]
async fn blackhole_upstream_returns_empty_within_the_hard_timeout() {
    let r = DnsResolver::new(
        &ns(&["https://192.0.2.1/dns-query"]),
        Duration::from_millis(800),
        64,
        Duration::from_secs(30),
    )
    .unwrap();

    let t = Instant::now();
    let ips = r.lookup_for_routing("example.com").await;
    let elapsed = t.elapsed();

    assert!(ips.is_empty(), "黑洞上游必须返回空切片而非挂起");
    assert!(
        elapsed < Duration::from_secs(3),
        "硬超时没生效：耗时 {elapsed:?}。\
         若接近 15s，说明 lookup_for_routing 丢了外层 tokio::time::timeout —— \
         ResolverOpts::timeout 只在名字服务器池的轮与轮之间检查 deadline，\
         挡不住卡在 TCP/TLS 建连里的那一轮"
    );
}

#[tokio::test]
async fn a_failed_lookup_never_panics_and_never_returns_err() {
    // 签名上就没有 Result —— 这个测试锁住的是这条设计不被后人改回去
    let r = DnsResolver::new(
        &ns(&["https://192.0.2.1/dns-query"]),
        Duration::from_millis(300),
        64,
        Duration::from_secs(30),
    )
    .unwrap();
    let ips: Vec<std::net::IpAddr> = r.lookup_for_routing("whatever.invalid").await;
    assert!(ips.is_empty());
}

// ── bootstrap 隔离（纪律①）────────────────────────────────

#[test]
fn bootstrap_is_a_separate_instance_from_the_routing_resolver() {
    // 隔离靠的是「压根是两个对象」，不是靠某个 if 分支
    let routing = DnsResolver::new(
        &ns(&["https://1.1.1.1/dns-query"]),
        Duration::from_secs(2),
        4096,
        Duration::from_secs(30),
    )
    .unwrap();
    let boot = bootstrap();
    assert!(boot.is_ok(), "系统 DNS 配置应可读：{:?}", boot.err());
    // 两者类型不同、无共享字段 —— 这一行的意义是让「把 bootstrap 改成
    // 复用 routing 的缓存」这种改动无法悄悄通过编译
    let _ = routing;
}

#[test]
fn bootstrap_with_explicit_upstreams_enforces_ip_literals_too() {
    assert!(bootstrap_with(&ns(&["1.1.1.1"])).is_ok());
    assert!(bootstrap_with(&ns(&["https://dns.google/dns-query"])).is_err());
    assert!(bootstrap_with(&[]).is_err());
}

// ── 联网用例（无网自动跳过）───────────────────────────────

#[tokio::test]
async fn live_doh_over_ip_literal_resolves() {
    let r = DnsResolver::new(
        &ns(&["https://1.1.1.1/dns-query"]),
        Duration::from_secs(4),
        256,
        Duration::from_secs(30),
    )
    .unwrap();
    let ips = r.lookup_for_routing("example.com").await;
    if ips.is_empty() {
        eprintln!("跳过：无外网连通性");
        return;
    }
    eprintln!("example.com → {ips:?}");
}

#[tokio::test]
async fn live_cache_makes_the_second_lookup_dramatically_faster() {
    // 证明「缓存不用自己写」这个结论：hickory 内置 ResponseCache 已生效
    let r = DnsResolver::new(
        &ns(&["https://1.1.1.1/dns-query"]),
        Duration::from_secs(4),
        256,
        Duration::from_secs(30),
    )
    .unwrap();

    let t = Instant::now();
    let first = r.lookup_for_routing("example.com").await;
    let d1 = t.elapsed();
    if first.is_empty() {
        eprintln!("跳过：无外网连通性");
        return;
    }

    let t = Instant::now();
    let second = r.lookup_for_routing("example.com").await;
    let d2 = t.elapsed();

    assert_eq!(first, second, "缓存命中应返回同一批 IP");
    assert!(
        d2 * 5 < d1,
        "第二次查询没有明显变快（{d1:?} → {d2:?}），内置缓存可能没生效"
    );
    eprintln!("缓存生效：首次 {d1:?} → 二次 {d2:?}");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p wsieve-dns --test resolver`
Expected: `cannot find type DnsResolver` / `cannot find function bootstrap`

- [ ] **Step 3: 写实现**

`crates/wsieve-dns/src/resolver.rs`：

```rust
//! 内部解析器：路由引擎两阶段求值的第二轮供料方。
//!
//! 这一层**不监听任何端口、不需要 root**（设计文档 §7.1 第一层）。
//! 唯一的消费者是路由引擎——SOCKS5 与 HTTP CONNECT 本就把域名原样递过来，
//! 代理模式下客户端无需解析即可转发；解析只在一处被需要：让 GEOIP / IP-CIDR
//! 这类规则对域名目标生效。
//!
//! 三条纪律（设计文档 §7.2 / §7.3）在本模块的落点：
//! - 纪律①：出站服务器域名走 `bootstrap()`，绝不经过本解析器
//! - 纪律②：上游必须是 IP 字面量，由 `upstream::parse_nameserver` 在加载期拦下
//! - 纪律③：解析超时/失败**返回空切片而非错误**，判决继续往下走

use std::net::IpAddr;
use std::time::Duration;

use hickory_resolver::config::{ResolveHosts, ResolverConfig, ResolverOpts};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::{Resolver, TokioResolver};

use crate::upstream::{parse_nameserver, UpstreamError};

#[derive(Debug, thiserror::Error)]
pub enum ResolverError {
    #[error("nameserver 配置有误：{0}")]
    Upstream(#[from] UpstreamError),
    #[error("dns.nameserver 为空。内部解析器至少需要一个上游，\
             例如：https://1.1.1.1/dns-query")]
    NoNameservers,
    #[error("构建解析器失败：{0}")]
    Build(String),
    #[error("读取系统 DNS 配置失败：{0}。bootstrap 解析器依赖它来解析出站服务器域名")]
    SystemConf(String),
}

/// 内部解析器。判决路径专用。
///
/// 缓存**不自建**：hickory 内置 `ResponseCache`（moka，按 TTL 过期）已同时
/// 覆盖设计文档 §7.3 要求的「遵循 TTL 的 LRU」与「失败必须负缓存」两项——
/// NXDOMAIN 走 `NoRecordsFound` 分支入缓存，TTL 由 `negative_min_ttl` /
/// `negative_max_ttl` 夹取。实测二次查询 165µs vs 首次 273ms，确认生效。
/// 自己再套一层只会与内置缓存的 TTL 记账打架。
///
/// `Debug` 不是装饰：测试里对 `Result<DnsResolver, _>` 调 `.unwrap_err()`
/// 要求 `T: Debug`，少了它本 Task 的构造校验测试直接编译不过。
/// hickory 的 `Resolver` 未实现 `Debug`，故手写而非 derive。
pub struct DnsResolver {
    inner: TokioResolver,
    /// 判决路径上的硬超时。**必须由外层 tokio::time::timeout 施加**，
    /// 见 `lookup_for_routing` 的注释。
    hard_timeout: Duration,
}

impl std::fmt::Debug for DnsResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不打印上游列表：那属于用户配置，日志里不该有。
        f.debug_struct("DnsResolver")
            .field("hard_timeout", &self.hard_timeout)
            .finish_non_exhaustive()
    }
}

impl DnsResolver {
    /// 用配置里的 `dns.nameserver` 建立解析器。
    ///
    /// - `nameservers`：上游列表，必须是 IP 字面量（纪律②）
    /// - `timeout`：判决路径硬超时（设计文档 §7.3 定为 2s）
    /// - `cache_max` / `negative_ttl`：对应配置的 `dns.cache.{max,negative-ttl-s}`
    pub fn new(
        nameservers: &[String],
        timeout: Duration,
        cache_max: u64,
        negative_ttl: Duration,
    ) -> Result<Self, ResolverError> {
        if nameservers.is_empty() {
            return Err(ResolverError::NoNameservers);
        }
        let mut servers = Vec::with_capacity(nameservers.len());
        for spec in nameservers {
            servers.push(parse_nameserver(spec)?);
        }

        let mut opts = ResolverOpts::default();
        // 这个 timeout 只约束「池内单轮」，不等于端到端墙钟上限——真正的
        // 硬超时在 lookup_for_routing 里用 tokio::time::timeout 施加。
        // 这里仍然设小，是为了让池尽早放弃一台坏上游去试下一台。
        opts.timeout = timeout;
        // 判决路径上不做重试：重试的时间预算还不如直接让 IP 规则不匹配，
        // 流程继续往下（纪律③）。attempts 默认为 2，必须显式压到 1。
        opts.attempts = 1;
        opts.cache_size = cache_max;
        // 负缓存下限：服务端给的 NXDOMAIN TTL 可能是 0，那样等于没有负缓存，
        // 不存在的域名会被反复查询（设计文档 §7.3）。
        opts.negative_min_ttl = Some(negative_ttl);
        // 判决只关心「这批 IP 落在哪个网段」，中间的 CNAME 记录一概不需要，
        // 留着只会占缓存容量。
        opts.preserve_intermediates = false;
        // 关键：**绝不读系统 hosts 文件**。shard.rs 会把出站服务器域名写成
        // `127.0.0.1 <域名> # wsieve-managed`；若解析器读了它，路由判决会
        // 认为该域名是环回地址，从而命中内网直连规则。判决必须看到真实 IP。
        // 默认值是 Auto，**会读** —— 实测见本计划开头「实测发现②」。
        opts.use_hosts_file = ResolveHosts::Never;

        let inner = Resolver::builder_with_config(
            ResolverConfig::from_parts(None, vec![], servers),
            TokioRuntimeProvider::default(),
        )
        .with_options(opts)
        .build()
        .map_err(|e| ResolverError::Build(e.to_string()))?;

        Ok(Self {
            inner,
            hard_timeout: timeout,
        })
    }

    /// 判决路径专用的解析。**永不返回错误**。
    ///
    /// 设计文档 §6.2 与 §12 的硬要求：超时或失败一律视为「该 IP 规则不匹配」，
    /// 流程继续往下走，绝不因一次 DNS 故障阻断整条连接。因此签名是
    /// `-> Vec<IpAddr>`，调用方把它原样传给 `evaluate(target, Some(&ips), ..)`，
    /// 空切片即不匹配。
    ///
    /// **硬超时必须由外层 `tokio::time::timeout` 施加，不能只靠
    /// `ResolverOpts::timeout`。** 已实测：把 `opts.timeout` 设成 500ms、
    /// 上游指向黑洞地址（192.0.2.1，RFC 5737 文档段），单次 `lookup_ip`
    /// 实际耗时 **15.03 秒**。原因是 `opts.timeout` 只在名字服务器池的
    /// **轮与轮之间**检查 deadline（`name_server_pool.rs:290`），而 TCP/TLS
    /// 建连本身卡在系统 connect 超时里，一轮都没走完。外层包一层之后实测
    /// 稳定在 2.00s。
    pub async fn lookup_for_routing(&self, domain: &str) -> Vec<IpAddr> {
        match tokio::time::timeout(self.hard_timeout, self.inner.lookup_ip(domain)).await {
            Ok(Ok(lookup)) => lookup.iter().collect(),
            Ok(Err(e)) => {
                // 不静默吞掉：解析失败是排查「为什么这个域名没走对路」的关键线索。
                tracing::debug!("解析 {domain} 失败，该 IP 规则按不匹配处理：{e}");
                Vec::new()
            }
            Err(_) => {
                tracing::warn!(
                    "解析 {domain} 超过硬超时 {:?}，该 IP 规则按不匹配处理，连接继续",
                    self.hard_timeout
                );
                Vec::new()
            }
        }
    }
}

/// **bootstrap 解析器：专解出站服务器域名，绝不经过我们自己的 DNS。**
///
/// 这是设计文档 §7.2 纪律①的落点，也是本阶段最要紧的一条。`shard.rs:90`
/// 的 `resolve_upstream` 必须拿到**真实 IP**：一旦阶段 6 的 fake-ip 生效，
/// 走我们自己的 DNS 只会拿到 `198.18.x.x`，转发器于是连向虚空——而且是
/// 静默失败，表现为「握手一直不成功」，极难排查。
///
/// 实现上刻意用一个**独立的 Resolver 实例**、读系统配置（`/etc/resolv.conf`
/// 或 Windows 注册表），与 `DnsResolver` 没有任何共享状态：没有共享缓存、
/// 没有共享上游、没有共享 fake-ip 映射表。隔离靠的是「压根是两个对象」，
/// 而不是靠某个 if 分支——分支会被改错，两个对象不会。
///
/// 对应配置项 `dns.proxy-server-nameserver: [system]`（沿用 Clash 字段名，
/// 语义恰好是「专门用来解析代理服务器域名的」，见设计文档 §5.3 取舍④）。
///
/// **注意这里保留系统 hosts 的读取（不设 Never）**：`builder_tokio()` 走
/// 系统配置，用户自己在 hosts 里写的条目属于「用户明示的意图」，应当尊重。
/// 我们自己写的 `# wsieve-managed` 行由 `shard_setup.rs` 的既有顺序防线
/// 处理 —— 它在 `clear_managed()` 之后才解析（见 Task 6）。
pub fn bootstrap() -> Result<TokioResolver, ResolverError> {
    Resolver::builder_tokio()
        .map_err(|e| ResolverError::SystemConf(e.to_string()))?
        .build()
        .map_err(|e| ResolverError::Build(e.to_string()))
}

/// 用显式上游建立 bootstrap 解析器，供 `proxy-server-nameserver` 写了具体
/// 地址（而非 `system`）时使用。
///
/// 注意这里**不设负缓存下限也不压 attempts**：bootstrap 服务的是「建立出站
/// 连接」这条路径，宁可多等一会儿也要拿到真实 IP；而判决路径的纪律是宁可
/// 不匹配也不阻塞。两条路径的取舍方向相反，所以不共用配置。
pub fn bootstrap_with(nameservers: &[String]) -> Result<TokioResolver, ResolverError> {
    if nameservers.is_empty() {
        return Err(ResolverError::NoNameservers);
    }
    let mut servers = Vec::with_capacity(nameservers.len());
    for spec in nameservers {
        servers.push(parse_nameserver(spec)?);
    }
    let mut opts = ResolverOpts::default();
    // 显式上游意味着用户绕开了系统配置，此时 hosts 也一并绕开 —— 出站域名
    // 正是被我们写进 hosts 的那一个，读它只会拿到 127.0.0.1。
    opts.use_hosts_file = ResolveHosts::Never;
    Resolver::builder_with_config(
        ResolverConfig::from_parts(None, vec![], servers),
        TokioRuntimeProvider::default(),
    )
    .with_options(opts)
    .build()
    .map_err(|e| ResolverError::Build(e.to_string()))
}
```

**编译细节备忘**（都实测踩过）：

- `TokioRuntimeProvider` 的路径是 **`hickory_resolver::net::runtime::TokioRuntimeProvider`**。hickory 自己的文档注释写的 `name_server::TokioConnectionProvider` 在 0.26 是**私有模块**，照抄会报 `private module`
- `ResolveHosts` 从 `hickory_resolver::config` 导出
- `ResolverOpts` 带 `#[non_exhaustive]`，因此**只能** `default()` 之后逐字段赋值，不能用结构体字面量

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p wsieve-dns --test resolver -- --nocapture`
Expected: **9 个测试全部 PASS**。

**特别确认这两条**：

- `blackhole_upstream_returns_empty_within_the_hard_timeout` —— 本阶段的命脉。若它失败且耗时接近 15s，说明外层 `tokio::time::timeout` 丢了
- `live_cache_makes_the_second_lookup_dramatically_faster` 的 `--nocapture` 输出。**把实测数字记进提交信息**，这是「不自建缓存」这个决定的实证依据

计划评审时的实测输出（供对照）：

```
example.com → [2606:4700:10::6814:179a, ..., 172.66.147.243]
缓存生效：首次 170.81ms → 二次 156.25µs
```

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-dns/src/resolver.rs crates/wsieve-dns/tests/resolver.rs
git commit -m "feat(dns): DnsResolver 与硬超时；bootstrap 解析器隔离

硬超时必须外包 tokio::time::timeout —— 实测仅靠 ResolverOpts::timeout
设 500ms 时黑洞上游耗时 15.03s（超出 30 倍），因池的 deadline 只在
轮与轮之间检查。缓存沿用 hickory 内置 ResponseCache，实测首次 170ms
→ 二次 156µs，负缓存首次 273ms → 二次 165µs，无需自建。"
```

---

### Task 4: 解析注入（`RoutingResolver` + `StubResolver`）

**Files:**
- Create: `crates/wsieve-dns/src/inject.rs`

设计文档 §14 给阶段 3 定的验证方式是「**解析注入** + 规则命中单测」。本 Task 就是那个「注入」。

真解析器要发网络请求，在测试里既慢又不确定 —— 断言会变成「今天 Cloudflare 心情如何」。把「拿一批 IP」抽成 trait，判决链路依赖 trait 而非具体解析器，整条链路就能脱网穷举。

- [ ] **Step 1: 写实现（本 Task 的测试即 Task 5 的整个测试文件）**

```rust
//! 解析注入：让「解析 → 判决」这条链路可以脱网穷举单测。
//!
//! 设计文档 §14 给阶段 3 定的验证方式是「解析注入 + 规则命中单测」。真解析器
//! 要发网络请求，在测试里既慢又不确定；因此把「拿一批 IP」这件事抽成 trait，
//! 判决链路依赖 trait 而非具体解析器。
//!
//! trait 方法**不返回 Result**：这是纪律③（§7.2）在类型层面的表达——解析
//! 失败不是一个需要调用方处理的错误，而是「这批 IP 是空的」这一普通事实。
//! 签名里根本没有失败这条路，调用方也就无从写出「解析失败就阻断连接」的代码。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;

/// 判决路径需要的全部解析能力。
///
/// 手写 boxed future 而非引入 `async-trait`：只有一个方法，为它多一个
/// 过程宏依赖不值当 —— 与仓库里手写 base64 的取舍一致
/// （见 `src-tauri/src/bridge.rs:285` 的注释）。
pub trait RoutingResolver: Send + Sync {
    /// 解析域名。**永不失败**：超时、NXDOMAIN、上游不可达一律返回空 Vec，
    /// 由调用方传给 `evaluate(target, Some(&ips), ..)`，空切片即不匹配。
    fn resolve<'a>(
        &'a self,
        domain: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<IpAddr>> + Send + 'a>>;
}

impl RoutingResolver for crate::resolver::DnsResolver {
    fn resolve<'a>(
        &'a self,
        domain: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<IpAddr>> + Send + 'a>> {
        Box::pin(self.lookup_for_routing(domain))
    }
}

/// 测试与「规则试算」用的注入式解析器。
///
/// 除了单测，UI 的 `rule_test` 命令（设计文档 §11.2）也能用它做「假设这个
/// 域名解析到这批 IP，判决会是什么」的假设推演。
#[derive(Default)]
pub struct StubResolver {
    table: HashMap<String, Vec<IpAddr>>,
    /// 记录被问过哪些域名，用来断言「不该解析时确实没解析」。
    asked: Mutex<Vec<String>>,
}

impl StubResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注入一条解析结果。未注入的域名解析为空（等价于解析失败/超时）。
    pub fn with(mut self, domain: &str, ips: &[&str]) -> Self {
        self.table.insert(
            normalize(domain),
            ips.iter()
                .map(|s| s.parse().unwrap_or_else(|_| panic!("非法 IP：{s}")))
                .collect(),
        );
        self
    }

    /// 被查询过的域名列表（按顺序）。
    pub fn asked(&self) -> Vec<String> {
        self.asked.lock().expect("stub 解析器状态锁中毒").clone()
    }
}

fn normalize(d: &str) -> String {
    d.trim_end_matches('.').to_ascii_lowercase()
}

impl RoutingResolver for StubResolver {
    fn resolve<'a>(
        &'a self,
        domain: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<IpAddr>> + Send + 'a>> {
        let key = normalize(domain);
        self.asked
            .lock()
            .expect("stub 解析器状态锁中毒")
            .push(key.clone());
        let ips = self.table.get(&key).cloned().unwrap_or_default();
        Box::pin(async move { ips })
    }
}
```

**`asked()` 是本模块的关键设计。** 它让「不该解析时确实一次 DNS 都没发」成为**可断言的事实**，而不是靠读代码相信。设计文档 §4.2 说两阶段协议买到的三样东西之一是「不需要时零 DNS 泄漏」—— 没有 `asked()`，这句话就只是一句声明。

- [ ] **Step 2: 验证编译**

Run: `cargo check -p wsieve-dns`
Expected: 通过。此时 `inject` 还没有自己的测试 —— 它的测试就是 Task 5 的整个 `tests/routing.rs`。

- [ ] **Step 3: 提交**

```bash
git add crates/wsieve-dns/src/inject.rs
git commit -m "feat(dns): RoutingResolver trait 与注入式 StubResolver"
```

---

### Task 5: 两阶段求值驱动方 + 解析注入验收

**Files:**
- Create: `crates/wsieve-dns/src/decide.rs`
- Create: `crates/wsieve-dns/tests/routing.rs`

这是本阶段的**交付核心**：阶段 1 造好了 `evaluate`，阶段 3 造好了解析器，本 Task 是把二者接起来的那道缝，也是设计文档 §14 阶段 3「解析注入 + 规则命中单测」的落地处。

集中成**一个函数**而非散在各入口，为的是让三件事只需要证明一次：最多解析一次、第二轮永不再抛 `NeedResolve`、解析失败不阻断连接。阶段 2 的混合端口、阶段 6 的 TUN，都调用这同一个 `decide()`。

- [ ] **Step 1: 写失败的测试**

`crates/wsieve-dns/tests/routing.rs`：

```rust
//! 解析注入 + 规则命中（设计文档 §14 阶段 3 的验证方式）。
//!
//! 全部脱网：解析结果由 StubResolver 注入，因此断言的是**判决链路**本身，
//! 而不是某台 DNS 服务器今天的心情。

use std::collections::HashSet;
use std::path::PathBuf;

use wsieve_dns::{decide, StubResolver};
use wsieve_geo::GeoDb;
use wsieve_proto::addr::{AddrPort, TargetAddr};
use wsieve_route::{Decision, Mode, RuleSet};

fn geo_stub() -> GeoDb {
    // 指向不存在的文件：所有 GEO 查询都会失败，从而顺带验证
    // 「GEO 不可用时规则跳过而非阻断」这条纪律（设计文档 §12）
    GeoDb::new(
        PathBuf::from("/nonexistent/geoip.dat"),
        PathBuf::from("/nonexistent/geosite.dat"),
    )
}

fn rs(lines: &[&str], outbounds: &[&str]) -> RuleSet {
    let known: HashSet<String> = outbounds.iter().map(|s| s.to_string()).collect();
    RuleSet::build(
        &lines.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        Mode::Rule,
        "",
        &known,
    )
    .unwrap()
}

fn domain(d: &str, port: u16) -> AddrPort {
    AddrPort { addr: TargetAddr::Domain(d.into()), port }
}

fn ipv4(a: [u8; 4], port: u16) -> AddrPort {
    AddrPort { addr: TargetAddr::V4(a), port }
}

// ── 不该解析的时候，一次 DNS 都不许发 ────────────────────────

#[tokio::test]
async fn domain_rule_hit_never_touches_dns() {
    let set = rs(
        &["DOMAIN-SUFFIX,example.com,PROXY", "GEOIP,CN,DIRECT", "MATCH,REJECT"],
        &["PROXY"],
    );
    let r = StubResolver::new().with("example.com", &["1.2.3.4"]);
    let out = decide(&set, &domain("www.example.com", 443), &geo_stub(), &r).await;

    assert_eq!(out.decision, Decision::Outbound("PROXY".into()));
    assert!(!out.resolved, "域名规则命中，不该触发解析");
    assert!(r.asked().is_empty(), "解析器根本不该被调用，实际被问：{:?}", r.asked());
}

#[tokio::test]
async fn ip_target_is_decided_without_resolution() {
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let r = StubResolver::new();
    let out = decide(&set, &ipv4([10, 1, 2, 3], 443), &geo_stub(), &r).await;

    assert_eq!(out.decision, Decision::Direct);
    assert!(!out.resolved);
    assert!(r.asked().is_empty(), "目标本就是 IP，无需解析");
}

#[tokio::test]
async fn no_resolve_flag_suppresses_the_query() {
    // 局域网段规则应默认带 no-resolve（设计文档 §6.1）
    let set = rs(
        &["IP-CIDR,192.168.0.0/16,DIRECT,no-resolve", "MATCH,PROXY"],
        &["PROXY"],
    );
    let r = StubResolver::new().with("a.com", &["192.168.1.1"]);
    let out = decide(&set, &domain("a.com", 443), &geo_stub(), &r).await;

    assert_eq!(out.decision, Decision::Outbound("PROXY".into()));
    assert!(!out.resolved);
    assert!(r.asked().is_empty(), "no-resolve 必须真的不解析");
}

// ── 该解析的时候，解析结果要真的进判决 ──────────────────────

#[tokio::test]
async fn injected_ip_drives_the_cidr_rule() {
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let r = StubResolver::new().with("intranet.corp", &["10.7.7.7"]);
    let out = decide(&set, &domain("intranet.corp", 443), &geo_stub(), &r).await;

    assert_eq!(out.decision, Decision::Direct, "解析到内网段应判直连");
    assert!(out.resolved);
    assert_eq!(r.asked(), vec!["intranet.corp"], "且只解析一次");
}

#[tokio::test]
async fn injected_ip_outside_the_cidr_falls_through() {
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let r = StubResolver::new().with("a.com", &["93.184.216.34"]);
    let out = decide(&set, &domain("a.com", 443), &geo_stub(), &r).await;

    assert_eq!(out.decision, Decision::Outbound("PROXY".into()));
    assert!(out.resolved);
}

#[tokio::test]
async fn any_one_of_several_ips_matching_is_enough() {
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let r = StubResolver::new().with("multi.com", &["93.184.216.34", "10.0.0.1"]);
    let out = decide(&set, &domain("multi.com", 443), &geo_stub(), &r).await;

    assert_eq!(out.decision, Decision::Direct, "有一个 IP 落在段内即命中");
    assert_eq!(out.ips.len(), 2, "解析结果要如实带回，供 UI 展示");
}

#[tokio::test]
async fn resolution_happens_at_most_once_across_many_ip_rules() {
    // 第二轮从头重扫，会再次经过多条 IP 规则 —— 但解析只能发生一次
    let set = rs(
        &[
            "IP-CIDR,10.0.0.0/8,DIRECT",
            "IP-CIDR,172.16.0.0/12,DIRECT",
            "GEOIP,CN,DIRECT",
            "MATCH,PROXY",
        ],
        &["PROXY"],
    );
    let r = StubResolver::new().with("a.com", &["8.8.8.8"]);
    let out = decide(&set, &domain("a.com", 443), &geo_stub(), &r).await;

    assert_eq!(out.decision, Decision::Outbound("PROXY".into()));
    assert_eq!(r.asked().len(), 1, "跨多条 IP 规则也只许解析一次：{:?}", r.asked());
}

// ── 纪律③：解析失败绝不阻断连接 ────────────────────────────

#[tokio::test]
async fn resolution_failure_is_treated_as_no_match_not_as_an_error() {
    // 未注入 = 解析返回空 = 超时/NXDOMAIN/上游不可达
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let r = StubResolver::new();
    let out = decide(&set, &domain("unreachable.example", 443), &geo_stub(), &r).await;

    assert_eq!(
        out.decision,
        Decision::Outbound("PROXY".into()),
        "解析失败必须继续往下走到 MATCH，而不是阻断连接"
    );
    assert!(out.resolved, "确实尝试过解析");
    assert!(out.ips.is_empty());
}

#[tokio::test]
async fn resolution_failure_still_reaches_a_reject_verdict_if_thats_the_fallback() {
    // 兜底是 REJECT 时也一样：判决照常产出，只是内容是拒绝
    let set = rs(&["GEOIP,CN,DIRECT", "MATCH,REJECT"], &[]);
    let r = StubResolver::new();
    let out = decide(&set, &domain("nowhere.example", 443), &geo_stub(), &r).await;

    assert_eq!(out.decision, Decision::Reject);
    assert!(out.resolved);
}

#[tokio::test]
async fn geo_unavailable_plus_resolution_failure_still_decides() {
    // GEO 文件读不到 + 解析也失败：两种故障叠加，仍必须给出判决
    let set = rs(
        &["GEOSITE,cn,DIRECT", "GEOIP,CN,DIRECT", "MATCH,PROXY"],
        &["PROXY"],
    );
    let r = StubResolver::new();
    let out = decide(&set, &domain("baidu.com", 443), &geo_stub(), &r).await;

    assert_eq!(out.decision, Decision::Outbound("PROXY".into()));
}

// ── 首命中顺序语义在跨越解析后依然成立 ──────────────────────

#[tokio::test]
async fn earlier_ip_rule_wins_over_later_one_after_resolution() {
    let set = rs(
        &[
            "IP-CIDR,8.8.8.0/24,REJECT",
            "IP-CIDR,8.0.0.0/8,DIRECT",
            "MATCH,PROXY",
        ],
        &["PROXY"],
    );
    let r = StubResolver::new().with("dns.example", &["8.8.8.8"]);
    let out = decide(&set, &domain("dns.example", 443), &geo_stub(), &r).await;

    assert_eq!(out.decision, Decision::Reject, "两条都能匹配时取靠前的那条");
}

#[tokio::test]
async fn a_domain_rule_after_the_triggering_ip_rule_still_applies_in_pass_two() {
    // 第二轮从头重扫，靠后的域名规则在第一轮就已试过并未中；
    // 关键是第二轮不能因为「已经解析过」而跳过它
    let set = rs(
        &[
            "IP-CIDR,10.0.0.0/8,DIRECT",
            "DOMAIN-SUFFIX,special.com,REJECT",
            "MATCH,PROXY",
        ],
        &["PROXY"],
    );
    let r = StubResolver::new().with("a.special.com", &["93.184.216.34"]);
    let out = decide(&set, &domain("a.special.com", 443), &geo_stub(), &r).await;

    assert_eq!(out.decision, Decision::Reject);
}

// ── 幂等性 ────────────────────────────────────────────────

#[tokio::test]
async fn the_same_input_yields_the_same_decision_every_time() {
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let r = StubResolver::new().with("a.com", &["10.0.0.9"]);
    let t = domain("a.com", 443);

    let first = decide(&set, &t, &geo_stub(), &r).await;
    let second = decide(&set, &t, &geo_stub(), &r).await;
    assert_eq!(first, second, "同一输入必须得到同一判决");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p wsieve-dns --test routing`
Expected: `cannot find function decide in crate wsieve_dns`

- [ ] **Step 3: 写实现**

`crates/wsieve-dns/src/decide.rs`：

```rust
//! 两阶段求值的驱动方：路由引擎与解析器之间那道缝。
//!
//! 引擎是同步纯函数，解析是 async——设计文档 §4.2 纪律①用「把解析需求作为
//! 返回值抛出」把二者解开。本模块就是那个「接住 NeedResolve、解析、再来
//! 一轮」的调用方，也是整个项目里**唯一**允许调用解析器的判决入口。
//!
//! 集中成一个函数而非散在各入口，为的是让三件事只需要证明一次：
//! 最多解析一次、第二轮永不再抛 NeedResolve、解析失败不阻断连接。

use wsieve_geo::GeoDb;
use wsieve_proto::addr::AddrPort;
use wsieve_route::{Decision, RuleSet, Verdict};

use crate::inject::RoutingResolver;

/// 一次判决的完整记录。UI 的 `rule_test`（设计文档 §11.2）需要知道
/// 「到底解析了没、解析出了什么」，所以这些不能只留在日志里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub decision: Decision,
    /// 是否真的发生了 DNS 查询。多数流量在域名类规则处就命中，此值为 false ——
    /// 这正是两阶段协议「不需要时零 DNS 泄漏」的可观测证据。
    pub resolved: bool,
    /// 解析得到的 IP。`resolved == true` 且此表为空，即解析失败或超时。
    pub ips: Vec<std::net::IpAddr>,
}

/// 走完两阶段协议，返回最终判决。
///
/// - 第一轮 `evaluate(target, None, ..)`；命中即返回，**不发任何 DNS**
/// - 抛出 `NeedResolve` 才解析，然后 `evaluate(target, Some(&ips), ..)`
/// - 解析失败/超时得到空切片，该 IP 规则视为不匹配，流程继续（纪律③）
///
/// **注意判决结果的使用边界（设计文档 §6.3）**：解析出的 IP 只用于「判决」，
/// **绝不改写传给出站的地址**。判决走代理时仍把**域名**递给出站，由服务端
/// 做远程解析 —— 服务端离目标更近，且不受本地污染影响。`Outcome::ips` 存在
/// 只是为了给 UI 展示与直连路径复用，调用方不得拿它替换 `AddrPort`。
pub async fn decide<R: RoutingResolver + ?Sized>(
    rules: &RuleSet,
    target: &AddrPort,
    geo: &GeoDb,
    resolver: &R,
) -> Outcome {
    match rules.evaluate(target, None, geo) {
        Verdict::Decided(decision) => Outcome {
            decision,
            resolved: false,
            ips: Vec::new(),
        },
        Verdict::NeedResolve { domain } => {
            let ips = resolver.resolve(&domain).await;
            match rules.evaluate(target, Some(&ips), geo) {
                Verdict::Decided(decision) => Outcome {
                    decision,
                    resolved: true,
                    ips,
                },
                // 两阶段协议的死线。走到这里说明引擎违约了，静默兜底只会让
                // bug 藏进生产环境——宁可在测试里炸掉。
                Verdict::NeedResolve { domain } => unreachable!(
                    "第二轮不该再请求解析（domain={domain}）——\
                     这是 wsieve-route::evaluate 的协议违约，见设计文档 §4.2 纪律①"
                ),
            }
        }
    }
}
```

**`?Sized` 不是装饰**：加上它，`decide` 才能接受 `&dyn RoutingResolver`。阶段 2 的出站管理器很可能会把解析器装进 `Arc<dyn RoutingResolver>` 以便按配置在真解析器与 stub 之间切换 —— 现在加一个 `?Sized`，比将来改签名便宜。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p wsieve-dns --test routing`
Expected: **13 个测试全部 PASS**。

**特别确认这四条**，它们各锁一条纪律：

| 测试 | 锁住什么 |
|---|---|
| `domain_rule_hit_never_touches_dns` | 零 DNS 泄漏（§4.2 的核心收益） |
| `resolution_happens_at_most_once_across_many_ip_rules` | 最多解析一次 |
| `resolution_failure_is_treated_as_no_match_not_as_an_error` | **纪律③**，本阶段的命脉之一 |
| `no_resolve_flag_suppresses_the_query` | `no-resolve` 逃生舱真的有效 |

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-dns/src/decide.rs crates/wsieve-dns/tests/routing.rs
git commit -m "feat(dns): 两阶段求值驱动方 decide() 与解析注入验收"
```

---

> **Part A 到此结束。** 此时 `cargo test -p wsieve-dns` 应全绿（upstream 12 + resolver 9 + routing 13 = **34**），`wsieve-dns` 可独立使用。

---

## Part B — 接入既有代码

### Task 6: `resolve_upstream` 改走 bootstrap（纪律①）

**Files:**
- Modify: `src-tauri/src/shard.rs`
- Modify: `src-tauri/src/shard_setup.rs`
- Modify: `src-tauri/Cargo.toml`

**这是本阶段最要紧的一处改动，也是纪律①真正落地的地方。**

现状（`shard.rs:90`）用的是 `tokio::net::lookup_host`，即**系统解析器**。今天这没问题 —— 我们还没有自己的 DNS。但阶段 6 一旦让 fake-ip 生效并劫持系统查询，这一行就会拿到 `198.18.x.x`，转发器连向虚空。而且是**静默失败**：表现为「握手一直不成功」，没有任何一条日志会说「因为你解析到了假 IP」。

现在就把它换成显式的 bootstrap 解析器 —— 趁着改动无风险（行为等价），而不是等阶段 6 出了故障再来查。

> **注意：这不是「以后再说」的事。** 本阶段做这件事的成本是改两个函数签名；阶段 6 做的成本是在一堆 TUN 症状里定位一个静默的 DNS 污染。

- [ ] **Step 1: 加依赖**

在 `src-tauri/Cargo.toml` 的 `[dependencies]` 里，`wsieve-socks5` 那一行之后加：

```toml
wsieve-dns = { path = "../crates/wsieve-dns" }
```

- [ ] **Step 2: 改 `resolve_upstream`**

`src-tauri/src/shard.rs` 里把整个 `resolve_upstream` 替换为：

```rust
/// 解析真实服务端地址。
///
/// **必须走 bootstrap 解析器，绝不能走我们自己的 DNS**（设计文档 §7.2 纪律①）。
/// 也**必须在写 hosts 之前调用**：hosts 一旦把域名指向 127.0.0.1，解析器
/// 就会返回本地地址，转发器再解析就指向自己，形成死循环。
///
/// 两道防线针对的是两件不同的事，缺一不可：
/// - 「先解析后写 hosts」防的是**我们自己**写进 hosts 的那一行
/// - 「走 bootstrap」防的是阶段 6 的 **fake-ip**：届时系统 DNS 查询会被
///   劫持并返回 198.18.x.x，转发器会连向虚空，且完全静默
pub async fn resolve_upstream(
    boot: &hickory_resolver::TokioResolver,
    host: &str,
    port: u16,
) -> anyhow::Result<SocketAddr> {
    let lookup = boot
        .lookup_ip(host)
        .await
        .map_err(|e| anyhow::anyhow!("bootstrap 解析 {host} 失败: {e}"))?;
    let addrs: Vec<SocketAddr> = lookup.iter().map(|ip| SocketAddr::new(ip, port)).collect();
    // 优先 IPv4：hosts 里我们只写 127.0.0.1，链路两端保持同族更少意外。
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.first())
        .copied()
        .ok_or_else(|| anyhow::anyhow!("{host}:{port} 解析不到地址"))
}
```

在 `shard.rs` 顶部的 `use` 区补一行（`hickory_resolver` 经 `wsieve-dns` 传递引入，此处直接用全路径，故实际无需新增 use —— 若嫌全路径冗长可加 `use hickory_resolver::TokioResolver;` 并相应简化签名）。

**IPv4 优先的逻辑一字未改**，只是数据来源从 `lookup_host` 换成了 bootstrap 解析器。这是刻意的：本 Task 只换解析来源，不顺手改行为。

- [ ] **Step 3: 改调用方**

`src-tauri/src/shard_setup.rs` 里，把第 127 行附近的调用改为传入 bootstrap 解析器。在函数开头（`clear_managed` 之后、解析之前）建立它：

```rust
    // 2) 再解析真实地址（此刻系统解析器已不受我们污染）。
    //    走 bootstrap 解析器而非系统 lookup_host —— 阶段 6 的 fake-ip 生效后，
    //    系统查询会被劫持成 198.18.x.x（设计文档 §7.2 纪律①）。
    let boot = match wsieve_dns::bootstrap() {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("条带禁用：bootstrap 解析器建立失败（{e}）——退回单会话");
            return ShardPlan::degraded(server_url);
        }
    };
    let upstream = match shard::resolve_upstream(&boot, &host, port).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("条带禁用：解析 {host}:{port} 失败（{e}）——退回单会话");
            return ShardPlan::degraded(server_url);
        }
    };
```

**注意保持既有的三步顺序不变**（`clear_managed` → 解析 → 起转发器 → 写 hosts）。设计文档 §8.3.2 把这个顺序钉死了，任何一步颠倒都会静默失败。

- [ ] **Step 4: 验证编译与既有测试不回归**

Run:

```bash
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
```

Expected: 编译通过，`shard.rs` / `shard_setup.rs` 的既有测试全绿。

**手工验证 bootstrap 解析确实工作**（行为应与改动前完全一致）：

```bash
cargo run -p wsieve-dns --example bootstrap_probe 2>/dev/null || cat <<'NOTE'
可选：临时写一个 example 打印 resolve_upstream 的结果，确认
  - 能解析出真实公网 IP（不是 127.0.0.1、不是 198.18.x.x）
  - IPv4 优先仍然生效
计划评审时实测：example.com:443 → 172.66.147.243:443 (ipv4 preferred: true)
NOTE
```

- [ ] **Step 5: 提交**

```bash
git add src-tauri/Cargo.toml src-tauri/src/shard.rs src-tauri/src/shard_setup.rs
git commit -m "refactor(shard): resolve_upstream 改走 bootstrap 解析器

纪律①（spec §7.2）：出站服务器域名绝不走自家 DNS。今天行为等价，
但阶段 6 fake-ip 生效后，系统解析会返回 198.18.x.x，转发器连向虚空
且完全静默。趁改动无风险时先换掉。"
```

---

### Task 7: 收尾 —— 全量测试与 clippy

**Files:** 无新增

- [ ] **Step 1: 跑整个 workspace**

Run: `cargo test --workspace`
Expected: 全绿。阶段 1 的 geo / route / config 测试与既有的 proto / transport / xhttp / mux / socks5 测试均不受影响。

- [ ] **Step 2: 跑 clippy**

Run: `cargo clippy -p wsieve-dns --all-targets -- -D warnings`
Expected: 无警告。

> **为什么不跑 `--workspace`**：与阶段 1 计划 Task 15 Step 4 同理 —— 仓库既有代码有 3 处与本阶段无关的 clippy 报错（`wsieve-proto/src/stripe.rs:178` 的 `type_complexity`、`hello.rs:10-11` 的文档缩进）。想顺手清掉是好事，但**单独提交**。

- [ ] **Step 3: 确认 src-tauri 也没坏**

Run: `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`
Expected: 无**新增**警告（`src-tauri` 是独立 workspace，不在根 workspace 的 `--workspace` 范围内）。

- [ ] **Step 4: 回填设计文档**

本阶段有两处实测结论值得回填 spec，免得日后重新踩：

1. **§7.3 的「超时」一条**补一句：硬超时必须由调用方用 `tokio::time::timeout` 施加，`hickory` 的 `ResolverOpts::timeout` 只约束池内单轮，实测黑洞上游可达设定值的 30 倍
2. **§7.2 纪律①**补一句：除 fake-ip 外，**系统 hosts 文件**也是污染源 —— 我们自己就往里写劫持行，判决用的解析器必须设 `use_hosts_file = Never`

```bash
# 编辑 docs/superpowers/specs/2026-08-24-client-routing-and-ui-design.md 后
git add docs/superpowers/specs/2026-08-24-client-routing-and-ui-design.md
git commit -m "docs(spec): §7.2/§7.3 回填阶段 3 的两处实测结论"
```

- [ ] **Step 5: 提交**

```bash
git commit --allow-empty -m "chore(dns): 阶段 3 收尾，34 个测试全绿"
```

---

## 阶段 3 完成标准

全部勾选后本阶段才算完成：

- [ ] `cargo test --workspace` 全绿
- [ ] `cargo test -p wsieve-dns` 全绿，**34 个测试**（upstream 12 + resolver 9 + routing 13）
- [ ] `cargo clippy -p wsieve-dns --all-targets -- -D warnings` 无警告
- [ ] `blackhole_upstream_returns_empty_within_the_hard_timeout` 通过 —— **本阶段的命脉**。若它耗时接近 15s，硬超时没生效，不要放宽断言
- [ ] Task 2 Step 5 的 DoH 实证跑过，三家 DoH 服务商的 IP 字面量证书都能通过校验
- [ ] `live_cache_makes_the_second_lookup_dramatically_faster` 的实测数字记进了提交信息
- [ ] `src-tauri` 编译通过且既有测试不回归
- [ ] spec §7.2 / §7.3 已回填两处实测结论

## 三条纪律的验收对照

实现完成后逐条核对 —— 这是本阶段存在的全部理由：

| 纪律 | 怎么验 |
|---|---|
| **① 出站域名走独立 bootstrap** | `bootstrap_is_a_separate_instance_from_the_routing_resolver` 通过；且 `shard.rs::resolve_upstream` 的签名里**收的是 `&TokioResolver`**，不再有任何路径能让它走 `DnsResolver` |
| **② DoH 上游必须 IP 字面量** | `doh_with_hostname_is_rejected_naming_the_host` 通过；且 `DnsResolver::new` 与 `bootstrap_with` 都会因域名上游而**构造失败**，不是运行时才报 |
| **③ 超时/失败 = 不匹配，继续往下** | `resolution_failure_is_treated_as_no_match_not_as_an_error` 通过；且 `lookup_for_routing` 的签名是 `-> Vec<IpAddr>`，**没有 `Result`**，调用方无从写出阻断逻辑 |

## 交给阶段 2 / 阶段 6 的接口

阶段 2 的入口层拿到 `AddrPort` 之后，整条判决链路就是一行：

```rust
let cfg = wsieve_config::load_file(&path)?;
cfg.validate()?;
let rules = wsieve_route::RuleSet::build(
    &cfg.rules.iter().map(|r| r.value.clone()).collect::<Vec<_>>(),
    cfg.mode.parse()?,
    &cfg.global_outbound,
    &cfg.outbound_names(),
)?;
let geo = wsieve_geo::GeoDb::new(geo_dir.join("geoip.dat"), geo_dir.join("geosite.dat"));
let dns = wsieve_dns::DnsResolver::new(
    &cfg.dns.nameserver,
    std::time::Duration::from_millis(cfg.dns.timeout_ms),
    cfg.dns.cache.max as u64,
    std::time::Duration::from_secs(cfg.dns.cache.negative_ttl_s),
)?;

// 每条入站连接：
let outcome = wsieve_dns::decide(&rules, &target, &geo, &dns).await;
match outcome.decision {
    Decision::Outbound(name) => {
        // §6.3：仍把**域名**递给出站，由服务端远程解析。
        // 绝不用 outcome.ips 替换 target —— 那会丢掉远程解析的抗污染能力。
        dispatch_to(&name, target).await
    }
    // 直连可复用刚解析出的 IP，省一次重复查询（§6.3）
    Decision::Direct => connect_direct(target, &outcome.ips).await,
    Decision::Reject => reject(),
}
```

**UI 的 `rule_test(target, resolve: bool)`（§11.2）** 也复用这套：`resolve: false` 时只跑 `rules.evaluate(target, None, &geo)` 看是否返回 `NeedResolve`；`resolve: true` 时调 `decide()`。`Outcome::{resolved, ips}` 正是 UI 需要回显的「是否需要解析 / 解析到了什么」。

**已知的未尽事项**（不在本阶段，别顺手做）：

- **fake-ip 与对外 DNS 服务器** —— 设计文档 §7.1 第二层，阶段 6 随 TUN 一起做。本阶段连 `dns.listen` / `dns.enhanced_mode` / `dns.fake_ip_range` 三个配置字段都不读
- **`dns.proxy-server-nameserver` 的完整语义** —— 本阶段实现了 `bootstrap()`（`system`）与 `bootstrap_with()`（显式 IP）两条路径，但**没有**接配置字段做分发。阶段 2 接入出站管理器时补上那几行
- **DNS 查询走代理** —— §7.2 纪律③提到「其余查询默认走代理，代理未就绪时排队等待」。本阶段的解析器直连上游。这条要等阶段 2 的出站管理器就位后才有意义
- **解析结果推给 UI** —— §11.2 的事件流，阶段 4

## 附：hickory-resolver 0.26 API 速查

踩过的坑集中在这里，实现时对照着写可以省一轮编译失败：

| 要做的事 | 正确写法 | 错误写法 / 陷阱 |
|---|---|---|
| 拿 tokio provider | `hickory_resolver::net::runtime::TokioRuntimeProvider` | ~~`name_server::TokioConnectionProvider`~~ 在 0.26 是**私有模块**（官方文档注释仍在用，照抄必挂） |
| 建解析器 | `Resolver::builder_with_config(cfg, provider).with_options(opts).build()?` | `build()` 返回 `Result`，不是直接给 `Resolver` |
| 系统配置解析器 | `Resolver::builder_tokio()?.build()?` | 需要 `system-config` feature |
| 配 DoH | `ProtocolConfig::Https { server_name: Arc<str>, path: Arc<str> }` | `path` 是 `Arc<str>` **不是 `Option`**；`ConnectionConfig::https()` 才收 `Option` |
| 覆盖端口 | `ConnectionConfig::new(proto)` 后改 `.port` 字段 | 结构体带 `#[non_exhaustive]`，但 `port` 是 `pub`，**外部 crate 可赋值** |
| 设选项 | `ResolverOpts::default()` 后逐字段赋值 | 带 `#[non_exhaustive]`，**不能用结构体字面量** |
| 缓存容量 | `opts.cache_size: u64`（默认 8192） | — |
| 负缓存 | `opts.negative_min_ttl: Option<Duration>` | 默认 `None` = 下限 0，服务端给 0 就等于没有负缓存 |
| 不读 hosts | `opts.use_hosts_file = ResolveHosts::Never` | 默认是 `Auto`，**会读** —— 而我们自己往 hosts 里写劫持行 |
| 硬超时 | 外层 `tokio::time::timeout(d, fut)` | `opts.timeout` **不是**墙钟上限，实测可超 30 倍 |
| 取解析结果 | `lookup.iter()` 产出 `IpAddr` | 需要类型标注：`.collect::<Vec<IpAddr>>()` |
| 判断 NXDOMAIN | `NetError::Dns(DnsError::NoRecordsFound(_))` | 从 `hickory_resolver::net` 导出 |
| feature 组合 | `https-ring` 自动开 `__https` → `__tls`，DoT 一并可用 | 不需要再单独加 `tls-ring` |

