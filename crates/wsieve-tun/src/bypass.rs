//! TUN bypass 集合：转发器出网连接的「绕行名单」。
//!
//! 环路陷阱（设计文档 §8.3.1）：TUN 捕获全部流量，**包括转发器连向真实
//! 服务器 IP 的那条**。若不绕行，该连接会被路由层判「走代理」，转身又回到
//! WebView → 127.0.0.1:18443，形成死循环。
//!
//! websieve 的特殊之处：服务器 IP 是 `shard.rs:249 resolve_upstream` 在
//! **运行时**解析得到的，因此名单必须动态维护，随出站增删而增删。
//!
//! **netstack 不会替我们避开环路**（计划文档已实证：把发往服务器 IP 的 SYN
//! 喂进协议栈，`TcpListener` 原样当成一条连接吐出来）。bypass 是唯一防线。

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};

/// 一次名单变更对**路由表**的净影响。
///
/// 为什么不是「谁变了就删谁的路由」：两个出站可能落在同一台机器上（同 IP
/// 不同域名）。此时摘掉其中一个出站，那个 IP 的路由**仍被另一个需要**——
/// 照着变更盲目删路由，剩下那个出站当场进环路。因此这里报的是**引用计数
/// 归零后**才该动的那部分。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteDelta {
    /// 此前无人持有、现在需要新增 bypass 路由的 IP。
    pub added: Vec<IpAddr>,
    /// 已无任何出站持有、可以安全删除路由的 IP。
    pub removed: Vec<IpAddr>,
}

