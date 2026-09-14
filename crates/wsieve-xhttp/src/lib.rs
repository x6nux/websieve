//! xhttp 传输层（spec §6/§7）：默认 mux 选型常量 + 客户端/服务端实现。

pub mod client;
pub mod link_profile;
pub mod server;

use wsieve_proto::hello::MuxId;

/// 默认 mux（spec §7.7 benchmark 选定，2026-08-24 实测）。
///
/// 选 smux 而非预期中的 tokio-yamux：四场景实测中 smux 的首字节延迟
/// 全面更低（cross 场景 P50 11.6ms vs 13.4ms，weak 场景 35.0ms vs 41.7ms），
/// 吞吐持平或更好（burst 0.7 MB/s 持平、local 17.8 vs 11.3 MB/s），公平性
/// 极差更小，且全程零失败。yamux 仍是无交集时的协商回退基线（§7.5，
/// 线格式跨语言），两者职责不同：DEFAULT_MUX 是偏好列表首位，回退基线
/// 是协议底线。
pub const DEFAULT_MUX: MuxId = MuxId::Wsmux;
