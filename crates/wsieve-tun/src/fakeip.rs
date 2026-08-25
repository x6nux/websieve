//! fake-ip 池：域名 ⇄ 保留段假 IP 的双向映射（设计文档 §7.1 第二层）。
//!
//! TUN 只看得到 IP 包。要让路由层拿回域名做判决，唯一办法是先在 DNS 层
//! 发一个假 IP，再在 TUN 层反查回来。范围固定 `198.18.0.0/15`
//! （RFC 2544 基准测试保留段，公网不会出现，撞不上真实目标）。
//!
//! **出站服务器域名必须自动进 filter**（§7.2 纪律①）：它们一旦拿到 fake-ip，
//! 转发器就会连向虚空。用户不该需要记住这件事。

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Mutex;

/// 198.18.0.0/15 的首尾（含）。前 4 个地址留作网络号与保留用途，不分配。
const RANGE_START: u32 = u32::from_be_bytes([198, 18, 0, 4]);
const RANGE_END: u32 = u32::from_be_bytes([198, 19, 255, 254]);

/// 池容量（可分配地址个数）。
const CAPACITY: u32 = RANGE_END - RANGE_START + 1;

/// 分配失败的原因。**绝不静默**：调用方必须区分「该域名按规矩不给假 IP」
/// 与「池满了」，两者的处置完全不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AllocError {
    /// 域名命中 filter，应走真实解析（§7.2 纪律①）。这是**正常**路径。
    #[error("域名在 fake-ip-filter 中，应走真实解析")]
    Filtered,
    /// 池内每一个地址都仍被占用且不可回收。
    #[error("fake-ip 池已耗尽（容量 {capacity}，全部在用）")]
    Exhausted { capacity: u32 },
}

/// fake-ip 分配与反查。
///
/// # 耗尽与复用
///
/// 池满时**从最老的映射开始回收**（插入序 FIFO），而不是按 `next` 指针
/// 盲目覆盖。区别很实在：盲目覆盖会踩到刚刚分配、正在被 TUN 反查的条目，
/// 把流量送到错误的主机上；FIFO 保证被回收的一定是当前存活映射里最老的
/// 那个。回收时**双向表同时摘除**，绝不留下指向已失效域名的反查项。
///
/// ponytail: 回收策略是 FIFO 而非 LRU —— 不记录「最后一次反查时间」，
/// 因此一个很老但仍在活跃使用的域名可能先于一个新分配却已闲置的域名被回收。
/// **上限**：容量 13 万个域名同时在用；正常浏览远达不到，达到时 FIFO 与 LRU
/// 的差别也只影响少数边缘条目。**升级路径**：`lookup()` 里给条目打时间戳，
/// 回收时跳过 60s 内被反查过的条目（`recycle_oldest` 是唯一需要改的地方）。
pub struct FakeIpPool {
    inner: Mutex<Inner>,
    filter: Vec<String>,
}

struct Inner {
    /// 下一个**从未分配过**的地址。走到 `RANGE_END` 之后不再前进，
    /// 后续分配一律走回收路径。
    next: u32,
    by_domain: HashMap<String, Ipv4Addr>,
    by_ip: HashMap<Ipv4Addr, String>,
    /// 按分配先后排列的域名队列，队首是最老的映射。回收时从队首取。
    order: std::collections::VecDeque<String>,
}

impl FakeIpPool {
    /// `filter` 是不参与 fake-ip 的域名后缀集合。出站服务器域名由调用方
    /// （TUN 编排层）**自动**加入，见 [`with_server_domains`](Self::with_server_domains)。
    pub fn new(filter: Vec<String>) -> Self {
        let mut normalized: Vec<String> = Vec::with_capacity(filter.len());
        for s in filter {
            let n = normalize(&s);
            // 配置里重复写同一条后缀不该让 filter 膨胀，也让 len() 可断言。
            if !normalized.contains(&n) {
                normalized.push(n);
            }
        }
        Self {
            inner: Mutex::new(Inner {
                next: RANGE_START,
                by_domain: HashMap::new(),
                by_ip: HashMap::new(),
                order: std::collections::VecDeque::new(),
            }),
            filter: normalized,
        }
    }

