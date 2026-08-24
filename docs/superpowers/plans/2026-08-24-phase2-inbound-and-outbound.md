# 阶段 2：入口层与出站管理 — 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把单服务器、无分流的客户端改造成「多出站并存 + 规则分流」的形态：混合端口入口、`proxy.rs` 参数化为多出站管理器、单 WebView 承载全部出站，并落地设计文档 §9.4 的四条恢复路径优化与 §10 的外部状态托管抽象。

**Architecture:** 入口层统一产出 `(AddrPort, 双向流)` 后交给阶段 1 的 `RuleSet::evaluate()` 判决，判决结果决定走哪个出站实例、直连还是拒绝。出站管理器为每个启用的出站维持一份独立的会话循环，全部共享同一个隐藏 WebView（各自用绝对 URL）。hosts 与系统代理统一到 `ManagedSystemState` trait 之下，崩溃残留由启动时的 `clear_stale` 兜底。

**Tech Stack:** Rust 2021 · Tauri 2 · 阶段 1 的 `wsieve-config` / `wsieve-route` / `wsieve-geo` · 既有的 `wsieve-mux` / `wsieve-xhttp` / `wsieve-socks5`

**依据:** `docs/superpowers/specs/2026-08-24-client-routing-and-ui-design.md` §6.4 §8.1 §8.2 §9 §10 §14

---

## 前置阅读

- **§9.1** — 单 WebView 承载多出站，及其成立前提（§3.2：sid 走 query 不走 cookie）
- **§9.3 / §9.4** — 连接生命周期与四条要落地的优化
- **§9.5** — 两条**明确不做**的优化及理由。不要好心加上
- **§6.4** — 出站不可用时拒绝而非静默回退。这是隐私防线，不是可用性折衷
- **§10** — 外部状态托管
- **§14** — 本阶段第一件事必须是 spike

**既有代码**：`src-tauri/src/proxy.rs`（重构主体）、`shard.rs`（预建 TCP）、`hosts.rs`（托管范本）、`bridge.rs`（`with_base` 已就绪）。

**房规**：注释用中文；刻意简化用 `ponytail:` 标注上限与升级路径；错误绝不静默跳过（范本：`shard.rs:214` 的 `port_conflict_is_reported_not_skipped`）。

---

## 文件结构

```
crates/wsieve-inbound/             新建 —— 混合端口与 HTTP 代理
  Cargo.toml
  src/lib.rs                       Inbound trait 与统一接口
  src/sniff.rs                     首字节协议嗅探
  src/http.rs                      HTTP CONNECT 与普通转发
  tests/sniff.rs

src-tauri/src/
  custody/mod.rs                   新建 —— ManagedSystemState trait
  custody/hosts.rs                 由现有 hosts.rs 移入并实现 trait
  custody/sysproxy.rs              新建 —— 系统代理托管
  outbound/mod.rs                  新建 —— 出站管理器
  outbound/instance.rs             新建 —— 单个出站的会话循环（proxy.rs 的主体迁入）
  outbound/carrier.rs              新建 —— WebView 承载器（shared / isolated）
  router.rs                        新建 —— 入口判决与分派
  proxy.rs                         删除（内容拆入 outbound/）
  hosts.rs                         删除（移入 custody/）
  shard.rs                         修改：预建 TCP + 多域名分段

crates/wsieve-server/src/lib.rs    修改：CORS 放宽（Task 1）
```

---

## Part A — spike（阻塞后续全部任务）

设计文档 §14 的硬要求：**阶段 2 的第一件事必须是这个 spike**。

`carrier: shared` 是默认值，它成立与否全押在 §3.2 那条推论上——sid 走 query，故跨域名 fetch 不受 ITP 第三方 cookie 拦截影响。推论经代码核实无误，但**从未有一次真实的跨域名会话建立来确证**。

失败的代价可控（§4.2 纪律③让承载方式对出站层透明，退回 `isolated` 只是换个承载器实现），但**必须在出站管理器动工之前知道答案**。

### Task 1: 服务端 CORS 放宽

**Files:**
- Modify: `crates/wsieve-server/src/lib.rs:271-286`（`cors_origin`）

- [ ] **Step 1: 写失败的测试**

在 `crates/wsieve-server/src/lib.rs` 的测试模块中追加（若无测试模块则新建）：

```rust
#[cfg(test)]
mod cors_tests {
    use axum::body::Body;
    use axum::http::{header, Request};

    fn req(origin: &str, host: &str) -> Request<Body> {
        Request::builder()
            .header(header::ORIGIN, origin)
            .header(header::HOST, host)
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn same_domain_different_port_still_allowed() {
        // 多端口条带的既有场景，不能回归
        let r = req("https://a.com:18444", "a.com");
        assert_eq!(super::cors_origin(&r).as_deref(), Some("https://a.com:18444"));
    }

    #[test]
    fn cross_domain_is_now_allowed() {
        // 单 WebView 承载多出站：页面在 A，请求发往 B
        let r = req("https://host-a.com", "server-b.net");
        assert_eq!(super::cors_origin(&r).as_deref(), Some("https://host-a.com"));
    }

    #[test]
    fn missing_origin_yields_none() {
        // 同源请求不带 Origin —— 不该凭空造一个 CORS 头出来
        let r = Request::builder()
            .header(header::HOST, "a.com")
            .body(Body::empty())
            .unwrap();
        assert!(super::cors_origin(&r).is_none());
    }

    #[test]
    fn malformed_origin_yields_none() {
        let r = req("not-a-url", "a.com");
        assert!(super::cors_origin(&r).is_none());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p wsieve-server cors_tests`
Expected: `cross_domain_is_now_allowed` FAIL（当前实现要求同域名）

- [ ] **Step 3: 改实现**

把 `cors_origin` 整个替换为：

```rust
/// 跨源许可：回显请求的 `Origin`。
///
/// **这个函数看起来很宽松，但它不是防线。** 真正的防线在调用点：
/// `apply_cors` 只加在**认证成功**的响应上（§8：未认证请求一律走伪装处理器，
/// 那条路径根本不经过这里）。探测者发不出合法的 msg1，就永远看不到任何
/// CORS 痕迹 —— 无论他把 Origin 构造成什么样。
///
/// 放宽的原因（设计文档 §9.1）：单 WebView 承载多出站时，页面加载自宿主
/// 出站的域名，而请求发往其他出站的域名，二者本就不同域。服务端无从预知
/// 客户端把哪台机器当宿主，因此不能再做同域名判据。
///
/// 历史：此前限制为「Origin 与 Host 同域名」，那是为多端口条带
/// （同域名不同端口）设计的。该场景仍被覆盖 —— 它是本函数的一个特例。
fn cors_origin(req: &axum::http::Request<Body>) -> Option<String> {
    let origin = req.headers().get(header::ORIGIN)?.to_str().ok()?;
    // 仅做最低限度的形态校验：必须是个 scheme://host 形状的东西。
    // 目的不是安全（安全由调用点保证），而是避免把垃圾原样回显进响应头。
    let rest = origin.split("://").nth(1)?;
    let host = rest.split('/').next()?.split(':').next()?;
    (!host.is_empty()).then(|| origin.to_string())
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p wsieve-server`
Expected: 4 个 cors 测试 PASS，既有测试不回归

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-server/src/lib.rs
git commit -m "feat(server): CORS 放宽至任意 Origin（防线仍是「仅认证成功才带头」）"
```

> **同步修订文档**：设计文档 §3.2 的行动项 2、3 现在可以勾掉了 —— 传输 spec §6.7 第 3 条与本函数的文档注释都已随本次改动更新。

---

### Task 2: 跨域名会话建立 spike

**Files:**
- Create: `scripts/spike-cross-origin.sh`

这不是单元测试，是一次**真实的端到端确证**。它要回答一个问题：WKWebView 里，一个加载自域名 A 的页面，能否与域名 B 完成完整的 Noise 握手并传输数据。

- [ ] **Step 1: 读懂现有 E2E 脚本**

Run: `cat scripts/e2e.sh`

看清它如何起服务端、如何生成密钥、如何设置环境变量拉起客户端。spike 脚本要复用同一套做法，只是起**两个**服务端、用**两个**域名。

- [ ] **Step 2: 准备两个本地域名**

```bash
# 需要管理员权限。spike 结束后请手工删除这两行。
sudo sh -c 'printf "127.0.0.1 wsieve-a.test\n127.0.0.1 wsieve-b.test\n" >> /etc/hosts'
grep wsieve /etc/hosts
```

Expected: 两行都在。

> 用 `.test` 是因为它是 RFC 6761 保留给测试的顶级域，永远不会被真实 DNS 解析，不会与任何真实站点冲突。

- [ ] **Step 3: 写 spike 脚本**

```bash
#!/usr/bin/env bash
# 跨域名会话建立 spike（设计文档 §14 / §15 待实测项 #3）。
#
# 问题：单 WebView 承载多出站时，页面加载自域名 A，而向域名 B 发的
# fetch 是跨域名的。它能否完成完整握手？
#
# 这条路成立的前提是 sid 走 query 而非 cookie（设计文档 §3.2）——
# 若 sid 在 cookie 里，WKWebView 的 ITP 会丢掉它，握手必然失败。
#
# 前置：/etc/hosts 里有
#   127.0.0.1 wsieve-a.test
#   127.0.0.1 wsieve-b.test
set -euo pipefail
cd "$(dirname "$0")/.."

