//! TUN 入口与 fake-ip（设计文档 §7.1 第二层 + §8.3）。
//!
//! 本 crate 的存在理由是**可测性**：TUN 的两个陷阱（环路、启动顺序）
//! 都是逻辑问题而非 IO 问题，把逻辑摘出来就能穷举单测，不需要 root。
//! 真正需要特权的两件事——创建 utun、写路由表——被隔离在 `startup::TunStage`
//! 与 `managed::RouteBackend` 两个 trait 后面。
//!
//! **TUN 是纯增量**（§4.2 纪律②）：netstack 把 IP 包转成 `TcpStream` 之后，
//! 路由层与出站层**完全复用**，与混合端口入站走同一条路。本 crate 不含
//! 任何转发、任何出站、任何规则匹配。
//!
//! # 模块声明的纪律
//!
//! 模块**随实现一起加进来**，不预先声明空壳。计划文档里一次性列全九个
//! `pub mod` 会让骨架提交无法通过 `cargo check`，而「每个提交都能构建」
//! 是本仓库的既有底线（workspace 门禁是 `cargo test --workspace`）。
//! 后续 task 各自补一行声明即可。

pub mod bypass;
pub mod fakeip;
