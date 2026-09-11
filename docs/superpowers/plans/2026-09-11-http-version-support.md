# 服务端 HTTP/1.1 + HTTP/2 + HTTP/3 支持 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `wsieve-server` 在 TCP 443 上宣告并提供 HTTP/1.1 与 HTTP/2，在 UDP 443 上提供 HTTP/3，并通过 `Alt-Svc` 引导客户端升级。

**Architecture:** 两个监听面各自独立：TCP 面沿用现有的 `rustls` + `hyper_util::auto::Builder`（已能 h1/h2 自适应，只缺 ALPN 宣告）；UDP 面新增 `quinn` QUIC endpoint + `h3` 协议栈，并通过一个适配层把 h3 的手动 stream API 接到现有的 axum `Router` 上。两面共用同一份证书，但**必须各自构建 rustls config**（QUIC 对 `max_early_data_size` 有硬约束）。

**Tech Stack:** Rust / axum 0.8 / hyper 1 / rustls 0.23 / quinn 0.11 / h3 0.0.8 / h3-quinn 0.0.10

**Spec:** `docs/superpowers/specs/2026-09-11-http-version-support-design.md`

## Global Constraints

- **TCP 443 必须始终监听。** h3 是叠加能力，任何情况下都不能替代 TCP 面。UDP 绑定失败必须降级为"仅 h1/h2"并继续提供服务，而不是拒绝启动（与 `shard_setup` 的"条带禁用则降级"同源纪律）。
- **下行必须流式。** xhttp 的下行是长流，h3 适配层若先把 response body 缓冲完再发，条带与低延迟会全部失效。`send_data()` 必须在每个 body chunk 到达时调用，不得聚合。
- **两个监听面的 rustls config 不得复制粘贴。** 把 `rustls_config()` 参数化，只让 ALPN 与 `max_early_data_size` 分叉；复制会在将来某次只改一处时产生静默分歧。
- **TCP 面的 `max_early_data_size = 16_384` 保持不变**（§6.8 第 2 层 0-RTT 依赖它）。QUIC 面只能取 `0` 或 `u32::MAX`。
- **不得改动 xhttp 协议本身**。emitter 用标准 fetch，协议版本由 WebKit 自选，客户端预期零改动。
- 所有新增日志沿用现有中文风格与 `tracing`/`eprintln!` 既有用法，不引入新的日志门面。

---

### Task 1: TCP 面宣告 h2（ALPN）

**Files:**
- Modify: `crates/wsieve-server/src/tls.rs:66-77`（`rustls_config`）
- Test: `crates/wsieve-server/src/tls.rs`（同文件 `#[cfg(test)]`）

**Interfaces:**
- Consumes: 无
- Produces: `fn rustls_config(certs, key, alpn: Vec<Vec<u8>>, early_data: u32) -> Result<ServerConfig>` —— Task 3 的 QUIC 面会用同一个函数，传 `vec![b"h3".to_vec()]` 与 `u32::MAX`

- [ ] **Step 1: 写失败测试**

在 `crates/wsieve-server/src/tls.rs` 末尾加（若已有 `mod tests` 则并入）：

```rust
#[cfg(test)]
mod alpn_tests {
    use super::*;

    /// 自签一对证书供测试用（不走文件系统）。
    fn test_cert() -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let (c, k) = self_signed().unwrap();
        (vec![c], k)
    }

    #[test]
    fn tcp_config_advertises_h2_then_http11() {
        // 顺序即服务端偏好：h2 在前表示优先 h2，客户端不支持时回落 http/1.1。
        // 不宣告 ALPN 的 HTTPS 服务端是可被动识别的异常特征——真实世界的
        // HTTPS 服务端几乎全部会协商 h2（设计文档 §5.1）。
        let (certs, key) = test_cert();
        let cfg = rustls_config(
            certs,
            key,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()],
            16_384,
        )
        .unwrap();
        assert_eq!(
            cfg.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()],
            "TCP 面必须按 h2 优先的顺序宣告 ALPN"
        );
        assert_eq!(cfg.max_early_data_size, 16_384, "TCP 面的 0-RTT 配置不得被改动");
    }

    #[test]
    fn quic_config_advertises_only_h3_with_quic_legal_early_data() {
        // QUIC 只接受 0 或 u32::MAX；16384 会让 QuicServerConfig::try_from 失败。
        // 这是两个监听面不能共用一份 config 的根本原因（设计文档 §4.1）。
        let (certs, key) = test_cert();
        let cfg = rustls_config(certs, key, vec![b"h3".to_vec()], u32::MAX).unwrap();
        assert_eq!(cfg.alpn_protocols, vec![b"h3".to_vec()]);
        assert_eq!(cfg.max_early_data_size, u32::MAX);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cargo test -p wsieve-server alpn_tests
```

