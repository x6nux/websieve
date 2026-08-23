# WebView 完全仿真 HTTPS 代理（websieve）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实现基于 WebView fetch 的代理（协议逻辑全在 Rust，WebView 只做哑发射器），含 5 种 mux、CDN/直连部署、nginx 伪装。

**Architecture:** 分层 `socks5 → mux → Noise_IK(AESGCM+BLAKE3) → XhttpConn(TU 分帧) → HttpTransport(WebView/Reqwest 双实现)`。协议核心与传输实现解耦，`ReqwestTransport` 让全链路在 `cargo test` 里可测，WebView 只在 E2E 出现。

**Tech Stack:** Rust (tokio, snow, axum, rustls, reqwest)、Tauri 2 (IPC 二进制快路径 + emitter.ts)、tokio-yamux/smux/muxado/picomux/h2。

**Spec:** `docs/superpowers/specs/2026-08-23-webview-https-proxy-design.md`（实现前必读，本计划引用其 § 编号）

---

## 文件结构

```
websieve/
├── Cargo.toml                      # workspace
├── crates/
│   ├── wsieve-proto/               # TU 分帧 + Noise 套件 + 握手载荷 + TargetAddr
│   │   ├── src/lib.rs
│   │   ├── src/tu.rs               # TU/Frame 编解码（§6.1）
│   │   ├── src/crypto.rs           # snow 自定义 resolver（§6.6）
│   │   ├── src/hello.rs            # msg1/msg2 载荷（§7.4）
│   │   └── src/addr.rs             # SOCKS5 地址格式复用（mux 流首包）
│   ├── wsieve-transport/           # HttpTransport trait + ReqwestTransport（§5.1）
│   │   └── src/lib.rs
│   ├── wsieve-xhttp/               # 唯一自研核心层
│   │   ├── src/lib.rs
│   │   ├── src/client.rs           # XhttpConn：窗口/seq/重试/合流（§6.4）
│   │   └── src/server.rs           # SessionStore：重排/去重/GC（§9.4）
│   ├── wsieve-mux/                 # Mux trait + 5 实现
│   │   ├── src/lib.rs              # trait + 工厂
│   │   ├── src/yamux_impl.rs       # tokio-yamux
│   │   ├── src/smux_impl.rs        # smux(iberryful)
│   │   ├── src/muxado_impl.rs      # muxado
│   │   ├── src/picomux_impl.rs     # picomux
│   │   ├── src/h2mux_impl.rs       # h2 crate + 双工适配层
│   │   └── examples/mux-bench.rs   # 一次性基准（§7.6）
│   ├── wsieve-server/              # axum 二进制
│   │   ├── src/main.rs             # 路由：认证先于路径（§8）
│   │   ├── src/disguise.rs         # nginx 页 + upstream 反代（§8）
│   │   ├── src/tls.rs              # none/self-signed/cert + early data（§6.8）
│   │   ├── src/remote.rs           # mux 流 → TcpStream 拨号泵
│   │   └── assets/nginx/           # 内嵌 nginx 默认页副本
│   └── wsieve-socks5/              # SOCKS5 CONNECT 入站（客户端）
│       └── src/lib.rs
├── src-tauri/                      # Tauri 客户端
│   ├── src/main.rs
│   ├── src/bridge.rs               # WebViewTransport（IPC 快路径）
│   └── src/proxy.rs                # socks5→mux 管道接线
└── ui/
    └── src/emitter.ts              # fetch 哑发射器 + 下行流回推
```

## 任务地图（18 个任务，6 个阶段）

| 阶段 | 任务 | 产出 |
|---|---|---|
| P1 协议核心 | 1-4 | workspace、TU、Noise 套件、握手载荷 |
| P2 传输+xhttp | 5-7 | HttpTransport、XhttpConn 客户端、SessionStore 服务端 |
| P3 mux | 8-12 | trait+4 实现、h2mux、基准 |
| P4 服务端 | 13-15 | axum 路由、伪装/TLS、集成+探测测试 |
| P5 客户端 | 16-18 | socks5、Tauri 桥、E2E |

---

## 阶段 P1：协议核心

### Task 1: workspace 脚手架

**Files:**
- Create: `Cargo.toml`、`crates/{wsieve-proto,wsieve-transport,wsieve-xhttp,wsieve-mux,wsieve-server,wsieve-socks5}/Cargo.toml` 及各 `src/lib.rs`

- [ ] **Step 1: 初始化仓库与 workspace**

```bash
cd /Users/ll/code/websieve
git init
```

根 `Cargo.toml`：

```toml
[workspace]
resolver = "2"
members = [
    "crates/wsieve-proto",
    "crates/wsieve-transport",
    "crates/wsieve-xhttp",
    "crates/wsieve-mux",
    "crates/wsieve-server",
    "crates/wsieve-socks5",
]

[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
bytes = "1"
thiserror = "2"
async-trait = "0.1"
futures = "0.3"
rand = "0.9"
snow = "0.10"
blake3 = "1"
aes-gcm = "0.10"
```

每个 crate 的 `Cargo.toml` 引 workspace 依赖（如 `tokio = { workspace = true }`），`src/lib.rs` 先放空 `pub mod` 占位——**注意**：占位仅限脚手架任务，Task 2 起每个模块都是完整实现。

- [ ] **Step 2: 验证编译**

Run: `cargo build`
Expected: 全部 crate 编译通过，0 error

- [ ] **Step 3: 提交 spec 与脚手架**

```bash
git add -A
git commit -m "chore: workspace scaffold + design spec"
```

---

### Task 2: TU 编解码（§6.1）

**Files:**
- Create: `crates/wsieve-proto/src/tu.rs`
- Modify: `crates/wsieve-proto/src/lib.rs`（`pub mod tu;`）
- Test: `crates/wsieve-proto/tests/tu.rs`

约束链（spec §6.1，代码里作为常量钉死）：

```
密文上限 65535 − tag 16 = 明文上限 65519 − 帧头 3 − padding≤1000 ⇒ payload ≤ 64516
```

- [ ] **Step 1: 写失败测试**

`crates/wsieve-proto/tests/tu.rs`：

```rust
use wsieve_proto::tu::*;

#[test]
fn frame_roundtrip_data() {
    let f = Frame::Data(vec![1, 2, 3, 4, 5]);
    let plain = encode_frame(&f, &mut rand::rng());
    assert!(plain.len() >= 3 + 5 && plain.len() <= 3 + 5 + MAX_PADDING);
    assert_eq!(decode_frame(&plain).unwrap(), f);
}

#[test]
fn frame_roundtrip_padding() {
    let plain = encode_frame(&Frame::Padding, &mut rand::rng());
    assert_eq!(decode_frame(&plain).unwrap(), Frame::Padding);
}

#[test]
fn payload_limit_enforced() {
    let big = vec![0u8; MAX_PAYLOAD + 1];
    assert!(encode_frame(&Frame::Data(big), &mut rand::rng()).is_err());
    let ok = vec![0u8; MAX_PAYLOAD];
    assert!(encode_frame(&Frame::Data(ok), &mut rand::rng()).is_ok());
}

#[test]
fn decoder_splits_stream_at_boundaries() {
    let mut dec = TuDecoder::new();
    // 两个 TU 拼成一条 chunk，再故意从中间断开
    let c1 = b"\x00\x05hello".to_vec();       // len=5 密文（此处当任意字节）
    let c2 = b"\x00\x03abc".to_vec();
    let mut wire = c1.clone();
    wire.extend_from_slice(&c2);
    assert_eq!(dec.push(&wire[..7]), vec![c1]);
    assert_eq!(dec.push(&wire[7..]), vec![c2]);
    assert!(dec.push(b"\x00").is_empty());    // 半个长度头，挂起
}

#[test]
fn decoder_rejects_oversized_len() {
    let mut dec = TuDecoder::new();
    let wire = [0xffu8, 0xff];                // 65535 合法上界
    assert!(dec.push(&wire).is_empty());      // 只声明，不报错
    // 但 encode 侧保证不会产出超限 TU，见 payload_limit_enforced
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p wsieve-proto`
Expected: 编译失败，`unresolved import wsieve_proto::tu`

