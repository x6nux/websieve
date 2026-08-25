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
pub use inject::RoutingResolver;
pub use resolver::{bootstrap, bootstrap_with, DnsResolver, ResolverError};
pub use upstream::{parse_nameserver, UpstreamError};

/// `bootstrap()` 返回的解析器类型，从本 crate 转出。
///
/// `bootstrap()` 的返回值是 hickory 的类型，调用方（`shard::resolve_upstream`）
/// 要在签名里写出它。转出一份而非让调用方自己依赖 `hickory-resolver`：
/// 版本号只钉在本 crate 的 Cargo.toml 一处，下游跟着走。两处各写一个版本
/// 约束的话，升级时会出现「同一个 `TokioResolver` 却是两个类型」这种
/// 极难读懂的编译错误。
pub use hickory_resolver::TokioResolver;
