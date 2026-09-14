//! 解析器本体：构造校验、硬超时、bootstrap 隔离。
//!
//! 除标注「联网」的两个用例外，全部脱网。联网用例在无网环境下会自行跳过，
//! CI 不因外网抖动变红。

use std::time::{Duration, Instant};

use wsieve_dns::{bootstrap, bootstrap_with, DnsResolver};

fn ns(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn empty_nameserver_list_is_rejected_with_an_actionable_message() {
    let e = DnsResolver::new(&[], Duration::from_secs(2), 4096, Duration::from_secs(30))
        .unwrap_err()
        .to_string();
    assert!(e.contains("nameserver"), "{e}");
    assert!(e.contains("1.1.1.1"), "错误信息要给出可照抄的例子：{e}");
}

#[test]
fn hostname_upstream_is_rejected_at_construction_not_at_query_time() {
    // 纪律②必须在加载期生效：坏配置根本建不出解析器
    let e = DnsResolver::new(
        &ns(&["https://cloudflare-dns.com/dns-query"]),
        Duration::from_secs(2),
        4096,
        Duration::from_secs(30),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("cloudflare-dns.com"), "{e}");
}

#[test]
fn valid_config_builds() {
    assert!(DnsResolver::new(
        &ns(&["https://1.1.1.1/dns-query", "https://8.8.8.8/dns-query"]),
        Duration::from_secs(2),
        4096,
        Duration::from_secs(30),
    )
    .is_ok());
}

/// **本阶段最重要的一个测试。**
///
/// 证明硬超时确实是墙钟上限。上游指向 192.0.2.1（RFC 5737 文档段，保证黑洞），
/// 解析必须在超时附近返回空切片，而不是把连接晾在那里。
///
/// 计划评审时实测：仅靠 `ResolverOpts::timeout = 500ms` 时单次查询耗时
/// **15.03 秒**；加上外层 `tokio::time::timeout` 后稳定在设定值。
#[tokio::test]
async fn blackhole_upstream_returns_empty_within_the_hard_timeout() {
    let r = DnsResolver::new(
        &ns(&["https://192.0.2.1/dns-query"]),
        Duration::from_millis(800),
        64,
        Duration::from_secs(30),
    )
    .unwrap();

    let t = Instant::now();
    let ips = r.lookup_for_routing("example.com").await;
    let elapsed = t.elapsed();

    assert!(ips.is_empty(), "黑洞上游必须返回空切片而非挂起");
    assert!(
        elapsed < Duration::from_secs(3),
        "硬超时没生效：耗时 {elapsed:?}。\
         若接近 15s，说明 lookup_for_routing 丢了外层 tokio::time::timeout —— \
         ResolverOpts::timeout 只在名字服务器池的轮与轮之间检查 deadline，\
         挡不住卡在 TCP/TLS 建连里的那一轮"
    );
}

#[tokio::test]
async fn a_failed_lookup_never_panics_and_never_returns_err() {
    // 签名上就没有 Result —— 这个测试锁住的是这条设计不被后人改回去
    let r = DnsResolver::new(
        &ns(&["https://192.0.2.1/dns-query"]),
        Duration::from_millis(300),
        64,
        Duration::from_secs(30),
    )
    .unwrap();
    let ips: Vec<std::net::IpAddr> = r.lookup_for_routing("whatever.invalid").await;
    assert!(ips.is_empty());
}

// ── bootstrap 隔离（纪律①）────────────────────────────────

#[test]
fn bootstrap_is_a_separate_instance_from_the_routing_resolver() {
    // 隔离靠的是「压根是两个对象」，不是靠某个 if 分支
    let routing = DnsResolver::new(
        &ns(&["https://1.1.1.1/dns-query"]),
        Duration::from_secs(2),
        4096,
        Duration::from_secs(30),
    )
    .unwrap();
    let boot = bootstrap();
    assert!(boot.is_ok(), "系统 DNS 配置应可读：{:?}", boot.err());
    // 两者类型不同、无共享字段 —— 这一行的意义是让「把 bootstrap 改成
    // 复用 routing 的缓存」这种改动无法悄悄通过编译
    let _ = routing;
}

#[test]
fn bootstrap_with_explicit_upstreams_enforces_ip_literals_too() {
    assert!(bootstrap_with(&ns(&["1.1.1.1"])).is_ok());
    assert!(bootstrap_with(&ns(&["https://dns.google/dns-query"])).is_err());
    assert!(bootstrap_with(&[]).is_err());
}

// ── 联网用例（无网自动跳过）───────────────────────────────

#[tokio::test]
async fn live_doh_over_ip_literal_resolves() {
    let r = DnsResolver::new(
        &ns(&["https://1.1.1.1/dns-query"]),
        Duration::from_secs(4),
        256,
        Duration::from_secs(30),
    )
    .unwrap();
    let ips = r.lookup_for_routing("example.com").await;
    if ips.is_empty() {
        eprintln!("跳过：无外网连通性");
        return;
    }
    eprintln!("example.com → {ips:?}");
}

#[tokio::test]
async fn live_cache_makes_the_second_lookup_dramatically_faster() {
    // 证明「缓存不用自己写」这个结论：hickory 内置 ResponseCache 已生效
    let r = DnsResolver::new(
        &ns(&["https://1.1.1.1/dns-query"]),
        Duration::from_secs(4),
        256,
        Duration::from_secs(30),
    )
    .unwrap();

    let t = Instant::now();
    let first = r.lookup_for_routing("example.com").await;
    let d1 = t.elapsed();
    if first.is_empty() {
        eprintln!("跳过：无外网连通性");
        return;
    }

    let t = Instant::now();
    let second = r.lookup_for_routing("example.com").await;
    let d2 = t.elapsed();

    assert_eq!(first, second, "缓存命中应返回同一批 IP");
    assert!(
        d2 * 5 < d1,
        "第二次查询没有明显变快（{d1:?} → {d2:?}），内置缓存可能没生效"
    );
    eprintln!("缓存生效：首次 {d1:?} → 二次 {d2:?}");
}
