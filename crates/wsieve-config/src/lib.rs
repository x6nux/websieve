//! Clash 风格 YAML 配置的读写。
//!
//! 读走 serde-saphyr 的反序列化；写**不走** serde 序列化，而是按行号
//! 定点改写（见 edit.rs 与设计文档 §5.6）—— 否则用户手写的规则注释
//! 会在 UI 点一次开关之后全部消失。

pub mod edit;
pub mod model;

pub use model::{Config, Dns, DnsCache, GeoxUrl, Proxy, Tun};
