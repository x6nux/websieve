# 阶段 1：配置与路由引擎 — 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 建立三个纯 Rust 库 crate —— GEO 数据解析、Clash 规则解析与两阶段路由引擎、YAML 配置读写（含注释保留），并以一个 CLI example 验证「给定目标 → 打印判决」的完整链路。

**Architecture:** 三个 crate 各自零 async、零网络、零 Tauri 依赖，因此可被穷举单测。`wsieve-geo` 无内部依赖；`wsieve-route` 依赖 geo 与 proto；`wsieve-config` 独立（只管 YAML，不解析规则语义）。路由引擎按设计文档 §4.2 的两阶段协议工作：需要 DNS 时把需求作为返回值抛给调用方，自身保持同步纯函数。

**Tech Stack:** Rust 2021 · 手写 protobuf wire format 解析（零依赖）· `serde-saphyr` 1.1（YAML 读）· `Span::byte_offset` 定点改写（YAML 写）· `ipnet` 仅用于 CIDR 文本解析

**依据:** `docs/superpowers/specs/2026-08-24-client-routing-and-ui-design.md` §4.2 §5 §6 §13 §14

---

## 前置阅读（实现者必读）

在动手前请读设计文档的这几节，它们是本计划每个决定的出处：

- **§4.2 纪律①** — 两阶段求值协议与 `Verdict` 签名。这是本阶段最核心的类型
- **§5.5** — 缺 `MATCH` 即报错，不设隐式默认
- **§5.6** — 注释保留策略与其实现约束（**不能用 `Commented<T>`**）
- **§6.1 / §6.2** — 规则类型表与判决流程
- **§6.5 / §6.6** — 数据结构选型与「手写 protobuf 解析」的理由

**项目既有惯例（请遵守）：**

- 代码注释用**中文**，与仓库现有代码一致
- 刻意的简化用 `ponytail:` 注释标注并写明上限与升级路径 —— 参见 `docs/superpowers/specs/2026-08-23-webview-https-proxy-design.md` §9.3 的既有用法
- 错误绝不静默跳过。参照 `src-tauri/src/shard.rs:214` 的测试 `port_conflict_is_reported_not_skipped`

---

## 文件结构

```
crates/wsieve-geo/                 新建 —— GEO 数据，无内部依赖
  Cargo.toml
  src/lib.rs                       公开门面 GeoDb（懒加载）
  src/pb.rs                        protobuf wire format 最小读取器
  src/site.rs                      GeoSiteList 解析 + 域名匹配集
  src/ip.rs                        GeoIPList 解析 + CIDR 区间集
  tests/fixtures.rs                构造小样本 .dat 的辅助（测试专用）

crates/wsieve-route/               新建 —— 规则与引擎
  Cargo.toml
  src/lib.rs
  src/rule.rs                      Clash 规则语法解析
  src/engine.rs                    evaluate() 两阶段求值
  examples/route.rs                CLI 验证入口
  tests/engine.rs                  判决矩阵穷举

crates/wsieve-config/              新建 —— YAML 读写
  Cargo.toml
  src/lib.rs
  src/model.rs                     serde 结构定义
  src/edit.rs                      基于字节 span 的规则定点改写
  tests/roundtrip.rs               注释保留往返

Cargo.toml                         修改：workspace members 增加三项
```

**为什么拆三个而不是一个**：`wsieve-geo` 完全独立（将来可单独发布或复用），`wsieve-config` 不需要知道规则语义（它只把 `rules` 当字符串数组），`wsieve-route` 是唯一同时需要二者的。按职责切，不按技术层切。

---

## Part A — `wsieve-geo`

### Task 1: 建立 crate 骨架

**Files:**
- Create: `crates/wsieve-geo/Cargo.toml`
- Create: `crates/wsieve-geo/src/lib.rs`
- Modify: `Cargo.toml`（workspace members）

- [ ] **Step 1: 创建 Cargo.toml**

```toml
[package]
name = "wsieve-geo"
version = "0.1.0"
edition = "2021"
description = "geosite.dat / geoip.dat 解析与匹配（手写 protobuf，零依赖）"

[dependencies]
thiserror = { workspace = true }

[dev-dependencies]
```

- [ ] **Step 2: 注册进 workspace**

在根 `Cargo.toml` 的 `members` 数组里，`"crates/wsieve-proto"` 之前插入一行：

```toml
    "crates/wsieve-geo",
```

- [ ] **Step 3: 写占位 lib.rs**

```rust
//! geosite.dat / geoip.dat 的解析与匹配。
//!
//! 这两个文件是 v2ray 系的事实标准格式，protobuf 编码。结构极简
//! （枚举 + 字符串 + bytes + uint32，三层嵌套），因此手写 wire format
//! 解析而不引入 prost —— 与仓库里手写 base64 的取舍一致
//! （见 src-tauri/src/bridge.rs:285 的注释）。
//!
//! 权威定义见 .research/Xray-core/common/geodata/geodat.proto。

pub mod ip;
pub mod pb;
pub mod site;
```

- [ ] **Step 4: 验证能编译**

Run: `cargo check -p wsieve-geo`
Expected: 报 `file not found for module ip/pb/site` —— 这是预期的，下一个 task 补上。先确认 workspace 注册生效（不该出现 "package not found"）。

- [ ] **Step 5: 提交**

```bash
git add Cargo.toml crates/wsieve-geo/
git commit -m "chore(geo): 建立 wsieve-geo crate 骨架"
```

---

### Task 2: protobuf wire format 读取器

**Files:**
- Create: `crates/wsieve-geo/src/pb.rs`

只实现我们真正会遇到的三种 wire type，其余走 `skip`。**未知字段必须能安全跳过**——geo 文件将来加字段时我们不能崩。

- [ ] **Step 1: 写失败的测试**

在 `crates/wsieve-geo/src/pb.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_decodes_multibyte() {
        // 300 = 0b10_0101100 → varint 编码 [0xAC, 0x02]
        let mut r = Reader::new(&[0xAC, 0x02]);
        assert_eq!(r.varint().unwrap(), 300);
        assert!(r.is_empty());
    }

    #[test]
    fn tag_splits_field_and_wire() {
        // field 2, wire 2 → (2<<3)|2 = 0x12
        let mut r = Reader::new(&[0x12]);
        assert_eq!(r.tag().unwrap(), (2, 2));
    }

    #[test]
    fn bytes_reads_length_delimited() {
        let mut r = Reader::new(&[0x03, b'a', b'b', b'c']);
        assert_eq!(r.bytes().unwrap(), b"abc");
    }

    #[test]
    fn truncated_input_errors_not_panics() {
        // 声明 5 字节却只给 2 —— 必须报错而不是 panic 或静默截断
        let mut r = Reader::new(&[0x05, b'a', b'b']);
        assert!(matches!(r.bytes(), Err(PbError::Truncated)));
    }

    #[test]
    fn unknown_field_is_skipped_safely() {
        // 未知 field 9 wire 0（varint 300），后跟 field 1 wire 2 ("hi")
        let mut r = Reader::new(&[0x48, 0xAC, 0x02, 0x0A, 0x02, b'h', b'i']);
        let (f, w) = r.tag().unwrap();
        assert_eq!((f, w), (9, 0));
        r.skip(w).unwrap();
        let (f, w) = r.tag().unwrap();
        assert_eq!((f, w), (1, 2));
        assert_eq!(r.bytes().unwrap(), b"hi");
    }

    #[test]
    fn varint_overflow_errors() {
        // 11 个带续位的字节 —— 超过 u64 能表示的范围
        let mut r = Reader::new(&[0xFF; 11]);
        assert!(r.varint().is_err());
    }

    #[test]
    fn overlong_tenth_byte_errors_instead_of_truncating() {
        // 10 字节 varint 的第 10 字节只能是 0 或 1。给 0x02 意味着这个数
        // 超出 u64，必须报错而不是把高位静默移出去 —— 上一个测试用 11 个
        // 字节，覆盖的是第 11 字节那条路径，覆盖不到这里。
        let mut buf = vec![0xFF; 9];
        buf.push(0x02);
        let mut r = Reader::new(&buf);
        assert!(matches!(r.varint(), Err(PbError::VarintOverflow)));
    }

    #[test]
    fn maximal_valid_varint_still_decodes() {
        // 反向守住：合法的 u64::MAX（第 10 字节恰为 0x01）不能被误判为溢出。
        // 只加守卫不加这条，很容易把边界上的合法值一起挡掉。
        let mut buf = vec![0xFF; 9];
        buf.push(0x01);
        let mut r = Reader::new(&buf);
        assert_eq!(r.varint().unwrap(), u64::MAX);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p wsieve-geo --lib pb::`
Expected: 编译失败，`cannot find type Reader in this scope`

- [ ] **Step 3: 写实现**

把这段放在 `pb.rs` 的测试模块**之前**：

```rust
//! protobuf wire format 的最小读取器。
//!
//! 只覆盖 geodat.proto 实际用到的部分：varint(0)、length-delimited(2)，
//! 外加 64-bit(1) / 32-bit(5) 的跳过能力。groups（3/4）已在 proto3 废弃，
//! 遇到即报错。

#[derive(Debug, thiserror::Error)]
pub enum PbError {
    #[error("数据被截断")]
    Truncated,
    #[error("varint 超过 64 位")]
    VarintOverflow,
    #[error("不支持的 wire type: {0}")]
    BadWireType(u8),
    #[error("字符串不是合法 UTF-8")]
    BadUtf8,
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    pub fn varint(&mut self) -> Result<u64, PbError> {
        let mut v = 0u64;
        let mut shift = 0u32;
        loop {
            let b = *self.buf.get(self.pos).ok_or(PbError::Truncated)?;
            self.pos += 1;
            if shift >= 64 {
                return Err(PbError::VarintOverflow);
            }
            // protobuf 的 u64 varint 最多 10 字节：前 9 个各贡献 7 位（63 位），
            // 第 10 个只剩 1 位可用。此时高位非零说明这个数超出 u64 ——
            // 必须报错，不能让 << 63 把它们静默移出去（房规：错误绝不静默）。
            if shift == 63 && b & 0x7f > 1 {
                return Err(PbError::VarintOverflow);
            }
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
        }
    }

    /// 返回 (field_number, wire_type)。
    pub fn tag(&mut self) -> Result<(u32, u8), PbError> {
        let t = self.varint()?;
        Ok(((t >> 3) as u32, (t & 7) as u8))
    }

    /// 读一段 length-delimited 数据，返回借用切片（零拷贝）。
    pub fn bytes(&mut self) -> Result<&'a [u8], PbError> {
        let len = self.varint()? as usize;
        let end = self.pos.checked_add(len).ok_or(PbError::Truncated)?;
        let s = self.buf.get(self.pos..end).ok_or(PbError::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    pub fn string(&mut self) -> Result<String, PbError> {
        let b = self.bytes()?;
        String::from_utf8(b.to_vec()).map_err(|_| PbError::BadUtf8)
    }

    /// 跳过一个不认识的字段。geo 文件将来加字段时，我们必须还能读。
    pub fn skip(&mut self, wire: u8) -> Result<(), PbError> {
        match wire {
            0 => {
                self.varint()?;
            }
            1 => self.advance(8)?,
            2 => {
                self.bytes()?;
            }
            5 => self.advance(4)?,
            other => return Err(PbError::BadWireType(other)),
        }
        Ok(())
    }

    fn advance(&mut self, n: usize) -> Result<(), PbError> {
        let end = self.pos.checked_add(n).ok_or(PbError::Truncated)?;
        if end > self.buf.len() {
            return Err(PbError::Truncated);
        }
        self.pos = end;
        Ok(())
    }
}
```

在 `Cargo.toml` 的 `[dependencies]` 确认已有 `thiserror`。根 workspace 已定义 `thiserror = "2"`，故用 `thiserror = { workspace = true }` 即可。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p wsieve-geo --lib pb::`
Expected: 8 个测试全部 PASS

> 注意：`lib.rs` 里已声明 `pub mod ip;` 与 `pub mod site;`，但那两个文件要到 Task 3/4 才写。本步骤前先建两个**空文件**占位，否则整个 crate 编译不过。

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-geo/src/pb.rs
git commit -m "feat(geo): protobuf wire format 最小读取器"
```

---

### Task 3: 域名匹配集与 GeoSiteList 解析

**Files:**
- Create: `crates/wsieve-geo/src/site.rs`

proto 定义（`.research/Xray-core/common/geodata/geodat.proto:9-46`）：

```proto
message Domain {
  enum Type { Substr = 0; Regex = 1; Domain = 2; Full = 3; }
  Type type = 1;  string value = 2;  repeated Attribute attribute = 3;
}
message GeoSite     { string code = 1; repeated Domain domain = 2; }
message GeoSiteList { repeated GeoSite entry = 1; }
```

四种 Type 到 Clash 语义的对应：`Full` = 精确、`Domain` = 后缀（含自身）、`Substr` = 关键词、`Regex` = 正则。