PORT_A=18081
PORT_B=18082

echo "== 检查 hosts =="
grep -q "wsieve-a.test" /etc/hosts || { echo "缺 wsieve-a.test，见本脚本头部"; exit 1; }
grep -q "wsieve-b.test" /etc/hosts || { echo "缺 wsieve-b.test，见本脚本头部"; exit 1; }

echo "== 生成两套密钥 =="
# 复用 e2e.sh 的密钥生成方式；具体命令以 e2e.sh 中的写法为准
# （实现时照抄，不要另创一套）

echo "== 起两个服务端 =="
# A 监听 $PORT_A，B 监听 $PORT_B。两者各自的伪装页要能区分，
# 便于确认页面确实来自 A。

echo "== 起客户端，宿主指向 A，额外出站指向 B =="
# WSIEVE_SERVER_URL=http://wsieve-a.test:$PORT_A/
# 额外出站用 B 的绝对 URL（WebViewTransport::with_base）

echo "== 判据 =="
echo "1. 客户端日志出现 B 的会话「connected」→ 跨域名握手成功"
echo "2. 通过 B 的出站发一次请求并拿到响应 → 数据面也通"
echo "3. 服务端 B 的日志里，认证成功的响应带 CORS 头；未认证请求不带"
```

> 脚本骨架给到这里是**有意的**：密钥生成与服务端启动的具体命令必须照抄 `scripts/e2e.sh` 的既有写法，而不是另发明一套。实现时把 `e2e.sh` 的对应片段搬过来填进上面的注释位置。

- [ ] **Step 4: 跑 spike 并记录结论**

```bash
chmod +x scripts/spike-cross-origin.sh
./scripts/spike-cross-origin.sh
```

**三种可能的结果，分别怎么办：**

| 结果 | 行动 |
|---|---|
| ✅ 三条判据全过 | `carrier: shared` 成立。继续 Part B，本阶段按计划推进 |
| ❌ 握手失败且日志显示 CORS 被拒 | 检查 Task 1 是否真的生效、`apply_cors` 是否被调用。这是**配置问题**，可修 |
| ❌ 握手失败且原因不是 CORS | **停下来**。把 `carrier` 的默认值改为 `isolated`，并在设计文档 §9.1 与 §15 记录实测结论。后续 Task 10 改为实现 isolated 承载。**不要硬推 shared** |

- [ ] **Step 5: 清理并提交**

```bash
sudo sed -i.bak '/wsieve-[ab]\.test/d' /etc/hosts
git add scripts/spike-cross-origin.sh
git commit -m "test(spike): 跨域名会话建立验证脚本"
```

把结论（含日志片段）写进提交信息。这是设计文档 §15 待实测项 #3 的交付物。

---

## Part B — 外部状态托管

设计文档 §10：hosts、系统代理、TUN 路由三者共性明确——**改了系统全局状态，进程崩溃后必须能恢复**。项目已在 `hosts.rs` 解决过一次，现在把它提升成接口，三处共用一套纪律与一套测试。

### Task 3: `ManagedSystemState` trait

**Files:**
- Create: `src-tauri/src/custody/mod.rs`

- [ ] **Step 1: 写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Default)]
    struct Spy {
        applied: AtomicUsize,
        reverted: AtomicUsize,
    }

    impl ManagedSystemState for Arc<Spy> {
        fn name(&self) -> &'static str {
            "spy"
        }
        fn apply(&self) -> anyhow::Result<()> {
            self.applied.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn revert(&self) -> anyhow::Result<()> {
            self.reverted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn clear_stale(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn guard_reverts_on_drop() {
        let spy = Arc::new(Spy::default());
        {
            let _g = CustodyGuard::acquire(spy.clone()).unwrap();
            assert_eq!(spy.applied.load(Ordering::SeqCst), 1);
            assert_eq!(spy.reverted.load(Ordering::SeqCst), 0);
        }
        assert_eq!(spy.reverted.load(Ordering::SeqCst), 1, "drop 必须恢复");
    }

    #[test]
    fn acquire_clears_stale_before_applying() {
        // 顺序错了就会：先写新条目，再被 clear_stale 抹掉
        #[derive(Default)]
        struct OrderSpy {
            log: std::sync::Mutex<Vec<&'static str>>,
        }
        impl ManagedSystemState for Arc<OrderSpy> {
            fn name(&self) -> &'static str {
                "order"
            }
            fn apply(&self) -> anyhow::Result<()> {
                self.log.lock().unwrap().push("apply");
                Ok(())
            }
            fn revert(&self) -> anyhow::Result<()> {
                self.log.lock().unwrap().push("revert");
                Ok(())
            }
            fn clear_stale(&self) -> anyhow::Result<()> {
                self.log.lock().unwrap().push("clear_stale");
                Ok(())
            }
        }
        let spy = Arc::new(OrderSpy::default());
        drop(CustodyGuard::acquire(spy.clone()).unwrap());
        assert_eq!(
            *spy.log.lock().unwrap(),
            vec!["clear_stale", "apply", "revert"]
        );
    }

    #[test]
    fn failed_apply_does_not_leave_a_guard() {
        struct Failing;
        impl ManagedSystemState for Failing {
            fn name(&self) -> &'static str {
                "failing"
            }
            fn apply(&self) -> anyhow::Result<()> {
                anyhow::bail!("故意失败")
            }
            fn revert(&self) -> anyhow::Result<()> {
                panic!("apply 失败后不该调用 revert")
            }
            fn clear_stale(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }
        assert!(CustodyGuard::acquire(Failing).is_err());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p wsieve-app custody::`
Expected: `cannot find trait ManagedSystemState`

- [ ] **Step 3: 写实现**

```rust
//! 外部系统状态的托管（设计文档 §10）。
//!
//! hosts、系统代理、TUN 路由三者共性明确：**改了系统全局状态，
//! 进程崩溃后必须能恢复**。三处共用这一套纪律与一套测试。
//!
//! 崩溃路径靠 `clear_stale` 兜底 —— 它在任何 `apply` 之前调用，
//! 负责清掉上一次运行留下的残留。这是 hosts.rs 里已经验证过的做法
//! （不清残留的话，域名会一直指向一个已经不在跑的转发器，
//! 本机之后访问该域名全部失败）。

pub mod hosts;
pub mod sysproxy;

/// 一项被托管的系统状态。
///
/// 实现者必须保证 `apply` 与 `revert` **幂等** —— 重复调用不产生额外效果。
/// 崩溃、SIGKILL、拔电源都可能让 `revert` 根本没机会跑，所以正确性
/// 不能依赖它一定被调用；`clear_stale` 才是最后一道防线。
pub trait ManagedSystemState {
    /// 用于日志与错误信息的人类可读名字。
    fn name(&self) -> &'static str;

    /// 写入系统状态。
    fn apply(&self) -> anyhow::Result<()>;

    /// 恢复系统状态。必须幂等：没 apply 过也能安全调用。
    fn revert(&self) -> anyhow::Result<()>;

    /// 清理上一次运行的残留。在任何 `apply` 之前调用。
    fn clear_stale(&self) -> anyhow::Result<()>;
}

/// 持有即生效，drop 即恢复。
///
/// 注意 `acquire` 的顺序：**先 clear_stale，再 apply**。反过来的话，
/// 刚写好的条目会被紧接着的清理抹掉。
pub struct CustodyGuard<T: ManagedSystemState> {
    inner: T,
}

impl<T: ManagedSystemState> CustodyGuard<T> {
    pub fn acquire(inner: T) -> anyhow::Result<Self> {
        inner
            .clear_stale()
            .with_context_name(inner.name(), "清理残留")?;
        inner.apply().with_context_name(inner.name(), "写入")?;
        Ok(Self { inner })
    }

    pub fn get(&self) -> &T {
        &self.inner
    }
}

impl<T: ManagedSystemState> Drop for CustodyGuard<T> {
    fn drop(&mut self) {
        // drop 里不能 ? —— 失败只能记日志。这也是 clear_stale 必须存在的原因。
        if let Err(e) = self.inner.revert() {
            tracing::warn!("恢复 {} 失败：{e:#}", self.inner.name());
        } else {
            tracing::info!("已恢复 {}", self.inner.name());
        }
    }
}

/// 给错误加上「哪一项托管、在哪个环节」的上下文。
trait WithContextName<T> {
    fn with_context_name(self, name: &str, stage: &str) -> anyhow::Result<T>;
}

impl<T> WithContextName<T> for anyhow::Result<T> {
    fn with_context_name(self, name: &str, stage: &str) -> anyhow::Result<T> {
        use anyhow::Context;
        self.with_context(|| format!("{name}：{stage}失败"))
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p wsieve-app custody::`
Expected: 3 个测试全部 PASS。**`acquire_clears_stale_before_applying` 是这组的核心** —— 顺序错了是静默失败。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/custody/mod.rs
git commit -m "feat(custody): ManagedSystemState trait 与 CustodyGuard"
```

---

### Task 4: hosts 迁入托管

**Files:**
- Create: `src-tauri/src/custody/hosts.rs`（由 `src-tauri/src/hosts.rs` 移入）
- Delete: `src-tauri/src/hosts.rs`
- Modify: `src-tauri/src/main.rs`、`src-tauri/src/shard_setup.rs`（改引用路径）

现有 `hosts.rs` 的逻辑**一行都不用改** —— 它本来就是这个模式的范本。只是包一层。

- [ ] **Step 1: 移动文件并保留全部既有测试**

```bash
git mv src-tauri/src/hosts.rs src-tauri/src/custody/hosts.rs
```

Run: `cargo test -p wsieve-app hosts::`
Expected: 移动后既有测试仍全部 PASS（改 `mod` 引用即可）

- [ ] **Step 2: 写新托管类型的失败测试**

在 `custody/hosts.rs` 末尾追加：

```rust
#[cfg(test)]
mod custody_tests {
    use super::*;
    use crate::custody::{CustodyGuard, ManagedSystemState};