预期：编译失败，`rustls_config` 只接受 2 个参数。

- [ ] **Step 3: 参数化 `rustls_config`**

把 `crates/wsieve-server/src/tls.rs:66-77` 整体替换为：

```rust
/// 读取 PEM 证书 + 私钥 → rustls ServerConfig（TLS 1.3 only）。
///
/// `alpn` 与 `early_data` 必须由调用方给出，**不设默认值**：两个监听面在这
/// 两项上的取值是互斥的，给默认值等于把其中一面写进函数里，另一面每次都要
/// 记得覆盖——那正是将来只改一处就产生静默分歧的地方（设计文档 §4.1）。
///
///   TCP 443：alpn = ["h2","http/1.1"]，early_data = 16384（§6.8 第 2 层）
///   UDP 443：alpn = ["h3"]，early_data = u32::MAX 或 0
///
/// QUIC 只接受 0 或 u32::MAX；拿 16384 去 `QuicServerConfig::try_from` 会失败。
fn rustls_config(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    alpn: Vec<Vec<u8>>,
    early_data: u32,
) -> Result<ServerConfig> {
    ensure_crypto_provider();
    let mut cfg = ServerConfig::builder_with_protocol_versions(&[&TLS13])
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("证书/私钥加载失败")?;
    cfg.alpn_protocols = alpn;
    // 开 early data 让浏览器 0-RTT；重放面由 msg1 防重放（§6.3）+ seq 去重
    // （§6.4）全额兜底。
    cfg.max_early_data_size = early_data;
    Ok(cfg)
}

/// TCP 监听面的 ALPN：h2 优先，回落 http/1.1。
pub(crate) const ALPN_TCP: [&[u8]; 2] = [b"h2", b"http/1.1"];
/// QUIC 监听面的 ALPN：只有 h3。
pub(crate) const ALPN_QUIC: [&[u8]; 1] = [b"h3"];

fn alpn_vec(items: &[&[u8]]) -> Vec<Vec<u8>> {
    items.iter().map(|s| s.to_vec()).collect()
}
```

- [ ] **Step 4: 更新 `build()` 的三个调用点**

`crates/wsieve-server/src/tls.rs` 的 `build()` 里有两处 `rustls_config(...)` 调用（`CdnFullSelfSigned` 与 `Direct` 分支），各自补上参数：

```rust
rustls_config(vec![cert], key, alpn_vec(&ALPN_TCP), 16_384)?
```

```rust
rustls_config(certs, key, alpn_vec(&ALPN_TCP), 16_384)?
```

- [ ] **Step 5: 运行测试确认通过**

```bash
cargo test -p wsieve-server
```

预期：全绿。

- [ ] **Step 6: 真机验证 ALPN 协商**

部署到测试节点后：

```bash
echo | openssl s_client -connect <host>:443 -servername <host> -alpn h2,http/1.1 2>&1 | grep -i "ALPN"
```

预期：`ALPN protocol: h2`（改动前是 `No ALPN negotiated`）。

```bash
curl --http2 -o /dev/null -w "%{http_version}\n" https://<host>/
```

预期：`2`（改动前是 `1.1`）。

- [ ] **Step 7: 量化连接数与吞吐代价**

这一步**不是可选的**——它产出阶段 2 是否必须做的决策依据（设计文档 §3.1）。

大流量下载进行中，于服务端执行：

```bash
ss -tn state established '( sport = :443 )' | tail -n +2 | wc -l
```

预期：从 **6 降为 1**。记录此前此后的 64MiB 单流吞吐（多轮取中位数，跨境链路抖动可达 ±3 倍，单轮数字不构成证据）。