- [ ] **Step 1: 写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// 手工编码一个最小的 GeoSiteList，避免测试依赖外部 .dat 文件。
    fn encode_geosite_list(entries: &[(&str, &[(u8, &str)])]) -> Vec<u8> {
        fn varint(v: u64, out: &mut Vec<u8>) {
            let mut v = v;
            loop {
                let b = (v & 0x7f) as u8;
                v >>= 7;
                if v == 0 {
                    out.push(b);
                    return;
                }
                out.push(b | 0x80);
            }
        }
        fn field(num: u32, wire: u8, out: &mut Vec<u8>) {
            varint(((num as u64) << 3) | wire as u64, out);
        }
        fn delimited(num: u32, payload: &[u8], out: &mut Vec<u8>) {
            field(num, 2, out);
            varint(payload.len() as u64, out);
            out.extend_from_slice(payload);
        }

        let mut list = Vec::new();
        for (code, domains) in entries {
            let mut site = Vec::new();
            delimited(1, code.as_bytes(), &mut site); // GeoSite.code
            for (kind, value) in *domains {
                let mut dom = Vec::new();
                field(1, 0, &mut dom); // Domain.type
                varint(*kind as u64, &mut dom);
                delimited(2, value.as_bytes(), &mut dom); // Domain.value
                delimited(2, &dom, &mut site); // GeoSite.domain
            }
            delimited(1, &site, &mut list); // GeoSiteList.entry
        }
        list
    }

    #[test]
    fn parses_codes_and_domains() {
        let buf = encode_geosite_list(&[
            ("cn", &[(3, "baidu.com"), (2, "qq.com")]),
            ("category-ads", &[(0, "doubleclick")]),
        ]);
        let db = SiteDb::parse(&buf).unwrap();
        assert!(db.has("cn"));
        assert!(db.has("category-ads"));
        assert!(!db.has("us"));
    }

    #[test]
    fn full_matches_exactly_only() {
        let buf = encode_geosite_list(&[("t", &[(3, "example.com")])]);
        let db = SiteDb::parse(&buf).unwrap();
        assert!(db.matches("t", "example.com"));
        assert!(!db.matches("t", "a.example.com"));
    }

    #[test]
    fn domain_matches_self_and_subdomains_on_label_boundary() {
        let buf = encode_geosite_list(&[("t", &[(2, "example.com")])]);
        let db = SiteDb::parse(&buf).unwrap();
        assert!(db.matches("t", "example.com"), "应匹配自身");
        assert!(db.matches("t", "a.b.example.com"), "应匹配多级子域");
        // 关键边界：不能把 notexample.com 当成 example.com 的子域
        assert!(!db.matches("t", "notexample.com"));
    }

    #[test]
    fn substr_matches_anywhere() {
        let buf = encode_geosite_list(&[("t", &[(0, "google")])]);
        let db = SiteDb::parse(&buf).unwrap();
        assert!(db.matches("t", "www.google.com"));
        assert!(db.matches("t", "googleapis.com"));
        assert!(!db.matches("t", "example.com"));
    }

    #[test]
    fn regex_entries_are_skipped_and_counted() {
        let buf = encode_geosite_list(&[("t", &[(1, ".*\\.example\\.com"), (3, "keep.com")])]);
        let db = SiteDb::parse(&buf).unwrap();
        assert_eq!(db.skipped_regex(), 1, "Regex 条目应被计数");
        assert!(db.matches("t", "keep.com"), "同类别的其他条目不受影响");
    }

    #[test]
    fn unknown_class_never_matches() {
        let buf = encode_geosite_list(&[("t", &[(3, "a.com")])]);
        let db = SiteDb::parse(&buf).unwrap();
        assert!(!db.matches("nonexistent", "a.com"));
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p wsieve-geo --lib site::`
Expected: `cannot find type SiteDb in this scope`

- [ ] **Step 3: 写实现**

放在测试模块之前：

```rust
//! geosite.dat 的解析与域名匹配。

use std::collections::{HashMap, HashSet};

use crate::pb::{PbError, Reader};

/// 一个类别（如 "cn"、"category-ads"）的域名匹配集。
#[derive(Default)]
struct SiteClass {
    /// Domain.Type = Full：精确匹配
    full: HashSet<String>,
    /// Domain.Type = Domain：后缀匹配，用反转标签 trie
    suffix: DomainTrie,
    /// Domain.Type = Substr：关键词包含。数量少，线性扫即可
    substr: Vec<String>,
}

pub struct SiteDb {
    classes: HashMap<String, SiteClass>,
    skipped_regex: usize,
}

impl SiteDb {
    pub fn parse(buf: &[u8]) -> Result<Self, PbError> {
        let mut classes: HashMap<String, SiteClass> = HashMap::new();
        let mut skipped_regex = 0usize;

        let mut r = Reader::new(buf);
        while !r.is_empty() {
            let (field, wire) = r.tag()?;
            if field != 1 || wire != 2 {
                r.skip(wire)?;
                continue;
            }
            // GeoSiteList.entry
            let entry = r.bytes()?;
            let (code, class, skipped) = parse_geosite(entry)?;
            skipped_regex += skipped;
            // 同一 code 出现多次时合并而非覆盖
            let slot = classes.entry(code).or_default();
            slot.full.extend(class.full);
            slot.substr.extend(class.substr);
            slot.suffix.merge(class.suffix);
        }
        Ok(Self { classes, skipped_regex })
    }

    pub fn has(&self, code: &str) -> bool {
        self.classes.contains_key(&normalize_code(code))
    }

    /// 类别不存在时返回 false —— 由调用方在加载阶段校验并报警，
    /// 判决路径上绝不因为一个缺失的类别而中断整条连接。
    pub fn matches(&self, code: &str, domain: &str) -> bool {
        let Some(class) = self.classes.get(&normalize_code(code)) else {
            return false;
        };
        let d = domain.trim_end_matches('.').to_ascii_lowercase();
        if class.full.contains(&d) {
            return true;
        }
        if class.suffix.matches(&d) {
            return true;
        }
        class.substr.iter().any(|s| d.contains(s.as_str()))
    }

    /// ponytail: 不支持 Domain.Type = Regex，解析时跳过并计数。
    /// 上限：带正则的 geosite 条目不会命中，表现为漏匹配（绝不会错匹配）。
    /// 升级路径：若实测漏匹配显著，引入 regex crate 并在此加一个 Vec<Regex>。
    pub fn skipped_regex(&self) -> usize {
        self.skipped_regex
    }
}

fn normalize_code(code: &str) -> String {
    code.trim().to_ascii_lowercase()
}

fn parse_geosite(buf: &[u8]) -> Result<(String, SiteClass, usize), PbError> {
    let mut code = String::new();
    let mut class = SiteClass::default();
    let mut skipped = 0usize;

    let mut r = Reader::new(buf);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, 2) => code = r.string()?,
            (2, 2) => {
                let dom = r.bytes()?;
                match parse_domain(dom)? {
                    Some((0, v)) => class.substr.push(v),
                    Some((2, v)) => class.suffix.insert(&v),
                    Some((3, v)) => {
                        class.full.insert(v);
                    }
                    Some((1, _)) => skipped += 1, // Regex
                    Some((_, _)) | None => skipped += 1,
                }
            }
            _ => r.skip(wire)?,
        }
    }
    Ok((normalize_code(&code), class, skipped))
}

/// 返回 (type, value)。attribute 字段整体跳过 —— 我们不做属性过滤。
fn parse_domain(buf: &[u8]) -> Result<Option<(u64, String)>, PbError> {
    let mut kind = 0u64;
    let mut value: Option<String> = None;

    let mut r = Reader::new(buf);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, 0) => kind = r.varint()?,
            (2, 2) => value = Some(r.string()?.trim_end_matches('.').to_ascii_lowercase()),
            _ => r.skip(wire)?,
        }
    }
    Ok(value.map(|v| (kind, v)))
}

/// 反转标签 trie：按 `.` 分段、从右向左插入与查询。
///
/// 这样「边界必须落在点上」这件事由结构本身保证，不需要额外的字符串比较 ——
/// notexample.com 与 example.com 在第一层（com）之后就分叉了。
#[derive(Default)]
struct DomainTrie {
    root: TrieNode,
}

#[derive(Default)]
struct TrieNode {
    children: HashMap<String, TrieNode>,
    terminal: bool,
}

impl DomainTrie {
    fn insert(&mut self, domain: &str) {
        let mut node = &mut self.root;
        for seg in domain.rsplit('.') {
            node = node.children.entry(seg.to_string()).or_default();
        }
        node.terminal = true;
    }

    fn matches(&self, domain: &str) -> bool {
        let mut node = &self.root;
        for seg in domain.rsplit('.') {
            match node.children.get(seg) {
                Some(n) => {
                    node = n;
                    // 命中一个终结节点即可 —— 它是查询域名的某级父域
                    if node.terminal {
                        return true;
                    }
                }
                None => return false,
            }
        }
        false
    }

    fn merge(&mut self, other: DomainTrie) {
        merge_node(&mut self.root, other.root);
    }
}

fn merge_node(dst: &mut TrieNode, src: TrieNode) {
    dst.terminal |= src.terminal;
    for (k, v) in src.children {
        merge_node(dst.children.entry(k).or_default(), v);
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p wsieve-geo --lib site::`
Expected: 6 个测试全部 PASS。特别确认 `domain_matches_self_and_subdomains_on_label_boundary` 通过——它锁住的是最容易写错的边界。

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-geo/src/site.rs
git commit -m "feat(geo): geosite 解析与域名匹配（反转标签 trie）"
```

---

### Task 4: CIDR 区间集与 GeoIPList 解析

**Files:**
- Create: `crates/wsieve-geo/src/ip.rs`

proto 定义（`geodat.proto:61-82`）：

```proto
message CIDR      { bytes ip = 1; uint32 prefix = 2; }
message GeoIP     { string code = 1; repeated CIDR cidr = 2; bool reverse_match = 3; }
message GeoIPList { repeated GeoIP entry = 1; }
```

**实现选型说明**：设计文档 §6.5 写的是「前缀树」，此处落地为**合并区间 + 二分查找**。二者对本用途等效：数十万条 CIDR 下 `log₂(n) ≈ 18` 次比较，与前缀树的 32/128 步同一量级，而代码量少一大半、无指针跳转、cache 友好。

- [ ] **Step 1: 写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    // 别名是为了避开 clippy::type_complexity —— 裸写四层嵌套会被拦下
    type CidrSpec<'a> = (&'a [u8], u32);
    type GeoIpSpec<'a> = (&'a str, &'a [CidrSpec<'a>]);

    fn encode_geoip_list(entries: &[GeoIpSpec<'_>]) -> Vec<u8> {
        fn varint(v: u64, out: &mut Vec<u8>) {
            let mut v = v;
            loop {
                let b = (v & 0x7f) as u8;
                v >>= 7;
                if v == 0 {
                    out.push(b);
                    return;
                }
                out.push(b | 0x80);
            }
        }
        fn field(num: u32, wire: u8, out: &mut Vec<u8>) {
            varint(((num as u64) << 3) | wire as u64, out);
        }
        fn delimited(num: u32, payload: &[u8], out: &mut Vec<u8>) {
            field(num, 2, out);
            varint(payload.len() as u64, out);
            out.extend_from_slice(payload);
        }

        let mut list = Vec::new();
        for (code, cidrs) in entries {
            let mut geoip = Vec::new();
            delimited(1, code.as_bytes(), &mut geoip);
            for (ip, prefix) in *cidrs {
                let mut c = Vec::new();
                delimited(1, ip, &mut c);
                field(2, 0, &mut c);
                varint(*prefix as u64, &mut c);
                delimited(2, &c, &mut geoip);
            }
            delimited(1, &geoip, &mut list);
        }
        list
    }

    #[test]
    fn matches_v4_inside_and_outside() {
        // 192.168.0.0/16
        let buf = encode_geoip_list(&[("private", &[(&[192, 168, 0, 0], 16)])]);
        let db = IpDb::parse(&buf).unwrap();
        assert!(db.matches("private", Ipv4Addr::new(192, 168, 1, 1).into()));
        assert!(db.matches("private", Ipv4Addr::new(192, 168, 255, 255).into()));
        assert!(!db.matches("private", Ipv4Addr::new(192, 169, 0, 1).into()));
        assert!(!db.matches("private", Ipv4Addr::new(10, 0, 0, 1).into()));
    }

    #[test]
    fn prefix_zero_matches_everything() {
        let buf = encode_geoip_list(&[("all", &[(&[0, 0, 0, 0], 0)])]);
        let db = IpDb::parse(&buf).unwrap();
        assert!(db.matches("all", Ipv4Addr::new(8, 8, 8, 8).into()));
    }

    #[test]
    fn full_prefix_matches_single_address() {
        let buf = encode_geoip_list(&[("one", &[(&[1, 2, 3, 4], 32)])]);
        let db = IpDb::parse(&buf).unwrap();
        assert!(db.matches("one", Ipv4Addr::new(1, 2, 3, 4).into()));
        assert!(!db.matches("one", Ipv4Addr::new(1, 2, 3, 5).into()));
    }

    #[test]
    fn v6_is_kept_separate_from_v4() {
        let v6 = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let buf = encode_geoip_list(&[("doc", &[(&v6, 32)])]);
        let db = IpDb::parse(&buf).unwrap();
        assert!(db.matches("doc", "2001:db8::1".parse::<Ipv6Addr>().unwrap().into()));
        // v4 地址绝不能落进 v6 集合
        assert!(!db.matches("doc", Ipv4Addr::new(32, 1, 13, 184).into()));
    }

    #[test]
    fn overlapping_ranges_are_merged() {
        // 10.0.0.0/8 与 10.1.0.0/16 重叠，合并后应只剩一段
        let buf = encode_geoip_list(&[("cn", &[(&[10, 0, 0, 0], 8), (&[10, 1, 0, 0], 16)])]);
        let db = IpDb::parse(&buf).unwrap();
        assert_eq!(db.range_count("cn"), 1);
        assert!(db.matches("cn", Ipv4Addr::new(10, 1, 2, 3).into()));
    }

    #[test]
    fn malformed_ip_length_is_rejected() {
        // 3 字节既不是 v4 也不是 v6 —— 必须报错，不能猜
        let buf = encode_geoip_list(&[("bad", &[(&[1, 2, 3], 24)])]);
        assert!(IpDb::parse(&buf).is_err());
    }

    #[test]
    fn unknown_class_never_matches() {
        let buf = encode_geoip_list(&[("cn", &[(&[10, 0, 0, 0], 8)])]);
        let db = IpDb::parse(&buf).unwrap();
        assert!(!db.matches("us", Ipv4Addr::new(10, 0, 0, 1).into()));
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p wsieve-geo --lib ip::`
Expected: `cannot find type IpDb in this scope`

- [ ] **Step 3: 写实现**

```rust
//! geoip.dat 的解析与 CIDR 匹配。
//!
//! 匹配结构用「合并区间 + 二分」而非前缀树：数十万条 CIDR 下
//! log2(n) ≈ 18 次比较，与前缀树的 32/128 步同一量级，但代码量少一半、
//! 无指针跳转、cache 友好。

use std::collections::HashMap;
use std::net::IpAddr;

use crate::pb::{PbError, Reader};

#[derive(Debug, thiserror::Error)]
pub enum IpDbError {
    #[error(transparent)]
    Pb(#[from] PbError),
    #[error("CIDR 地址长度非法：{0} 字节（只接受 4 或 16）")]
    BadIpLen(usize),
    #[error("CIDR 前缀长度越界：/{prefix}（地址为 {bits} 位）")]
    BadPrefix { prefix: u32, bits: u32 },
}

#[derive(Default)]
struct IpClass {
    v4: Vec<(u32, u32)>,   // 闭区间 [start, end]
    v6: Vec<(u128, u128)>,
}

pub struct IpDb {
    classes: HashMap<String, IpClass>,
}

impl IpDb {
    pub fn parse(buf: &[u8]) -> Result<Self, IpDbError> {
        let mut classes: HashMap<String, IpClass> = HashMap::new();

        let mut r = Reader::new(buf);
        while !r.is_empty() {
            let (field, wire) = r.tag()?;
            if field != 1 || wire != 2 {
                r.skip(wire)?;
                continue;
            }
            let entry = r.bytes()?;
            let (code, class) = parse_geoip(entry)?;
            let slot = classes.entry(code).or_default();
            slot.v4.extend(class.v4);
            slot.v6.extend(class.v6);
        }

        // 合并重叠区间，之后才能二分
        for class in classes.values_mut() {
            class.v4 = merge_ranges(std::mem::take(&mut class.v4));
            class.v6 = merge_ranges(std::mem::take(&mut class.v6));
        }
        Ok(Self { classes })
    }

    pub fn has(&self, code: &str) -> bool {
        self.classes.contains_key(&code.trim().to_ascii_lowercase())
    }

    pub fn matches(&self, code: &str, ip: IpAddr) -> bool {
        let Some(class) = self.classes.get(&code.trim().to_ascii_lowercase()) else {
            return false;
        };
        match ip {
            IpAddr::V4(a) => contains(&class.v4, u32::from(a)),
            IpAddr::V6(a) => contains(&class.v6, u128::from(a)),
        }
    }

    #[doc(hidden)] // 测试用
    pub fn range_count(&self, code: &str) -> usize {
        self.classes
            .get(&code.trim().to_ascii_lowercase())
            .map(|c| c.v4.len() + c.v6.len())
            .unwrap_or(0)
    }
}

fn parse_geoip(buf: &[u8]) -> Result<(String, IpClass), IpDbError> {
    let mut code = String::new();
    let mut class = IpClass::default();

    let mut r = Reader::new(buf);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, 2) => code = r.string()?,
            (2, 2) => {
                let c = r.bytes()?;
                match parse_cidr(c)? {
                    Cidr::V4(s, e) => class.v4.push((s, e)),
                    Cidr::V6(s, e) => class.v6.push((s, e)),
                }
            }
            // field 3 是 reverse_match。我们不支持反向匹配的 geoip 类别
            // （标准 geoip.dat 里不存在），遇到即跳过。
            _ => r.skip(wire)?,
        }
    }
    Ok((code.trim().to_ascii_lowercase(), class))
}