    /// 把出站服务器域名并入 filter。**这是纪律①的落点**：配置里
    /// `fake-ip-filter` 写没写都不影响，服务器域名一律进。
    pub fn with_server_domains(mut self, domains: &[String]) -> Self {
        for d in domains {
            let n = normalize(d);
            if !self.filter.contains(&n) {
                self.filter.push(n);
            }
        }
        self
    }

    /// 当前 filter 条目数（已归一化去重）。
    pub fn filter_len(&self) -> usize {
        self.filter.len()
    }

    /// 该域名是否被排除在 fake-ip 之外（应走真实解析）。
    pub fn is_filtered(&self, domain: &str) -> bool {
        let d = normalize(domain);
        self.filter.iter().any(|f| {
            // 通配后缀 `*.example.com` 与裸后缀 `example.com` 都按后缀匹配，
            // 且必须落在点边界上 —— `notexample.com` 不该被 `example.com` 命中。
            let f = f.strip_prefix("*.").unwrap_or(f);
            d == *f || (d.len() > f.len() && d.ends_with(f) && d.as_bytes()[d.len() - f.len() - 1] == b'.')
        })
    }

    /// 取（或分配）该域名的 fake-ip。
    ///
    /// filter 命中时返回 [`AllocError::Filtered`]，调用方转真实解析；
    /// 池耗尽时返回 [`AllocError::Exhausted`]，**不静默回绕**。
    pub fn allocate(&self, domain: &str) -> Result<Ipv4Addr, AllocError> {
        if self.is_filtered(domain) {
            return Err(AllocError::Filtered);
        }
        let key = normalize(domain);
        let mut g = self.inner.lock().expect("fakeip 锁中毒");
        if let Some(ip) = g.by_domain.get(&key) {
            return Ok(*ip);
        }

        let ip = if g.next <= RANGE_END {
            // 还有从未分配过的地址，直接取。
            let ip = Ipv4Addr::from(g.next);
            g.next += 1;
            ip
        } else {
            // 池已铺满，回收最老的映射。
            g.recycle_oldest()?
        };

        g.by_ip.insert(ip, key.clone());
        g.by_domain.insert(key.clone(), ip);
        g.order.push_back(key);
        Ok(ip)
    }

    /// 反查：TUN 拿到目的 IP 时用它换回域名做路由判决。
    pub fn lookup(&self, ip: Ipv4Addr) -> Option<String> {
        self.inner
            .lock()
            .expect("fakeip 锁中毒")
            .by_ip
            .get(&ip)
            .cloned()
    }

