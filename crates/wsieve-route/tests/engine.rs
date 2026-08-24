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
    GeoDb::new(
        PathBuf::from("/nonexistent/geoip.dat"),
        PathBuf::from("/nonexistent/geosite.dat"),
    )
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
    AddrPort {
        addr: TargetAddr::Domain(d.into()),
        port,
    }
}

fn ipv4(a: [u8; 4], port: u16) -> AddrPort {
    AddrPort {
        addr: TargetAddr::V4(a),
        port,
    }
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
    let set = rs(
        &["DOMAIN-SUFFIX,example.com,PROXY", "MATCH,DIRECT"],
        &["PROXY"],
    );
    let g = geo_stub();
    assert_eq!(
        decided(set.evaluate(&domain("example.com", 443), None, &g)),
        Decision::Outbound("PROXY".into())
    );
    assert_eq!(
        decided(set.evaluate(&domain("a.example.com", 443), None, &g)),
        Decision::Outbound("PROXY".into())
    );
    // 边界：不能把 notexample.com 当子域
    assert_eq!(
        decided(set.evaluate(&domain("notexample.com", 443), None, &g)),
        Decision::Direct
    );
}

#[test]
fn domain_exact_does_not_match_subdomain() {
    let set = rs(&["DOMAIN,example.com,PROXY", "MATCH,DIRECT"], &["PROXY"]);
    let g = geo_stub();
    assert_eq!(
        decided(set.evaluate(&domain("example.com", 443), None, &g)),
        Decision::Outbound("PROXY".into())
    );
    assert_eq!(
        decided(set.evaluate(&domain("a.example.com", 443), None, &g)),
        Decision::Direct
    );
}

#[test]
fn domain_rules_are_skipped_for_ip_targets() {
    let set = rs(
        &["DOMAIN-SUFFIX,example.com,PROXY", "MATCH,DIRECT"],
        &["PROXY"],
    );
    let g = geo_stub();
    assert_eq!(
        decided(set.evaluate(&ipv4([1, 2, 3, 4], 443), None, &g)),
        Decision::Direct
    );
}

#[test]
fn domain_match_is_case_and_trailing_dot_insensitive() {
    let set = rs(&["DOMAIN,example.com,PROXY", "MATCH,DIRECT"], &["PROXY"]);
    let g = geo_stub();
    assert_eq!(
        decided(set.evaluate(&domain("EXAMPLE.COM.", 443), None, &g)),
        Decision::Outbound("PROXY".into())
    );
}

// ── IP 类规则与两阶段 ──────────────────────────────────────

#[test]
fn ip_rule_matches_ip_target_in_first_pass() {
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let g = geo_stub();
    assert_eq!(
        decided(set.evaluate(&ipv4([10, 1, 2, 3], 443), None, &g)),
        Decision::Direct
    );
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
    let set = rs(
        &["IP-CIDR,10.0.0.0/8,DIRECT,no-resolve", "MATCH,PROXY"],
        &["PROXY"],
    );
    let g = geo_stub();
    assert_eq!(
        decided(set.evaluate(&domain("a.com", 443), None, &g)),
        Decision::Outbound("PROXY".into())
    );
}

#[test]
fn second_pass_uses_resolved_ips() {
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let g = geo_stub();
    let ips: Vec<IpAddr> = vec!["10.1.2.3".parse().unwrap()];
    assert_eq!(
        decided(set.evaluate(&domain("a.com", 443), Some(&ips), &g)),
        Decision::Direct
    );
}

#[test]
fn second_pass_with_empty_ips_falls_through() {
    // 解析失败/超时 → 传空切片 → 该规则不匹配，继续往下，绝不阻断
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let g = geo_stub();
    assert_eq!(
        decided(set.evaluate(&domain("a.com", 443), Some(&[]), &g)),
        Decision::Outbound("PROXY".into())
    );
}

#[test]
fn second_pass_never_asks_to_resolve_again() {
    // 这条是两阶段协议的死线：第二轮再抛 NeedResolve 就会死循环
    let set = rs(
        &[
            "IP-CIDR,10.0.0.0/8,DIRECT",
            "IP-CIDR,192.168.0.0/16,DIRECT",
            "MATCH,PROXY",
        ],
        &["PROXY"],
    );
    let g = geo_stub();
    let v = set.evaluate(&domain("a.com", 443), Some(&[]), &g);
    assert!(
        matches!(v, Verdict::Decided(_)),
        "第二轮必须给出判决，实为 {v:?}"
    );
}

#[test]
fn domain_rule_before_ip_rule_short_circuits_without_dns() {
    // 关键收益：被域名规则提前命中的流量，零 DNS 查询
    let set = rs(
        &[
            "DOMAIN-SUFFIX,a.com,PROXY",
            "IP-CIDR,10.0.0.0/8,DIRECT",
            "MATCH,REJECT",
        ],
        &["PROXY"],
    );
    let g = geo_stub();
    assert_eq!(
        decided(set.evaluate(&domain("a.com", 443), None, &g)),
        Decision::Outbound("PROXY".into())
    );
}

// ── 其余 ──────────────────────────────────────────────────

#[test]
fn dst_port_is_decidable_for_both_address_kinds() {
    let set = rs(&["DST-PORT,22,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let g = geo_stub();
    assert_eq!(
        decided(set.evaluate(&domain("a.com", 22), None, &g)),
        Decision::Direct
    );
    assert_eq!(
        decided(set.evaluate(&ipv4([1, 2, 3, 4], 22), None, &g)),
        Decision::Direct
    );
    assert_eq!(
        decided(set.evaluate(&domain("a.com", 443), None, &g)),
        Decision::Outbound("PROXY".into())
    );
}

#[test]
fn first_match_wins() {
    let set = rs(
        &[
            "DOMAIN-SUFFIX,a.com,PROXY",
            "DOMAIN-SUFFIX,a.com,REJECT",
            "MATCH,DIRECT",
        ],
        &["PROXY"],
    );
    let g = geo_stub();
    assert_eq!(
        decided(set.evaluate(&domain("a.com", 443), None, &g)),
        Decision::Outbound("PROXY".into())
    );
}

#[test]
fn unavailable_geo_skips_the_rule_never_blocks() {
    // GeoDb 指向不存在的文件。GEOSITE 规则应被跳过，流程继续。
    let set = rs(&["GEOSITE,cn,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let g = geo_stub();
    assert_eq!(
        decided(set.evaluate(&domain("baidu.com", 443), None, &g)),
        Decision::Outbound("PROXY".into())
    );
}

#[test]
fn mode_direct_short_circuits_everything() {
    let known: HashSet<String> = ["PROXY".to_string()].into_iter().collect();
    let set = RuleSet::build(&["MATCH,PROXY".to_string()], Mode::Direct, "", &known).unwrap();
    let g = geo_stub();
    assert_eq!(
        decided(set.evaluate(&domain("a.com", 443), None, &g)),
        Decision::Direct
    );
}

#[test]
fn mode_global_uses_global_outbound() {
    let known: HashSet<String> = ["PROXY".to_string(), "ALT".to_string()].into_iter().collect();
    let set = RuleSet::build(&["MATCH,PROXY".to_string()], Mode::Global, "ALT", &known).unwrap();
    let g = geo_stub();
    assert_eq!(
        decided(set.evaluate(&domain("a.com", 443), None, &g)),
        Decision::Outbound("ALT".into())
    );
}