enum Cidr {
    V4(u32, u32),
    V6(u128, u128),
}

fn parse_cidr(buf: &[u8]) -> Result<Cidr, IpDbError> {
    let mut ip: Option<Vec<u8>> = None;
    let mut prefix = 0u32;

    let mut r = Reader::new(buf);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, 2) => ip = Some(r.bytes()?.to_vec()),
            (2, 0) => prefix = r.varint()? as u32,
            _ => r.skip(wire)?,
        }
    }

    let ip = ip.ok_or(IpDbError::BadIpLen(0))?;
    match ip.len() {
        4 => {
            if prefix > 32 {
                return Err(IpDbError::BadPrefix { prefix, bits: 32 });
            }
            let base = u32::from_be_bytes([ip[0], ip[1], ip[2], ip[3]]);
            let (start, end) = range_u32(base, prefix);
            Ok(Cidr::V4(start, end))
        }
        16 => {
            if prefix > 128 {
                return Err(IpDbError::BadPrefix { prefix, bits: 128 });
            }
            let mut b = [0u8; 16];
            b.copy_from_slice(&ip);
            let base = u128::from_be_bytes(b);
            let (start, end) = range_u128(base, prefix);
            Ok(Cidr::V6(start, end))
        }
        n => Err(IpDbError::BadIpLen(n)),
    }
}

/// prefix=0 时 shift 会溢出，必须单独处理。
fn range_u32(base: u32, prefix: u32) -> (u32, u32) {
    if prefix == 0 {
        return (0, u32::MAX);
    }
    let mask = u32::MAX << (32 - prefix);
    (base & mask, (base & mask) | !mask)
}

fn range_u128(base: u128, prefix: u32) -> (u128, u128) {
    if prefix == 0 {
        return (0, u128::MAX);
    }
    let mask = u128::MAX << (128 - prefix);
    (base & mask, (base & mask) | !mask)
}

/// 排序后合并相邻或重叠的区间。相邻（end + 1 == next.start）也要合并，
/// 否则 0.0.0.0/1 与 128.0.0.0/1 会留成两段。
fn merge_ranges<T>(mut v: Vec<(T, T)>) -> Vec<(T, T)>
where
    T: Copy + Ord + num_like::NumLike,
{
    if v.is_empty() {
        return v;
    }
    v.sort_unstable_by_key(|r| r.0);
    let mut out: Vec<(T, T)> = Vec::with_capacity(v.len());
    for (s, e) in v {
        match out.last_mut() {
            Some(last) if s <= last.1.saturating_add_one() => {
                if e > last.1 {
                    last.1 = e;
                }
            }
            _ => out.push((s, e)),
        }
    }
    out
}

fn contains<T: Copy + Ord>(ranges: &[(T, T)], x: T) -> bool {
    // 最后一个 start <= x 的区间，就是唯一可能命中的那个
    let i = ranges.partition_point(|r| r.0 <= x);
    i > 0 && ranges[i - 1].1 >= x
}

/// merge_ranges 需要 "end + 1" 且要防溢出。为 u32/u128 各实现一次，
/// 避免为这一个方法引入 num-traits 依赖。
mod num_like {
    pub trait NumLike {
        fn saturating_add_one(self) -> Self;
    }
    impl NumLike for u32 {
        fn saturating_add_one(self) -> Self {
            self.saturating_add(1)
        }
    }
    impl NumLike for u128 {
        fn saturating_add_one(self) -> Self {
            self.saturating_add(1)
        }
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p wsieve-geo --lib ip::`
Expected: 7 个测试全部 PASS

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-geo/src/ip.rs
git commit -m "feat(geo): geoip 解析与 CIDR 区间匹配"
```

---

### Task 5: `GeoDb` 懒加载门面

**Files:**
- Modify: `crates/wsieve-geo/src/lib.rs`

只加载规则里真正引用到的类别是**设计文档 §6.5 的明确要求**。但 v2ray 的 dat 格式是一整个 `GeoSiteList`，无法只解析其中一个 entry —— 所以「懒」体现在**文件级**：直到第一次有人问 geosite，才去读并解析 `geosite.dat`；只用 geoip 的配置永远不会为 geosite 付出代价。

- [ ] **Step 1: 写失败的测试**

在 `lib.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_reports_path_not_panics() {
        let db = GeoDb::new("/nonexistent/geoip.dat".into(), "/nonexistent/geosite.dat".into());
        let err = db.site_matches("cn", "a.com").unwrap_err().to_string();
        assert!(err.contains("geosite.dat"), "错误信息要指明是哪个文件：{err}");
    }

    #[test]
    fn site_file_is_not_read_until_first_query() {
        // geosite 路径不存在，但只查 geoip 不应触发它的加载
        let dir = std::env::temp_dir().join("wsieve-geo-lazy-test");
        std::fs::create_dir_all(&dir).unwrap();
        let ip_path = dir.join("geoip.dat");
        std::fs::write(&ip_path, Vec::<u8>::new()).unwrap();

        let db = GeoDb::new(ip_path, "/nonexistent/geosite.dat".into());
        // 空文件解析出空库，查询返回 false 而非报错
        // （写成 assert!(!…) 而非 assert_eq!(…, false)，后者会被 clippy 拦）
        assert!(!db.ip_matches("cn", "1.2.3.4".parse().unwrap()).unwrap());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p wsieve-geo --lib tests::`
Expected: `cannot find type GeoDb in this scope`

- [ ] **Step 3: 写实现**

替换 `lib.rs` 全部内容（保留原有模块声明）：

```rust
//! geosite.dat / geoip.dat 的解析与匹配。
//!
//! 这两个文件是 v2ray 系的事实标准格式，protobuf 编码。结构极简
//! （枚举 + 字符串 + bytes + uint32，三层嵌套），因此手写 wire format
//! 解析而不引入 prost —— 与仓库里手写 base64 的取舍一致
//! （见 src-tauri/src/bridge.rs:285 的注释）。
//!
//! 权威定义见 .research/Xray-core/common/geodata/geodat.proto。

pub mod ip;
pub mod pb;
pub mod site;

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::OnceLock;

pub use ip::IpDb;
pub use site::SiteDb;

#[derive(Debug, thiserror::Error)]
pub enum GeoError {
    #[error("读取 {path} 失败：{source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("解析 {path} 失败：{source}")]
    Parse {
        path: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// GEO 数据的门面。两个文件各自**惰性加载**：只用 geoip 的配置
/// 永远不会为 geosite 付出解析代价（geosite.dat 通常是前者的数倍大）。
///
/// 加载失败不 panic，错误上抛给调用方 —— 按设计文档 §12，
/// 涉 GEO 的规则跳过并告警，绝不阻断启动。
pub struct GeoDb {
    ip_path: PathBuf,
    site_path: PathBuf,
    ip: OnceLock<Result<IpDb, String>>,
    site: OnceLock<Result<SiteDb, String>>,
}

impl GeoDb {
    pub fn new(ip_path: PathBuf, site_path: PathBuf) -> Self {
        Self {
            ip_path,
            site_path,
            ip: OnceLock::new(),
            site: OnceLock::new(),
        }
    }

    pub fn ip_matches(&self, code: &str, addr: IpAddr) -> Result<bool, GeoError> {
        let db = self.ip.get_or_init(|| {
            std::fs::read(&self.ip_path)
                .map_err(|e| format!("读取 {} 失败：{e}", self.ip_path.display()))
                .and_then(|buf| {
                    IpDb::parse(&buf)
                        .map_err(|e| format!("解析 {} 失败：{e}", self.ip_path.display()))
                })
        });
        match db {
            Ok(db) => Ok(db.matches(code, addr)),
            Err(msg) => Err(GeoError::Io {
                path: self.ip_path.display().to_string(),
                source: std::io::Error::other(msg.clone()),
            }),
        }
    }

    pub fn site_matches(&self, code: &str, domain: &str) -> Result<bool, GeoError> {
        let db = self.site.get_or_init(|| {
            std::fs::read(&self.site_path)
                .map_err(|e| format!("读取 {} 失败：{e}", self.site_path.display()))
                .and_then(|buf| {
                    SiteDb::parse(&buf)
                        .map_err(|e| format!("解析 {} 失败：{e}", self.site_path.display()))
                })
        });
        match db {
            Ok(db) => Ok(db.matches(code, domain)),
            Err(msg) => Err(GeoError::Io {
                path: self.site_path.display().to_string(),
                source: std::io::Error::other(msg.clone()),
            }),
        }
    }

    /// 加载阶段校验用：规则引用的类别是否存在。
    /// 返回 Err 表示文件本身读不了；Ok(false) 表示文件正常但没这个类别。
    pub fn has_site_class(&self, code: &str) -> Result<bool, GeoError> {
        self.site_matches(code, "\u{0}invalid\u{0}")?; // 触发加载
        Ok(self
            .site
            .get()
            .and_then(|r| r.as_ref().ok())
            .map(|db| db.has(code))
            .unwrap_or(false))
    }

    pub fn has_ip_class(&self, code: &str) -> Result<bool, GeoError> {
        self.ip_matches(code, IpAddr::from([0, 0, 0, 0]))?;
        Ok(self
            .ip
            .get()
            .and_then(|r| r.as_ref().ok())
            .map(|db| db.has(code))
            .unwrap_or(false))
    }
}
```

- [ ] **Step 4: 运行全部测试**

Run: `cargo test -p wsieve-geo`
Expected: 全部 PASS（pb 6 + site 6 + ip 7 + lib 2 = 21）

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-geo/src/lib.rs
git commit -m "feat(geo): GeoDb 门面与文件级惰性加载"
```

---

### Task 6: 用真实 .dat 文件验收

**Files:**
- Create: `crates/wsieve-geo/tests/real_dat.rs`

手写编码的测试证明不了「能读懂真实文件」。这个测试在**有真实文件时**才跑，没有就跳过——不让 CI 依赖外部下载。

- [ ] **Step 1: 写测试**

```rust
//! 用真实 geoip.dat / geosite.dat 验收。
//!
//! 文件不存在时自动跳过：CI 不应依赖外部下载。本地验收时先执行
//!   mkdir -p /tmp/wsieve-geo
//!   curl -Lo /tmp/wsieve-geo/geoip.dat   https://github.com/Loyalsoldier/v2ray-rules-dat/releases/latest/download/geoip.dat
//!   curl -Lo /tmp/wsieve-geo/geosite.dat https://github.com/Loyalsoldier/v2ray-rules-dat/releases/latest/download/geosite.dat

use std::path::PathBuf;

fn dir() -> PathBuf {
    PathBuf::from("/tmp/wsieve-geo")
}

#[test]
fn real_geoip_cn_contains_known_chinese_address() {
    let p = dir().join("geoip.dat");
    if !p.exists() {
        eprintln!("跳过：{} 不存在", p.display());
        return;
    }
    let db = wsieve_geo::IpDb::parse(&std::fs::read(&p).unwrap()).unwrap();
    assert!(db.has("cn"), "geoip.dat 应含 cn 类别");
    // 114.114.114.114 是南京信风 DNS，稳定属于 CN
    assert!(db.matches("cn", "114.114.114.114".parse().unwrap()));
    // 8.8.8.8 是 Google DNS，绝不属于 CN
    assert!(!db.matches("cn", "8.8.8.8".parse().unwrap()));
}

#[test]
fn real_geosite_cn_and_ads_behave() {
    let p = dir().join("geosite.dat");
    if !p.exists() {
        eprintln!("跳过：{} 不存在", p.display());
        return;
    }
    let db = wsieve_geo::SiteDb::parse(&std::fs::read(&p).unwrap()).unwrap();
    assert!(db.has("cn"));
    assert!(db.matches("cn", "www.baidu.com"), "baidu 应属 cn");
    assert!(!db.matches("cn", "www.google.com"), "google 不应属 cn");
    eprintln!("跳过的 Regex 条目数：{}", db.skipped_regex());
}
```

- [ ] **Step 2: 无文件时运行，确认跳过而非失败**

Run: `cargo test -p wsieve-geo --test real_dat`
Expected: PASS，输出含「跳过：/tmp/wsieve-geo/geoip.dat 不存在」

- [ ] **Step 3: 下载真实文件后再跑一次**

```bash
mkdir -p /tmp/wsieve-geo
curl -Lo /tmp/wsieve-geo/geoip.dat   https://github.com/Loyalsoldier/v2ray-rules-dat/releases/latest/download/geoip.dat
curl -Lo /tmp/wsieve-geo/geosite.dat https://github.com/Loyalsoldier/v2ray-rules-dat/releases/latest/download/geosite.dat
cargo test -p wsieve-geo --test real_dat -- --nocapture
```

Expected: 两个测试都 PASS。**若失败，说明解析器与真实格式有出入，必须停下来查 proto 定义**，不要修改断言迁就实现。

- [ ] **Step 4: 记录跳过的 Regex 条目数**

把 `--nocapture` 输出里的数字记进提交信息。这是 `ponytail:` 简化的实测依据。

**参考值：计划评审时用 Loyalsoldier 版实测为 371 条**（geosite.dat 约 11MB）。相对于全库数十万条域名，占比极低 —— 跳过 Regex 是可接受的取舍。若你的实测值与此**量级不同**，说明数据源不同或解析有偏差，值得查一下再往下走。

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-geo/tests/real_dat.rs
git commit -m "test(geo): 真实 dat 文件验收（无文件时跳过）"
```

---

> **Part A 到此结束。** 此时 `cargo test -p wsieve-geo` 应全绿，`wsieve-geo` 可独立使用。

---

## Part B — `wsieve-route`

**命名对照**：设计文档 §4.2 把三种判决写作 `Outbound / Direct / Block`。代码里第三者命名为 `Reject`，与配置中的内置出站名 `REJECT` 保持一致（§5.3）。语义相同。

### Task 7: 建立 crate 骨架

**Files:**
- Create: `crates/wsieve-route/Cargo.toml`
- Create: `crates/wsieve-route/src/lib.rs`
- Modify: `Cargo.toml`（workspace members）

- [ ] **Step 1: 创建 Cargo.toml**

```toml
[package]
name = "wsieve-route"
version = "0.1.0"
edition = "2021"
description = "Clash 语法规则解析与两阶段路由引擎（纯同步、无网络）"

[dependencies]
wsieve-proto = { path = "../wsieve-proto" }
wsieve-geo = { path = "../wsieve-geo" }
thiserror = { workspace = true }
ipnet = "2"

[dev-dependencies]
```

`ipnet` 提供 `IpNet::contains(&IpAddr)` 与 CIDR 文本解析，正确处理 v4/v6 与边界。这是「格式有坑就用成熟库」那一侧的判断（对照 §6.6：geo 的 protobuf 结构极简所以手写）。

- [ ] **Step 2: 注册进 workspace**

在根 `Cargo.toml` 的 `members` 里加：

```toml
    "crates/wsieve-route",
```

- [ ] **Step 3: 写 lib.rs**

```rust
//! Clash 语法的分流规则解析与判决引擎。
//!
//! 全部同步、无网络、无 IO —— 唯一的外部交互是查 GeoDb，而那是只读的。
//! 这条纪律让整个判决逻辑可被穷举单测，也让 UI 的「规则试算」能复用
//! 同一份代码，保证试算结果与真实判决永远一致
//! （见设计文档 §4.2 纪律①）。
//!
//! DNS 解析不在本 crate 内发生。需要解析时，evaluate 会返回
//! Verdict::NeedResolve 把需求抛给调用方，由调用方在 async 上下文里
//! 解析后再调一轮。

pub mod engine;
pub mod rule;

pub use engine::{Decision, RuleSet, Verdict};
pub use rule::{Mode, Rule, RuleError, RuleKind, Target};
```

- [ ] **Step 4: 验证 workspace 注册**

Run: `cargo check -p wsieve-route`
Expected: 报模块缺失，但**不该**报 "package not found"

- [ ] **Step 5: 提交**

```bash
git add Cargo.toml crates/wsieve-route/
git commit -m "chore(route): 建立 wsieve-route crate 骨架"
```

---

### Task 8: Clash 规则语法解析

**Files:**
- Create: `crates/wsieve-route/src/rule.rs`

语法：`TYPE,VALUE,TARGET[,no-resolve]`，`MATCH` 只有两段：`MATCH,TARGET`。

- [ ] **Step 1: 写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Rule {
        Rule::parse(s).unwrap_or_else(|e| panic!("解析 {s:?} 失败：{e}"))
    }