    fn temp_hosts(content: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("wsieve-hosts-{}", std::process::id()));
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn guard_writes_all_domains_and_removes_them_on_drop() {
        let p = temp_hosts("127.0.0.1 localhost\n");
        let custody = HostsCustody::new(
            HostsFile::new(&p),
            "127.0.0.1".into(),
            vec!["a.com".into(), "b.net".into()],
        );
        {
            let _g = CustodyGuard::acquire(custody).unwrap();
            let s = std::fs::read_to_string(&p).unwrap();
            assert!(s.contains("a.com"), "多域名要一次写入：{s}");
            assert!(s.contains("b.net"));
            assert!(s.contains("localhost"), "用户原有条目不能动");
        }
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(!s.contains("a.com"), "drop 后必须摘干净：{s}");
        assert!(s.contains("localhost"), "用户条目仍在");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn stale_entries_from_a_crash_are_cleared_on_acquire() {
        // 模拟上次崩溃：hosts 里留着托管条目
        let p = temp_hosts("127.0.0.1 localhost\n127.0.0.1 ghost.com # wsieve-managed\n");
        let custody = HostsCustody::new(HostsFile::new(&p), "127.0.0.1".into(), vec!["a.com".into()]);
        let _g = CustodyGuard::acquire(custody).unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(!s.contains("ghost.com"), "上次的残留必须被清掉：{s}");
        assert!(s.contains("a.com"));
        std::fs::remove_file(&p).ok();
    }
}
```

- [ ] **Step 3: 实现 `HostsCustody`**

在 `custody/hosts.rs` 末尾（测试模块之前）追加：

```rust
/// hosts 条目的托管封装。
///
/// 底下的 `HostsFile` 逻辑一行未改 —— 它本来就是 §10 这个模式的范本，
/// 这里只是把它接进统一接口，好让系统代理与 TUN 路由共用同一套纪律。
pub struct HostsCustody {
    file: HostsFile,
    ip: String,
    hosts: Vec<String>,
}

impl HostsCustody {
    pub fn new(file: HostsFile, ip: String, hosts: Vec<String>) -> Self {
        Self { file, ip, hosts }
    }

    pub fn writable(&self) -> bool {
        self.file.writable()
    }
}

impl crate::custody::ManagedSystemState for HostsCustody {
    fn name(&self) -> &'static str {
        "hosts 条目"
    }

    fn apply(&self) -> anyhow::Result<()> {
        self.file.set_managed(&self.ip, &self.hosts)?;
        Ok(())
    }

    fn revert(&self) -> anyhow::Result<()> {
        self.file.clear_managed()?;
        Ok(())
    }

    fn clear_stale(&self) -> anyhow::Result<()> {
        // 与 revert 同一动作：摘掉所有带 marker 的行。
        // 崩溃路径与正常退出路径共用这一条，正是它幂等的价值。
        self.file.clear_managed()?;
        Ok(())
    }
}
```

- [ ] **Step 4: 运行测试**

Run: `cargo test -p wsieve-app hosts`
Expected: 既有测试 + 2 个新测试全部 PASS

- [ ] **Step 5: 提交**

```bash
git add -A src-tauri/src/
git commit -m "refactor(custody): hosts 迁入 ManagedSystemState 托管"
```

---

### Task 5: 系统代理托管

**Files:**
- Create: `src-tauri/src/custody/sysproxy.rs`

设计文档 §8.2：崩溃残留与 hosts 是同一类问题——进程崩了而系统代理还指着已停止的端口，**用户整机断网**。

- [ ] **Step 1: 写失败的测试**

平台命令无法在单测里真跑，所以把「决定执行什么命令」与「执行」分开，测前者。

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_apply_sets_both_socks_and_http_for_each_service() {
        let cmds = macos_apply_commands(&["Wi-Fi".into(), "Ethernet".into()], "127.0.0.1", 7890);
        // 每个网络服务都要设 socks + http + https，共 3 条
        assert_eq!(cmds.len(), 6, "两个服务 × 3 条命令");
        assert!(cmds.iter().any(|c| c.contains("-setsocksfirewallproxy") && c.contains("Wi-Fi")));
        assert!(cmds.iter().any(|c| c.contains("-setwebproxy") && c.contains("Ethernet")));
        assert!(cmds.iter().all(|c| c.contains("7890")));
    }

    #[test]
    fn macos_revert_turns_every_proxy_off() {
        let cmds = macos_revert_commands(&["Wi-Fi".into()]);
        assert_eq!(cmds.len(), 3);
        assert!(cmds.iter().all(|c| c.contains("off")));
    }

    #[test]
    fn service_names_with_spaces_are_quoted() {
        // "Wi-Fi" 没空格，但 "Thunderbolt Bridge" 有 —— 不引号会被拆成两个参数
        let cmds = macos_apply_commands(&["Thunderbolt Bridge".into()], "127.0.0.1", 7890);
        assert!(
            cmds.iter().all(|c| c.contains("\"Thunderbolt Bridge\"")),
            "含空格的服务名必须加引号：{cmds:?}"
        );
    }

    #[test]
    fn empty_service_list_is_an_error_not_a_silent_noop() {
        // 枚举不到任何网络服务时，静默成功会让用户以为代理已生效
        assert!(SysProxyCustody::new("127.0.0.1".into(), 7890, vec![]).is_err());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p wsieve-app sysproxy::`
Expected: `cannot find function macos_apply_commands`

- [ ] **Step 3: 写实现**