- [ ] **Step 8: 提交**

```bash
git add crates/wsieve-server/src/tls.rs
git commit -m "feat(server): TCP 面宣告 ALPN h2/http1.1

不协商 ALPN 的 HTTPS 服务端是可被动识别的异常特征。hyper 的 http2
feature 与 auto::Builder 本就具备 h2 处理能力（实测 --http2-prior-knowledge
返回 HTTP/2 200），缺的只是宣告。

顺带把 rustls_config 参数化，为 UDP 面留出 ALPN 与 early_data 的分叉点：
QUIC 只接受 max_early_data_size 为 0 或 u32::MAX，与 TCP 面的 16384 互斥。"
```

---

### Task 2: 让 Alt-Svc 覆盖数据面响应

**⚠️ 现状修正（执行 Task 1 前读代码时发现，原计划此处有误）**

Alt-Svc 基础设施**已经存在**，不需要从零实现：

| 已有 | 位置 |
|---|---|
| `--alt-svc-port N` 命令行参数 | `crates/wsieve-server/src/main.rs:57` |
| `AltSvc` 类型与 `header_value()` | `crates/wsieve-server/src/tls.rs:37-45` |
| 实际加头逻辑 | `crates/wsieve-server/src/lib.rs:204-208` |

但它**只作用于 `disguise_resp`**（伪装页面响应，`lib.rs:176`）。而 xhttp 的数据面响应不经过那条路径。

这是真实缺陷而非设计取舍：承载页自 2026-09-10 起是本机 http 壳，WebView 的数据面请求直接走 xhttp 路径，**从不请求伪装页面**。因此客户端永远看不到 Alt-Svc 头，h3 永远不会被启用——Apple 的网络栈不做推测性 QUIC 尝试（设计文档 §2.3）。

所以本任务是两件事：把加头位置提升到覆盖全部响应；让端口由 h3 的实际绑定结果驱动，而非手工传参。

**Files:**
- Modify: `crates/wsieve-server/src/lib.rs:176-210`（把加头从 `disguise_resp` 提出来）
- Modify: `crates/wsieve-server/src/lib.rs:131`（`router`，挂 layer）
- Test: `crates/wsieve-server/tests/alt_svc.rs`（新建）

**Interfaces:**
- Consumes: `AppState::disguise.alt_svc_port`（已有字段，保持不变）
- Produces: 无新签名。`--alt-svc-port` 的语义从"手工声明"变为"由 Task 4 按 UDP 实际绑定结果填入"，参数本身保留（排障时仍可手工覆盖）。

- [ ] **Step 1: 写失败测试**

新建 `crates/wsieve-server/tests/alt_svc.rs`：

```rust
//! Alt-Svc 必须覆盖**数据面**响应，不能只加在伪装页面上。
//!
//! 承载页自 2026-09-10 起是本机 http 壳，WebView 的数据面请求走 xhttp 路径，
//! 从不请求伪装页面。只给伪装响应加头 = 客户端永远看不到 = h3 永远不启用
//! （Apple 的网络栈不做推测性 QUIC 尝试，设计文档 §2.3）。

use std::sync::Arc;
use tower::ServiceExt;

/// 构造带 alt_svc_port 的 AppState——沿用本 crate 既有测试的构造方式，
/// 不要另起一套。
fn state_with_alt_svc(port: Option<u16>) -> Arc<wsieve_server::AppState> {
    todo!("按 crates/wsieve-server/tests/ 下既有用例构造 AppState，设 disguise.alt_svc_port = port")
}

#[tokio::test]
async fn data_plane_response_carries_alt_svc() {
    // 这条是本任务的全部意义：数据面路径也要带上这个头。
    let app = state_with_alt_svc(Some(443)).router();
    let resp = app
        .oneshot(
            http::Request::builder()
                .method("POST")
                .uri("/")            // xhttp 数据面入口
                .body(axum::body::Body::from(&b"not-a-valid-frame"[..]))
                .unwrap(),
        )
        .await
        .unwrap();
    let v = resp
        .headers()
        .get(http::header::ALT_SVC)
        .expect("数据面响应必须带 Alt-Svc，否则 h3 永远不会被启用")
        .to_str()
        .unwrap();
    assert!(v.contains(r#"h3=":443""#), "实际: {v}");
}

#[tokio::test]
async fn stays_silent_when_h3_unavailable() {
    // 宣告一个连不上的 QUIC 端点，会让客户端每次连接都先试 QUIC 超时
    // 再回落，徒增延迟（设计文档 §6）。
    let app = state_with_alt_svc(None).router();
    let resp = app
        .oneshot(
            http::Request::builder()
                .uri("/")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp.headers().get(http::header::ALT_SVC).is_none());
}
```