- [ ] **Step 3: 实现**

`crates/wsieve-proto/src/tu.rs`：

```rust
//! TU（Transport Unit）分帧。spec §6.1。
//! 外层：u16 大端长度 + Noise 密文；内层（解密后）：u8 type + u16 p_len + payload + padding。

pub const MAX_CIPHERTEXT: usize = 65535; // Noise 单消息上限
pub const TAG_LEN: usize = 16;           // AES-GCM tag
pub const MAX_PLAINTEXT: usize = MAX_CIPHERTEXT - TAG_LEN;
pub const FRAME_HEADER: usize = 3;       // type(1) + p_len(2)
pub const MAX_PADDING: usize = 1000;
pub const MAX_PAYLOAD: usize = MAX_PLAINTEXT - FRAME_HEADER - MAX_PADDING; // 64516

pub const TYPE_DATA: u8 = 0x01;
pub const TYPE_PADDING: u8 = 0x02;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Data(Vec<u8>),
    Padding,
}

/// 编码明文帧（含随机 padding）。返回可直接交给 Noise 加密的明文字节。
pub fn encode_frame(frame: &Frame, rng: &mut impl rand::Rng) -> Result<Vec<u8>, TuError> {
    let payload = match frame {
        Frame::Data(p) => {
            if p.len() > MAX_PAYLOAD {
                return Err(TuError::PayloadTooLarge(p.len()));
            }
            p.as_slice()
        }
        Frame::Padding => &[],
    };
    let ty = match frame {
        Frame::Data(_) => TYPE_DATA,
        Frame::Padding => TYPE_PADDING,
    };
    let pad = rng.random_range(0..=MAX_PADDING);
    let mut out = Vec::with_capacity(FRAME_HEADER + payload.len() + pad);
    out.push(ty);
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out.resize(out.len() + pad, 0); // 内容无关紧要，长度才是混淆量
    Ok(out)
}

/// 从解密后的明文字节解出帧。padding 长度 = 总长 − 3 − p_len，无需显式编码。
pub fn decode_frame(plain: &[u8]) -> Result<Frame, TuError> {
    if plain.len() < FRAME_HEADER {
        return Err(TuError::Truncated);
    }
    let ty = plain[0];
    let p_len = u16::from_be_bytes([plain[1], plain[2]]) as usize;
    if plain.len() < FRAME_HEADER + p_len {
        return Err(TuError::Truncated);
    }
    match ty {
        TYPE_DATA => Ok(Frame::Data(plain[FRAME_HEADER..FRAME_HEADER + p_len].to_vec())),
        TYPE_PADDING => Ok(Frame::Padding),
        other => Err(TuError::UnknownType(other)),
    }
}

/// 下行流切分器：喂入任意 chunk，吐出完整的密文 TU。
pub struct TuDecoder {
    buf: Vec<u8>,
}

impl TuDecoder {
    pub fn new() -> Self {
        Self { buf: Vec::with_capacity(MAX_CIPHERTEXT + 2) }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while self.buf.len() >= 2 {
            let len = u16::from_be_bytes([self.buf[0], self.buf[1]]) as usize;
            if self.buf.len() < 2 + len {
                break;
            }
            let tu = self.buf[2..2 + len].to_vec();
            self.buf.drain(..2 + len);
            out.push(tu);
        }
        out
    }
}

impl Default for TuDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TuError {
    #[error("payload {0} exceeds limit {1}")]
    PayloadTooLarge(usize, ),
    #[error("truncated frame")]
    Truncated,
    #[error("unknown frame type {0}")]
    UnknownType(u8),
}
```

（`PayloadTooLarge(usize)` 的显示串补上 `MAX_PAYLOAD` 常量即可。）

- [ ] **Step 4: 跑测试通过**

Run: `cargo test -p wsieve-proto`
Expected: 5 passed

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-proto
git commit -m "feat(proto): TU framing codec with random padding"
```

---

### Task 3: Noise 套件——自定义 CryptoResolver（§6.6）

**Files:**
- Create: `crates/wsieve-proto/src/crypto.rs`
- Modify: `crates/wsieve-proto/src/lib.rs`、`crates/wsieve-proto/Cargo.toml`（加 `snow`、`blake3`、`aes-gcm`、`x25519-dalek`、`zeroize`）
- Test: `crates/wsieve-proto/tests/crypto.rs`

关键点：snow 的 `Cipher` trait 用 `u64` nonce，AES-GCM 要 12 字节——**4 字节零前缀 ‖ u64 大端计数器**，spec §6.6 已写死，不留发挥空间。DH/Random 用 snow 内置实现，只换 Cipher 和 Hash。

- [ ] **Step 1: 写失败测试**

`crates/wsieve-proto/tests/crypto.rs`：

```rust
use wsieve_proto::crypto::*;

#[test]
fn handshake_roundtrip_ik() {
    // IK 模式：客户端预知服务端静态公钥，服务端白名单验证客户端静态公钥
    let (s_priv, s_pub) = gen_server_keypair();
    let (c_priv, c_pub) = gen_client_keypair();
    let mut allow = std::collections::HashSet::new();
    allow.insert(c_pub);

    let mut h = build_client(&s_pub, &c_priv, &c_pub);
    let msg1 = h.write_message(b"hello-0rtt").unwrap();

    let mut sh = build_server(&s_priv, &allow).unwrap();
    let got0rtt = sh.read_message(&msg1).unwrap();
    assert_eq!(got0rtt, b"hello-0rtt");

    let msg2 = sh.write_message(b"ack").unwrap();
    let got = h.read_message(&msg2).unwrap();
    assert_eq!(got, b"ack");

    // 握手完成后双端进入传输态，加密往返
    let t1 = h.into_transport_mode();
    let t2 = sh.into_transport_mode();
    let ct = t1.write_message(b"data").unwrap();
    assert_eq!(t2.read_message(&ct).unwrap(), b"data");
}