    #[test]
    fn parses_each_rule_type() {
        assert!(matches!(p("DOMAIN,a.com,PROXY").kind, RuleKind::Domain));
        assert!(matches!(p("DOMAIN-SUFFIX,a.com,PROXY").kind, RuleKind::DomainSuffix));
        assert!(matches!(p("DOMAIN-KEYWORD,goog,PROXY").kind, RuleKind::DomainKeyword));
        assert!(matches!(p("GEOSITE,cn,DIRECT").kind, RuleKind::GeoSite));
        assert!(matches!(p("IP-CIDR,10.0.0.0/8,DIRECT").kind, RuleKind::IpCidr));
        assert!(matches!(p("IP-CIDR6,fe80::/10,DIRECT").kind, RuleKind::IpCidr));
        assert!(matches!(p("GEOIP,CN,DIRECT").kind, RuleKind::GeoIp));
        assert!(matches!(p("DST-PORT,22,DIRECT").kind, RuleKind::DstPort));
        assert!(matches!(p("MATCH,PROXY").kind, RuleKind::Match));
    }

    #[test]
    fn builtin_targets_are_recognized() {
        assert!(matches!(p("MATCH,DIRECT").target, Target::Direct));
        assert!(matches!(p("MATCH,REJECT").target, Target::Reject));
        match p("MATCH,日本节点").target {
            Target::Outbound(n) => assert_eq!(n, "日本节点"),
            other => panic!("应是 Outbound，实为 {other:?}"),
        }
    }

    #[test]
    fn no_resolve_flag_is_parsed() {
        assert!(p("IP-CIDR,10.0.0.0/8,DIRECT,no-resolve").no_resolve);
        assert!(!p("IP-CIDR,10.0.0.0/8,DIRECT").no_resolve);
        // 大小写不敏感
        assert!(p("IP-CIDR,10.0.0.0/8,DIRECT,NO-RESOLVE").no_resolve);
    }

    #[test]
    fn type_and_builtin_target_are_case_insensitive() {
        assert!(matches!(p("domain-suffix,a.com,direct").kind, RuleKind::DomainSuffix));
        assert!(matches!(p("domain-suffix,a.com,direct").target, Target::Direct));
    }

    #[test]
    fn whitespace_around_fields_is_trimmed() {
        let r = p("  IP-CIDR , 10.0.0.0/8 , DIRECT , no-resolve ");
        assert!(r.no_resolve);
        assert!(matches!(r.target, Target::Direct));
    }