- [ ] **Step 2: 运行确认失败**

```bash
cargo test -p wsieve-server --test alt_svc
```

预期：`data_plane_response_carries_alt_svc` 失败（数据面响应没有该头）。

- [ ] **Step 3: 把加头提升为 Router 层**

从 `lib.rs:204-208` 移除 `disguise_resp` 里的加头逻辑（那 5 行），改为在 `router()` 里挂一层，使其覆盖全部响应：

```rust
    pub fn router(self: Arc<Self>) -> Router {
        let alt_svc = self.disguise.alt_svc_port;
        let r = Router::new()
            .route("/", any(fallback))
            .route("/{*rest}", any(fallback))
            // ……保留原有其余 route 与 with_state 链
            ;
        // 加在 Router 层而非 disguise_resp 里：数据面响应不经过伪装路径，
        // 只给伪装响应加头等于客户端永远看不到（见本任务开头的现状修正）。
        match alt_svc.and_then(|p| {
            axum::http::HeaderValue::from_str(&format!("h3=\":{p}\"; ma=86400")).ok()
        }) {
            Some(v) => r.layer(tower_http::set_header::SetResponseHeaderLayer::if_not_present(
                axum::http::header::ALT_SVC,
                v,
            )),
            None => r,
        }
    }
```

`ma=86400` **保持现值不改**：设计文档 §6 曾建议首版取小值（如 600）以免 h3 故障时客户端反复重试，但 86400 是真实世界的普遍取值（nginx 的官方示例即为此值），改小反而成为可被动识别的特征。伪装一致性优先于这点边际延迟——**这是对设计文档 §6 建议的有意偏离，理由记录在此**。

若 `tower-http` 不在依赖里，加：

```toml
tower-http = { version = "0.6", features = ["set-header"] }
```

- [ ] **Step 4: 补全测试里的 `todo!()`**

按 `crates/wsieve-server/tests/` 下既有用例的方式构造 `AppState`，**不得留 `todo!()` 跑过**。

- [ ] **Step 5: 运行测试确认通过**

```bash
cargo test -p wsieve-server
```

- [ ] **Step 6: 提交**

```bash
git add -A crates/wsieve-server
git commit -m "fix(server): Alt-Svc 覆盖数据面响应，不再只加在伪装页面上

承载页自 2026-09-10 起是本机 http 壳，数据面请求走 xhttp 路径、从不请求
伪装页面——只给 disguise_resp 加头等于客户端永远看不到这个头，h3 也就
永远不会被启用（Apple 的网络栈不做推测性 QUIC 尝试）。

改为在 Router 层挂 SetResponseHeaderLayer，覆盖全部响应。ma 保持 86400
不动：真实世界普遍是这个值，改小反成特征。"
```

---
### Task 3: QUIC endpoint 与 h3 → axum 适配层

这是本计划的主体。h3 crate 的 API 是手动 `accept()` 取 `(Request, RequestStream)`，与 hyper/axum 的 `Service` 模型不同，需要一层转换。

**Files:**
- Create: `crates/wsieve-server/src/h3.rs`
- Modify: `crates/wsieve-server/Cargo.toml`（新增依赖）
- Modify: `crates/wsieve-server/src/lib.rs`（`pub mod h3;`）
- Test: `crates/wsieve-server/tests/h3_roundtrip.rs`（新建）

**Interfaces:**
- Consumes: `tls::rustls_config(certs, key, alpn, early_data)`（Task 1 产出）、`AppState::router(Some(port))`（Task 2 产出）
- Produces: `pub async fn serve_h3(certs, key, listen: SocketAddr, router: Router) -> anyhow::Result<()>` —— 绑定 UDP 并进入 accept 循环。**绑定失败以 `Err` 返回**，由 Task 4 的调用方决定降级，本函数自己不做降级决策。