impl RouteDelta {
    /// 路由表无需改动。
    pub fn is_noop(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

/// 动态 bypass 名单。按出站 id 分组持有，便于出站下线时精确摘除。
///
/// 用 `BTreeMap` 而非 `HashMap`：条目数是个位数，有序遍历让 `snapshot()`
/// 结果稳定，路由表 diff 与测试断言都因此可复现。
///
/// 引用计数不单独维护，而是在需要时扫一遍其余出站。条目数量级决定了这样
/// 更划算——一张独立的计数表只会多一个可能与主表失配的状态。
#[derive(Clone, Default)]
pub struct BypassSet {
    inner: Arc<RwLock<BTreeMap<String, Vec<IpAddr>>>>,
}

impl BypassSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记某出站解析出的服务器 IP。同 id 重复调用即覆盖（幂等）。
    ///
    /// 返回的 [`RouteDelta`] 是**路由表该怎么改**：`added` 要加路由，
    /// `removed` 是重解析后失效且已无人持有、可以删掉的旧 IP。
    pub fn insert(&self, outbound_id: &str, ips: Vec<IpAddr>) -> RouteDelta {
        let mut fresh = ips;
        fresh.sort();
        fresh.dedup();

        let mut g = self.inner.write().expect("bypass 锁中毒");
        let previous = g.insert(outbound_id.to_string(), fresh.clone()).unwrap_or_default();

        // 其余出站持有的 IP —— 它们的路由无论如何都得留着。
        let others = Self::held_by_others(&g, outbound_id);

        let added = fresh
            .iter()
            .filter(|ip| !previous.contains(ip) && !others.contains(ip))
            .copied()
            .collect();
        let removed = previous
            .iter()
            .filter(|ip| !fresh.contains(ip) && !others.contains(ip))
            .copied()
            .collect();
        RouteDelta { added, removed }
    }

    /// 出站下线时摘除。
    ///
    /// 返回**可以安全删除路由**的 IP —— 即摘除之后再没有任何出站持有的那些。
    /// 共享 IP 不在其中：把它的路由删掉会让仍在线的另一个出站当场进环路。
    pub fn remove(&self, outbound_id: &str) -> Vec<IpAddr> {
        let mut g = self.inner.write().expect("bypass 锁中毒");
        let Some(gone) = g.remove(outbound_id) else {
            return Vec::new();
        };
        let others = Self::held_by_others(&g, outbound_id);
        let mut safe: Vec<IpAddr> = gone
            .into_iter()
            .filter(|ip| !others.contains(ip))
            .collect();
        safe.sort();
        safe.dedup();
        safe
    }

    /// 该 IP 是否应绕过 TUN 直连物理网卡。
    ///
    /// **这是环路的唯一防线**，路由层在做任何规则判断之前先问这一句。
    pub fn contains(&self, ip: &IpAddr) -> bool {
        self.inner
            .read()
            .expect("bypass 锁中毒")
            .values()
            .any(|v| v.contains(ip))
    }

    /// 当前全部 bypass IP（去重、有序）。写路由表用。
    pub fn snapshot(&self) -> Vec<IpAddr> {
        let g = self.inner.read().expect("bypass 锁中毒");
        let mut all: Vec<IpAddr> = g.values().flatten().copied().collect();
        all.sort();
        all.dedup();
        all
    }

    /// 持有该 IP 的出站 id（有序）。诊断环路时用来回答「这条路由是谁要的」。
    pub fn holders(&self, ip: &IpAddr) -> Vec<String> {
        self.inner
            .read()
            .expect("bypass 锁中毒")
            .iter()
            .filter(|(_, ips)| ips.contains(ip))
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().expect("bypass 锁中毒").is_empty()
    }

    /// 除 `except` 之外的出站所持有的全部 IP。
    fn held_by_others(map: &BTreeMap<String, Vec<IpAddr>>, except: &str) -> Vec<IpAddr> {
        map.iter()
            .filter(|(id, _)| id.as_str() != except)
            .flat_map(|(_, ips)| ips.iter().copied())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn insert_then_contains() {
        let b = BypassSet::new();
        assert!(!b.contains(&ip("1.2.3.4")));
        b.insert("jp", vec![ip("1.2.3.4")]);
        assert!(b.contains(&ip("1.2.3.4")));
    }

    #[test]
    fn insert_is_idempotent_per_outbound() {
        let b = BypassSet::new();
        b.insert("jp", vec![ip("1.2.3.4")]);
        b.insert("jp", vec![ip("1.2.3.4")]);
        assert_eq!(b.snapshot(), vec![ip("1.2.3.4")]);
    }

    #[test]
    fn reresolve_replaces_stale_ip() {
        // 服务器换 IP 后重解析：旧 IP 必须消失，否则 bypass 名单越积越长，
        // 陈旧条目会把不该绕行的地址放出去。
        let b = BypassSet::new();
        b.insert("jp", vec![ip("1.2.3.4")]);
        b.insert("jp", vec![ip("5.6.7.8")]);
        assert!(!b.contains(&ip("1.2.3.4")));
        assert!(b.contains(&ip("5.6.7.8")));
    }

    #[test]
    fn remove_returns_ips_for_route_deletion() {
        let b = BypassSet::new();
        b.insert("jp", vec![ip("1.2.3.4"), ip("1.2.3.5")]);
        let gone = b.remove("jp");
        assert_eq!(gone, vec![ip("1.2.3.4"), ip("1.2.3.5")]);
        assert!(b.is_empty());
    }

    #[test]
    fn two_outbounds_sharing_an_ip_survive_one_removal() {
        // 两个出站在同一台机器上（同 IP 不同域名）。摘掉一个之后，
        // 另一个的 bypass 必须还在 —— 否则它当场进环路。
        let b = BypassSet::new();
        b.insert("jp", vec![ip("1.2.3.4")]);
        b.insert("us", vec![ip("1.2.3.4")]);
        b.remove("jp");
        assert!(b.contains(&ip("1.2.3.4")), "共享 IP 不能被另一出站的下线带走");
    }

    #[test]
    fn snapshot_dedups_and_sorts() {
        let b = BypassSet::new();
        b.insert("a", vec![ip("9.9.9.9"), ip("1.1.1.1")]);
        b.insert("b", vec![ip("1.1.1.1")]);
        assert_eq!(b.snapshot(), vec![ip("1.1.1.1"), ip("9.9.9.9")]);
    }

    // ---- 路由表该怎么改：共享 IP 是这一组的全部理由 ----

    #[test]
    fn removing_one_holder_of_a_shared_ip_reports_no_route_deletion() {
        // 名单里还在（上一条测过了），但**路由表**同样不能动 ——
        // 调用方照着 remove() 的返回值删路由，删掉就等于 us 进环路。
        let b = BypassSet::new();
        b.insert("jp", vec![ip("1.2.3.4")]);
        b.insert("us", vec![ip("1.2.3.4")]);
        assert_eq!(
            b.remove("jp"),
            Vec::<IpAddr>::new(),
            "共享 IP 的路由仍被 us 需要，绝不能报给调用方去删"
        );
        assert_eq!(b.holders(&ip("1.2.3.4")), vec!["us".to_string()]);
    }

    #[test]
    fn last_holder_removal_does_report_route_deletion() {
        // 引用计数归零才删 —— 归零了就必须删，否则路由表泄漏。
        let b = BypassSet::new();
        b.insert("jp", vec![ip("1.2.3.4")]);
        b.insert("us", vec![ip("1.2.3.4")]);
        assert!(b.remove("jp").is_empty());
        assert_eq!(b.remove("us"), vec![ip("1.2.3.4")], "最后一个持有者下线才删路由");
        assert!(b.is_empty());
    }

    #[test]
    fn adding_a_second_holder_of_an_existing_ip_adds_no_duplicate_route() {
        // 路由已经在了，再写一遍要么报错要么产生重复条目。
        let b = BypassSet::new();
        let first = b.insert("jp", vec![ip("1.2.3.4")]);
        assert_eq!(first.added, vec![ip("1.2.3.4")]);
        let second = b.insert("us", vec![ip("1.2.3.4")]);
        assert!(second.is_noop(), "同一个 IP 的路由不该被写第二次");
    }

    #[test]
    fn reresolve_delta_adds_new_and_drops_stale() {
        let b = BypassSet::new();
        b.insert("jp", vec![ip("1.2.3.4")]);
        let d = b.insert("jp", vec![ip("5.6.7.8")]);
        assert_eq!(d.added, vec![ip("5.6.7.8")]);
        assert_eq!(d.removed, vec![ip("1.2.3.4")], "陈旧路由必须报出来删掉");
    }

    #[test]
    fn reresolve_keeps_shared_stale_ip_route_alive() {
        // jp 换了 IP，但旧 IP 恰好也是 us 的服务器。旧路由不能删。
        let b = BypassSet::new();
        b.insert("jp", vec![ip("1.2.3.4")]);
        b.insert("us", vec![ip("1.2.3.4")]);
        let d = b.insert("jp", vec![ip("5.6.7.8")]);
        assert_eq!(d.added, vec![ip("5.6.7.8")]);
        assert!(d.removed.is_empty(), "us 还在用 1.2.3.4，路由不能删");
        assert!(b.contains(&ip("1.2.3.4")));
    }

    #[test]
    fn idempotent_insert_is_a_route_noop() {
        let b = BypassSet::new();
        b.insert("jp", vec![ip("1.2.3.4")]);
        assert!(
            b.insert("jp", vec![ip("1.2.3.4")]).is_noop(),
            "重复登记不该反复改路由表"
        );
    }

    #[test]
    fn insert_dedups_input_from_multi_record_dns() {
        // 一次 A 记录查询可能回重复地址；重复写路由会报「already in table」。
        let b = BypassSet::new();
        let d = b.insert("jp", vec![ip("1.2.3.4"), ip("1.2.3.4"), ip("1.2.3.5")]);
        assert_eq!(d.added, vec![ip("1.2.3.4"), ip("1.2.3.5")]);
        assert_eq!(b.snapshot(), vec![ip("1.2.3.4"), ip("1.2.3.5")]);
    }

    #[test]
    fn removing_unknown_outbound_is_not_an_error_and_deletes_nothing() {
        let b = BypassSet::new();
        b.insert("jp", vec![ip("1.2.3.4")]);
        assert!(b.remove("never-existed").is_empty());
        assert!(b.contains(&ip("1.2.3.4")), "别人的条目不能被误摘");
    }

    #[test]
    fn ipv6_server_addresses_are_carried_too() {
        // 服务器可能只有 AAAA 记录，环路对 IPv6 一样成立。
        let b = BypassSet::new();
        let v6 = ip("2606:4700:4700::1111");
        b.insert("jp", vec![v6]);
        assert!(b.contains(&v6));
        assert_eq!(b.remove("jp"), vec![v6]);
    }

    #[test]
    fn multi_record_outbound_survives_partial_overlap_removal() {
        // jp 有两个 A 记录，其中一个与 us 共享。摘掉 jp 只能删不共享的那个。
        let b = BypassSet::new();
        b.insert("jp", vec![ip("1.2.3.4"), ip("1.2.3.5")]);
        b.insert("us", vec![ip("1.2.3.5")]);
        assert_eq!(
            b.remove("jp"),
            vec![ip("1.2.3.4")],
            "只有独占的那个才能删路由"
        );
        assert!(b.contains(&ip("1.2.3.5")));
        assert!(!b.contains(&ip("1.2.3.4")));
    }

    #[test]
    fn contains_is_the_defense_and_answers_for_every_registered_ip() {
        // 环路防线的完整性：凡是登记过的 IP，contains 必须一律为真。
        // 漏掉任何一个都等于那条出站的转发器连接被 TUN 抓走。
        let b = BypassSet::new();
        let ips: Vec<IpAddr> = (1..=20).map(|i| ip(&format!("10.0.0.{i}"))).collect();
        for (i, addr) in ips.iter().enumerate() {
            b.insert(&format!("ob{i}"), vec![*addr]);
        }
        for addr in &ips {
            assert!(b.contains(addr), "{addr} 未被防线覆盖");
        }
        assert!(!b.contains(&ip("10.0.1.1")), "未登记的地址不该被放行绕过");
        assert_eq!(b.snapshot().len(), 20);
    }

    #[test]
    fn clone_shares_one_underlying_set() {
        // 编排层会把它 clone 进各个任务；共享同一份状态是前提，
        // 否则某个任务登记的 bypass 在路由层那份里根本不存在。
        let a = BypassSet::new();
        let b = a.clone();
        a.insert("jp", vec![ip("1.2.3.4")]);
        assert!(b.contains(&ip("1.2.3.4")), "clone 必须共享同一份状态");
        b.remove("jp");
        assert!(!a.contains(&ip("1.2.3.4")));
    }

    #[test]
    fn concurrent_inserts_and_reads_keep_the_defense_consistent() {
        // 出站上线是并发的，防线不能有「登记了但查不到」的窗口。
        use std::sync::Arc;
        let b = Arc::new(BypassSet::new());
        let mut handles = Vec::new();
        for t in 0..8 {
            let b = Arc::clone(&b);
            handles.push(std::thread::spawn(move || {
                let addr = ip(&format!("10.1.{t}.1"));
                b.insert(&format!("ob{t}"), vec![addr]);
                // 自己刚登记的，立刻查必须命中
                assert!(b.contains(&addr), "登记后立即查询必须命中");
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(b.snapshot().len(), 8);
    }
}