```rust
//! 系统代理设置的托管（设计文档 §8.2）。
//!
//! 崩溃残留与 hosts 是同一类问题，而且后果更重：进程崩了而系统代理
//! 还指着一个已经不在跑的端口，**用户整机断网**，且他多半不知道
//! 是这个程序干的。所以 clear_stale 不是可选项。
//!
//! ponytail: 目前只实现 macOS（networksetup）。上限：Windows / Linux
//! 上 system-proxy 开关不生效，UI 需灰掉并提示手工设置。
//! 升级路径：Windows 写 HKCU\...\Internet Settings 并广播
//! WM_SETTINGCHANGE；Linux 走 gsettings（仅 GNOME 系）。

use crate::custody::ManagedSystemState;

pub struct SysProxyCustody {
    host: String,
    port: u16,
    services: Vec<String>,
}

impl SysProxyCustody {
    /// `services` 为空即报错 —— 枚举不到网络服务时静默成功，
    /// 会让用户以为代理已生效（照 shard.rs:214 的房规：报错不静默跳过）。
    pub fn new(host: String, port: u16, services: Vec<String>) -> anyhow::Result<Self> {
        if services.is_empty() {
            anyhow::bail!("枚举不到任何网络服务，无法设置系统代理");
        }
        Ok(Self { host, port, services })
    }

    /// 枚举当前机器的网络服务名。
    #[cfg(target_os = "macos")]
    pub fn enumerate_services() -> anyhow::Result<Vec<String>> {
        let out = std::process::Command::new("networksetup")
            .arg("-listallnetworkservices")
            .output()?;
        if !out.status.success() {
            anyhow::bail!("networksetup -listallnetworkservices 失败");
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .skip(1) // 首行是说明文字
            // 带 * 前缀的是已禁用的服务，跳过
            .filter(|l| !l.starts_with('*') && !l.trim().is_empty())
            .map(|l| l.trim().to_string())
            .collect())
    }

    #[cfg(not(target_os = "macos"))]
    pub fn enumerate_services() -> anyhow::Result<Vec<String>> {
        anyhow::bail!("当前平台尚未实现系统代理设置")
    }
}

/// 生成 apply 阶段要执行的命令（每条是完整的 shell 命令行）。
/// 拆出来是为了可测 —— 平台命令没法在单测里真跑。
fn macos_apply_commands(services: &[String], host: &str, port: u16) -> Vec<String> {
    let mut cmds = Vec::with_capacity(services.len() * 3);
    for s in services {
        let q = quote(s);
        cmds.push(format!("networksetup -setwebproxy {q} {host} {port}"));
        cmds.push(format!("networksetup -setsecurewebproxy {q} {host} {port}"));
        cmds.push(format!("networksetup -setsocksfirewallproxy {q} {host} {port}"));
    }
    cmds
}

fn macos_revert_commands(services: &[String]) -> Vec<String> {
    let mut cmds = Vec::with_capacity(services.len() * 3);
    for s in services {
        let q = quote(s);
        cmds.push(format!("networksetup -setwebproxystate {q} off"));
        cmds.push(format!("networksetup -setsecurewebproxystate {q} off"));
        cmds.push(format!("networksetup -setsocksfirewallproxystate {q} off"));
    }
    cmds
}

/// 网络服务名可能含空格（"Thunderbolt Bridge"），不加引号会被拆成两个参数。
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\\\""))
}

fn run_all(cmds: &[String]) -> anyhow::Result<()> {
    for c in cmds {
        let st = std::process::Command::new("sh").arg("-c").arg(c).status()?;
        if !st.success() {
            anyhow::bail!("执行失败：{c}");
        }
    }
    Ok(())
}

impl ManagedSystemState for SysProxyCustody {
    fn name(&self) -> &'static str {
        "系统代理设置"
    }

    fn apply(&self) -> anyhow::Result<()> {
        run_all(&macos_apply_commands(&self.services, &self.host, self.port))
    }

    fn revert(&self) -> anyhow::Result<()> {
        run_all(&macos_revert_commands(&self.services))
    }

    fn clear_stale(&self) -> anyhow::Result<()> {
        // 与 revert 同一动作。上次崩溃留下的代理设置在这里被关掉。
        //
        // 注意这会连带关掉**用户自己设的**代理 —— 这是刻意的取舍：
        // 分不清「我们留下的」和「用户设的」时，宁可关掉。开着一个
        // 指向死端口的代理会让用户整机断网，而关掉最多是让他重设一次。
        self.revert()
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p wsieve-app sysproxy::`
Expected: 4 个测试全部 PASS

- [ ] **Step 5: 手工验证一次真实生效（macOS）**

```bash
# 查看当前设置
networksetup -getsocksfirewallproxy "Wi-Fi"
```

写一个临时 example 或用 `cargo test -- --ignored` 的方式真跑一次 apply + revert，然后再查一遍，确认恢复到了原状。**这一步不能省** —— 单测只验证了命令文本，没验证命令真的有效。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/custody/sysproxy.rs
git commit -m "feat(custody): 系统代理托管（macOS networksetup）"
```

---

> **Part A / B 到此结束。** spike 有结论、托管抽象就位、hosts 与系统代理都接入。

---

## Part C — 混合入口

设计文档 §8.1：三种入口统一产出 `(AddrPort, 双向流)`，下游对入口类型无感。本阶段做前两种（混合端口），TUN 是阶段 6 的事——但接口现在就要留对。

### Task 6: crate 骨架与协议嗅探

**Files:**
- Create: `crates/wsieve-inbound/Cargo.toml`
- Create: `crates/wsieve-inbound/src/lib.rs`
- Create: `crates/wsieve-inbound/src/sniff.rs`
- Modify: `Cargo.toml`

- [ ] **Step 1: 建 crate**

```toml
[package]
name = "wsieve-inbound"
version = "0.1.0"
edition = "2021"
description = "混合入口：同一端口上嗅探 SOCKS5 与 HTTP"

[dependencies]
wsieve-proto = { path = "../wsieve-proto" }
wsieve-socks5 = { path = "../wsieve-socks5" }
tokio = { workspace = true }
futures = { workspace = true }
thiserror = { workspace = true }
tracing = "0.1"

[dev-dependencies]
tokio = { workspace = true, features = ["test-util"] }
```

根 `Cargo.toml` 的 members 加 `"crates/wsieve-inbound"`。

- [ ] **Step 2: 写失败的测试**

`src/sniff.rs`：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::{TcpListener, TcpStream};

    /// 起一个监听器，把 `payload` 写进去，返回服务端侧的连接。
    async fn conn_with(payload: &[u8]) -> TcpStream {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let payload = payload.to_vec();
        tokio::spawn(async move {
            let mut c = TcpStream::connect(addr).await.unwrap();
            c.write_all(&payload).await.unwrap();
            // 保持连接开着，否则 peek 可能读到 EOF
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        });
        l.accept().await.unwrap().0
    }

    #[tokio::test]
    async fn socks5_greeting_is_detected() {
        // SOCKS5 握手首字节是版本号 0x05
        let s = conn_with(&[0x05, 0x01, 0x00]).await;
        assert_eq!(sniff(&s).await.unwrap(), Protocol::Socks5);
    }

    #[tokio::test]
    async fn http_verbs_are_detected() {
        for verb in ["GET / HTTP/1.1\r\n", "CONNECT a.com:443 HTTP/1.1\r\n", "POST /x HTTP/1.1\r\n"] {
            let s = conn_with(verb.as_bytes()).await;
            assert_eq!(sniff(&s).await.unwrap(), Protocol::Http, "verb: {verb}");
        }
    }

    #[tokio::test]
    async fn socks4_is_rejected_explicitly() {
        // 不静默当成 HTTP —— 那会产生一个莫名其妙的 400
        let s = conn_with(&[0x04, 0x01]).await;
        assert!(matches!(sniff(&s).await, Err(SniffError::UnsupportedSocks4)));
    }

    #[tokio::test]
    async fn garbage_is_rejected() {
        let s = conn_with(&[0xFF, 0xFE]).await;
        assert!(sniff(&s).await.is_err());
    }

    #[tokio::test]
    async fn peek_does_not_consume() {
        // 嗅探之后，真正的处理器必须还能读到完整的首字节
        let s = conn_with(&[0x05, 0x01, 0x00]).await;
        assert_eq!(sniff(&s).await.unwrap(), Protocol::Socks5);
        let mut buf = [0u8; 3];
        use tokio::io::AsyncReadExt;
        let mut s = s;
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, [0x05, 0x01, 0x00], "peek 不能消耗数据");
    }
}
```

- [ ] **Step 3: 运行测试确认失败**

Run: `cargo test -p wsieve-inbound --lib sniff::`
Expected: `cannot find function sniff`

- [ ] **Step 4: 写实现**

```rust
//! 首字节协议嗅探。
//!
//! 关键是用 `peek()` 而非 `read()` —— peek 不消耗数据，
//! 于是分派之后，真正的处理器还能读到完整的协议首字节，
//! 不需要把「已经读掉的那一段」再拼回去。

use tokio::net::TcpStream;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Socks5,
    Http,
}

#[derive(Debug, thiserror::Error)]
pub enum SniffError {
    #[error("读取首字节失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("连接未发送任何数据即关闭")]
    Empty,
    #[error("SOCKS4 不受支持，请使用 SOCKS5")]
    UnsupportedSocks4,
    #[error("无法识别的协议，首字节为 {0:#04x}")]
    Unknown(u8),
}

pub async fn sniff(stream: &TcpStream) -> Result<Protocol, SniffError> {
    let mut b = [0u8; 1];
    let n = stream.peek(&mut b).await?;
    if n == 0 {
        return Err(SniffError::Empty);
    }
    match b[0] {
        0x05 => Ok(Protocol::Socks5),
        0x04 => Err(SniffError::UnsupportedSocks4),
        // HTTP 方法名一律是 ASCII 大写字母开头：GET / POST / CONNECT / PUT / …
        c if c.is_ascii_uppercase() => Ok(Protocol::Http),
        other => Err(SniffError::Unknown(other)),
    }
}
```

- [ ] **Step 5: 运行测试并提交**

Run: `cargo test -p wsieve-inbound --lib sniff::`
Expected: 5 个测试 PASS。**`peek_does_not_consume` 是这组的核心**。

```bash
git add Cargo.toml crates/wsieve-inbound/
git commit -m "feat(inbound): 首字节协议嗅探"
```

---

### Task 7: HTTP 代理

**Files:**
- Create: `crates/wsieve-inbound/src/http.rs`