- [ ] **Step 1: 加依赖**

`crates/wsieve-server/Cargo.toml`：

```toml
quinn = "0.11"
h3 = "0.0.8"
h3-quinn = "0.0.10"
bytes = { workspace = true }
futures = { workspace = true }
```

并给 rustls 加 `quic` feature（现有行改为）：

```toml
rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12", "quic"] }
```

- [ ] **Step 2: 写失败测试**

新建 `crates/wsieve-server/tests/h3_roundtrip.rs`：

```rust
//! h3 端到端往返 + 流式下行。
//!
//! 流式那条是硬要求：xhttp 的下行是长流，适配层若先把 response body
//! 缓冲完再发，条带与低延迟全部失效（设计文档 §7.2）。

use std::net::SocketAddr;

#[tokio::test]
async fn h3_round_trip_over_quic() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (certs, key) = wsieve_server::tls::self_signed_pair().unwrap();
    let router = axum::Router::new().route(
        "/echo",
        axum::routing::post(|body: axum::body::Bytes| async move { body }),
    );
    let bound = wsieve_server::h3::bind(certs, key, addr, router).await.unwrap();

    // 客户端用 h3 发一次 POST，断言回显一致
    let got = h3_client_post(bound, "/echo", b"hello-h3").await.unwrap();
    assert_eq!(&got[..], b"hello-h3");
}

#[tokio::test]
async fn downstream_is_streamed_not_buffered() {
    // 造一个"先发一块、等一会儿、再发一块"的 body：若适配层缓冲，
    // 第一块的到达时间会被第二块拖到同一时刻。
    // 断言首块的到达时间明显早于末块——这正是流式的定义。
    // 具体构造见实现时的 axum::body::Body::from_stream。
    todo!("按上述语义实现：断言 first_chunk_at < last_chunk_at - 100ms")
}
```

> 注：第二个测试的 `todo!()` 需在 Step 4 实现时补全为真实断言，**不得留空跑过**。

- [ ] **Step 3: 运行确认失败**

```bash
cargo test -p wsieve-server --test h3_roundtrip
```

预期：`wsieve_server::h3` 模块不存在。

- [ ] **Step 4: 实现 `crates/wsieve-server/src/h3.rs`**