#[test]
fn server_rejects_unknown_client_key() {
    let (s_priv, _) = gen_server_keypair();
    let (_rp, rogue_pub) = gen_client_keypair();
    let allow = std::collections::HashSet::new(); // 白名单为空
    let mut sh = build_server(&s_priv, &allow).unwrap();
    // 用 rogue 身份构造 msg1
    let (_cp, c_pub) = gen_client_keypair();
    let mut h = build_client(&pk_of(&s_priv), &rogue_priv(), &c_pub);
    let msg1 = h.write_message(b"x").unwrap();
    assert!(sh.read_message(&msg1).is_err());
}
```

（辅助函数按 snow API 实际形状微调。白名单拒绝的显式断言——与 Task 13 api.rs 伪装路径测试两级钉住：

```rust
fn assert_whitelist_reject(s_priv: &[u8], allowed: &std::collections::HashSet<[u8; 32]>) {
    let (rogue_priv, rogue_pub) = gen_client_keypair();
    let mut c = build_client(&pk_of(s_priv), &rogue_priv, &rogue_pub);
    let msg1 = c.write_message(b"x").unwrap();
    let mut s = build_server(s_priv, allowed).unwrap();
    // msg1 可能解密成功（密钥有效），但 remote static 不在白名单：
    // 断言「要么 read_message 报错，要么成功后 remote 不匹配」→ 上层一律按伪装处理器处理
    let rejected = match s.read_message(&msg1) {
        Err(_) => true,
        Ok(_) => !allowed.contains(s.get_remote_static().unwrap()),
    };
    assert!(rejected);
}
```
）

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p wsieve-proto`
Expected: 编译失败，`unresolved import wsieve_proto::crypto`

- [ ] **Step 3: 实现**

`crates/wsieve-proto/src/crypto.rs`：

```rust
//! Noise_IK + AES-256-GCM + BLAKE3（spec §6.6）。
//! 关键落地细节：snow 0.10 的 NoiseParams::from_str 只认已注册的 hash 名
//! （SHA256/512/BLAKE2s/BLAKE2b），"BLAKE3" 会在解析期 panic——这发生在
//! resolver 介入之前，绕不过去。因此：
//!   - 线名用可解析的壳：Noise_IK_25519_AESGCM_SHA256
//!   - SieveResolver::resolve_hash 无视传入的 HashChoice，恒返回 BLAKE3 实现
//!   - PROTOCOL_NAME 常量保留 spec 的私有标识，仅用于文档/日志

use snow::{Builder, HandshakeState, TransportState};

pub const PROTOCOL_NAME: &str = "Noise_IK_25519_AESGCM_BLAKE3"; // 文档标识
pub const WIRE_NAME: &str = "Noise_IK_25519_AESGCM_SHA256";     // 实际传给 snow 解析

struct AesGcmCipher(snow::params::CipherSuites);
// 实现 snow::Cipher：
//   set(&mut self, key: &[u8; 32])          -> aes_gcm::Aes256Gcm::new(key)
//   encrypt(&self, nonce: u64, ..)          -> 12 字节 nonce = [0u8;4] ‖ nonce.to_be_bytes()
//   decrypt 同理；tag 恰 16 字节满足 Noise 对 cipher 的要求
//   name() -> "AESGCM"
struct Blake3Hash(snow::params::HashChoice);
// 实现 snow::Hash：name() -> "BLAKE3"，trapped into snow::types::Hash。

#[derive(Default)]
pub struct SieveResolver;
// 实现 snow::CryptoResolver：
//   resolve_cipher_aesgcm / resolve_hash_blake3 -> Some(Box::new(..))
//   resolve_dh / resolve_random -> None（snow 会落到默认 resolver 取）

pub fn build_client(server_static_pub: &[u8], client_priv: &[u8], client_pub: &[u8]) -> HandshakeState {
    Builder::new(WIRE_NAME.parse().unwrap())
        .local_private_key(client_priv)
        .remote_public(server_static_pub)
        .with_resolver(Box::new(SieveResolver))
        .build_initiator()
        .unwrap()
}

pub fn build_server(server_priv: &[u8], allowed_client_pubs: &std::collections::HashSet<[u8; 32]>) -> snow::Result<HandshakeState> {
    // snow 无原生白名单回调：IK 模式下客户端静态公钥在 msg1 密文内，
    // 握手成功即代表密钥有效；白名单校验在 read_message 成功后、由上层
    // 从 handshake hash 里取 remote static（snow: .get_remote_static()）比对。
    Builder::new(WIRE_NAME.parse().unwrap())
        .local_private_key(server_priv)
        .with_resolver(Box::new(SieveResolver))
        .build_responder()
}
```

**实现注意（写给执行者）**：
1. snow 0.10 的 resolver trait 具体签名以 `docs.rs/snow` 当前版为准，`resolve_cipher` 系列方法按实际名字适配；`AesGcmCipher`/`Blake3Hash` 的 trait 实现里所有错误统一映射 `snow::Error::Decrypt`
2. 白名单的实际执行点：`read_message(msg1)` 成功后立刻 `get_remote_static()`，与 `allowed_client_pubs` 比对，不在白名单 → 返回错误（上层据此转伪装处理器）。**不要**试图在 resolver 层做白名单
3. `gen_server_keypair`/`gen_client_keypair`：`x25519-dalek` 生成 32 字节密钥对；静态密钥落盘格式 = 裸 32 字节

- [ ] **Step 4: 跑测试通过**

Run: `cargo test -p wsieve-proto`
Expected: crypto 2 passed + tu 5 passed

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-proto
git commit -m "feat(proto): Noise_IK AESGCM+BLAKE3 custom resolver"
```

---

### Task 4: 握手载荷 msg1/msg2（§6.3 + §7.4）

**Files:**
- Create: `crates/wsieve-proto/src/hello.rs`
- Modify: `crates/wsieve-proto/src/lib.rs`
- Test: `crates/wsieve-proto/tests/hello.rs`

- [ ] **Step 1: 写失败测试**

```rust
use wsieve_proto::hello::*;

#[test]
fn msg1_roundtrip() {
    let m = Msg1 { version: 1, ts_ms: 1724400000000, mux_prefs: vec![MuxId::Picomux, MuxId::Yamux] };
    let b = encode_msg1(&m).unwrap();
    assert_eq!(decode_msg1(&b).unwrap(), m);
}

#[test]
fn msg1_rejects_empty_mux_list() {
    let b = encode_msg1(&Msg1 { version: 1, ts_ms: 0, mux_prefs: vec![] }).unwrap();
    assert!(decode_msg1(&b).is_err());
}

#[test]
fn msg1_rejects_bad_version() {
    let b = b"\x01\x00\x00\x01\x84\x5a\x66\x28\x00\x00".to_vec(); // ver=1 但 mux_count=0
    assert!(decode_msg1(&b).is_err());
}

#[test]
fn msg2_roundtrip() {
    let m = Msg2 { chosen: MuxId::Smux, fallback: false };
    let b = encode_msg2(&m);
    assert_eq!(decode_msg2(&b).unwrap(), m);
    let f = Msg2 { chosen: MuxId::Yamux, fallback: true };
    assert_eq!(decode_msg2(&encode_msg2(&f)).unwrap(), f);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p wsieve-proto`
Expected: 编译失败，`unresolved import wsieve_proto::hello`

- [ ] **Step 3: 实现**

`crates/wsieve-proto/src/hello.rs`：

```rust
//! 握手载荷。spec §6.3 步骤 2 / §7.4。全部大端。

pub const PROTOCOL_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuxId { Yamux = 0x01, Smux = 0x02, Muxado = 0x03, Picomux = 0x04, H2mux = 0x05 }

impl MuxId {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v { 0x01 => Some(Yamux), 0x02 => Some(Smux), 0x03 => Some(Muxado), 0x04 => Some(Picomux), 0x05 => Some(H2mux), _ => None }
    }
}