- [ ] **Step 1: 写失败的测试**

只测**解析**部分——它是全部逻辑所在，而 IO 转发是样板。

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_target_is_parsed() {
        let r = parse_request_line("CONNECT example.com:443 HTTP/1.1").unwrap();
        assert!(matches!(r, HttpRequest::Connect { ref host, port: 443 } if host == "example.com"));
    }

    #[test]
    fn connect_without_port_defaults_to_443() {
        let r = parse_request_line("CONNECT example.com HTTP/1.1").unwrap();
        assert!(matches!(r, HttpRequest::Connect { port: 443, .. }));
    }

    #[test]
    fn absolute_uri_is_split_into_target_and_origin_form() {
        let r = parse_request_line("GET http://example.com/a/b?c=1 HTTP/1.1").unwrap();
        match r {
            HttpRequest::Plain { host, port, rewritten } => {
                assert_eq!(host, "example.com");
                assert_eq!(port, 80, "http 默认 80");
                assert_eq!(rewritten, "GET /a/b?c=1 HTTP/1.1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn absolute_uri_with_explicit_port() {
        let r = parse_request_line("GET http://example.com:8080/x HTTP/1.1").unwrap();
        assert!(matches!(r, HttpRequest::Plain { port: 8080, .. }));
    }

    #[test]
    fn root_path_becomes_slash_not_empty() {
        // http://example.com → 路径是 "/"，不是空串（空串会让上游 400）
        let r = parse_request_line("GET http://example.com HTTP/1.1").unwrap();
        match r {
            HttpRequest::Plain { rewritten, .. } => assert_eq!(rewritten, "GET / HTTP/1.1"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn origin_form_without_absolute_uri_is_rejected() {
        // 直连服务器的请求形态，不是代理请求 —— 明确报错好过转发到虚空
        assert!(parse_request_line("GET /a/b HTTP/1.1").is_err());
    }

    #[test]
    fn https_absolute_uri_defaults_to_443() {
        let r = parse_request_line("GET https://example.com/x HTTP/1.1").unwrap();
        assert!(matches!(r, HttpRequest::Plain { port: 443, .. }));
    }

    #[test]
    fn malformed_lines_are_rejected() {
        assert!(parse_request_line("").is_err());
        assert!(parse_request_line("GET").is_err());
        assert!(parse_request_line("CONNECT").is_err());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p wsieve-inbound --lib http::`

- [ ] **Step 3: 写实现**

```rust
//! HTTP 代理入口：CONNECT 隧道与普通请求转发。
//!
//! ponytail: 普通 HTTP 请求（非 CONNECT）只处理连接上的**第一个**请求，
//! 并在转发时注入 `Connection: close`。
//! 上限：代理侧不支持 keep-alive 复用，每个普通 HTTP 请求一条连接。
//! 为什么可接受：现代工具对代理几乎一律走 CONNECT（https 是默认），
//! 普通 HTTP 主要来自 curl/apt 这类简单场景，连接开销可忽略。
//! 升级路径：要支持 keep-alive 就得完整解析每一轮请求（后续请求
//! 仍是绝对 URI 形式），届时引入 httparse 而不是手写。

#[derive(Debug, PartialEq, Eq)]
pub enum HttpRequest {
    Connect {
        host: String,
        port: u16,
    },
    Plain {
        host: String,
        port: u16,
        /// 请求行已改写为 origin-form（上游服务器要的形态）
        rewritten: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("请求行格式非法：{0}")]
    BadRequestLine(String),
    #[error("不是代理请求：请求行使用了 origin-form，代理需要绝对 URI")]
    NotAProxyRequest,
    #[error("非法端口：{0}")]
    BadPort(String),
}

pub fn parse_request_line(line: &str) -> Result<HttpRequest, HttpError> {
    let mut parts = line.split_whitespace();
    let method = parts.next().ok_or_else(|| HttpError::BadRequestLine(line.into()))?;
    let uri = parts.next().ok_or_else(|| HttpError::BadRequestLine(line.into()))?;
    let version = parts.next().unwrap_or("HTTP/1.1");

    if method.eq_ignore_ascii_case("CONNECT") {
        // CONNECT 的目标形如 host:port，端口省略时默认 443
        let (host, port) = split_host_port(uri, 443)?;
        return Ok(HttpRequest::Connect { host, port });
    }

    // 其余方法必须是绝对 URI（代理请求的形态）
    let (scheme, rest) = uri
        .split_once("://")
        .ok_or(HttpError::NotAProxyRequest)?;
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "https" => 443,
        _ => 80,
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"), // 没有路径时补 "/"，空串会让上游 400
    };
    let (host, port) = split_host_port(authority, default_port)?;
    Ok(HttpRequest::Plain {
        host,
        port,
        rewritten: format!("{method} {path} {version}"),
    })
}

fn split_host_port(s: &str, default: u16) -> Result<(String, u16), HttpError> {
    // IPv6 字面量形如 [::1]:8080
    if let Some(rest) = s.strip_prefix('[') {
        let (h, tail) = rest
            .split_once(']')
            .ok_or_else(|| HttpError::BadRequestLine(s.into()))?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| HttpError::BadPort(p.into()))?,
            None => default,
        };
        return Ok((h.to_string(), port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => {
            Ok((h.to_string(), p.parse().map_err(|_| HttpError::BadPort(p.into()))?))
        }
        _ => {
            if s.is_empty() {
                return Err(HttpError::BadRequestLine(s.into()));
            }
            Ok((s.to_string(), default))
        }
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p wsieve-inbound --lib http::`
Expected: 8 个测试 PASS

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-inbound/src/http.rs
git commit -m "feat(inbound): HTTP 代理请求行解析（CONNECT 与绝对 URI）"
```

---

### Task 8: 统一入口接口与 serve 循环

**Files:**
- Modify: `crates/wsieve-inbound/src/lib.rs`

- [ ] **Step 1: 定义接口**

```rust
//! 混合入口：同一端口上同时接受 SOCKS5 与 HTTP 代理请求。
//!
//! 设计文档 §8.1。三种入口（混合端口 / 系统代理 / TUN）统一产出
//! `(AddrPort, 双向流)`，下游对入口类型无感 —— TUN 因此是纯增量，
//! 加一个入口不动其余任何一层。

pub mod http;
pub mod sniff;

use std::future::Future;
use std::pin::Pin;

use wsieve_proto::addr::AddrPort;

/// 入口把每条连接交给它，由调用方决定去哪。
///
/// 返回的 DuplexStream 是「已经连上目标」的双向管道；入口负责把它
/// 与客户端连接对接。判决为拒绝时返回 Err，入口据此给客户端一个
/// 合乎协议的失败响应（SOCKS5 回复码 / HTTP 502）。
pub type Dispatch = std::sync::Arc<
    dyn Fn(AddrPort) -> Pin<Box<dyn Future<Output = std::io::Result<tokio::io::DuplexStream>> + Send>>
        + Send
        + Sync,
>;
```

- [ ] **Step 2: 写 serve 循环的失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// 一个把目标地址回显给调用方的 dispatch，便于断言解析结果。
    fn echo_dispatch(
        seen: std::sync::Arc<tokio::sync::Mutex<Vec<String>>>,
    ) -> Dispatch {
        std::sync::Arc::new(move |target: AddrPort| {
            let seen = seen.clone();
            Box::pin(async move {
                seen.lock().await.push(target.display());
                let (a, mut b) = tokio::io::duplex(4096);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 1024];
                    while let Ok(n) = b.read(&mut buf).await {
                        if n == 0 || b.write_all(&buf[..n]).await.is_err() {
                            return;
                        }
                    }
                });
                Ok(a)
            })
        })
    }

    #[tokio::test]
    async fn http_connect_reaches_dispatch_with_right_target() {
        let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(serve(l, echo_dispatch(seen.clone())));

        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n").await.unwrap();
        let mut buf = [0u8; 12];
        c.read_exact(&mut buf).await.unwrap();
        assert!(buf.starts_with(b"HTTP/1.1 200"), "应回 200 建立隧道");
        assert_eq!(seen.lock().await.as_slice(), &["example.com:443".to_string()]);
    }

    #[tokio::test]
    async fn socks5_reaches_dispatch_with_right_target() {
        let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(serve(l, echo_dispatch(seen.clone())));

        let mut c = TcpStream::connect(addr).await.unwrap();
        // 无认证握手
        c.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut b = [0u8; 2];
        c.read_exact(&mut b).await.unwrap();
        assert_eq!(b, [0x05, 0x00]);
        // CONNECT example.com:443
        let mut req = vec![0x05, 0x01, 0x00, 0x03, 11];
        req.extend_from_slice(b"example.com");
        req.extend_from_slice(&443u16.to_be_bytes());
        c.write_all(&req).await.unwrap();
        let mut resp = [0u8; 10];
        c.read_exact(&mut resp).await.unwrap();
        assert_eq!(resp[1], 0x00, "应回成功");
        assert_eq!(seen.lock().await.as_slice(), &["example.com:443".to_string()]);
    }

    #[tokio::test]
    async fn rejected_target_yields_protocol_correct_failure() {
        // dispatch 返回 Err（规则判决 REJECT）时，两种协议都要给出
        // 合乎自己规范的失败响应，而不是直接断开
        let d: Dispatch = std::sync::Arc::new(|_| {
            Box::pin(async { Err(std::io::Error::other("rejected")) })
        });
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(serve(l, d));

        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(b"CONNECT a.com:443 HTTP/1.1\r\n\r\n").await.unwrap();
        let mut buf = [0u8; 12];
        c.read_exact(&mut buf).await.unwrap();
        assert!(buf.starts_with(b"HTTP/1.1 502"), "拒绝应回 502");
    }
}
```

- [ ] **Step 3: 实现 serve**

在 `lib.rs` 追加。核心是嗅探后分派到两个既有处理器：

```rust
use tokio::net::TcpListener;

pub async fn serve(listener: TcpListener, dispatch: Dispatch) -> std::io::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let dispatch = dispatch.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, dispatch).await {
                // 单条连接失败不能拖垮监听循环
                tracing::debug!("来自 {peer} 的连接处理结束：{e}");
            }
        });
    }
}