```rust
//! HTTP/3 监听面：QUIC endpoint + h3 → axum 适配。
//!
//! 与 TCP 面（`crate::tls`）是**叠加关系而非替代**：UDP 443 在跨境 QoS、
//! 企业防火墙、Private Relay 下都可能不通，客户端会自动回落到 TCP 上的
//! h2/h1。因此本模块任何失败都不得影响 TCP 面（设计文档 §6）。

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use bytes::{Buf, Bytes, BytesMut};
use futures::StreamExt;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tower::ServiceExt;

/// 绑定 UDP 并 spawn accept 循环，返回实际绑定地址（`:0` 测试用）。
///
/// **绑定失败原样返回 Err**：要不要因此降级是启动编排的决策，不是本模块的。
pub async fn bind(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    listen: SocketAddr,
    router: Router,
) -> Result<SocketAddr> {
    // QUIC 面单独构建 config：alpn 只有 h3，early_data 必须是 0 或 u32::MAX
    // （16384 这类值会让 QuicServerConfig::try_from 失败，见设计文档 §4.1）。
    let tls = crate::tls::rustls_config(certs, key, vec![b"h3".to_vec()], u32::MAX)?;
    let quic = quinn::crypto::rustls::QuicServerConfig::try_from(Arc::new(tls))
        .context("rustls config 不满足 QUIC 要求（检查 TLS1.3 与 early_data）")?;
    let server_cfg = quinn::ServerConfig::with_crypto(Arc::new(quic));

    let endpoint = quinn::Endpoint::server(server_cfg, listen)
        .with_context(|| format!("QUIC 监听 {listen} 失败"))?;
    let addr = endpoint.local_addr()?;

    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            let router = router.clone();
            tokio::spawn(async move {
                let conn = match incoming.await {
                    Ok(c) => c,
                    // 握手失败是常态（扫描、UDP 伪造源），不值得 error 级别
                    Err(e) => {
                        tracing::debug!("QUIC 握手失败: {e}");
                        return;
                    }
                };
                if let Err(e) = serve_conn(conn, router).await {
                    tracing::debug!("h3 连接结束: {e}");
                }
            });
        }
    });
    Ok(addr)
}

async fn serve_conn(conn: quinn::Connection, router: Router) -> Result<()> {
    let mut h3_conn = h3::server::builder()
        .build(h3_quinn::Connection::new(conn))
        .await?;
    loop {
        match h3_conn.accept().await {
            Ok(Some(resolver)) => {
                let router = router.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve_request(resolver, router).await {
                        tracing::debug!("h3 请求处理失败: {e}");
                    }
                });
            }
            Ok(None) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
}

/// 一个 h3 请求 → axum Router → 流式写回。
async fn serve_request<C>(
    resolver: h3::server::RequestResolver<C, Bytes>,
    router: Router,
) -> Result<()>
where
    C: h3::quic::Connection<Bytes>,
{
    let (req, mut stream) = resolver.resolve_request().await?;
    let (parts, _) = req.into_parts();

    // 先收完请求 body。xhttp 的上行是一次性的 Uint8Array（见 ui/emitter.js
    // 的 fetch 调用，没有用 duplex streaming），所以这里收完再处理是安全的。
    let mut body = BytesMut::new();
    while let Some(mut chunk) = stream.recv_data().await? {
        body.extend_from_slice(chunk.chunk());
    }

    let resp = router
        .oneshot(http::Request::from_parts(
            parts,
            axum::body::Body::from(body.freeze()),
        ))
        .await
        .map_err(|e| anyhow::anyhow!("router 失败: {e}"))?;
    let (parts, body) = resp.into_parts();

    // 先发响应头，再逐块发 body——**逐块是硬要求**：下行是长流，
    // 聚合等于把流式退化成一次性返回（设计文档 §7.2 与本计划 Global Constraints）。
    stream
        .send_response(http::Response::from_parts(parts, ()))
        .await?;
    let mut data = body.into_data_stream();
    while let Some(chunk) = data.next().await {
        stream.send_data(chunk.context("下行 body 读取失败")?).await?;
    }
    stream.finish().await?;
    Ok(())
}
```

同时在 `crates/wsieve-server/src/lib.rs` 顶部加 `pub mod h3;`，并把 `tls::rustls_config` 与 `self_signed` 的可见性提升到 `pub(crate)`（`h3.rs` 要用），另导出一个供测试用的 `pub fn self_signed_pair()`。

- [ ] **Step 5: 补全流式测试**

把 Step 2 里的 `todo!()` 换成真实断言：构造一个分两次产出、中间 `sleep(200ms)` 的 `Body::from_stream`，在客户端记录每块到达时刻，断言 `last - first > 100ms`。若适配层缓冲了 body，两块会同时到达，断言失败。

- [ ] **Step 6: 运行测试**

```bash
cargo test -p wsieve-server --test h3_roundtrip
```

预期：两个测试都通过。

- [ ] **Step 7: 提交**

```bash
git add -A crates/wsieve-server
git commit -m "feat(server): HTTP/3 监听面（QUIC endpoint + h3→axum 适配）

h3 的 API 是手动 accept stream，与 axum 的 Service 模型不同，故加一层适配。
下行**逐块 send_data** 而非聚合：xhttp 的下行是长流，缓冲会让条带与低延迟
全部失效，专门有测试钉住这条。

QUIC 面单独构建 rustls config——QUIC 只接受 max_early_data_size 为 0 或
u32::MAX，与 TCP 面的 16384 互斥。"
```

---

### Task 4: 启动编排与降级

把三层接起来，并确保 UDP 面的任何失败都不拖累 TCP 面。

**Files:**
- Modify: `crates/wsieve-server/src/main.rs:140-160` 一带（启动流程）
- Test: `crates/wsieve-server/tests/h3_degrade.rs`（新建）