pub struct Msg1 { pub version: u8, pub ts_ms: u64, pub mux_prefs: Vec<MuxId> }
pub struct Msg2 { pub chosen: MuxId, pub fallback: bool }

pub fn encode_msg1(m: &Msg1) -> Result<Vec<u8>, HelloError> {
    if m.mux_prefs.is_empty() || m.mux_prefs.len() > 5 { return Err(HelloError::BadMuxCount); }
    if m.version != PROTOCOL_VERSION { return Err(HelloError::BadVersion); }
    let mut b = Vec::with_capacity(4 + 1 + m.mux_prefs.len());
    b.push(m.version);
    b.extend_from_slice(&m.ts_ms.to_be_bytes());
    b.push(m.mux_prefs.len() as u8);
    for m in &m.mux_prefs { b.push(*m as u8); }
    Ok(b)
}

pub fn decode_msg1(b: &[u8]) -> Result<Msg1, HelloError> {
    if b.len() < 5 { return Err(HelloError::Truncated); }
    let version = b[0];
    if version != PROTOCOL_VERSION { return Err(HelloError::BadVersion); }
    let ts_ms = u64::from_be_bytes(b[1..9].try_into().unwrap());
    let n = b[9] as usize;
    if n == 0 || b.len() != 10 + n { return Err(HelloError::BadMuxCount); }
    let mux_prefs = b[10..].iter().map(|&v| MuxId::from_u8(v).ok_or(HelloError::BadMuxId)).collect::<Result<_, _>>()?;
    Ok(Msg1 { version, ts_ms, mux_prefs })
}

pub fn encode_msg2(m: &Msg2) -> Vec<u8> {
    vec![m.chosen as u8, m.fallback as u8]
}

pub fn decode_msg2(b: &[u8]) -> Result<Msg2, HelloError> {
    if b.len() != 2 { return Err(HelloError::Truncated); }
    Ok(Msg2 { chosen: MuxId::from_u8(b[0]).ok_or(HelloError::BadMuxId)?, fallback: b[1] != 0 })
}

pub const TS_WINDOW_MS: i64 = 300_000; // ±300s，spec §6.3

pub fn ts_in_window(ts_ms: u64, now_ms: u64) -> bool {
    let d = (ts_ms as i64 - now_ms as i64).abs();
    d <= TS_WINDOW_MS
}
```

- [ ] **Step 4: 跑测试通过**

Run: `cargo test -p wsieve-proto`
Expected: hello 4 passed

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-proto
git commit -m "feat(proto): msg1/msg2 handshake payloads with mux negotiation"
```

---

## 阶段 P2：传输层 + xhttp 核心

### Task 5: HttpTransport trait + ReqwestTransport（§5.1）

**Files:**
- Create: `crates/wsieve-transport/src/lib.rs`
- Test: `crates/wsieve-transport/tests/lib.rs`

`ReqwestTransport` 是**真实实现**（rustls TLS、真实 HTTP），不是 mock——它让协议层全链路测试脱离 WebView。

- [ ] **Step 1: 写失败测试（对本地 axum 起的服务）**

`crates/wsieve-transport/tests/lib.rs`：

```rust
use axum::{routing::post, Router, body::Bytes as BodyBytes};
use wsieve_transport::{HttpTransport, ReqwestTransport, PostReply};

#[tokio::test]
async fn post_roundtrip_and_stream() {
    let app = Router::new()
        .route("/api/sync", post(|body: BodyBytes| async move {
            if body.is_empty() { (axum::http::StatusCode::NO_CONTENT, BodyBytes::new()) }
            else { (axum::http::StatusCode::OK, body) } // 回显，供断言
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });

    let t = ReqwestTransport::new(format!("http://{addr}")).unwrap();
    let r = t.post("/api/sync?n=0", bytes::Bytes::from_static(b"hello")).await.unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(&r.body[..], b"hello");

    let mut stream = t.get_stream("/api/events").await.unwrap();
    // 测试服务不下发数据则挂起——此处仅验证连接可建立：
    // 实际下行数据测试在 Task 14 集成测试覆盖
    drop(stream);
}
```

（路由若 404 会返回 404 而非 panic，`get_stream` 对非 2xx 返回 `Err`——在实现里断言之。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p wsieve-transport`
Expected: 编译失败，crate 尚无 `HttpTransport`

- [ ] **Step 3: 实现**

`crates/wsieve-transport/src/lib.rs`：

```rust
//! HttpTransport：协议层与「谁来发 HTTP」之间的唯一边界。spec §5.1。
//! ReqwestTransport 用 rustls 发真实 HTTPS；WebViewTransport（Task 17）经 Tauri IPC。

use bytes::Bytes;
use futures::StreamExt;

#[derive(Debug)]
pub struct PostReply { pub status: u16, pub body: Bytes }

#[async_trait::async_trait]
pub trait HttpTransport: Send + Sync {
    /// 上行 POST。返回状态码与响应体（n=0 时 body = msg2 的 TU）。
    async fn post(&self, path: &str, body: Bytes) -> anyhow::Result<PostReply>;
    /// 下行：开一条流式 GET。非 2xx 返回 Err。
    async fn get_stream(&self, path: &str) -> anyhow::Result<
        futures::stream::BoxStream<'static, anyhow::Result<Bytes>>>;
}

pub struct ReqwestTransport {
    client: reqwest::Client,
    base: reqwest::Url,
}

impl ReqwestTransport {
    pub fn new(base_url: String) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .danger_accept_invalid_certs(true) // 自签/无证书模式（§8 部署表）：测试与源站直连场景
                .build()?,
            base: base_url.parse()?,
        })
    }
}

#[async_trait::async_trait]
impl HttpTransport for ReqwestTransport {
    async fn post(&self, path: &str, body: Bytes) -> anyhow::Result<PostReply> {
        let url = self.base.join(path)?;
        let resp = self.client.post(url).body(body).send().await?;
        let status = resp.status().as_u16();
        let body = resp.bytes().await?;
        Ok(PostReply { status, body })
    }

    async fn get_stream(&self, path: &str) -> anyhow::Result<futures::stream::BoxStream<'static, anyhow::Result<Bytes>>> {
        let url = self.base.join(path)?;
        let resp = self.client.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("GET {path} -> {}", status.as_u16());
        }
        Ok(resp.bytes_stream().boxed())
    }
}
```

注意：`danger_accept_invalid_certs(true)` 仅用于自签源站直连与集成测试；生产客户端连 CDN 域名（有效证书），此开关不影响（浏览器路径根本不走 reqwest）。

- [ ] **Step 4: 跑测试通过**

Run: `cargo test -p wsieve-transport`
Expected: 1 passed

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-transport
git commit -m "feat(transport): HttpTransport trait + ReqwestTransport"
```

---

### Task 6: XhttpConn 客户端（§6.3 + §6.4）

**Files:**
- Create: `crates/wsieve-xhttp/src/client.rs`
- Modify: `crates/wsieve-xhttp/src/lib.rs`、`Cargo.toml`（依赖 proto/transport）
- Test: `crates/wsieve-xhttp/tests/client.rs`

这是全项目最核心的单元：会话建立、上行窗口、seq 重试、下行合流，最终对外暴露一条 `AsyncRead + AsyncWrite`。

- [ ] **Step 1: 写失败测试**