async fn handle(stream: tokio::net::TcpStream, dispatch: Dispatch) -> anyhow::Result<()> {
    match sniff::sniff(&stream).await? {
        sniff::Protocol::Socks5 => {
            // 复用既有的 wsieve-socks5：它的 handler 签名与 Dispatch 同形
            wsieve_socks5::serve_conn(stream, move |t| dispatch(t)).await?;
        }
        sniff::Protocol::Http => http::serve_conn(stream, dispatch).await?,
    }
    Ok(())
}
```

> **实现提示**：`wsieve-socks5` 现在只暴露 `serve(listener, handler)`（整个监听循环）。本任务需要**单连接**版本 `serve_conn(stream, handler)`。做法是把现有 `serve` 里 accept 之后的那段抽成公开函数，`serve` 自己再调它——既有测试因此不受影响。先做这个抽取，再写 `http::serve_conn`。

`http::serve_conn` 的职责：读到 `\r\n\r\n` 为止的头部 → `parse_request_line` → 调 dispatch → CONNECT 回 `HTTP/1.1 200 Connection Established\r\n\r\n` 后双向 copy；普通请求把改写后的请求行 + 原头部（注入 `Connection: close`）写给上游后双向 copy；dispatch 返回 Err 时回 `HTTP/1.1 502 Bad Gateway\r\n\r\n`。

- [ ] **Step 4: 运行测试**

Run: `cargo test -p wsieve-inbound`
Expected: 全绿。**`rejected_target_yields_protocol_correct_failure` 对应设计文档 §6.4** —— 拒绝要让客户端明确知道，不是静默断开。

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-inbound/ crates/wsieve-socks5/
git commit -m "feat(inbound): 混合端口 serve 循环与协议分派"
```

---

## Part D — 多出站管理器

`proxy.rs` 从「一个会话循环」变成「每出站一份」。这是本阶段改动最大的部分。

**一个关键的现有事实**：多端口条带里，多个 `WebViewTransport` **已经共享同一个 `TransportCore`**（`proxy.rs:177` 的 `t2.set_core(core.clone())`），request_id 由 core 统一分配。多出站沿用同一模式——**core 对应一个 WebView，不对应一个会话**。

由此得到干净的两级生命周期：

| 层级 | 死亡含义 | 处置 |
|---|---|---|
| **core / WebView** | 页面崩了、心跳停摆 | reload 页面 → **全部出站**重建 |
| **单个出站会话** | 握手失败、下行流断、服务端 GC | **只重建这一个**，不碰 core，不 reload |

后者正是设计文档 §9.4 优化①。

### Task 9: 出站实例与会话循环

**Files:**
- Create: `src-tauri/src/outbound/instance.rs`（`proxy.rs` 的会话循环主体迁入）
- Create: `src-tauri/src/outbound/mod.rs`

- [ ] **Step 1: 先读懂要迁移的代码**

Run: `sed -n '96,227p' src-tauri/src/proxy.rs`

看清 `run()` 的五个阶段：等心跳 → 死亡监视 → 握手+mux → 供 SOCKS5 → 拆解重来。迁移时保持这个骨架，只把「单份」变成「每出站一份」。

- [ ] **Step 2: 写失败的测试**

会话循环依赖真实 WebView，无法单测。可测的是**状态机与退避**：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps_at_30s() {
        let mut b = Backoff::new();
        assert_eq!(b.next(), Duration::from_millis(100));
        assert_eq!(b.next(), Duration::from_millis(200));
        assert_eq!(b.next(), Duration::from_millis(400));
        for _ in 0..20 {
            b.next();
        }
        assert_eq!(b.next(), Duration::from_secs(30), "必须封顶");
    }

    #[test]
    fn success_resets_backoff() {
        let mut b = Backoff::new();
        b.next();
        b.next();
        b.reset();
        assert_eq!(b.next(), Duration::from_millis(100));
    }

    #[test]
    fn state_transitions_are_observable() {
        let s = OutboundState::default();
        assert_eq!(s.get(), Status::Stopped);
        s.set(Status::Connecting);
        assert_eq!(s.get(), Status::Connecting);
        s.set(Status::Connected { sessions: 4 });
        assert!(matches!(s.get(), Status::Connected { sessions: 4 }));
    }

    #[test]
    fn dialer_is_absent_while_not_connected() {
        // 出站不可用时必须拿不到 dialer —— 这是 §6.4「拒绝而非静默回退」
        // 在数据结构层面的保证
        let inst = OutboundInstance::new_for_test("测试节点");
        assert!(inst.dialer().is_none());
    }
}
```

- [ ] **Step 3: 实现**

`outbound/instance.rs` 的骨架（会话循环主体从 `proxy.rs:96-227` 迁入，改动点已标注）：

```rust
//! 单个出站的会话循环。
//!
//! 由 proxy.rs 的 run() 迁入并参数化。与原版的三处差异：
//!
//! 1. **不再无条件 reload 页面**（设计文档 §9.4 优化①）。
//!    会话死亡分两种：core 死了（页面问题）才 reload，仅本出站会话死
//!    则复用现有页面直接重新握手 —— 省掉恢复路径上最贵的一段。
//! 2. transport 的 base URL 来自本出站配置，而非全局。
//! 3. 状态变化推给 UI（Status），不再只写日志。

use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Stopped,
    Connecting,
    Connected { sessions: usize },
    Retrying { after: Duration },
    Failed { reason: String },
}

impl Default for Status {
    fn default() -> Self {
        Status::Stopped
    }
}

/// 指数退避：100ms 起，翻倍，30s 封顶，成功即重置。
/// 与原 proxy.rs 的行为一致，只是抽成可测的类型。
pub struct Backoff {
    cur: Duration,
}

impl Backoff {
    pub fn new() -> Self {
        Self { cur: Duration::from_millis(100) }
    }
    pub fn next(&mut self) -> Duration {
        let d = self.cur;
        self.cur = (self.cur * 2).min(Duration::from_secs(30));
        d
    }
    pub fn reset(&mut self) {
        self.cur = Duration::from_millis(100);
    }
}
```

`OutboundInstance` 持有：出站配置、`Arc<RwLock<Option<Arc<StripeDialer>>>>`、`OutboundState`、以及一个「请求重启」的 `Notify`。它的 `run(core: Arc<TransportCore>, ...)` 就是迁移过来的循环，但**第 4 步的 reload 改为条件执行**：

```rust
        // 4. 拆干净。是否 reload 取决于 core 是否还活着（优化①）
        if core.is_dead() {
            // 页面/WebView 出问题了：reload 会让全部出站一起重建，
            // 由管理器统一处理，这里只负责退出本循环
            emit(&app, &self.name, Status::Failed { reason: "承载页面已失效".into() });
            return;
        }
        // core 还活着 ⇒ emitter 健在 ⇒ 直接重新握手，不碰页面。
        // 这省掉了 WebView 冷启动 + 完整页面加载 —— 恢复路径上最贵的两步。
        self.mark_session_dead().await;
```

- [ ] **Step 4: 运行测试**

Run: `cargo test -p wsieve-app outbound::`
Expected: 4 个测试 PASS

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/outbound/
git commit -m "feat(outbound): 出站实例与会话循环（含优化①：会话死不 reload 页面）"
```

---

### Task 10: WebView 承载器

**Files:**
- Create: `src-tauri/src/outbound/carrier.rs`

设计文档 §4.2 纪律③：承载方式对出站层透明。出站只声明「我要一个 transport」。