**Interfaces:**
- Consumes: `h3::bind(...)`（Task 3）、`AppState::router(Option<u16>)`（Task 2）
- Produces: 无（终端编排）

- [ ] **Step 1: 写失败测试**

新建 `crates/wsieve-server/tests/h3_degrade.rs`：

```rust
//! UDP 面失败必须降级，不得阻断启动。
//!
//! 与 shard_setup 的"条带禁用则退回单会话"同源纪律：优先建立可用服务 +
//! 警告日志，而不是拒绝启动。没有这条，一台 UDP 443 被占用的机器会直接
//! 起不来，而它本可以完好地提供 h1/h2。

use std::net::SocketAddr;

#[tokio::test]
async fn udp_bind_failure_does_not_block_startup() {
    // 先占住一个 UDP 端口，再让服务端去绑同一个
    let squatter = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let taken: SocketAddr = squatter.local_addr().unwrap();

    let outcome = wsieve_server::start_listeners_for_test(taken).await;
    assert!(outcome.tcp_ok, "TCP 面必须照常起来");
    assert!(!outcome.h3_ok, "UDP 被占用时 h3 应当失败");
    assert_eq!(
        outcome.alt_svc_port, None,
        "h3 起不来就不能宣告 Alt-Svc——宣告了客户端每次连接都要白等一轮 QUIC 超时"
    );
}
```

- [ ] **Step 2: 运行确认失败**

```bash
cargo test -p wsieve-server --test h3_degrade
```

- [ ] **Step 3: 实现启动编排**

`crates/wsieve-server/src/main.rs` 中把原先单行的 serve 调用展开为：

```rust
    // UDP 面先试，因为 Alt-Svc 要不要宣告取决于它成不成功。
    // 失败只警告不中止：h3 是叠加能力，TCP 面必须照常服务
    //（设计文档 §6；与 shard_setup 的条带降级同源）。
    let h3_port = match cfg.deployment.tls_material() {
        Some((certs, key)) => {
            match wsieve_server::h3::bind(certs, key, cfg.listen, state.clone().router(None)).await
            {
                Ok(addr) => {
                    tracing::info!("HTTP/3 就绪: udp://{addr}");
                    Some(addr.port())
                }
                Err(e) => {
                    tracing::warn!("HTTP/3 未启用（{e:#}）——继续以 h1/h2 提供服务");
                    None
                }
            }
        }
        // 明文部署（cdn-flexible）没有证书，QUIC 无从谈起
        None => None,
    };

    wsieve_server::tls::serve(&cfg.deployment, cfg.listen, state.router(h3_port)).await?;
```

其中 `tls_material()` 是给 `Deployment` 新加的小方法：`Direct` 返回读好的证书链与私钥，`CdnFullSelfSigned` 返回自签的那对，`CdnFlexible` 返回 `None`。**证书只读一次**，TCP 与 UDP 两面共用，避免两处各读一遍导致轮换期间读到不同版本。

同时补一个 `pub async fn start_listeners_for_test(udp: SocketAddr) -> ListenOutcome`，把上面这段逻辑以可测形式暴露（`ListenOutcome { tcp_ok, h3_ok, alt_svc_port }`）。

- [ ] **Step 4: 运行全部测试**

```bash
cargo test -p wsieve-server
```

- [ ] **Step 5: 真机验证降级**

在测试节点上人为占住 UDP 443 后重启服务，确认：日志出现 `HTTP/3 未启用`，且 `curl https://<host>/` 仍然 200，响应**不含** `alt-svc` 头。

- [ ] **Step 6: 提交**

```bash
git add -A crates/wsieve-server
git commit -m "feat(server): 同时监听 TCP(h1/h2) 与 UDP(h3)，UDP 失败则降级

h3 是叠加能力不是替代：UDP 绑不上时只警告并继续以 h1/h2 服务，且此时
不发 Alt-Svc（宣告一个连不上的端点会让客户端每次连接白等一轮超时）。
证书只读一次供两面共用，避免轮换期间两面拿到不同版本。"
```

---

### Task 5: 端到端验证与条带参数重标定

代码完成后必须实测，且**不接受"理论上更好"**。`target_lanes: 4` 这个默认值是在 h1 的 6 条连接前提下测出来的，协议一换就失效。