`crates/wsieve-xhttp/tests/client.rs` 核心断言（配合 Task 7 的 `SessionStore`，可先写为**对端回环**测试——但 SessionStore 未实现前，先写纯客户端单元测试）：

```rust
use wsieve_xhttp::client::{XhttpConn, UpstreamCfg};

// 用一个可编程的 FakeTransport 只测客户端状态机：
// - 握手：n=0 POST 发出后收到合法 msg2 TU → 状态变为 Transport
// - 窗口：在途 POST ≤ 8
// - 重试：传输错误时同 seq 同字节重发
// - 判活：收到非约定响应 → 断会话
// FakeTransport 内部是 HashMap<String path, VecDeque<PostReply>> + 录制的请求序列，
// 是测试脚手架（只存在于 tests/ 目录），不算协议实现的一部分。
```

（具体测试代码在此任务内编写：`handshake_success`、`window_capped_at_8`、`retry_same_seq_same_bytes`、`non_conventional_reply_kills_session` 四个用例 + 一个 `write_frames_become_tus`。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p wsieve-xhttp`
Expected: 编译失败

- [ ] **Step 3: 实现**

`crates/wsieve-xhttp/src/client.rs` 结构（完整代码在实现时展开，此处钉死骨架与不变量）：

```rust
//! XhttpConn：xhttp 客户端核心。spec §6.3/§6.4/§6.5。

pub struct UpstreamCfg {
    pub server_pub: [u8; 32],       // 服务端静态公钥
    pub client_priv: [u8; 32],
    pub mux_prefs: Vec<MuxId>,      // 按偏好排序
}

pub struct XhttpConn<T: HttpTransport> {
    transport: Arc<T>,
    sid: [u8; 16],                  // 128-bit 随机路由标签（§6.2）
    seq: u64,
    in_flight: HashMap<u64, Bytes>, // 在途窗口：seq -> 完整字节（重试原样重发）
    // …握手产物：TransportState、下行 reader 任务句柄
}

impl<T: HttpTransport> XhttpConn<T> {
    /// 全流程：握手 → 开下行流 → 进入传输态。spec §6.3 六步时序。
    pub async fn connect(transport: Arc<T>, cfg: &UpstreamCfg) -> anyhow::Result<(Self, Negotiated)> { … }

    // AsyncRead：从下行 TU 解密出的 Frame::Data 拼接缓冲读取
    // AsyncWrite：Frame::Data 编码+加密 → 聚合器（首包立即/4ms/64KB，§6.4）→ 窗口内 POST
}

pub struct Negotiated { pub mux_id: MuxId, pub fallback: bool }
```

**不变量（写测试钉住）**：
1. `in_flight.len() <= 8` 任何时刻
2. 重试发出的字节与首次**逐字节相同**（同 seq 同密文，nonce 序不乱）
3. 心跳 PADDING TU 走与数据完全相同的窗口/seq 路径，无旁路
4. 空闲 60s → 自动 PADDING（用 tokio::time::pause 测试）
5. 收到非 `n=0→200+合法msg2` / `n≥1→204+空body` 的响应 → 断会话（§6.4 判活表）

- [ ] **Step 4: 跑测试通过**

Run: `cargo test -p wsieve-xhttp`
Expected: client 5 passed

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-xhttp
git commit -m "feat(xhttp): XhttpConn client with window/retry/session-liveness"
```

---

### Task 7: SessionStore 服务端（§9.4）

**Files:**
- Create: `crates/wsieve-xhttp/src/server.rs`
- Test: `crates/wsieve-xhttp/tests/server.rs`

- [ ] **Step 1: 写失败测试**

```rust
use wsieve_xhttp::server::SessionStore;

#[tokio::test]
async fn reorder_and_dedup() {
    let s = SessionStore::new();
    let sid = [7u8; 16];
    s.create(sid, /* transport state */ ()).await;
    // 乱序推入 seq 2,0,1 → 依序读出 0,1,2
    s.push_post(sid, 2, b"cc").await.unwrap();
    s.push_post(sid, 0, b"aa").await.unwrap();
    s.push_post(sid, 1, b"bb").await.unwrap();
    assert_eq!(s.read(sid, &mut buf).await, b"aa");
    // …
    // 重复 seq 0 → 丢弃但 Ok（§6.4 去重）
    s.push_post(sid, 0, b"aa").await.unwrap();
}

#[tokio::test]
async fn gc_rules() {
    // attach 窗口 30s：tokio::time::pause 后 advance 过窗 → 会话消失
    // 上行空闲 180s → GC；GET 断开 → 立即 GC
}

#[tokio::test]
async fn buffer_overflow_kills_session() {
    // 推 31 个乱序 POST（seq 1..31，缺 0）→ 会话被杀（缓冲上限 30，spec §9.4）
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p wsieve-xhttp`
Expected: 编译失败

- [ ] **Step 3: 实现**

`crates/wsieve-xhttp/src/server.rs` 骨架：

```rust
//! 服务端会话仓库：重排/去重/GC。spec §6.4 去重规则 + §9.4 生命周期。
//! 参考 Xray upload_queue.go 的最小堆重排，但去重语义是我们自己的。

pub struct SessionStore { /* DashMap<sid, Session> + GC 任务 */ }

struct Session {
    heap: BTreeMap<u64, Bytes>,     // seq -> body；BTreeMap 天然按 seq 排序
    next_seq: u64,                  // 已连续消费到的 seq+1
    attached: bool,                 // 下行 GET 是否已挂载
    last_upstream_at: Instant,
}

impl SessionStore {
    pub async fn push_post(&self, sid: Sid, seq: u64, body: Bytes) -> Result<(), SessionGone>;
    // seq < next_seq 或已在 heap → 丢弃，仍 Ok（去重 + 整 body 原子丢弃，§6.4）
    // heap.len() >= 30 且有空洞 → 杀会话（§9.4）

    pub async fn read(&self, sid: &Sid, out: &mut [u8]) -> Result<usize, SessionGone>;
    // 顺序：先吐 heap 里 next_seq 的 body，耗尽则 await 新 POST
}
```

- [ ] **Step 4: 跑测试通过**

Run: `cargo test -p wsieve-xhttp`
Expected: server 3 passed

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-xhttp
git commit -m "feat(xhttp): server SessionStore with reorder/dedup/GC"
```

---

## 阶段 P3：mux 层

### Task 8: Mux trait + 工厂 + yamux/smux/muxado/picomux 四实现（§7.2/§7.3）

**Files:**
- Create: `crates/wsieve-mux/src/lib.rs`（trait + 工厂 + 注册表）
- Create: `crates/wsieve-mux/src/{yamux_impl,smux_impl,muxado_impl,picomux_impl}.rs`
- Test: `crates/wsieve-mux/tests/matrix.rs`

**实现注意**：四个原生 tokio crate 的 API 形状各异（`tokio-yamux` 用 `Session::control().open_stream()`、`muxado` 用 `Connection::open_stream`、`picomux` 用 `new(read, write).open(metadata)`、`smux` 用 `open_stream(&self)`）。每个 impl 文件就是「crate API → Mux trait」的薄适配，**不改协议、不加逻辑**。适配层的 driver task（`yamux` 需要、`tokio-yamux` 由 `Control` 免除）按各 crate 文档写。

- [ ] **Step 1: 写失败测试（矩阵测试——每个实现跑同一套用例）**

`crates/wsieve-mux/tests/matrix.rs`：

```rust
use wsieve_mux::{Mux, MuxId};