- [ ] **Step 1: 写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_carrier_gives_host_relative_path_and_others_absolute() {
        let c = CarrierPlan::shared(
            "日本节点",
            &[("日本节点", "https://a.com/"), ("新加坡", "https://b.net/")],
        )
        .unwrap();
        // 宿主用相对路径 → 完全同源
        assert_eq!(c.base_for("日本节点"), Some(None));
        // 其余用绝对 URL → 跨域名，靠 CORS 放宽（Task 1）
        assert_eq!(c.base_for("新加坡"), Some(Some("https://b.net".to_string())));
    }

    #[test]
    fn host_defaults_to_first_enabled_when_unspecified() {
        let c = CarrierPlan::shared("", &[("A", "https://a.com/"), ("B", "https://b.net/")]).unwrap();
        assert_eq!(c.host_name(), "A");
    }

    #[test]
    fn unknown_host_name_is_an_error() {
        let e = CarrierPlan::shared("幽灵", &[("A", "https://a.com/")]).unwrap_err().to_string();
        assert!(e.contains("幽灵"), "{e}");
    }

    #[test]
    fn isolated_carrier_gives_every_outbound_its_own_window() {
        let c = CarrierPlan::isolated(&[("A", "https://a.com/"), ("B", "https://b.net/")]).unwrap();
        assert_eq!(c.window_label("A"), Some("wsieve-transport-A".to_string()));
        assert_eq!(c.window_label("B"), Some("wsieve-transport-B".to_string()));
        // 各自加载自己的页面，全部同源，都用相对路径
        assert_eq!(c.base_for("A"), Some(None));
        assert_eq!(c.base_for("B"), Some(None));
    }

    #[test]
    fn empty_outbound_list_is_an_error() {
        assert!(CarrierPlan::shared("", &[]).is_err());
    }
}
```

- [ ] **Step 2-3: 实现**

`CarrierPlan` 是**纯数据决策**（可单测），实际建窗与建 transport 由管理器按它执行。两种模式：

- `shared`：一个 WebView 加载宿主出站的页面；宿主 `base = None`（相对路径，完全同源），其余 `base = Some(origin)`（跨域名）
- `isolated`：每出站一个隐藏窗口，标签 `wsieve-transport-{name}`，各自加载自己的页面，全部 `base = None`

> 若 Task 2 的 spike **失败**，把 `shared` 的构造函数改为直接返回错误并在日志里说明，让配置的 `carrier: shared` 自动降级到 `isolated`。设计文档 §9.1 已把这条降级路径写在案上。

- [ ] **Step 4-5: 测试并提交**

Run: `cargo test -p wsieve-app carrier::` → 5 个 PASS

```bash
git commit -m "feat(outbound): WebView 承载器（shared / isolated 两种计划）"
```

---

### Task 11: 路由分派

**Files:**
- Create: `src-tauri/src/router.rs`

把阶段 1 的判决接进入口与出站之间。这是设计文档 §6.4 落地的地方。

- [ ] **Step 1: 写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn decision_reject_fails_immediately() {
        let r = test_router(&["DOMAIN,blocked.com,REJECT", "MATCH,DIRECT"]);
        let e = r.dispatch(domain("blocked.com", 443)).await.unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::ConnectionRefused);
    }

    #[tokio::test]
    async fn unavailable_outbound_refuses_never_falls_back() {
        // §6.4 的隐私防线：出站不可用时拒绝，绝不静默走直连
        let r = test_router_with_stopped_outbound(&["MATCH,日本节点"]);
        let e = r.dispatch(domain("a.com", 443)).await.unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::NotConnected);
        assert!(
            e.to_string().contains("日本节点"),
            "错误要点名是哪个出站出了问题：{e}"
        );
    }

    #[tokio::test]
    async fn starting_outbound_waits_then_times_out() {
        // 「正在启动中」短暂排队等待，超时转为拒绝 —— 等待不是回退
        let r = test_router_with_starting_outbound(&["MATCH,日本节点"], Duration::from_millis(50));
        let t0 = std::time::Instant::now();
        let e = r.dispatch(domain("a.com", 443)).await.unwrap_err();
        assert!(t0.elapsed() >= Duration::from_millis(50), "要真的等过");
        assert_eq!(e.kind(), std::io::ErrorKind::NotConnected);
    }

    #[tokio::test]
    async fn need_resolve_triggers_second_pass() {
        // 两阶段协议在这里闭环
        let r = test_router_with_resolver(&["GEOIP,CN,DIRECT", "MATCH,日本节点"], &["1.2.3.4"]);
        let d = r.decide(domain("a.com", 443)).await;
        assert!(matches!(d, Decision::Outbound(ref n) if n == "日本节点"));
        assert_eq!(r.resolver_calls(), 1, "只该解析一次");
    }
}
```

- [ ] **Step 2-3: 实现**

`Router` 持有 `RuleSet`、`GeoDb`、出站表、以及（阶段 3 之后的）Resolver。`dispatch` 的流程：

```rust
    pub async fn dispatch(&self, target: AddrPort) -> std::io::Result<DuplexStream> {
        match self.decide(&target).await {
            Decision::Reject => Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("{} 被规则拒绝", target.display()),
            )),
            Decision::Direct => direct_connect(&target).await,
            Decision::Outbound(name) => self.via_outbound(&name, &target).await,
        }
    }
```

`via_outbound` 是 §6.4 的落地点：

```rust
    async fn via_outbound(&self, name: &str, target: &AddrPort) -> std::io::Result<DuplexStream> {
        let Some(inst) = self.outbounds.get(name) else {
            // 加载期已校验过引用，走到这里说明出站被运行时删了
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("出站「{name}」不存在")));
        };

        // 正在启动中 → 短暂排队等待（等待不是回退）
        if matches!(inst.status(), Status::Connecting) {
            let _ = tokio::time::timeout(self.start_wait, inst.wait_connected()).await;
        }

        let Some(dialer) = inst.dialer() else {
            // 绝不回退直连 —— 用户以为在走代理、实际裸奔，
            // 那不是可用性折衷，是隐私事故（设计文档 §6.4）
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                format!("出站「{name}」当前不可用"),
            ));
        };
        // …开子流并桥接，逻辑同 proxy.rs 现有的 socks_handler
    }
```

- [ ] **Step 4-5: 测试并提交**

Run: `cargo test -p wsieve-app router::` → 4 个 PASS

```bash
git commit -m "feat(router): 判决分派（出站不可用时拒绝而非回退）"
```

---

### Task 12: 转发器预建 TCP（优化②）

**Files:**
- Modify: `src-tauri/src/shard.rs`

`shard.rs:78` 当前是懒连接：入站到达后才 `TcpStream::connect(upstream)`。改成预备若干条，用掉即补。省 1 RTT（跨国可达 200ms+）。

**不违反本模块的核心不变量**——它禁的是「多条入站汇聚到一条出站」的**复用**，预建仍严格一进一出。

- [ ] **Step 1: 写失败的测试**

```rust
    #[tokio::test]
    async fn prewarmed_connection_is_used_and_replenished() {
        let accepts = Arc::new(AtomicUsize::new(0));
        let upstream = echo_upstream(accepts.clone()).await;
        let fwd = spawn_prewarmed(39441, 1, upstream, 2).await.unwrap();

        // 预热完成后，上游应已看到 2 条连接，而客户端一条都没发起
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(accepts.load(Ordering::SeqCst), 2, "应预建 2 条");

        // 用掉一条
        let mut c = TcpStream::connect(("127.0.0.1", fwd.ports()[0])).await.unwrap();
        c.write_all(b"hi").await.unwrap();
        let mut b = [0u8; 2];
        c.read_exact(&mut b).await.unwrap();

        // 补回来
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(accepts.load(Ordering::SeqCst), 3, "用掉一条要补一条");
    }

    #[tokio::test]
    async fn each_inbound_still_gets_its_own_upstream_connection() {
        // 核心不变量不能被预建破坏：绝不复用
        let accepts = Arc::new(AtomicUsize::new(0));
        let upstream = echo_upstream(accepts.clone()).await;
        let fwd = spawn_prewarmed(39451, 1, upstream, 2).await.unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        let base = accepts.load(Ordering::SeqCst);

        let mut conns = Vec::new();
        for _ in 0..3 {
            conns.push(TcpStream::connect(("127.0.0.1", fwd.ports()[0])).await.unwrap());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        // 3 条入站 ⇒ 至少 3 条新出站（预建的被消耗 + 补充）
        assert!(accepts.load(Ordering::SeqCst) >= base + 3, "绝不能复用");
    }

    #[tokio::test]
    async fn stale_prewarmed_connection_is_discarded_not_served() {
        // 闲置太久的预建连接可能已被中间设备 RST。取用前必须探活，
        // 否则用户会遇到一次莫名其妙的失败
        // （实现：取用时用 non-blocking read 探测 EOF/错误）
    }
```