**Files:**
- Modify: `crates/wsieve-mux/src/stripe_runtime.rs:87`（`target_lanes` 默认值，按实测结果定）
- Modify: `docs/superpowers/plans/2026-09-07-live-walkthrough-checklist.md`（追加走查项）

- [ ] **Step 1: 协议生效验证**

```bash
echo | openssl s_client -connect <host>:443 -alpn h2,http/1.1 2>&1 | grep -i ALPN   # 期望 h2
curl --http3 -o /dev/null -w "%{http_version}\n" https://<host>/                    # 期望 3
```

- [ ] **Step 2: Alt-Svc 升级路径验证（必须连续两次）**

第一次连接走 h2 是**预期行为**，不是 bug——Apple 的网络栈不做推测性 QUIC 尝试，要先看到 Alt-Svc 才会在下次升级（设计文档 §2.3）。只测一次会得出"h3 没生效"的错误结论。

- [ ] **Step 3: 连接数模型实测**

大流量下载进行中，于服务端分别统计：

```bash
ss -tn state established '( sport = :443 )' | tail -n +2 | wc -l   # TCP 连接数
ss -uan | grep :443 | wc -l                                        # UDP/QUIC
```

记录 h1 基线（6）、h2（预期 1）、h3（预期 1）三组。

- [ ] **Step 4: 吞吐对照与 lane 重标定**

对 `h1 / h2 / h2+多origin / h3 / h3+多origin` 五种组合，各跑 64MiB 单流下载，lane 取 `1/2/4/8`，**每格三轮取中位数**。

测量纪律（由 2026-09-11 那轮实测的教训得出）：
- 跨境链路抖动可达 ±3 倍，任何单轮数字都不构成证据
- 每档第一轮天然偏低（拥塞窗口冷启动），取中位数正好排除
- 警惕测量工具自身的天花板：那轮踩过两次——目标端把响应头与 body 分两次 write 撞上 Nagle+delayed ACK 造成 40ms 假延迟；`socketserver` 默认 `request_queue_size=5` 造成假的并发上限

- [ ] **Step 5: 按实测结果定默认值**

若 h2/h3 下的最优 lane 数与 4 不同，改 `stripe_runtime.rs:87` 的 `target_lanes`，并在该处注释里写明**是在哪种协议、哪条链路上测出来的**——这个值没有普适最优解，脱离前提的数字会误导下一个人。

- [ ] **Step 6: 补走查清单并提交**

---

## Self-Review

**Spec 覆盖检查：**

| 设计文档章节 | 对应任务 |
|---|---|
| §4 ALPN 分层 | Task 1（TCP 面）+ Task 3（QUIC 面） |
| §4.1 config 不得共用 | Task 1 Step 3（参数化）+ Task 3 Step 4 |
| §5 伪装影响 | Task 1（h2 宣告消除异常特征）；§5.2 的 quinn transport 指纹加固**未排任务**——属设计文档 §10 阶段 4，留作后续 |
| §6 降级与容错 | Task 4 |
| §7.1 h2 实现 | Task 1 |
| §7.2 h3 实现 | Task 3 |
| §7.3 DNS HTTPS 记录 | **未排任务**——属部署配置而非代码，独立实施 |
| §8 客户端影响 | Task 5 Step 5（参数重标定） |
| §9 验证计划 | Task 5 |

**已知缺口（有意留下，不是遗漏）：**
1. quinn transport config 的指纹加固（设计文档 §5.2）—— 需要先有主流实现的 transport parameters 取值样本，属独立调研。
2. DNS HTTPS 记录 —— Cloudflare 控制台配置，无代码改动。
3. `extra-sessions` 的多 origin 打通 —— 已在代码中实现，卡在 `/etc/hosts` 写权限这一运维前提上，不是本计划的代码任务。但 **Task 5 Step 4 的"h2+多origin"对照组依赖它**，届时需要该权限。

**类型一致性：** `rustls_config` 的四参签名在 Task 1 定义、Task 3 使用；`router(Option<u16>)` 在 Task 2 定义、Task 4 使用；`h3::bind` 在 Task 3 定义、Task 4 使用。三处均已对齐。