    #[test]
    fn outbound_name_keeps_original_case_and_spaces() {
        // 出站名是用户起的，不能规范化 —— 规范化会让规则引用不到节点
        match p("MATCH, My Node ").target {
            Target::Outbound(n) => assert_eq!(n, "My Node"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_type_is_rejected_with_the_offending_text() {
        let e = Rule::parse("NOT-A-TYPE,x,PROXY").unwrap_err().to_string();
        assert!(e.contains("NOT-A-TYPE"), "错误信息要含冒犯的类型名：{e}");
    }

    #[test]
    fn malformed_cidr_is_rejected_at_parse_time() {
        // 判决路径上不该再做文本解析 —— 坏 CIDR 必须在加载时就被挡住
        assert!(Rule::parse("IP-CIDR,not-a-cidr,DIRECT").is_err());
        assert!(Rule::parse("IP-CIDR,10.0.0.0/33,DIRECT").is_err());
    }

    #[test]
    fn malformed_port_is_rejected() {
        assert!(Rule::parse("DST-PORT,70000,DIRECT").is_err());
        assert!(Rule::parse("DST-PORT,abc,DIRECT").is_err());
    }

    #[test]
    fn too_few_fields_is_rejected() {
        assert!(Rule::parse("DOMAIN,a.com").is_err(), "缺 target");
        assert!(Rule::parse("MATCH").is_err(), "MATCH 也要 target");
    }

    #[test]
    fn comment_and_blank_lines_are_not_rules() {
        assert!(Rule::parse_line("# 这是注释").unwrap().is_none());
        assert!(Rule::parse_line("   ").unwrap().is_none());
        assert!(Rule::parse_line("DOMAIN,a.com,PROXY").unwrap().is_some());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p wsieve-route --lib rule::`
Expected: `cannot find type Rule in this scope`

- [ ] **Step 3: 写实现**

```rust
//! Clash 规则语法：`TYPE,VALUE,TARGET[,no-resolve]`。
//!
//! 全部校验都在解析期完成 —— 判决路径上绝不再做文本解析。
//! 坏 CIDR、坏端口在加载时就报错并指出行号，而不是运行时静默不匹配。

use std::net::IpAddr;
use std::str::FromStr;

use ipnet::IpNet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Rule,
    Global,
    Direct,
}

impl FromStr for Mode {
    type Err = RuleError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "rule" => Ok(Mode::Rule),
            "global" => Ok(Mode::Global),
            "direct" => Ok(Mode::Direct),
            other => Err(RuleError::UnknownMode(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    Domain,
    DomainSuffix,
    DomainKeyword,
    GeoSite,
    IpCidr,
    GeoIp,
    DstPort,
    Match,
}

impl RuleKind {
    /// 该类型是否只能对域名目标生效。
    pub fn is_domain_kind(self) -> bool {
        matches!(
            self,
            RuleKind::Domain | RuleKind::DomainSuffix | RuleKind::DomainKeyword | RuleKind::GeoSite
        )
    }

    /// 该类型是否需要 IP 才能判定（域名目标要先解析）。
    pub fn is_ip_kind(self) -> bool {
        matches!(self, RuleKind::IpCidr | RuleKind::GeoIp)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Outbound(String),
    Direct,
    Reject,
}

#[derive(Debug, Clone)]
pub enum RuleValue {
    /// 域名类与 GEO 类：已转小写、已去尾点
    Text(String),
    Cidr(IpNet),
    Port(u16),
    None,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub kind: RuleKind,
    pub value: RuleValue,
    pub target: Target,
    pub no_resolve: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    #[error("未知规则类型：{0}")]
    UnknownKind(String),
    #[error("未知模式：{0}（应为 rule / global / direct）")]
    UnknownMode(String),
    #[error("字段不足：{0}")]
    TooFewFields(String),
    #[error("非法 CIDR：{0}")]
    BadCidr(String),
    #[error("非法端口：{0}")]
    BadPort(String),
    #[error("未知的第四段参数：{0}（只支持 no-resolve）")]
    UnknownFlag(String),
}

impl Rule {
    /// 解析一行；注释行与空行返回 Ok(None)。
    pub fn parse_line(line: &str) -> Result<Option<Rule>, RuleError> {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            return Ok(None);
        }
        Rule::parse(t).map(Some)
    }

    pub fn parse(s: &str) -> Result<Rule, RuleError> {
        let parts: Vec<&str> = s.split(',').map(str::trim).collect();
        if parts.len() < 2 {
            return Err(RuleError::TooFewFields(s.to_string()));
        }

        let kind = match parts[0].to_ascii_uppercase().as_str() {
            "DOMAIN" => RuleKind::Domain,
            "DOMAIN-SUFFIX" => RuleKind::DomainSuffix,
            "DOMAIN-KEYWORD" => RuleKind::DomainKeyword,
            "GEOSITE" => RuleKind::GeoSite,
            // IP-CIDR6 与 IP-CIDR 同一处理：IpNet 自己区分 v4/v6
            "IP-CIDR" | "IP-CIDR6" => RuleKind::IpCidr,
            "GEOIP" => RuleKind::GeoIp,
            "DST-PORT" => RuleKind::DstPort,
            "MATCH" | "FINAL" => RuleKind::Match,
            other => return Err(RuleError::UnknownKind(other.to_string())),
        };

        // MATCH 是 2 段，其余是 3 段（可选第 4 段）
        let (value_str, target_str, rest) = if kind == RuleKind::Match {
            ("", parts[1], &parts[2..])
        } else {
            if parts.len() < 3 {
                return Err(RuleError::TooFewFields(s.to_string()));
            }
            (parts[1], parts[2], &parts[3..])
        };

        let value = match kind {
            RuleKind::IpCidr => RuleValue::Cidr(
                value_str
                    .parse::<IpNet>()
                    .map_err(|_| RuleError::BadCidr(value_str.to_string()))?,
            ),
            RuleKind::DstPort => RuleValue::Port(
                value_str
                    .parse::<u16>()
                    .map_err(|_| RuleError::BadPort(value_str.to_string()))?,
            ),
            RuleKind::Match => RuleValue::None,
            // 域名与 GEO 类别统一规范化，匹配时不必再处理大小写与尾点
            _ => RuleValue::Text(value_str.trim_end_matches('.').to_ascii_lowercase()),
        };

        // 出站名保持用户原样（大小写、空格都不动），否则规则会引用不到节点
        let target = match target_str.to_ascii_uppercase().as_str() {
            "DIRECT" => Target::Direct,
            "REJECT" => Target::Reject,
            _ => Target::Outbound(target_str.to_string()),
        };

        let mut no_resolve = false;
        for flag in rest {
            match flag.to_ascii_lowercase().as_str() {
                "no-resolve" => no_resolve = true,
                "" => {}
                other => return Err(RuleError::UnknownFlag(other.to_string())),
            }
        }

        Ok(Rule {
            kind,
            value,
            target,
            no_resolve,
        })
    }

    /// IP 类规则匹配一个具体地址。
    pub(crate) fn matches_ip(&self, ip: IpAddr) -> bool {
        match &self.value {
            RuleValue::Cidr(net) => net.contains(&ip),
            _ => false,
        }
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p wsieve-route --lib rule::`
Expected: 11 个测试全部 PASS

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-route/src/rule.rs
git commit -m "feat(route): Clash 规则语法解析（解析期完成全部校验）"
```

---

### Task 9: `RuleSet` 构建与加载期校验

**Files:**
- Create: `crates/wsieve-route/src/engine.rs`

设计文档 §5.5 的硬要求：**缺 `MATCH` 即报错，不设隐式默认**。理由是隐式 `DIRECT` = 静默裸奔、隐式 `REJECT` = 莫名断网，两者都让用户在不知情下承担后果。

- [ ] **Step 1: 写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn outbounds(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn missing_match_rule_is_a_load_error() {
        let e = RuleSet::build(
            &lines(&["DOMAIN,a.com,PROXY"]),
            Mode::Rule,
            "",
            &outbounds(&["PROXY"]),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("MATCH"), "错误要点名 MATCH：{e}");
    }

    #[test]
    fn match_rule_makes_it_load() {
        let rs = RuleSet::build(
            &lines(&["DOMAIN,a.com,PROXY", "MATCH,PROXY"]),
            Mode::Rule,
            "",
            &outbounds(&["PROXY"]),
        )
        .unwrap();
        assert_eq!(rs.len(), 2);
    }

    #[test]
    fn rule_referencing_unknown_outbound_is_reported_with_line_number() {
        let e = RuleSet::build(
            &lines(&["DOMAIN,a.com,PROXY", "DOMAIN,b.com,GHOST", "MATCH,PROXY"]),
            Mode::Rule,
            "",
            &outbounds(&["PROXY"]),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("GHOST"), "要点名不存在的出站：{e}");
        assert!(e.contains('2'), "要给出行号（1-based）：{e}");
    }

    #[test]
    fn rules_after_match_are_rejected() {
        // MATCH 之后的规则永远不可达，静默忽略会让用户困惑
        let e = RuleSet::build(
            &lines(&["MATCH,PROXY", "DOMAIN,a.com,PROXY"]),
            Mode::Rule,
            "",
            &outbounds(&["PROXY"]),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("MATCH"), "{e}");
    }

    #[test]
    fn comments_and_blanks_do_not_break_line_numbers() {
        let e = RuleSet::build(
            &lines(&["# 注释", "", "DOMAIN,a.com,GHOST", "MATCH,PROXY"]),
            Mode::Rule,
            "",
            &outbounds(&["PROXY"]),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains('3'), "行号应是 3（含注释与空行）：{e}");
    }

    #[test]
    fn global_outbound_must_exist_when_set() {
        let e = RuleSet::build(
            &lines(&["MATCH,PROXY"]),
            Mode::Global,
            "GHOST",
            &outbounds(&["PROXY"]),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("GHOST"), "{e}");
    }

    #[test]
    fn empty_global_outbound_falls_back_to_match_target() {
        let rs = RuleSet::build(
            &lines(&["MATCH,PROXY"]),
            Mode::Global,
            "",
            &outbounds(&["PROXY"]),
        )
        .unwrap();
        assert_eq!(rs.global_target(), &Decision::Outbound("PROXY".into()));
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p wsieve-route --lib engine::`
Expected: `cannot find type RuleSet in this scope`

- [ ] **Step 3: 写实现**

```rust
//! 两阶段判决引擎（设计文档 §4.2 纪律① / §6.2）。

use std::collections::HashSet;
use std::net::IpAddr;

use wsieve_geo::GeoDb;
use wsieve_proto::addr::{AddrPort, TargetAddr};

use crate::rule::{Mode, Rule, RuleError, RuleKind, RuleValue, Target};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Outbound(String),
    Direct,
    Reject,
}

impl From<&Target> for Decision {
    fn from(t: &Target) -> Self {
        match t {
            Target::Outbound(n) => Decision::Outbound(n.clone()),
            Target::Direct => Decision::Direct,
            Target::Reject => Decision::Reject,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Decided(Decision),
    /// 扫到一条 IP 类规则、目标是域名、且未带 no-resolve。
    /// 调用方解析后带 Some(&ips) 重新调用一次；解析失败传 Some(&[])。
    NeedResolve { domain: String },
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("第 {line} 行：{source}")]
    Rule {
        line: usize,
        #[source]
        source: RuleError,
    },
    #[error("第 {line} 行引用了不存在的出站：{name}")]
    UnknownOutbound { line: usize, name: String },
    #[error("规则列表缺少 MATCH 兜底。websieve 不设隐式默认 —— \
             隐式直连等于静默裸奔，隐式拒绝等于莫名断网，两者都会让你在不知情下承担后果。\
             请显式添加一行，例如：MATCH,DIRECT")]
    MissingMatch,
    #[error("第 {line} 行位于 MATCH 之后，永远不会被执行。请移到 MATCH 之前或删除")]
    RuleAfterMatch { line: usize },
    #[error("global-outbound 指向不存在的出站：{0}")]
    UnknownGlobalOutbound(String),
}

/// `Debug` 是必需的，不是装饰：测试里对 `Result<RuleSet, _>` 调
/// `.unwrap_err()` 要求 `T: Debug`，少了它 Task 9 的 5 个测试全部编译失败。
#[derive(Debug)]
pub struct RuleSet {
    rules: Vec<Rule>,
    mode: Mode,
    global: Decision,
    /// MATCH 的目标。加载期已保证存在。
    fallback: Decision,
}

impl RuleSet {
    /// `lines` 是配置里 `rules:` 数组的原始字符串，逐行对应。
    /// 行号按数组下标 +1 报告，注释与空行也占号 —— 与用户在编辑器里看到的一致。
    pub fn build(
        lines: &[String],
        mode: Mode,
        global_outbound: &str,
        known_outbounds: &HashSet<String>,
    ) -> Result<Self, BuildError> {
        let mut rules = Vec::new();
        let mut fallback: Option<Decision> = None;

        for (i, raw) in lines.iter().enumerate() {
            let line = i + 1;
            let Some(rule) = Rule::parse_line(raw).map_err(|source| BuildError::Rule { line, source })?
            else {
                continue;
            };

            if fallback.is_some() {
                return Err(BuildError::RuleAfterMatch { line });
            }

            if let Target::Outbound(name) = &rule.target {
                if !known_outbounds.contains(name) {
                    return Err(BuildError::UnknownOutbound {
                        line,
                        name: name.clone(),
                    });
                }
            }

            if rule.kind == RuleKind::Match {
                fallback = Some(Decision::from(&rule.target));
            }
            rules.push(rule);
        }

        let fallback = fallback.ok_or(BuildError::MissingMatch)?;

        let global = if global_outbound.trim().is_empty() {
            fallback.clone()
        } else {
            let name = global_outbound.trim();
            if !known_outbounds.contains(name) {
                return Err(BuildError::UnknownGlobalOutbound(name.to_string()));
            }
            Decision::Outbound(name.to_string())
        };

        Ok(Self {
            rules,
            mode,
            global,
            fallback,
        })
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn global_target(&self) -> &Decision {
        &self.global
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p wsieve-route --lib engine::`
Expected: 7 个测试全部 PASS

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-route/src/engine.rs
git commit -m "feat(route): RuleSet 构建与加载期校验（缺 MATCH 即报错）"
```

---

### Task 10: 两阶段求值

**Files:**
- Modify: `crates/wsieve-route/src/engine.rs`
- Create: `crates/wsieve-route/tests/engine.rs`

这是本阶段最核心的一段。判决矩阵来自设计文档 §6.2，**必须逐格覆盖**。

- [ ] **Step 1: 写失败的集成测试**

`crates/wsieve-route/tests/engine.rs`：

```rust
//! 判决矩阵穷举。矩阵定义见设计文档 §6.2。

use std::collections::HashSet;
use std::net::IpAddr;
use std::path::PathBuf;

use wsieve_geo::GeoDb;
use wsieve_proto::addr::{AddrPort, TargetAddr};
use wsieve_route::{Decision, Mode, RuleSet, Verdict};

fn geo_stub() -> GeoDb {
    // 指向不存在的文件：所有 GEO 查询都会失败，从而验证
    // 「GEO 不可用时规则跳过而非阻断」这条纪律（设计文档 §12）
    GeoDb::new(PathBuf::from("/nonexistent/geoip.dat"), PathBuf::from("/nonexistent/geosite.dat"))
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

fn decided(v: Verdict) -> Decision {
    match v {
        Verdict::Decided(d) => d,
        Verdict::NeedResolve { domain } => panic!("预期已判决，实为 NeedResolve({domain})"),
    }
}

// ── 域名类规则 ────────────────────────────────────────────

#[test]
fn domain_suffix_matches_self_and_subdomain() {
    let set = rs(&["DOMAIN-SUFFIX,example.com,PROXY", "MATCH,DIRECT"], &["PROXY"]);
    let g = geo_stub();
    assert_eq!(decided(set.evaluate(&domain("example.com", 443), None, &g)), Decision::Outbound("PROXY".into()));
    assert_eq!(decided(set.evaluate(&domain("a.example.com", 443), None, &g)), Decision::Outbound("PROXY".into()));
    // 边界：不能把 notexample.com 当子域
    assert_eq!(decided(set.evaluate(&domain("notexample.com", 443), None, &g)), Decision::Direct);
}

#[test]
fn domain_exact_does_not_match_subdomain() {
    let set = rs(&["DOMAIN,example.com,PROXY", "MATCH,DIRECT"], &["PROXY"]);
    let g = geo_stub();
    assert_eq!(decided(set.evaluate(&domain("example.com", 443), None, &g)), Decision::Outbound("PROXY".into()));
    assert_eq!(decided(set.evaluate(&domain("a.example.com", 443), None, &g)), Decision::Direct);
}

#[test]
fn domain_rules_are_skipped_for_ip_targets() {
    let set = rs(&["DOMAIN-SUFFIX,example.com,PROXY", "MATCH,DIRECT"], &["PROXY"]);
    let g = geo_stub();
    assert_eq!(decided(set.evaluate(&ipv4([1, 2, 3, 4], 443), None, &g)), Decision::Direct);
}

#[test]
fn domain_match_is_case_and_trailing_dot_insensitive() {
    let set = rs(&["DOMAIN,example.com,PROXY", "MATCH,DIRECT"], &["PROXY"]);
    let g = geo_stub();
    assert_eq!(decided(set.evaluate(&domain("EXAMPLE.COM.", 443), None, &g)), Decision::Outbound("PROXY".into()));
}

// ── IP 类规则与两阶段 ──────────────────────────────────────

#[test]
fn ip_rule_matches_ip_target_in_first_pass() {
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let g = geo_stub();
    assert_eq!(decided(set.evaluate(&ipv4([10, 1, 2, 3], 443), None, &g)), Decision::Direct);
}

#[test]
fn ip_rule_with_domain_target_asks_for_resolution() {
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let g = geo_stub();
    match set.evaluate(&domain("a.com", 443), None, &g) {
        Verdict::NeedResolve { domain } => assert_eq!(domain, "a.com"),
        other => panic!("应请求解析，实为 {other:?}"),
    }
}

#[test]
fn no_resolve_suppresses_the_request() {
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT,no-resolve", "MATCH,PROXY"], &["PROXY"]);
    let g = geo_stub();
    assert_eq!(decided(set.evaluate(&domain("a.com", 443), None, &g)), Decision::Outbound("PROXY".into()));
}

#[test]
fn second_pass_uses_resolved_ips() {
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let g = geo_stub();
    let ips: Vec<IpAddr> = vec!["10.1.2.3".parse().unwrap()];
    assert_eq!(decided(set.evaluate(&domain("a.com", 443), Some(&ips), &g)), Decision::Direct);
}

#[test]
fn second_pass_with_empty_ips_falls_through() {
    // 解析失败/超时 → 传空切片 → 该规则不匹配，继续往下，绝不阻断
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let g = geo_stub();
    assert_eq!(decided(set.evaluate(&domain("a.com", 443), Some(&[]), &g)), Decision::Outbound("PROXY".into()));
}

#[test]
fn second_pass_never_asks_to_resolve_again() {
    // 这条是两阶段协议的死线：第二轮再抛 NeedResolve 就会死循环
    let set = rs(
        &["IP-CIDR,10.0.0.0/8,DIRECT", "IP-CIDR,192.168.0.0/16,DIRECT", "MATCH,PROXY"],
        &["PROXY"],
    );
    let g = geo_stub();
    let v = set.evaluate(&domain("a.com", 443), Some(&[]), &g);
    assert!(matches!(v, Verdict::Decided(_)), "第二轮必须给出判决，实为 {v:?}");
}

#[test]
fn domain_rule_before_ip_rule_short_circuits_without_dns() {
    // 关键收益：被域名规则提前命中的流量，零 DNS 查询
    let set = rs(
        &["DOMAIN-SUFFIX,a.com,PROXY", "IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,REJECT"],
        &["PROXY"],
    );
    let g = geo_stub();
    assert_eq!(decided(set.evaluate(&domain("a.com", 443), None, &g)), Decision::Outbound("PROXY".into()));
}

// ── 其余 ──────────────────────────────────────────────────

#[test]
fn dst_port_is_decidable_for_both_address_kinds() {
    let set = rs(&["DST-PORT,22,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let g = geo_stub();
    assert_eq!(decided(set.evaluate(&domain("a.com", 22), None, &g)), Decision::Direct);
    assert_eq!(decided(set.evaluate(&ipv4([1, 2, 3, 4], 22), None, &g)), Decision::Direct);
    assert_eq!(decided(set.evaluate(&domain("a.com", 443), None, &g)), Decision::Outbound("PROXY".into()));
}

#[test]
fn first_match_wins() {
    let set = rs(
        &["DOMAIN-SUFFIX,a.com,PROXY", "DOMAIN-SUFFIX,a.com,REJECT", "MATCH,DIRECT"],
        &["PROXY"],
    );
    let g = geo_stub();
    assert_eq!(decided(set.evaluate(&domain("a.com", 443), None, &g)), Decision::Outbound("PROXY".into()));
}

#[test]
fn unavailable_geo_skips_the_rule_never_blocks() {
    // GeoDb 指向不存在的文件。GEOSITE 规则应被跳过，流程继续。
    let set = rs(&["GEOSITE,cn,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let g = geo_stub();
    assert_eq!(decided(set.evaluate(&domain("baidu.com", 443), None, &g)), Decision::Outbound("PROXY".into()));
}

#[test]
fn mode_direct_short_circuits_everything() {
    let known: HashSet<String> = ["PROXY".to_string()].into_iter().collect();
    let set = RuleSet::build(&["MATCH,PROXY".to_string()], Mode::Direct, "", &known).unwrap();
    let g = geo_stub();
    assert_eq!(decided(set.evaluate(&domain("a.com", 443), None, &g)), Decision::Direct);
}

#[test]
fn mode_global_uses_global_outbound() {
    let known: HashSet<String> = ["PROXY".to_string(), "ALT".to_string()].into_iter().collect();
    let set = RuleSet::build(&["MATCH,PROXY".to_string()], Mode::Global, "ALT", &known).unwrap();
    let g = geo_stub();
    assert_eq!(decided(set.evaluate(&domain("a.com", 443), None, &g)), Decision::Outbound("ALT".into()));
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p wsieve-route --test engine`
Expected: `no method named evaluate found`

- [ ] **Step 3: 实现 evaluate**

在 `engine.rs` 的 `impl RuleSet` 块内追加：

```rust
    /// 两阶段求值。协议见设计文档 §4.2 纪律①。
    ///
    /// - 第一轮传 `resolved: None`。多数流量在域名类规则处命中，**不触发解析**
    /// - 返回 `NeedResolve` 时，调用方解析后传 `Some(&ips)` 再调一轮；
    ///   解析失败或超时传 `Some(&[])`
    /// - 第二轮**永不**再返回 `NeedResolve`
    ///
    /// 第二轮从头重扫而非断点续扫：规则只有几十条，开销可忽略，
    /// 换来的是函数完全幂等、无需维护游标状态。若断点续扫，`resolved`
    /// 只对触发点之后的规则可见，同一域名在更靠前的另一条 IP 规则上
    /// 会得到不同判决，幂等性直接破。
    pub fn evaluate(
        &self,
        target: &AddrPort,
        resolved: Option<&[IpAddr]>,
        geo: &GeoDb,
    ) -> Verdict {
        // mode 短路
        match self.mode {
            Mode::Direct => return Verdict::Decided(Decision::Direct),
            Mode::Global => return Verdict::Decided(self.global.clone()),
            Mode::Rule => {}
        }

        // 目标地址的两种形态，预先取出，避免每条规则重复 match
        let (domain, target_ip) = match &target.addr {
            TargetAddr::Domain(d) => (Some(d.trim_end_matches('.').to_ascii_lowercase()), None),
            TargetAddr::V4(o) => (None, Some(IpAddr::from(*o))),
            TargetAddr::V6(a) => (None, Some(IpAddr::from(*a))),
        };

        for rule in &self.rules {
            if rule.kind == RuleKind::Match {
                return Verdict::Decided(Decision::from(&rule.target));
            }

            let hit = match rule.kind {
                RuleKind::DstPort => matches!(&rule.value, RuleValue::Port(p) if *p == target.port),

                // ── 域名类：目标是 IP 就跳过 ──
                RuleKind::Domain
                | RuleKind::DomainSuffix
                | RuleKind::DomainKeyword
                | RuleKind::GeoSite => {
                    let Some(d) = domain.as_deref() else { continue };
                    let RuleValue::Text(v) = &rule.value else { continue };
                    match rule.kind {
                        RuleKind::Domain => d == v,
                        RuleKind::DomainSuffix => suffix_matches(d, v),
                        RuleKind::DomainKeyword => d.contains(v.as_str()),
                        // GEO 不可用时视为不匹配，绝不阻断连接（设计文档 §12）
                        RuleKind::GeoSite => geo.site_matches(v, d).unwrap_or(false),
                        _ => unreachable!(),
                    }
                }

                // ── IP 类：目标是域名则需要解析 ──
                RuleKind::IpCidr | RuleKind::GeoIp => {
                    let ips: &[IpAddr] = if let Some(ip) = &target_ip {
                        std::slice::from_ref(ip)
                    } else {
                        // 目标是域名
                        if rule.no_resolve {
                            continue;
                        }
                        match resolved {
                            None => {
                                // 第一轮：把解析需求抛给调用方
                                return Verdict::NeedResolve {
                                    domain: domain.clone().unwrap_or_default(),
                                };
                            }
                            // 第二轮：空切片即不匹配，继续往下
                            Some(ips) => ips,
                        }
                    };

                    match rule.kind {
                        RuleKind::IpCidr => ips.iter().any(|ip| rule.matches_ip(*ip)),
                        RuleKind::GeoIp => {
                            let RuleValue::Text(code) = &rule.value else { continue };
                            ips.iter()
                                .any(|ip| geo.ip_matches(code, *ip).unwrap_or(false))
                        }
                        _ => unreachable!(),
                    }
                }

                RuleKind::Match => unreachable!("已在循环开头处理"),
            };

            if hit {
                return Verdict::Decided(Decision::from(&rule.target));
            }
        }

        // build() 已保证 MATCH 存在，正常走不到这里；保底仍用 fallback
        Verdict::Decided(self.fallback.clone())
    }
```

在 `engine.rs` 文件末尾（测试模块之前）加辅助函数：

```rust
/// 后缀匹配，边界必须落在标签分隔点上。
/// `example.com` 匹配 `example.com` 与 `a.example.com`，但不匹配 `notexample.com`。
fn suffix_matches(domain: &str, suffix: &str) -> bool {
    if domain == suffix {
        return true;
    }
    domain
        .len()
        .checked_sub(suffix.len())
        .filter(|&i| i > 0)
        .is_some_and(|i| domain.as_bytes()[i - 1] == b'.' && &domain[i..] == suffix)
}
```

- [ ] **Step 4: 运行全部测试**

Run: `cargo test -p wsieve-route`
Expected: 全绿。**特别确认这三个**：`second_pass_never_asks_to_resolve_again`（两阶段协议的死线）、`domain_rule_before_ip_rule_short_circuits_without_dns`（零 DNS 收益）、`unavailable_geo_skips_the_rule_never_blocks`（GEO 故障不阻断）。

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-route/src/engine.rs crates/wsieve-route/tests/engine.rs
git commit -m "feat(route): 两阶段求值 evaluate() 与判决矩阵穷举测试"
```

---

### Task 11: CLI example 验证端到端

**Files:**
- Create: `crates/wsieve-route/examples/route.rs`

设计文档 §14 阶段 1 的验证方式是 CLI 子命令 `websieve route <target>`。此处落地为 cargo example —— 不污染 Tauri app，也不需要等配置 crate 就绪。

- [ ] **Step 1: 写 example**

```rust
//! 判决试算 CLI。
//!
//!   cargo run -p wsieve-route --example route -- \
//!       --rules rules.txt --geo-dir /tmp/wsieve-geo \
//!       --outbounds "日本节点,新加坡" \
//!       example.com:443
//!
//! rules.txt 每行一条 Clash 规则，支持 # 注释与空行。
//! 不做 DNS 解析：遇到 NeedResolve 会如实打印出来，这正是
//! 「哪些规则需要解析」的可视化。

use std::collections::HashSet;
use std::net::IpAddr;
use std::path::PathBuf;

use wsieve_geo::GeoDb;
use wsieve_proto::addr::{AddrPort, TargetAddr};
use wsieve_route::{Mode, RuleSet, Verdict};

fn main() {
    // 必须自己打印 Display。若直接让 main 返回 Result，Rust 打印的是
    // **Debug** 形式 —— thiserror 的 Debug 是 derive 来的，输出会是
    // `Error: MissingMatch`，而不是我们精心写的那段中文解释。
    if let Err(e) = run() {
        eprintln!("错误：{e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut rules_path = PathBuf::from("rules.txt");
    let mut geo_dir = PathBuf::from("/tmp/wsieve-geo");
    let mut outbounds: Vec<String> = Vec::new();
    let mut mode = Mode::Rule;
    let mut target_str: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--rules" => rules_path = args.next().ok_or("--rules 缺少参数")?.into(),
            "--geo-dir" => geo_dir = args.next().ok_or("--geo-dir 缺少参数")?.into(),
            "--outbounds" => {
                outbounds = args
                    .next()
                    .ok_or("--outbounds 缺少参数")?
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            }
            "--mode" => mode = args.next().ok_or("--mode 缺少参数")?.parse()?,
            other => target_str = Some(other.to_string()),
        }
    }

    let target_str = target_str.ok_or("用法：… [选项] <host:port>")?;
    let target = parse_target(&target_str)?;

    let lines: Vec<String> = std::fs::read_to_string(&rules_path)
        .map_err(|e| format!("读取 {} 失败：{e}", rules_path.display()))?
        .lines()
        .map(|s| s.to_string())
        .collect();

    let known: HashSet<String> = outbounds.into_iter().collect();
    let set = RuleSet::build(&lines, mode, "", &known)?;
    let geo = GeoDb::new(geo_dir.join("geoip.dat"), geo_dir.join("geosite.dat"));

    println!("目标：{}", target.display());
    println!("规则：{} 条", set.len());

    match set.evaluate(&target, None, &geo) {
        Verdict::Decided(d) => println!("判决：{d:?}（第一轮，未解析 DNS）"),
        Verdict::NeedResolve { domain } => {
            println!("第一轮请求解析：{domain}");
            // 演示第二轮：这里不真解析，用空结果表示「解析失败」
            match set.evaluate(&target, Some(&[] as &[IpAddr]), &geo) {
                Verdict::Decided(d) => {
                    println!("判决：{d:?}（第二轮，按解析失败处理）")
                }
                Verdict::NeedResolve { .. } => {
                    unreachable!("第二轮不该再请求解析 —— 若出现，是引擎 bug")
                }
            }
        }
    }
    Ok(())
}

fn parse_target(s: &str) -> Result<AddrPort, Box<dyn std::error::Error>> {
    let (host, port) = s.rsplit_once(':').ok_or("目标格式应为 host:port")?;
    let port: u16 = port.parse()?;
    let host = host.trim_matches(|c| c == '[' || c == ']');
    let addr = match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(a)) => TargetAddr::V4(a.octets()),
        Ok(IpAddr::V6(a)) => TargetAddr::V6(a.octets()),
        Err(_) => TargetAddr::Domain(host.to_string()),
    };
    Ok(AddrPort { addr, port })
}
```

- [ ] **Step 2: 造一个规则文件并跑**

```bash
cat > /tmp/wsieve-rules.txt <<'EOF'
# 广告拦截
GEOSITE,category-ads,REJECT
# 内网直连
IP-CIDR,192.168.0.0/16,DIRECT,no-resolve
DOMAIN-SUFFIX,google.com,日本节点
GEOSITE,cn,DIRECT
GEOIP,CN,DIRECT
MATCH,日本节点
EOF

cargo run -p wsieve-route --example route -- \
  --rules /tmp/wsieve-rules.txt --outbounds "日本节点" www.google.com:443
```

Expected: 输出 `判决：Outbound("日本节点")（第一轮，未解析 DNS）` —— 被 `DOMAIN-SUFFIX` 提前命中，**没有触发解析**。

- [ ] **Step 3: 验证需要解析的分支**

```bash
cargo run -p wsieve-route --example route -- \
  --rules /tmp/wsieve-rules.txt --outbounds "日本节点" unknown-site.example:443
```

Expected: 先打印 `第一轮请求解析：unknown-site.example`（被 `GEOIP,CN` 触发），再打印 `判决：Outbound("日本节点")（第二轮，按解析失败处理）`。

- [ ] **Step 4: 验证缺 MATCH 的报错可读**

```bash
grep -v '^MATCH' /tmp/wsieve-rules.txt > /tmp/wsieve-rules-nomatch.txt
cargo run -p wsieve-route --example route -- \
  --rules /tmp/wsieve-rules-nomatch.txt --outbounds "日本节点" a.com:443
```

Expected: 报错文本包含「缺少 MATCH 兜底」以及为什么不设隐式默认的解释。确认这条信息对用户是**可操作的**（告诉他加哪一行）。

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-route/examples/route.rs
git commit -m "feat(route): 判决试算 CLI example"
```

---

> **Part B 到此结束。** `cargo test -p wsieve-route` 应全绿，且 example 可跑通三种路径。

---

## Part C — `wsieve-config`

设计文档 §5.6 定下的纪律：**结构化保存必须保留规则注释**，否则选 YAML 的唯一理由就没了。

§5.6 同时给出了实现约束：**不能用 `serde-saphyr` 的 `Commented<T>`**（单 String 字段，无法区分前导与行尾注释，且 `comment_position` 是全局选项）。此处的具体落地是**行级定点改写**：

- 读：`serde_saphyr::from_str`，`rules` 用 `Vec<Spanned<String>>` 取得每条规则的**行号**
- 写：按行号定位，只替换该行 `- ` 之后、行尾注释之前的那一段。**缩进、前导注释、行尾注释、文件其余部分一个字节都不动**

用 `Location::line()`（总是可用）而非 `Span::byte_offset()`（返回 `Option`，"unavailable byte info" 时为 None）。

> **与设计文档的差异（有意为之，需回填 spec）**：§5.6 与 §14 写的是「`granit-parser` 事件流层面的定点改写」。此处落地为**行级**改写 —— 对「规则是字符串序列、每条占一行」这个特定结构，行级改写达成同样效果而代码量少一个数量级，且注释根本不经手。实现完成后请在 spec §5.6 补一句说明，免得日后看起来像是漏做了。
>
> **三条已知限制**（都以报错或良性降级收场，不会损坏文件）：
>
> 1. **规则值不含 `#`**。Clash 语法里 `#` 不出现在任何字段中，故「从右找 `#` 即行尾注释起点」是安全的。若将来支持含 `#` 的规则值，要改成带引号感知的扫描
> 2. **流式序列**（`rules: [A, B]`）里所有规则都在同一行，`replace_rule_line` 会返回 `Err(NotASequenceItem)` 而非破坏文件。UI 侧遇到这个错误应提示用户改用块式序列
> 3. **带引号的规则**（`- "DOMAIN,a.com,PROXY"`）被替换后引号会消失。值仍能正确读回，是良性的，但看到的人可能会意外

### Task 12: crate 骨架与配置模型

**Files:**
- Create: `crates/wsieve-config/Cargo.toml`
- Create: `crates/wsieve-config/src/lib.rs`
- Create: `crates/wsieve-config/src/model.rs`
- Modify: `Cargo.toml`

- [ ] **Step 1: 创建 Cargo.toml**

```toml
[package]
name = "wsieve-config"
version = "0.1.0"
edition = "2021"
description = "Clash 风格 YAML 配置的读取与保留注释的定点改写"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde-saphyr = "1"
thiserror = { workspace = true }
```

`serde-saphyr` 的默认 features 已含 `serialize` + `deserialize`，无需额外指定。

- [ ] **Step 2: 注册进 workspace**

```toml
    "crates/wsieve-config",
```

- [ ] **Step 3: 写模型**

`crates/wsieve-config/src/model.rs`：

```rust
//! 配置模型。字段名与设计文档 §5.2 的 YAML schema 一一对应，
//! 命名沿用 Clash 的 kebab-case —— 用户可以直接照抄现成配置。

use serde::{Deserialize, Serialize};
use serde_saphyr::Spanned;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Config {
    // ── 入口 ──
    pub mixed_port: u16,
    pub bind_address: String,
    pub allow_lan: bool,
    pub mode: String,
    pub global_outbound: String,
    pub log_level: String,
    pub system_proxy: bool,

    // ── 出站 ──
    pub proxies: Vec<Proxy>,

    // ── 规则 ──
    /// 带行号：定点改写靠它定位。业务侧读值用 `.value`。
    pub rules: Vec<Spanned<String>>,

    // ── 其余 ──
    pub dns: Dns,
    pub tun: Tun,
    pub geo_auto_update: bool,
    pub geo_update_interval: u32,
    pub geox_url: GeoxUrl,

    // ── websieve 专有 ──
    pub carrier: String,
    pub carrier_host: String,
    pub shard_base_port: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mixed_port: 7890,
            bind_address: "127.0.0.1".into(),
            allow_lan: false,
            mode: "rule".into(),
            global_outbound: String::new(),
            log_level: "info".into(),
            system_proxy: false,
            proxies: Vec::new(),
            rules: Vec::new(),
            dns: Dns::default(),
            tun: Tun::default(),
            geo_auto_update: true,
            geo_update_interval: 24,
            geox_url: GeoxUrl::default(),
            carrier: "shared".into(),
            carrier_host: String::new(),
            shard_base_port: 18443,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct Proxy {
    pub name: String,
    /// 只接受 "websieve"。其他类型（ss / vmess / trojan…）在校验期
    /// 明确报错而非静默忽略 —— 用户粘贴整份 Clash 配置时要知道为什么不生效。
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    pub server_pub: String,
    pub client_priv: String,
    #[serde(default = "default_extra_sessions")]
    pub extra_sessions: usize,
    #[serde(default = "default_mux_prefs")]
    pub mux_prefs: Vec<u8>,
}

fn default_extra_sessions() -> usize {
    3
}
fn default_mux_prefs() -> Vec<u8> {
    vec![0, 1, 2, 3, 4]
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Dns {
    pub enable: bool,
    pub listen: String,
    pub enhanced_mode: String,
    pub fake_ip_range: String,
    pub fake_ip_filter: Vec<String>,
    pub nameserver: Vec<String>,
    pub proxy_server_nameserver: Vec<String>,
    pub timeout_ms: u64,
    pub cache: DnsCache,
}

impl Default for Dns {
    fn default() -> Self {
        Self {
            enable: true,
            listen: String::new(),
            enhanced_mode: "fake-ip".into(),
            fake_ip_range: "198.18.0.0/15".into(),
            fake_ip_filter: Vec::new(),
            nameserver: vec!["https://1.1.1.1/dns-query".into()],
            proxy_server_nameserver: vec!["system".into()],
            timeout_ms: 2000,
            cache: DnsCache::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct DnsCache {
    pub max: usize,
    pub negative_ttl_s: u64,
}

impl Default for DnsCache {
    fn default() -> Self {
        Self { max: 4096, negative_ttl_s: 30 }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct Tun {
    pub enable: bool,
    pub stack: String,
    pub auto_route: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct GeoxUrl {
    pub geoip: String,
    pub geosite: String,
}
```

- [ ] **Step 4: 写 lib.rs 并验证编译**

```rust
//! Clash 风格 YAML 配置的读写。
//!
//! 读走 serde-saphyr 的反序列化；写**不走** serde 序列化，而是按行号
//! 定点改写（见 edit.rs 与设计文档 §5.6）—— 否则用户手写的规则注释
//! 会在 UI 点一次开关之后全部消失。

pub mod edit;
pub mod model;

pub use model::{Config, Dns, GeoxUrl, Proxy, Tun};
```

Run: `cargo check -p wsieve-config`
Expected: 报 `edit` 模块缺失。先建一个空的 `src/edit.rs` 再 check 一次，应通过。

- [ ] **Step 5: 提交**

```bash
git add Cargo.toml crates/wsieve-config/
git commit -m "chore(config): 建立 wsieve-config crate 与配置模型"
```

---

### Task 13: 读取、校验与语法错行号

**Files:**
- Modify: `crates/wsieve-config/src/lib.rs`

- [ ] **Step 1: 写失败的测试**

在 `lib.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
mixed-port: 7890
proxies:
  - name: "日本节点"
    type: websieve
    url: https://example.com/
    server-pub: "aa"
    client-priv: "bb"
rules:
  - MATCH,日本节点
"#;

    #[test]
    fn parses_minimal_config() {
        let c = load_str(MINIMAL).unwrap();
        assert_eq!(c.mixed_port, 7890);
        assert_eq!(c.proxies.len(), 1);
        assert_eq!(c.proxies[0].name, "日本节点");
        assert_eq!(c.rules.len(), 1);
        assert_eq!(c.rules[0].value, "MATCH,日本节点");
    }

    #[test]
    fn omitted_fields_get_defaults() {
        let c = load_str(MINIMAL).unwrap();
        assert_eq!(c.mode, "rule");
        assert_eq!(c.shard_base_port, 18443);
        assert_eq!(c.dns.timeout_ms, 2000);
        assert_eq!(c.carrier, "shared");
    }

    #[test]
    fn rules_carry_line_numbers() {
        // rules 从第 10 行开始（首行是空行）
        let c = load_str(MINIMAL).unwrap();
        assert!(c.rules[0].defined.line() > 0, "行号应为正数");
    }

    #[test]
    fn syntax_error_reports_a_line_number() {
        let bad = "mixed-port: 7890\n  bad-indent: true\n";
        let e = load_str(bad).unwrap_err();
        match e {
            ConfigError::Syntax { line, .. } => assert!(line > 0, "应给出行号"),
            other => panic!("应是语法错，实为 {other:?}"),
        }
    }

    #[test]
    fn unsupported_proxy_type_is_named_explicitly() {
        let cfg = r#"
proxies:
  - name: "别人的节点"
    type: vmess
    url: https://x.com/
    server-pub: "aa"
    client-priv: "bb"
rules:
  - MATCH,别人的节点
"#;
        let c = load_str(cfg).unwrap();
        let e = c.validate().unwrap_err().to_string();
        assert!(e.contains("vmess"), "要点名不支持的类型：{e}");
        assert!(e.contains("别人的节点"), "要点名是哪个节点：{e}");
    }

    #[test]
    fn duplicate_proxy_names_are_rejected() {
        // 规则用名字引用出站，重名会让引用产生歧义
        let cfg = r#"
proxies:
  - name: "A"
    type: websieve
    url: https://x.com/
    server-pub: "aa"
    client-priv: "bb"
  - name: "A"
    type: websieve
    url: https://y.com/
    server-pub: "cc"
    client-priv: "dd"
rules:
  - MATCH,A
"#;
        let e = load_str(cfg).unwrap().validate().unwrap_err().to_string();
        assert!(e.contains('A'), "{e}");
    }

    #[test]
    fn outbound_names_are_exposed_for_rule_validation() {
        let c = load_str(MINIMAL).unwrap();
        let names = c.outbound_names();
        assert!(names.contains("日本节点"));
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p wsieve-config --lib`
Expected: `cannot find function load_str`

- [ ] **Step 3: 写实现**

在 `lib.rs` 的模块声明之后、测试模块之前：

```rust
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("读取 {path} 失败：{source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("第 {line} 行 YAML 语法错误：{message}")]
    Syntax { line: u64, message: String },
    #[error("不支持的出站类型 {kind}（节点「{name}」）。\
             websieve 只支持自有协议，type 必须是 websieve。\
             若这是从 Clash 配置粘贴来的，其中的 ss / vmess / trojan 等节点无法使用")]
    UnsupportedProxyType { name: String, kind: String },
    #[error("出站名重复：{0}。规则用名字引用出站，重名会产生歧义")]
    DuplicateProxyName(String),
}

pub fn load_str(s: &str) -> Result<Config, ConfigError> {
    serde_saphyr::from_str::<Config>(s).map_err(|e| {
        // serde-saphyr 的错误自带位置信息；取不到时退化为 0
        let line = extract_line(&e);
        ConfigError::Syntax {
            line,
            message: e.to_string(),
        }
    })
}

pub fn load_file(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.display().to_string(),
        source,
    })?;
    load_str(&text)
}

/// 从错误文本里提取行号。serde-saphyr 的 Display 会带 "at line N"。
/// 拿不到就返回 0 —— UI 侧据此决定是否高亮某一行。
fn extract_line(e: &impl std::fmt::Display) -> u64 {
    let s = e.to_string();
    for key in ["line ", "行 "] {
        if let Some(i) = s.find(key) {
            let rest = &s[i + key.len()..];
            let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = num.parse::<u64>() {
                return n;
            }
        }
    }
    0
}

impl Config {
    /// 出站名集合，交给 `wsieve_route::RuleSet::build` 做规则引用校验。
    pub fn outbound_names(&self) -> HashSet<String> {
        self.proxies.iter().map(|p| p.name.clone()).collect()
    }

    /// 配置自身的校验。规则的校验在 wsieve-route 里做 ——
    /// 本 crate 刻意不认识规则语义。
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut seen: HashSet<&str> = HashSet::new();
        for p in &self.proxies {
            if p.kind != "websieve" {
                return Err(ConfigError::UnsupportedProxyType {
                    name: p.name.clone(),
                    kind: p.kind.clone(),
                });
            }
            if !seen.insert(p.name.as_str()) {
                return Err(ConfigError::DuplicateProxyName(p.name.clone()));
            }
        }
        Ok(())
    }
}
```

> `extract_line` 是个务实的近似。**Step 4 会验证它对真实错误有效**；若 serde-saphyr 的错误格式对不上，改用 `serde_saphyr::from_str_with_options` 配合 `Locations` 拿精确位置。不要留着一个从不生效的行号。

- [ ] **Step 4: 运行测试并确认行号真的有效**

Run: `cargo test -p wsieve-config --lib -- --nocapture`
Expected: 7 个测试全部 PASS。

**额外验证**（不要跳过）：临时在 `syntax_error_reports_a_line_number` 里加一行 `eprintln!("{e}")`，确认 `line` 不是 0。**若恒为 0，说明 `extract_line` 没匹配上格式，必须改用 `Locations` 而不是让测试将就通过。**

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-config/src/lib.rs
git commit -m "feat(config): YAML 读取、校验与语法错行号"
```

---

### Task 14: 规则的行级定点改写

**Files:**
- Create: `crates/wsieve-config/src/edit.rs`

- [ ] **Step 1: 写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "\
mixed-port: 7890
rules:
  # 公司内网走直连，勿删
  - IP-CIDR,10.0.0.0/8,DIRECT,no-resolve   # 内网段
  - GEOSITE,cn,DIRECT
  - MATCH,日本节点
";

    fn rules_of(src: &str) -> Vec<(u64, String)> {
        let c = crate::load_str(src).unwrap();
        c.rules
            .iter()
            .map(|r| (r.defined.line(), r.value.clone()))
            .collect()
    }

    #[test]
    fn replacing_a_rule_keeps_every_comment() {
        let rules = rules_of(SRC);
        let (line, _) = rules[1]; // GEOSITE,cn,DIRECT
        let out = replace_rule_line(SRC, line, "GEOSITE,cn,日本节点").unwrap();

        assert!(out.contains("# 公司内网走直连，勿删"), "前导注释必须还在");
        assert!(out.contains("# 内网段"), "另一行的行尾注释必须还在");
        assert!(out.contains("GEOSITE,cn,日本节点"), "新值要写进去");
        assert!(!out.contains("GEOSITE,cn,DIRECT"), "旧值要被替换");
    }

    #[test]
    fn replacing_keeps_the_rules_own_trailing_comment() {
        let rules = rules_of(SRC);
        let (line, _) = rules[0]; // 带行尾注释的那条
        let out = replace_rule_line(SRC, line, "IP-CIDR,172.16.0.0/12,DIRECT,no-resolve").unwrap();

        assert!(out.contains("# 内网段"), "被改那一行的行尾注释也要保留");
        assert!(out.contains("IP-CIDR,172.16.0.0/12"), "新值要写进去");
    }

    #[test]
    fn indentation_is_preserved() {
        let rules = rules_of(SRC);
        let out = replace_rule_line(SRC, rules[2].0, "MATCH,DIRECT").unwrap();
        assert!(out.contains("\n  - MATCH,DIRECT"), "两空格缩进要保留：\n{out}");
    }

    #[test]
    fn untouched_bytes_are_identical() {
        let rules = rules_of(SRC);
        let out = replace_rule_line(SRC, rules[1].0, "GEOSITE,cn,DIRECT").unwrap();
        // 用同样的值替换 → 应该逐字节等于原文
        assert_eq!(out, SRC, "同值替换必须是恒等操作");
    }

    #[test]
    fn deleting_a_rule_removes_its_leading_comments_too() {
        let rules = rules_of(SRC);
        let out = delete_rule_line(SRC, rules[0].0).unwrap();
        assert!(!out.contains("IP-CIDR,10.0.0.0/8"), "规则要被删掉");
        assert!(!out.contains("# 公司内网走直连"), "它的前导注释要一起删掉");
        assert!(out.contains("GEOSITE,cn,DIRECT"), "别的规则不受影响");
    }

    #[test]
    fn out_of_range_line_is_an_error_not_a_panic() {
        assert!(replace_rule_line(SRC, 9999, "MATCH,DIRECT").is_err());
        assert!(replace_rule_line(SRC, 0, "MATCH,DIRECT").is_err());
    }

    #[test]
    fn line_without_a_dash_is_rejected() {
        // 指到 "rules:" 那一行 —— 不是规则项，必须报错
        assert!(replace_rule_line(SRC, 2, "MATCH,DIRECT").is_err());
    }

    #[test]
    fn bare_dash_is_rejected_not_panicked_on() {
        // `  -` 是合法 YAML（空序列项）但不是规则。
        // 按 `- ` 的长度去切片会越界 panic，必须当作非规则项拒绝。
        let src = "rules:\n  -\n  - MATCH,DIRECT\n";
        assert!(replace_rule_line(src, 2, "MATCH,PROXY").is_err());
        assert!(delete_rule_line(src, 2).is_err());
    }

    #[test]
    fn crlf_input_keeps_crlf() {
        let src = SRC.replace('\n', "\r\n");
        let c = crate::load_str(&src).unwrap();
        let line = c.rules[2].defined.line();
        let out = replace_rule_line(&src, line, "MATCH,DIRECT").unwrap();
        assert!(out.contains("\r\n"), "不能把 CRLF 悄悄改成 LF");
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p wsieve-config --lib edit::`
Expected: `cannot find function replace_rule_line`

- [ ] **Step 3: 写实现**

```rust
//! 规则的行级定点改写。
//!
//! 为什么不用 serde 序列化整份写回：那会抹掉用户手写的全部注释，
//! 而「能写注释」是配置选用 YAML 的唯一理由（设计文档 §5.1 / §5.6）。
//!
//! 为什么不用 serde-saphyr 的 Commented<T>：它是单个 String 字段，
//! 无法区分前导与行尾注释，且 comment_position 是全局选项 —— 实测
//! 第一轮读写就会改变用户原文。详见设计文档 §5.6 的实现约束。
//!
//! 这里只碰目标行 `- ` 之后、行尾注释之前的那一段，其余字节原样透传。

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("行号 {0} 超出范围（文件共 {1} 行）")]
    LineOutOfRange(u64, usize),
    #[error("第 {0} 行不是一个规则项（找不到 `- ` 前缀）")]
    NotASequenceItem(u64),
}

/// 把第 `line` 行（1-based）的规则值换成 `new_value`。
/// 缩进、行尾注释、行尾换行符（LF / CRLF）全部保留。
pub fn replace_rule_line(src: &str, line: u64, new_value: &str) -> Result<String, EditError> {
    let lines: Vec<&str> = split_keep_ends(src);
    let idx = check_index(line, lines.len())?;

    let (body, eol) = split_eol(lines[idx]);
    let dash = find_dash(body).ok_or(EditError::NotASequenceItem(line))?;

    // `- ` 之后到行尾注释之前，就是值的地盘
    let after_dash = &body[dash..];
    let comment_at = after_dash.find('#');
    let (value_part, comment_part) = match comment_at {
        Some(c) => (&after_dash[..c], &after_dash[c..]),
        None => (after_dash, ""),
    };
    // 值与注释之间的空白照原样留着，视觉对齐不被破坏
    let gap: String = value_part
        .chars()
        .rev()
        .take_while(|c| c.is_whitespace())
        .collect();

    let mut out = String::with_capacity(src.len() + new_value.len());
    for (i, l) in lines.iter().enumerate() {
        if i == idx {
            out.push_str(&body[..dash]);
            out.push_str(new_value);
            out.push_str(&gap);
            out.push_str(comment_part);
            out.push_str(eol);
        } else {
            out.push_str(l);
        }
    }
    Ok(out)
}

/// 删除第 `line` 行的规则，连同紧贴它上方的前导注释块一起删。
///
/// 「紧贴」的定义：从目标行往上，连续的、缩进相同的纯注释行。
/// 中间一旦出现空行或别的规则就停 —— 那些注释不属于这一条。
pub fn delete_rule_line(src: &str, line: u64) -> Result<String, EditError> {
    let lines: Vec<&str> = split_keep_ends(src);
    let idx = check_index(line, lines.len())?;
    let (body, _) = split_eol(lines[idx]);
    if find_dash(body).is_none() {
        return Err(EditError::NotASequenceItem(line));
    }

    let mut start = idx;
    while start > 0 {
        let (prev, _) = split_eol(lines[start - 1]);
        if prev.trim_start().starts_with('#') && !prev.trim().is_empty() {
            start -= 1;
        } else {
            break;
        }
    }

    let mut out = String::with_capacity(src.len());
    for (i, l) in lines.iter().enumerate() {
        if i < start || i > idx {
            out.push_str(l);
        }
    }
    Ok(out)
}

fn check_index(line: u64, total: usize) -> Result<usize, EditError> {
    if line == 0 || line as usize > total {
        return Err(EditError::LineOutOfRange(line, total));
    }
    Ok(line as usize - 1)
}

/// 按行切分但**保留**行尾换行符，这样重组时 LF / CRLF 原样还原。
fn split_keep_ends(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, c) in s.char_indices() {
        if c == '\n' {
            out.push(&s[start..=i]);
            start = i + 1;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// 拆成 (正文, 行尾换行符)。
fn split_eol(line: &str) -> (&str, &str) {
    if let Some(b) = line.strip_suffix("\r\n") {
        (b, "\r\n")
    } else if let Some(b) = line.strip_suffix('\n') {
        (b, "\n")
    } else {
        (line, "")
    }
}

/// 返回 `- ` 之后第一个字符的下标。
///
/// 只接受 `- ` 开头的序列项。裸 `-`（YAML 里合法的空序列项）**拒绝**：
/// 它不是一条规则，而且它只有 indent+1 个字节 —— 若按 indent+2 切片会
/// 直接 panic（`byte index 4 is out of bounds of "  -"`）。
fn find_dash(body: &str) -> Option<usize> {
    let trimmed = body.trim_start();
    if !trimmed.starts_with("- ") {
        return None;
    }
    let indent = body.len() - trimmed.len();
    Some(indent + 2)
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p wsieve-config --lib edit::`
Expected: 9 个测试全部 PASS。**`untouched_bytes_are_identical` 是这组的核心** —— 它证明改写是外科手术而非重排；`bare_dash_is_rejected_not_panicked_on` 则守住了唯一一处会 panic 的边界。

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-config/src/edit.rs
git commit -m "feat(config): 规则的行级定点改写（保留全部注释）"
```

---

### Task 15: 注释保留往返验收

**Files:**
- Create: `crates/wsieve-config/tests/roundtrip.rs`

设计文档 §13 列的那条测试。单测已覆盖改写函数本身，这里验的是**完整往返**：读 → 改 → 写 → 再读，语义与注释同时成立。

- [ ] **Step 1: 写测试**

```rust
//! 注释保留往返（设计文档 §13）。
//!
//! 仅断言「语义不变」是不够的 —— serde 序列化也能通过那种测试，
//! 却会把注释全部抹掉。这里逐字检查注释仍在。

use wsieve_config::{edit, load_str};

const SRC: &str = "\
# websieve 配置
mixed-port: 7890
mode: rule

proxies:
  - name: \"日本节点\"
    type: websieve
    url: https://example.com/
    server-pub: \"aa\"
    client-priv: \"bb\"

rules:
  # 广告一律拦掉
  - GEOSITE,category-ads,REJECT
  # 内网直连，别删这条
  - IP-CIDR,192.168.0.0/16,DIRECT,no-resolve   # 家里的网段
  - GEOSITE,cn,DIRECT
  - MATCH,日本节点
";

#[test]
fn edit_one_rule_and_everything_else_survives() {
    let before = load_str(SRC).unwrap();
    assert_eq!(before.rules.len(), 4);

    // 把第三条 GEOSITE,cn,DIRECT 改成走代理
    let line = before.rules[2].defined.line();
    let after_text = edit::replace_rule_line(SRC, line, "GEOSITE,cn,日本节点").unwrap();

    // 1) 注释逐条还在
    for c in [
        "# websieve 配置",
        "# 广告一律拦掉",
        "# 内网直连，别删这条",
        "# 家里的网段",
    ] {
        assert!(after_text.contains(c), "注释丢失：{c}");
    }

    // 2) 语义正确
    let after = load_str(&after_text).unwrap();
    assert_eq!(after.rules.len(), 4);
    assert_eq!(after.rules[2].value, "GEOSITE,cn,日本节点");
    assert_eq!(after.rules[0].value, "GEOSITE,category-ads,REJECT");
    assert_eq!(after.rules[3].value, "MATCH,日本节点");

    // 3) 其余配置项没被动过
    assert_eq!(after.mixed_port, before.mixed_port);
    assert_eq!(after.proxies[0].name, before.proxies[0].name);
    assert_eq!(after.proxies[0].client_priv, before.proxies[0].client_priv);
}

#[test]
fn repeated_edits_do_not_accumulate_drift() {
    // 连续改 5 次，每次都能正确读回 —— 排除「每轮多一个空格」这类渐变
    let mut text = SRC.to_string();
    for i in 0..5 {
        let c = load_str(&text).unwrap();
        let line = c.rules[2].defined.line();
        let v = format!("GEOSITE,cn,节点{i}");
        text = edit::replace_rule_line(&text, line, &v).unwrap();
        let back = load_str(&text).unwrap();
        assert_eq!(back.rules[2].value, v);
        assert_eq!(back.rules.len(), 4, "第 {i} 轮规则数变了");
    }
    assert!(text.contains("# 家里的网段"), "多轮之后注释仍在");
}

#[test]
fn identity_edit_is_byte_for_byte_stable() {
    let c = load_str(SRC).unwrap();
    let line = c.rules[1].defined.line();
    let same = c.rules[1].value.clone();
    let out = edit::replace_rule_line(SRC, line, &same).unwrap();
    assert_eq!(out, SRC, "同值替换必须逐字节恒等");
}
```

- [ ] **Step 2: 运行测试**

Run: `cargo test -p wsieve-config --test roundtrip`
Expected: 3 个测试全部 PASS

- [ ] **Step 3: 跑整个 workspace**

Run: `cargo test --workspace`
Expected: 全绿。既有的 proto / transport / xhttp / mux / socks5 测试不受影响。

- [ ] **Step 4: 跑一次 clippy（只针对本阶段的三个新 crate）**

Run: `cargo clippy -p wsieve-geo -p wsieve-route -p wsieve-config --all-targets -- -D warnings`
Expected: 无警告。

> **执行期更新（2026-08-25）**：原先此处写着「仓库现有 3 处 clippy 报错属范围外，跑
> `--workspace` 会卡在别人的债上」。实际执行 Task 8 时发现**躲不掉**——
> `cargo clippy -p wsieve-route` 会连带检查依赖，`wsieve-proto` 的报错让
> `wsieve-route` 也过不了 `-D warnings`。
>
> 那三处已在提交 `style(proto): 清掉三处 clippy 报错以解除下游门禁阻塞` 中单独清理
> （`hello.rs` 的 `doc_lazy_continuation`、`stripe.rs` 的 `type_complexity`），
> 上面的命令现在可以直接通过。
>
> 教训：「范围外的技术债」在依赖图上游时，并不真的在范围外。

- [ ] **Step 5: 提交**

```bash
git add crates/wsieve-config/tests/roundtrip.rs
git commit -m "test(config): 注释保留往返验收"
```

---

## 阶段 1 完成标准

全部勾选后本阶段才算完成：

- [ ] `cargo test --workspace` 全绿
- [ ] `cargo clippy -p wsieve-geo -p wsieve-route -p wsieve-config --all-targets -- -D warnings` 无警告（仓库既有的 3 处 clippy 报错属阶段 1 范围外，见 Task 15 Step 4）
- [ ] 下载真实 `geoip.dat` / `geosite.dat` 后，`cargo test -p wsieve-geo --test real_dat -- --nocapture` 通过，且记录了跳过的 Regex 条目数
- [ ] `cargo run -p wsieve-route --example route` 能跑通三条路径：域名规则直接命中（零解析）、IP 规则请求解析、缺 MATCH 报出可操作的错误
- [ ] 注释保留往返测试通过 —— 这是本阶段最容易被将就过去的一条，**不要为了让测试通过而放宽断言**

## 交给阶段 2 的接口

阶段 2 会这样把三者串起来：

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

// 入口层拿到 AddrPort 之后：
match rules.evaluate(&target, None, &geo) {
    Verdict::Decided(d) => dispatch(d),
    Verdict::NeedResolve { domain } => {
        let ips = resolver.lookup(&domain).await.unwrap_or_default();
        match rules.evaluate(&target, Some(&ips), &geo) {
            Verdict::Decided(d) => dispatch(d),
            Verdict::NeedResolve { .. } => unreachable!("两阶段协议保证不会发生"),
        }
    }
}
```

**已知的未尽事项**（阶段 2 处理，此处不做）：

- 配置的**写入落盘**（0600 权限创建、原子替换）。本阶段只做内存里的文本改写，不碰文件系统
- 规则的**新增与排序**。本阶段只做替换与删除 —— 这两个足以驱动往返测试证明注释保留可行；新增/排序留到规则 UI 真正需要时（阶段 5）
- GEO 文件的下载与更新