- [ ] **Step 2-3: 实现**

给 `Forwarder` 加一个预热池：每个端口一个 `mpsc` 通道，后台任务持续补充到目标数量；`accept_loop` 取用时先从池里拿，拿到的先探活，不健康就丢弃并即时新建。

**必须处理的三件事**（否则预建反而制造问题）：

1. **有效期**：预建连接超过 N 秒未被使用即丢弃重建。中间设备与服务端都可能超时回收
2. **取用探活**：非阻塞读一次，若已 EOF 或出错则丢弃、改为即时新建
3. **数量要少**：默认 1–2 条。多了在服务端看来像端口扫描

`ponytail:` 注释写明：预热数与有效期都是拍脑袋的初值，**待实测**（设计文档 §15 待实测项 #2）。

- [ ] **Step 4-5: 测试并提交**

Run: `cargo test -p wsieve-app shard::`
Expected: 既有 4 个测试不回归 + 3 个新测试 PASS

```bash
git commit -m "feat(shard): 转发器预建 TCP 连接（优化②，一进一出不变量不变）"
```

---

### Task 13: 管理器总装（优化④⑤ + 端口分段）

**Files:**
- Modify: `src-tauri/src/outbound/mod.rs`
- Modify: `src-tauri/src/main.rs`
- Delete: `src-tauri/src/proxy.rs`

- [ ] **Step 1: 写失败的测试**

```rust
    #[test]
    fn ports_are_segmented_without_overlap() {
        // 每出站占 extra_sessions + 1 个端口，依次排开
        let seg = plan_ports(18443, &[("A", 3), ("B", 1), ("C", 0)]);
        assert_eq!(seg["A"], vec![18443, 18444, 18445, 18446]);
        assert_eq!(seg["B"], vec![18447, 18448]);
        assert_eq!(seg["C"], vec![18449]);
    }

    #[test]
    fn port_overflow_is_reported_not_wrapped() {
        // u16 溢出必须报错。悄悄回绕会让两个出站抢同一个端口
        assert!(try_plan_ports(65530, &[("A", 10)]).is_err());
    }

    #[tokio::test]
    async fn all_enabled_outbounds_start_in_parallel() {
        // 优化④：并行拉起，不是一个接一个
        let m = test_manager(&[("A", 100), ("B", 100), ("C", 100)]); // 各需 100ms
        let t0 = std::time::Instant::now();
        m.start_all().await;
        assert!(t0.elapsed() < Duration::from_millis(250), "串行会要 300ms+");
    }

    #[tokio::test]
    async fn enabling_an_outbound_starts_connecting_immediately() {
        // 优化⑤：启用即预连，不等第一个请求
        let m = test_manager(&[("A", 10)]);
        m.set_enabled("A", true).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(matches!(m.status("A"), Some(Status::Connecting) | Some(Status::Connected { .. })));
    }
```

- [ ] **Step 2-3: 实现并改写 main.rs**

`main.rs` 的启动序列改为：

```
读配置 → 校验 → 建 RuleSet / GeoDb
  → CustodyGuard::acquire(hosts)        ← Part B
  → 端口分段 → 起转发器（预建 TCP）      ← Task 12
  → 按 CarrierPlan 建 WebView            ← Task 10
  → 并行拉起全部 enabled 出站            ← 优化④
  → 起混合端口入口                        ← Part C
  → （若配置开启）CustodyGuard::acquire(sysproxy)
```

`RunEvent::Exit` 里按**相反顺序** drop 全部 guard。

删除 `proxy.rs`（内容已拆入 `outbound/`）。

- [ ] **Step 4: 全量验证**

Run: `cargo test --workspace`
Run: `cargo clippy -p wsieve-inbound -p wsieve-app --all-targets -- -D warnings`
Run: `./scripts/e2e.sh`

Expected: 全绿，且 E2E 仍然通过——**这是本阶段最重要的一条**：重构不能打破既有的端到端链路。

- [ ] **Step 5: 提交**

```bash
git add -A
git commit -m "refactor(outbound): proxy.rs 重构为多出站管理器（优化④⑤ + 端口分段）"
```

---

## 阶段 2 完成标准

- [ ] **spike 有明确结论**并记录在案（成功→shared 可用；失败→carrier 默认改 isolated 并回填设计文档 §9.1/§15）
- [ ] `cargo test --workspace` 全绿
- [ ] `cargo clippy -p wsieve-inbound -p wsieve-app --all-targets -- -D warnings` 无警告
- [ ] `./scripts/e2e.sh` 通过（重构未破坏既有链路）
- [ ] 混合端口实测：同一端口上，`curl -x socks5h://127.0.0.1:7890` 与 `curl -x http://127.0.0.1:7890` 都能工作
- [ ] 手工验证系统代理 apply / revert 真实生效并能恢复原状
- [ ] 手工验证「杀掉进程」后重启，hosts 与系统代理的残留都被 `clear_stale` 清掉

## 交给阶段 3 的接口

> **本节已与阶段 3 的实际实现对齐**（`docs/superpowers/plans/2026-08-24-phase3-dns-resolver.md`）。阶段 3 的计划是对着阶段 1 抽出来的真实 `RuleSet::evaluate` 编译验证过的，因此**以它为准**。

阶段 2 在 `Router::decide()` 里自己写两阶段循环，`Verdict::NeedResolve` 分支传 `Some(&[])`（视为解析失败）占位：

```rust
Verdict::NeedResolve { .. } => {
    let ips: Vec<IpAddr> = Vec::new();   // 阶段 2 占位
    match self.rules.evaluate(target, Some(&ips), &self.geo) { … }
}
```

这不是临时凑合：按设计文档 §6.2，解析失败本就该传空切片让流程继续。阶段 2 的行为是「所有 IP 类规则对域名目标都不匹配」，语义正确，只是覆盖面小。

**阶段 3 接入时，不要在 `Router::decide()` 里补一个 `lookup` 调用**——阶段 3 提供了一个已经封装好两阶段协议的泛型函数：

```rust
// 阶段 3 提供（wsieve-dns）
pub async fn decide<R: RoutingResolver + ?Sized>(…) -> Outcome
```

所以 `Router::decide()` 应当**整个委托给它**，而不是各写一套两阶段循环。两处各实现一次的话，「最多解析一次 / 第二轮永不再问 / 失败绝不阻断」这三条就要证明两遍，而其中一处迟早会漏。

两个已修正的细节，实现时注意：

| 阶段 2 原先的写法 | 阶段 3 的实际接口 |
|---|---|
| `resolver.lookup(&domain)` | `resolver.lookup_for_routing(&domain)` |
| `.await.unwrap_or_default()` | **不需要** —— 它返回 `Vec<IpAddr>` 而非 `Result` |

第二条是阶段 3 一个刻意的类型设计：**返回值里没有 `Result`，调用方就无法表达「DNS 失败就阻断连接」**。纪律被编码进类型，而不是写在注释里等人遵守。

## 交给阶段 4 / 5 的数据契约

> 本节由阶段 5 的计划回写（2026-08-25）。UI 的流量视图需要逐流明细，而这些信息**只有 `Router` 知道**——错过这个采集点，前端就再也拿不到了。

`Router::dispatch()` 在建立每条连接时，必须把下列信息一并交给事件聚合器：

| 字段 | 来源 | 为什么现在就要带上 |
|---|---|---|
| `target` | 入口层传入的 `AddrPort` | 桑基图左层的「目标站点」 |
| `rule` | 判决时命中的规则**文本** | 桑基图中层。判决完就丢掉的话，前端无从反推是哪条规则让它走这条路的 |
| `outbound` | `Decision` 的结果 | 桑基图右层，也是色码依据 |
| `bytes` | 连接结束时的双向字节数 | 流带宽度**就是**它 |

**这四样在判决路径上本就全部已知，顺手带出去接近零成本；事后补采集则要么做不到，要么要把判决再跑一遍。**

代价是真实的：阶段 5 的计划记录了，若 `bytes` 拿不到，流量视图只能退化成按**连接数**画流带宽度，并**在 UI 上如实标注**——绝不拿连接数假装字节数，那会让用户读出完全错误的结论（一个下载 1GB 的连接和一个失败握手的连接会画得一样粗）。

另有两处阶段 5 已自行绕开、不需要阶段 2 改动的：

- **没有 `rule_reorder` / `rule_enable` 命令** —— 规则的排序与启停改走 `config_save` 整体写回，由阶段 1 的 `edit.rs` 保证注释不丢。不必为此新增命令
- **`.sr-only` 工具类** —— 阶段 5 的表视图 `<caption>`、桑基图摘要、隐藏 `<th>` 都依赖它，属无障碍硬要求。它**须补在阶段 4 的 `tokens.css`**（token 的唯一定义处），阶段 5 已写明并配了契约测试，缺失即报红
