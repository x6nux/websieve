//! 静态解析表（配置的 `hosts` 段）。
//!
//! 两个消费者，共用**同一张表**：
//!
//! 1. 判决路径 —— [`HostsResolver`] 包在真解析器外层，命中就不查 DNS，
//!    于是 `GEOIP` / `IP-CIDR` 这类规则拿到的是用户指定的 IP；
//! 2. 转发路径 —— 调用方在判决**之后**用 [`Hosts::lookup`] 把域名目标
//!    换成 IP 字面量。
//!
//! 顺序不能反。先改写再判决的话，目标已经是 IP，`DOMAIN-SUFFIX` /
//! `DOMAIN-KEYWORD` 这类规则会集体失效——用户只会看到「配了 hosts 之后
//! 我的域名规则突然都不命中了」，而两处配置各自看着都没错。

use std::collections::BTreeMap;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;

use crate::inject::RoutingResolver;

/// 域名 → IP 的静态表。**精确匹配**，大小写不敏感（DNS 本就如此）。
///
/// 键在构造时统一转小写，查询时也转——不这么做的话，用户写
/// `Example.com` 而浏览器请求 `example.com`，条目静默失效。
#[derive(Debug, Clone, Default)]
pub struct Hosts(BTreeMap<String, IpAddr>);

impl Hosts {
    /// 从配置的 `hosts` 段构造。值解析不了的条目**跳过并告知调用方**，
    /// 不静默丢弃——`Config::validate` 已经拦过一道，走到这里还有非法值
    /// 说明校验与构造对「什么算合法」的看法不一致，那是个 bug，得看得见。
    pub fn build<'a, I>(entries: I) -> (Self, Vec<String>)
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut table = BTreeMap::new();
        let mut rejected = Vec::new();
        for (host, ip) in entries {
            let key = host.trim().to_ascii_lowercase();
            if key.is_empty() {
                rejected.push(format!("空域名键（值「{ip}」）"));
                continue;
            }
            match ip.trim().parse::<IpAddr>() {
                Ok(addr) => {
                    table.insert(key, addr);
                }
                Err(_) => rejected.push(format!("{host} → 「{ip}」不是 IP 字面量")),
            }
        }
        (Self(table), rejected)
    }

    /// 精确查一个域名。
    pub fn lookup(&self, domain: &str) -> Option<IpAddr> {
        self.0.get(&domain.trim().to_ascii_lowercase()).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// 把 [`Hosts`] 叠在另一个解析器前面：命中就直接返回，未命中才往下查。
///
/// 命中时**不再查 DNS**。这正是 hosts 的意义——用户写这张表就是不想让这个
/// 域名走解析器。命中后还去查一次并合并结果的话，判决会拿到一批用户明确
/// 不想要的 IP，`IP-CIDR` 规则据此命中，等于 hosts 没配。
pub struct HostsResolver {
    hosts: Arc<Hosts>,
    inner: Arc<dyn RoutingResolver>,
}

impl HostsResolver {
    pub fn new(hosts: Arc<Hosts>, inner: Arc<dyn RoutingResolver>) -> Self {
        Self { hosts, inner }
    }
}

impl RoutingResolver for HostsResolver {
    fn resolve<'a>(
        &'a self,
        domain: &'a str,
    ) -> Pin<Box<dyn Future<Output = Vec<IpAddr>> + Send + 'a>> {
        if let Some(ip) = self.hosts.lookup(domain) {
            return Box::pin(async move { vec![ip] });
        }
        self.inner.resolve(domain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 未命中时必须落到下游解析器，且下游确实被调用了。
    /// 只断言返回值的话，一个恒返回空的实现也能"通过"。
    struct Counting {
        hits: std::sync::atomic::AtomicUsize,
        answer: Vec<IpAddr>,
    }

    impl RoutingResolver for Counting {
        fn resolve<'a>(
            &'a self,
            _domain: &'a str,
        ) -> Pin<Box<dyn Future<Output = Vec<IpAddr>> + Send + 'a>> {
            self.hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let a = self.answer.clone();
            Box::pin(async move { a })
        }
    }

    fn counting(answer: &str) -> Arc<Counting> {
        Arc::new(Counting {
            hits: std::sync::atomic::AtomicUsize::new(0),
            answer: vec![answer.parse().unwrap()],
        })
    }

    #[test]
    fn build_lowercases_keys_and_trims() {
        let (h, rejected) = Hosts::build([("  Example.COM ", " 1.2.3.4 ")]);
        assert!(rejected.is_empty(), "{rejected:?}");
        assert_eq!(h.lookup("example.com"), Some("1.2.3.4".parse().unwrap()));
        assert_eq!(h.lookup("EXAMPLE.com"), Some("1.2.3.4".parse().unwrap()));
    }

    #[test]
    fn build_reports_rather_than_silently_dropping_bad_entries() {
        let (h, rejected) = Hosts::build([("a.com", "not-an-ip"), ("", "1.2.3.4")]);
        assert!(h.is_empty());
        assert_eq!(rejected.len(), 2, "两条都要被点名：{rejected:?}");
        assert!(rejected.iter().any(|r| r.contains("a.com")));
    }

    #[test]
    fn ipv6_values_are_accepted() {
        let (h, rejected) = Hosts::build([("v6.example", "2001:db8::1")]);
        assert!(rejected.is_empty());
        assert_eq!(h.lookup("v6.example"), Some("2001:db8::1".parse().unwrap()));
    }

    /// 精确匹配：不做后缀/通配。写了 `example.com` 不该连带命中子域。
    #[test]
    fn matching_is_exact_not_suffix() {
        let (h, _) = Hosts::build([("example.com", "1.2.3.4")]);
        assert_eq!(h.lookup("www.example.com"), None);
        assert_eq!(h.lookup("notexample.com"), None);
    }

    #[tokio::test]
    async fn a_hit_short_circuits_and_never_touches_the_inner_resolver() {
        let inner = counting("9.9.9.9");
        let (hosts, _) = Hosts::build([("pinned.example", "1.2.3.4")]);
        let r = HostsResolver::new(Arc::new(hosts), inner.clone());
        assert_eq!(
            r.resolve("pinned.example").await,
            vec!["1.2.3.4".parse::<IpAddr>().unwrap()]
        );
        assert_eq!(
            inner.hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "命中 hosts 还去查 DNS，等于把用户明确排除的 IP 又拿了回来"
        );
    }

    #[tokio::test]
    async fn a_miss_delegates_downstream() {
        let inner = counting("9.9.9.9");
        let (hosts, _) = Hosts::build([("pinned.example", "1.2.3.4")]);
        let r = HostsResolver::new(Arc::new(hosts), inner.clone());
        assert_eq!(
            r.resolve("other.example").await,
            vec!["9.9.9.9".parse::<IpAddr>().unwrap()]
        );
        assert_eq!(inner.hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