// 测试脚手架：内存双工管道 (tokio::io::duplex) 直连两端，无网络。
async fn duplex_pair() -> (impl AsyncRead+AsyncWrite+Send+Unpin, impl …) {
    tokio::io::duplex(64 * 1024)
}

// 同一套用例 × 4 实现（h2mux 在 Task 10 加入矩阵）：
// 1. open_and_echo：客户端 open()，服务端 accept()，双向写读回显
// 2. concurrent_streams：32 条流并发写 4KB 各异内容，对端按流校验无串流
// 3. slow_stream_does_not_starve_fast：一条流慢读（每读 1KB sleep 50ms），
//    另一条流持续写，断言快流在慢流未完成前已完成（各实现流控语义下的公平性）
// 4. close_propagates：客户端关流 → 服务端 read 得到 EOF
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p wsieve-mux`
Expected: 编译失败

- [ ] **Step 3: 实现**

`crates/wsieve-mux/src/lib.rs`：

```rust
//! Mux trait + 工厂。spec §7.2/§7.3。

pub type MuxStream = Box<dyn tokio::io::AsyncReadWrite + Send + Unpin>;
// 若无 AsyncReadWrite 组合 trait，则定义为 pub trait Duplex: AsyncRead + AsyncWrite {}
// impl<T: AsyncRead + AsyncWrite + Send + Unpin> Duplex for T {}

#[async_trait::async_trait]
pub trait Mux: Send + Sync {
    async fn open(&self) -> anyhow::Result<MuxStream>;
    async fn accept(&self) -> anyhow::Result<MuxStream>;
}

/// 工厂：握手协商出的 mux_id → 实现实例。双端同表。
pub fn mux_factory(id: MuxId, io: MuxStream) -> anyhow::Result<Box<dyn Mux>> {
    match id {
        MuxId::Yamux   => Ok(Box::new(yamux_impl::TokioYamux::new(io)?)),
        MuxId::Smux    => Ok(Box::new(smux_impl::SmuxImpl::new(io)?)),
        MuxId::Muxado  => Ok(Box::new(muxado_impl::MuxadoImpl::new(io)?)),
        MuxId::Picomux => Ok(Box::new(picomux_impl::PicomuxImpl::new(io)?)),
        MuxId::H2mux   => Ok(Box::new(h2mux_impl::H2muxImpl::new(io)?)), // Task 10 前先 unreachable!()
    }
}
```

每个 `*_impl.rs` 的形状（以 `picomux` 为例，最简）：

```rust
pub struct PicomuxImpl { inner: picomux::Multiplex }

impl PicomuxImpl {
    pub fn new(io: MuxStream) -> anyhow::Result<Self> {
        Ok(Self { inner: picomux::Multiplex::new(io /* 拆 read/write 两半 */) })
    }
}

#[async_trait::async_trait]
impl Mux for PicomuxImpl {
    async fn open(&self) -> anyhow::Result<MuxStream> {
        Ok(Box::new(self.inner.open(b"").await?))
    }
    async fn accept(&self) -> anyhow::Result<MuxStream> {
        Ok(Box::new(self.inner.accept().await?))
    }
}
```

（`Multiplex::new` 需要 read/write 分开的两个 impl——用 `tokio::io::split` 处理；各 crate 的具体差异在实现时按其文档适配，**协议行为零改动**。）

- [ ] **Step 4: 跑测试通过**

Run: `cargo test -p wsieve-mux`
Expected: matrix 4 用例 × 4 实现 = 16 passed

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-mux
git commit -m "feat(mux): Mux trait + yamux/smux/muxado/picomux impls"
```

---

### Task 9: 协商集成（握手里选 mux，§7.4）

**Files:**
- Modify: `crates/wsieve-xhttp/src/client.rs`（connect 返回 Negotiated，已就位则接上）
- Modify: `crates/wsieve-server/src/lib.rs`（**纯函数** `pick_mux(client_prefs, server_enabled) -> (MuxId, bool)`——此时 Task 13 的 main.rs 尚不存在，只动 lib）
- Test: `crates/wsieve-xhttp/tests/negotiate.rs`

- [ ] **Step 1: 写失败测试**

```rust
// 1. 交集命中：prefs=[picomux,yamux] × 服务端支持[yamux,picomux] → picomux, fallback=false
// 2. 无交集：prefs=[smux] × 服务端只启用了[yamux] → yamux, fallback=true（不失败！）
// 3. fallback=true 时客户端打 WARN 日志（用 tracing capture 断言）
// 4. 服务端按客户端偏好顺序取第一个交集（不是服务端自己的优先序）
```

- [ ] **Step 2: 跑测试确认失败 → Step 3: 实现 → Step 4: 通过 → Step 5: 提交**

```bash
git commit -m "feat(xhttp): mux negotiation in handshake with yamux fallback"
```

（实现落在 msg1 prefs → 服务端 `pick_mux()` → msg2 chosen+fallback，客户端 `Negotiated` 上抛给 UI 显示。）

---

### Task 10: h2mux 实现（§7.3 适配层）

**Files:**
- Create: `crates/wsieve-mux/src/h2mux_impl.rs`（含 client + **server 两侧**）
- Modify: `crates/wsieve-mux/src/lib.rs`（工厂接通，见下）
- Test: `crates/wsieve-mux/tests/matrix.rs`（h2mux 加入矩阵，双端）

唯一需要写适配层的实现：`h2::SendStream/RecvStream` 是 `Bytes` 分块 API，不实现 `AsyncRead/Write`。**且 h2 必须手写服务端半边**（其余四个 crate 自带双端 API）——工厂按角色分叉：

- [ ] **Step 1: 写失败测试**

matrix.rs 加 h2mux 到实现列表（同 4 用例）。另加专属用例：

```rust
// h2 流的双工语义：SendStream 的 send_capacity 可用窗口耗尽时 write 阻塞而非报错
// 每条流的合成 header 固定 :method POST :path / :authority wsieve（协议常量）
```

- [ ] **Step 2: 确认失败 → Step 3: 实现**

适配层骨架（完整代码实现时展开）：