    /// 当前存活的映射条数。
    pub fn len(&self) -> usize {
        self.inner.lock().expect("fakeip 锁中毒").by_domain.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 该 IP 是否落在 fake-ip 段内（不论是否已分配）。
    ///
    /// ⚠️ **段内 ≠ 出自本池**。本机 DNS 若被劫持，同样可能回 `198.18.x.x`
    /// （2026-08-25 实测：查询 `192.0.2.1` 得到 `198.18.0.211`）。要判断
    /// 一个地址确实是我们发出去的，必须用 [`lookup`](Self::lookup) 查本池
    /// 记录，而不是只看 CIDR 归属。
    pub fn in_range(ip: Ipv4Addr) -> bool {
        let v = u32::from(ip);
        (RANGE_START..=RANGE_END).contains(&v)
    }

    /// 池容量（可分配地址总数）。
    pub fn capacity() -> u32 {
        CAPACITY
    }

    /// 把池推进「铺满且无可回收条目」的状态，供**本 crate 的测试**驱动
    /// 真实的耗尽路径。
    ///
    /// 不是替身：被测的仍是 `allocate` 的真实实现，只是省掉逐个分配 13 万
    /// 个地址那一步（真跑一遍要几秒，且与被测行为无关）。与本模块测试里的
    /// `pool_with_headroom` 同一个手法，提到这里是因为 `fakedns` 也要用。
    #[cfg(test)]
    pub(crate) fn exhaust_for_test(&self) {
        let mut g = self.inner.lock().expect("fakeip 锁中毒");
        g.next = RANGE_END + 1;
    }
}

impl Inner {
    /// 回收最老的映射并交出它的地址。双向表与队列同时摘除。
    fn recycle_oldest(&mut self) -> Result<Ipv4Addr, AllocError> {
        while let Some(oldest) = self.order.pop_front() {
            // 队列里可能残留已被 by_domain 摘掉的陈旧键，跳过即可。
            if let Some(ip) = self.by_domain.remove(&oldest) {
                self.by_ip.remove(&ip);
                return Ok(ip);
            }
        }
        Err(AllocError::Exhausted {
            capacity: CAPACITY,
        })
    }
}

/// 统一小写并去掉根点，让 `Example.COM.` 与 `example.com` 是同一个键。
fn normalize(d: &str) -> String {
    d.trim_end_matches('.').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_is_stable_per_domain() {
        let p = FakeIpPool::new(vec![]);
        let a = p.allocate("example.com").unwrap();
        let b = p.allocate("example.com").unwrap();
        assert_eq!(a, b, "同域名必须拿同一个 fake-ip");
        assert_ne!(p.allocate("other.com").unwrap(), a);
    }

    #[test]
    fn reverse_lookup_returns_domain() {
        // 这是 TUN 能做域名路由的全部依据。
        let p = FakeIpPool::new(vec![]);
        let ip = p.allocate("example.com").unwrap();
        assert_eq!(p.lookup(ip).as_deref(), Some("example.com"));
    }

    #[test]
    fn allocations_stay_inside_198_18_0_0_15() {
        let p = FakeIpPool::new(vec![]);
        for i in 0..500 {
            let ip = p.allocate(&format!("d{i}.test")).unwrap();
            assert!(FakeIpPool::in_range(ip), "{ip} 越出 198.18.0.0/15");
        }
        assert!(!FakeIpPool::in_range("1.2.3.4".parse().unwrap()));
        assert!(!FakeIpPool::in_range("198.20.0.1".parse().unwrap()));
        assert!(FakeIpPool::in_range("198.19.255.1".parse().unwrap()));
    }

    #[test]
    fn server_domains_are_filtered_automatically() {
        // 纪律①：用户没在配置里写 fake-ip-filter，服务器域名照样不能拿假 IP。
        let p = FakeIpPool::new(vec![]).with_server_domains(&["srv.example.com".into()]);
        assert!(p.is_filtered("srv.example.com"));
        assert_eq!(p.allocate("srv.example.com"), Err(AllocError::Filtered));
        // 其余域名不受影响
        assert!(p.allocate("www.example.com").is_ok());
    }

    #[test]
    fn filter_matches_on_dot_boundary_only() {
        let p = FakeIpPool::new(vec!["example.com".into()]);
        assert!(p.is_filtered("example.com"));
        assert!(p.is_filtered("a.example.com"));
        assert!(!p.is_filtered("notexample.com"), "不能把邻居域名一起挡掉");
    }

    #[test]
    fn wildcard_filter_form_is_accepted() {
        let p = FakeIpPool::new(vec!["*.example.com".into()]);
        assert!(p.is_filtered("a.example.com"));
        assert!(p.is_filtered("example.com"));
    }

    #[test]
    fn case_and_trailing_dot_are_normalized() {
        let p = FakeIpPool::new(vec![]).with_server_domains(&["SRV.Example.com.".into()]);
        assert!(p.is_filtered("srv.example.com"));
        let ip = p.allocate("Foo.COM.").unwrap();
        assert_eq!(p.allocate("foo.com").unwrap(), ip);
    }

    #[test]
    fn duplicate_server_domains_do_not_accumulate() {
        let p = FakeIpPool::new(vec!["example.com".into()])
            .with_server_domains(&["example.com".into(), "example.com".into()]);
        assert_eq!(p.filter_len(), 1);
    }

    #[test]
    fn duplicate_filter_entries_in_constructor_do_not_accumulate() {
        // 配置里重复写同一条后缀（含大小写/根点差异）不该让 filter 膨胀。
        let p = FakeIpPool::new(vec![
            "example.com".into(),
            "Example.COM.".into(),
            "example.com".into(),
        ]);
        assert_eq!(p.filter_len(), 1);
    }

    #[test]
    fn allocation_never_hands_out_the_same_ip_twice_while_live() {
        // 两个不同域名绝不能拿到同一个地址 —— 否则反查必然指错主机。
        let p = FakeIpPool::new(vec![]);
        let mut seen = std::collections::HashSet::new();
        for i in 0..2000 {
            let ip = p.allocate(&format!("d{i}.test")).unwrap();
            assert!(seen.insert(ip), "{ip} 被重复分配给了两个存活域名");
        }
    }

    #[test]
    fn first_allocation_skips_reserved_head_of_range() {
        // 前 4 个地址（198.18.0.0-.3）留作网络号，不分配。
        let p = FakeIpPool::new(vec![]);
        let ip = p.allocate("first.test").unwrap();
        assert_eq!(ip, Ipv4Addr::new(198, 18, 0, 4));
    }

    #[test]
    fn range_bounds_match_198_18_0_0_15() {
        // 198.18.0.0/15 覆盖 198.18.0.0 - 198.19.255.255。
        assert!(FakeIpPool::in_range("198.18.0.4".parse().unwrap()));
        assert!(FakeIpPool::in_range("198.19.255.254".parse().unwrap()));
        // 边界外：广播地址与网段末尾之外都不分配
        assert!(!FakeIpPool::in_range("198.19.255.255".parse().unwrap()));
        assert!(!FakeIpPool::in_range("198.17.255.255".parse().unwrap()));
        assert_eq!(FakeIpPool::capacity(), RANGE_END - RANGE_START + 1);
    }

    #[test]
    fn in_range_membership_does_not_prove_pool_origin() {
        // 本机 DNS 被劫持时同样会回 198.18.x.x（2026-08-25 实测）。
        // 段内 ≠ 出自本池 —— 要证明来源必须查本池记录。
        let p = FakeIpPool::new(vec![]);
        let outsider: Ipv4Addr = "198.18.0.211".parse().unwrap();
        assert!(
            FakeIpPool::in_range(outsider),
            "该地址确实落在我们的段里"
        );
        assert_eq!(
            p.lookup(outsider),
            None,
            "但它不出自本池 —— lookup 才是权威判据"
        );
    }

    // ---- 耗尽与回收 ----
    //
    // 直接跑满 13 万个地址在单测里太慢，因此用一个把 next 推到末尾的
    // 私有构造来触达同一条代码路径。**被测的是真实实现**，只是把「铺满」
    // 这一步省掉了。

    /// 把池推到「只剩 n 个未分配地址」的状态，用真实的 allocate 铺满其余部分。
    fn pool_with_headroom(n: u32) -> FakeIpPool {
        let p = FakeIpPool::new(vec![]);
        {
            let mut g = p.inner.lock().unwrap();
            g.next = RANGE_END - n + 1;
        }
        p
    }

    #[test]
    fn exhausted_pool_recycles_oldest_mapping() {
        let p = pool_with_headroom(2);
        let a = p.allocate("a.test").unwrap();
        let b = p.allocate("b.test").unwrap();
        // 池已铺满，下一个分配必须回收最老的 a.test
        let c = p.allocate("c.test").unwrap();
        assert_eq!(c, a, "回收的应是最老的映射持有的地址");
        assert_eq!(
            p.lookup(a).as_deref(),
            Some("c.test"),
            "反查必须指向新主人"
        );
        assert_eq!(p.lookup(b).as_deref(), Some("b.test"), "b 不受影响");
    }

    #[test]
    fn recycling_removes_the_stale_domain_from_both_directions() {
        // 一个被回收的域名如果还留在 by_domain 里，它下次 allocate 会拿回
        // 一个已经属于别人的地址 —— 流量直接送错主机。
        let p = pool_with_headroom(2);
        let old = p.allocate("old.test").unwrap();
        let keep = p.allocate("keep.test").unwrap();
        // 池已铺满，new.test 回收队首的 old.test
        let new = p.allocate("new.test").unwrap();
        assert_eq!(new, old, "回收的是最老的 old.test 的地址");
        assert_eq!(p.lookup(old).as_deref(), Some("new.test"));
        assert_eq!(p.len(), 2, "old.test 必须已从正查表消失，容量不变");
        // old.test 再来一次，绝不能拿回此刻仍属于 new.test 的那个地址
        let again = p.allocate("old.test").unwrap();
        assert_ne!(
            again, new,
            "不能把仍在用的 new.test 的地址重新发给 old.test"
        );
        assert_eq!(again, keep, "这一轮轮到队首的 keep.test 被回收");
        assert_eq!(p.lookup(again).as_deref(), Some("old.test"));
        assert_eq!(p.lookup(new).as_deref(), Some("new.test"), "new 仍然完好");
    }

    #[test]
    fn recycling_is_fifo_not_pointer_overwrite() {
        // 按插入序回收：连续回收若干次，顺序必须是 a → b → c。
        let p = pool_with_headroom(3);
        let a = p.allocate("a.test").unwrap();
        let b = p.allocate("b.test").unwrap();
        let c = p.allocate("c.test").unwrap();
        assert_eq!(p.allocate("d.test").unwrap(), a);
        assert_eq!(p.allocate("e.test").unwrap(), b);
        assert_eq!(p.allocate("f.test").unwrap(), c);
        assert_eq!(p.len(), 3, "容量恒定，进一个出一个");
    }

    #[test]
    fn repeat_lookup_of_live_mapping_survives_recycling_pressure() {
        // 回收压力下，最近分配的映射必须始终可反查 —— 池不能把活的映射
        // 顺手回收掉。
        let p = pool_with_headroom(4);
        let keep = p.allocate("keep.test").unwrap();
        for i in 0..3 {
            p.allocate(&format!("churn{i}.test")).unwrap();
        }
        assert_eq!(p.lookup(keep).as_deref(), Some("keep.test"));
        // 第 4 次分配才轮到 keep 被回收（FIFO 队首）
        p.allocate("churn3.test").unwrap();
        assert_eq!(p.lookup(keep).as_deref(), Some("churn3.test"));
    }

    #[test]
    fn filtered_domain_is_a_distinct_error_from_exhaustion() {
        // 两种失败必须能被调用方区分：Filtered 转真实解析，Exhausted 是故障。
        let p = FakeIpPool::new(vec!["blocked.test".into()]);
        assert_eq!(p.allocate("blocked.test"), Err(AllocError::Filtered));
        assert_ne!(
            AllocError::Filtered,
            AllocError::Exhausted { capacity: CAPACITY }
        );
    }

    #[test]
    fn truly_exhausted_pool_reports_error_instead_of_misrouting() {
        // 池铺满**且回收队列为空**时必须报错，绝不能随手发一个仍在用的地址。
        // 构造：把 next 推过末尾，同时不留任何可回收条目。
        let p = FakeIpPool::new(vec![]);
        {
            let mut g = p.inner.lock().unwrap();
            g.next = RANGE_END + 1;
        }
        assert_eq!(
            p.allocate("nowhere.test"),
            Err(AllocError::Exhausted { capacity: CAPACITY }),
            "无可回收条目时必须显式报错"
        );
    }

    #[test]
    fn exhaustion_error_carries_capacity_for_diagnosis() {
        // 错误里带上容量，日志能直接告诉用户「13 万个域名同时在用」有多离谱。
        let p = FakeIpPool::new(vec![]);
        {
            let mut g = p.inner.lock().unwrap();
            g.next = RANGE_END + 1;
        }
        match p.allocate("x.test") {
            Err(AllocError::Exhausted { capacity }) => {
                assert_eq!(capacity, CAPACITY);
                assert_eq!(capacity, 131_067, "198.18.0.4 - 198.19.255.254 闭区间");
            }
            other => panic!("应报 Exhausted，实际 {other:?}"),
        }
    }

    #[test]
    fn concurrent_allocation_of_same_domain_yields_one_ip() {
        // 池在 DNS 服务器里被多线程共享，同域名并发查询必须收敛到同一个地址。
        use std::sync::Arc;
        let p = Arc::new(FakeIpPool::new(vec![]));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let p = Arc::clone(&p);
            handles.push(std::thread::spawn(move || {
                (0..50)
                    .map(|_| p.allocate("shared.test").unwrap())
                    .collect::<Vec<_>>()
            }));
        }
        let all: Vec<Ipv4Addr> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        let first = all[0];
        assert!(all.iter().all(|ip| *ip == first), "同域名并发必须同一个 IP");
        assert_eq!(p.len(), 1);
    }
}
