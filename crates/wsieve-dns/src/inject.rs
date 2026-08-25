//! 解析注入：把「拿一批 IP」这件事从判决链路里抽出来，成为一个接口。
//!
//! 判决驱动方 `decide()` 依赖本 trait 而非具体解析器，换来两样东西：
//!
//! 1. **阶段 2 的出站管理器可以持有 `Arc<dyn RoutingResolver>`**，在
//!    「配置了 DNS」与「尚未就绪」之间切换而不必改 `decide()` 的签名；
//! 2. 判决链路的测试可以对着一台**受控的本地 DNS 服务器**跑真解析器
//!    （见 `tests/routing.rs`），既不依赖外网，也不必伪造解析器本身。
//!
//! trait 方法**不返回 Result**：这是纪律③（设计文档 §7.2）在类型层面的
//! 表达 —— 解析失败不是一个需要调用方处理的错误，而是「这批 IP 是空的」
//! 这一普通事实。签名里根本没有失败这条路，调用方也就无从写出
//! 「解析失败就阻断连接」的代码。

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;

/// 判决路径需要的全部解析能力。
///
/// **为什么手写 boxed future 而不是用 `async fn`：** Rust 1.92 的 trait 里
/// 可以直接写 `async fn`（AFIT），但那样的 trait **不是 dyn 兼容的**，
/// `Arc<dyn RoutingResolver>` 会编译失败 —— 而阶段 2 正需要这种用法。
///
/// **为什么不用 `async-trait`：** 它确实在 workspace 里（`wsieve-transport`
/// 等 4 个 crate 已在用），加进来是零边际成本，也同样 dyn 安全。只是本
/// trait 只有一个方法、签名一眼可读，展开一层过程宏换不来什么。这是
/// 取舍不是禁忌 —— 将来若方法变多，换成 `#[async_trait]` 是纯收益。
pub trait RoutingResolver: Send + Sync {
    /// 解析域名。**永不失败**：超时、NXDOMAIN、上游不可达一律返回空 Vec，
    /// 由调用方传给 `evaluate(target, Some(&ips), ..)`，空切片即不匹配。
    fn resolve<'a>(
        &'a self,
        domain: &'a str,
    ) -> Pin<Box<dyn Future<Output = Vec<IpAddr>> + Send + 'a>>;
}

impl RoutingResolver for crate::resolver::DnsResolver {
    fn resolve<'a>(
        &'a self,
        domain: &'a str,
    ) -> Pin<Box<dyn Future<Output = Vec<IpAddr>> + Send + 'a>> {
        Box::pin(self.lookup_for_routing(domain))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 阶段 2 会把解析器装进 `Arc<dyn RoutingResolver>`（见计划的
    /// 「交给阶段 2 的接口」一节）。dyn 兼容性是本 trait 手写 boxed future
    /// 而非用 `async fn` 的**唯一理由**，因此必须有测试盯着它 ——
    /// 若哪天有人把签名改成 `async fn`，这里会立刻编译失败，
    /// 而不是等阶段 2 的代码红了才发现。
    #[test]
    fn the_trait_is_dyn_compatible_because_phase_two_needs_arc_dyn() {
        fn assert_usable_as_dyn(_: std::sync::Arc<dyn RoutingResolver>) {}

        let real = crate::resolver::DnsResolver::new(
            &["https://1.1.1.1/dns-query".to_string()],
            std::time::Duration::from_secs(2),
            4096,
            std::time::Duration::from_secs(30),
        )
        .expect("上游是合法 IP 字面量，应能建起来");

        // 不发任何查询，只验证类型层面的可用性。
        assert_usable_as_dyn(std::sync::Arc::new(real));
    }
}