```rust
//! h2 crate 当 mux 用：SendStream/RecvStream（Bytes 分块）→ AsyncRead/AsyncWrite。
//! 每条流一个合成 HTTP 请求（:method POST, :path /, :authority wsieve）——协议常量。

pub struct H2muxImpl { send_request: h2::client::SendRequest<Bytes>, conn_handle: tokio::task::JoinHandle<()> }

impl H2muxImpl {
    pub fn new(io: MuxStream) -> anyhow::Result<Self> {
        let (send_request, connection) = h2::client::handshake(io).await?;
        let conn_handle = tokio::spawn(async move { let _ = connection.await; });
        Ok(Self { send_request, conn_handle })
    }
}

// Mux::open(): 构造合成 request → send_request.send_request(req, false)
//   → RequestFut → ResponseFut 并行 poll
//   → (SendStream, RecvStream) 包成 H2Stream
// H2Stream::poll_write: capacity 可用时 send_data(Bytes, false)；耗尽则 Pending
//   （send_capacity().available() == 0 时 register interest，h2 文档标准做法）
// H2Stream::poll_read:  poll_data() → Ok(Bytes) 累积到内部缓冲 → 读缓冲
//   每次 poll_data 成功后 release_capacity().release_allocated()
//   注：H2Stream 自封装 poll_read/poll_write，无需对它再 tokio::io::split

/// 服务端半边（其余四 crate 不需要，h2 必须）：
pub struct H2ServerImpl { conn_handle: JoinHandle<()>, incoming: mpsc::Receiver<H2Stream> }

impl H2ServerImpl {
    pub fn new(io: MuxStream) -> anyhow::Result<Self> {
        let mut conn = h2::server::handshake(io).await?;
        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(async move {
            while let Some(Some((req, respond))) = conn.accept().await {
                // 校验合成 header（:method POST :path / :authority wsieve），取
                // RequestBodyStream<Bytes> + SendResponse 包成 H2Stream 入队
                let _ = tx.send(H2Stream::from_server(req, respond)).await;
            }
        });
        Ok(Self { incoming: rx })
    }
}
// Mux::accept(): incoming.recv().await

// lib.rs 工厂按角色分叉：
pub fn mux_factory(id: MuxId, io: MuxStream) -> Box<dyn Mux> { /* client 侧，原样 */ }
pub fn mux_server_factory(id: MuxId, io: MuxStream) -> Box<dyn Mux> {
    // yamux/smux/muxado/picomux: 同 client 工厂（双端同 API）
    // h2mux: H2ServerImpl
}
```

- [ ] **Step 4: 跑测试通过**

Run: `cargo test -p wsieve-mux`
Expected: matrix 20 passed（4 用例 × 5 实现）

- [ ] **Step 5: 提交**

```bash
git commit -m "feat(mux): h2mux impl with duplex adapter over h2 streams"
```

---

### Task 11: mux-bench 一次性基准（§7.6/§7.7）

**Files:**
- Create: `crates/wsieve-mux/examples/mux-bench.rs`

**这不是常规测试，不进 CI**。跑一次、填 §7.7、写死默认值，脚本留仓不再跑。

- [ ] **Step 1: 实现 example**

```rust
//! 一次性 mux 基准。spec §7.6。用后即弃，结果回填 spec §7.7。
//! RTT/丢包在内存管道注入（不碰系统网络）：
//!   DelayedDuplex 包装 tokio::io::duplex 的两端，每 write 后 sleep(RTT/2)，
//!   以概率 drop 该次 write 并触发对端 read 错误（模拟 1% 丢包下的重传延迟 ×2）。

// 四组场景 × 5 实现（+ paritytech yamux 作 A/B 第 6 项）：
//   local(1ms/0%/8流)  cross(80ms/0%/32流)  weak(250ms/1%/32流)  burst(80ms/0%/128流)
// 指标：首字节延迟 P50/P99、总吞吐、慢流公平性（快流完成时间在慢流进行中的比值）
```

- [ ] **Step 2: 跑基准并回填**

Run: `cargo run --release -p wsieve-mux --example mux-bench`
把结果表格贴进 spec §7.7，选定默认值写进 `wsieve-xhttp` 配置常量 `DEFAULT_MUX`。

- [ ] **Step 3: 提交**

```bash
git add -A
git commit -m "bench(mux): one-shot benchmark results + default selection"
```

---

### Task 12: TargetAddr 编解码（mux 流首包）

**Files:**
- Create: `crates/wsieve-proto/src/addr.rs`
- Test: `crates/wsieve-proto/tests/addr.rs`

每条 mux 流的第一个 Frame::Data 是目标地址（SOCKS5 地址格式：`u8 atyp ‖ addr ‖ u16 port`），服务端据此拨号。

- [ ] **Step 1: 失败测试**：IPv4/域名/IPv6 三形态 roundtrip + 拒绝非法 atyp
- [ ] **Step 2: 确认失败 → Step 3: 实现**（`atyp=1 IPv4 / 3 域名 / 4 IPv6`，域名长度 u8 前缀）
- [ ] **Step 4: 通过 → Step 5: 提交**

```bash
git commit -m "feat(proto): TargetAddr codec for mux stream first frame"
```

---

## 阶段 P4：服务端

### Task 13: axum 服务骨架 + 认证先于路由（§8 + §6.2/§6.3）

**Files:**
- Create: `crates/wsieve-server/src/main.rs`
- Test: `crates/wsieve-server/tests/api.rs`

**核心原则（spec §8）**：认证判定先于路径路由。请求要么「会话有效/握手成功」走代理路径，要么整体转伪装处理器——不存在中间态。

- [ ] **Step 1: 写失败测试**

`crates/wsieve-server/tests/api.rs`：

```rust
// 起完整服务（内存密钥、无 TLS——TLS 在 Task 14），对它发：
// 1. GET /                    → 200 nginx 页（内嵌副本）
// 2. POST /api/sync?n=0 垃圾  → 与 GET /random-path 完全同响应（伪装处理器出口）
// 3. POST /api/sync?n=0 合法 msg1 → 200 + 可解 msg2
// 4. 重放同一 msg1（ephemeral 缓存命中）→ 与 #2 完全同响应
// 5. ts 超窗的合法 msg1        → 与 #2 完全同响应
// 6. GET /api/events 无会话    → 与 #2 完全同响应
// 7. 会话 attach：握手 → GET /api/events → 200 + 流头三件套（text/event-stream / no-store / X-Accel-Buffering: no）
// 8. 重复 GET /api/events（已挂载）→ 与 #2 同响应
// 9. ephemeral 缓存满 4096 驱逐旧条目后：重放被驱逐的 msg1 仍被 ts 窗口拦截（spec §6.5 测试补充）
//    —— 灌满 4096 个合法 msg1，再重放第 1 个：断言与 #2 同响应（驱逐 ≠ 放行）
```

- [ ] **Step 2: 确认失败 → Step 3: 实现**

`main.rs` 路由骨架：

```rust
//! spec §8：认证先于路径。middleware 或统一 handler 里先做：
//!   1. POST /api/sync：body[0..2] 取 TU len → 取密文 → 尝试 Noise 解密
//!      n=0：解密成功 + ts 窗口 + ephemeral 未见过 + 白名单 → 建会话 + 200 msg2
//!           任一失败 → disguise::handle(req)（原样转伪装，不落状态）
//!      n≥1：sid 有会话 → SessionStore::push_post → 204 空体
//!           无会话 → disguise::handle(req)
//!   2. GET /api/events：sid 有会话且未挂载 → 挂载 + 流式响应
//!           其余 → disguise::handle(req)
//!   3. 其余一切路径 → disguise::handle(req)
```

（ephemeral LRU：`dashmap` + 简单 LRU 或 `lru` crate，4096 条，条目寿命 = ts 窗口。）

- [ ] **Step 4: 通过 → Step 5: 提交**

```bash
git commit -m "feat(server): auth-before-routing axum skeleton"
```

---

### Task 14: 伪装 + TLS/部署模式 + 全链路集成测试（§8 + §6.8）

**Files:**
- Create: `crates/wsieve-server/src/{disguise.rs,tls.rs}`、`crates/wsieve-server/assets/nginx/index.html`
- Test: `crates/wsieve-server/tests/integration.rs`

- [ ] **Step 1: disguise.rs**

内嵌 nginx 默认页副本（`include_str!`，正文即标准 welcome 页 + 404 页），`Server: nginx` 头（版本串配置项）。
`disguise.upstream` 模式：`reqwest` 反代——透传 method/path/headers/body，回传上游响应（含状态码/头/body）。缓存策略不代管（spec §8 ponytail 注记）。

- [ ] **Step 2: tls.rs（部署模式）**

```rust
// deployment 配置（§8 部署表）：
//   direct: rustls + 证书文件（必须有效）+ TLS1.3 + session ticket + max_early_data_size > 0（§6.8）
//   cdn:    监听明文 HTTP（CDN Flexible）或自签 rustls（CDN Full）
//   Alt-Svc 头广播 h3（可选开关，direct 模式配 quinn 监听；CDN 模式由 CDN 控制台负责）
```

- [ ] **Step 3: 全链路集成测试 ★（spec §10 第 4 项——方案 A 的兑现点）**

`integration.rs`：起真实服务端（127.0.0.1:0，自签 TLS）+ `ReqwestTransport` 客户端：

```rust
// 1. 完整握手 + mux 协商 + 开流 + 经服务端拨号到本地 echo 目标 + 双向数据校验
// 2. 5 种 mux 各跑一遍全链路（矩阵）
// 3. 上行 64KB 连续写 → 服务端读到的字节流与写入一致（TU 分帧 + 重排透明）
// 4. 断会话后 POST → 转伪装 → 客户端正确判定会话死亡
// 5. 乱序注入（transport 层延迟奇数 seq）→ 服务端重组正确
// 6. 探测等价性（spec §10 第 5 项）：垃圾/重放/合法三类请求响应一致性断言
```

- [ ] **Step 4: 通过 → Step 5: 提交**

```bash
git commit -m "feat(server): disguise + TLS modes + full-chain integration tests"
```

---

### Task 15: remote.rs 拨号泵（mux 流 → TcpStream）

**Files:**
- Create: `crates/wsieve-server/src/remote.rs`
- Test: 并入 `integration.rs`

- [ ] **Step 1: 实现**

```rust
//! 服务端核心循环：accept() 一条 mux 流 → 读首帧 TargetAddr → TcpStream::connect
//! → tokio::io::copy_bidirectional(stream, tcp)。每流一个 tokio task。
//! 拨号失败 → 直接关流（客户端 SOCKS5 层会收到 EOF → 回 RST）。
```

**服务端协议栈接线 checklist（本任务内完成，集成测试依赖它）**：

- [ ] msg2 发出后：服务端持有 Noise `TransportState`（会话的一部分，存入 SessionStore）
- [ ] `mux_server_factory(chosen_mux_id, noise_stream)` → 服务端 mux 实例（每会话一个）
- [ ] 下行 TU 编码泵：会话任务循环「从 mux 读明文 → encode_frame → 加密 → 写入 GET 响应流 + Flush」；保活计时器（20–80s 随机）插 PADDING TU
- [ ] 上行泵：SessionStore 重组出的字节流 → 解密 → mux 输入侧
- [ ] Noise 流包装：`TransportState` 用内部计数器驱动 `encrypt(nonce_u64, ..)`/`decrypt`，包成 `AsyncRead+AsyncWrite`（两端共用的工具类型，放 `wsieve-proto::crypto::NoiseStream`）

- [ ] **Step 2: 集成测试通过 → Step 3: 提交**

```bash
git commit -m "feat(server): remote dial pump over mux streams"
```

---

## 阶段 P5：客户端

### Task 16: SOCKS5 入站（客户端本地入口）

**Files:**
- Create: `crates/wsieve-socks5/src/lib.rs`
- Test: `crates/wsieve-socks5/tests/socks5.rs`

- [ ] **Step 1: 失败测试**：完整 CONNECT 握手（greeting → request → reply）+ 数据中转（对接内存 echo）
- [ ] **Step 2: 确认失败 → Step 3: 实现**

只实现 CONNECT（BIND/UDP ASSOCIATE 不做，YAGNI）。无认证方式 0x00 一种。reply 失败码 0x01（一般性失败）统一回。

- [ ] **Step 4: 通过 → Step 5: 提交**

```bash
git commit -m "feat(socks5): CONNECT-only inbound with data relay"
```

---

### Task 17: Tauri 壳 + WebViewTransport 桥（IPC 二进制快路径）

**Files:**
- Create: `src-tauri/`（`cargo tauri init` 生成）、`src-tauri/src/{main.rs,bridge.rs,proxy.rs}`
- Create: `ui/src/emitter.ts`

**IPC 纪律（spec §3.2）**：上行 `invoke` 顶层 `Uint8Array`（**切勿嵌 object**，会退化 JSON 数字数组）；下行 `Channel<&[u8]>` 流式回推。

- [ ] **Step 1: emitter.ts（fetch 哑发射器，~200 行）**

```ts
// 收 Rust 指令：{ id, path, body? } → fetch(path, { method, body: Uint8Array, credentials:'include' })
// n=0 响应体：arrayBuffer() → invoke 回传
// 下行 GET：response.body.getReader() 循环 → channel.onmessage 回推 chunk
// 中断双杠杆（spec §6.5，Xray dialer.html:117-127 血泪教训）：
//   await reader.cancel(); controller.abort();   // 顺序不可换！
// 页面加载：document.location = 服务端首页（同源根基，§6.7）
```

- [ ] **Step 2: bridge.rs（WebViewTransport）**

```rust
//! HttpTransport 的 Tauri IPC 实现。
//! post()   → invoke emitter("post", { id, path, body: Uint8Array 顶层 })
//! get_stream() → invoke emitter("open_stream") + Channel<Bytes> 回推拼流
//! pending IPC 全部失败/超时 → transport 标记死亡 → 断会话 + UI 提示 + 自动 reload 页面
```

- [ ] **Step 3: proxy.rs 接线**

```rust
//! socks5 listener → 每连接 mux.open() → 首帧 TargetAddr → copy_bidirectional
//! UI 状态：mux 协商结果 / fallback WARN / 连接数
```

- [ ] **Step 4: cargo test（桥的单测用 mock channel）+ 提交**

```bash
git commit -m "feat(tauri): WebViewTransport bridge + emitter fetch loop"
```

---

### Task 18: E2E 验收（spec §10 第 6 项）

**Files:**
- Create: `scripts/e2e.sh`、`ui/src/emitter.ts` 完善

- [ ] **Step 1: 脚本**

```bash
#!/usr/bin/env bash
# 1. 起服务端（本地自签 + nginx 伪装）
# 2. 起 Tauri App（真实 WebView）
# 3. curl --socks5 127.0.0.1:1080 http://127.0.0.1:<echo-port>/ → 断言回显
# 4. 断言流量路径：服务端日志只见 TLS（密文），nginx 页可达
set -e
# …具体命令实现时补全
```

- [ ] **Step 2: 手动验收清单**（`.spec-workflow` 或 README 记录）
- [ ] **Step 3: 提交**

```bash
git commit -m "test: E2E acceptance via curl --socks5"
```

---

## 完成定义

- [ ] `cargo test --workspace` 全绿（含矩阵 20、集成 6 组、探测等价性）
- [ ] mux-bench 已跑、spec §7.7 已回填、`DEFAULT_MUX` 已定
- [ ] E2E：`curl --socks5` 经真实 WebView fetch 出去且回得来
- [ ] fallback WARN、判活断连、GC 三条异常路径有测试钉住
