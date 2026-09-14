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

// ── I5：加载期的 GEO 引用校验 ──────────────────────────────

/// 造一份真实的 geosite.dat / geoip.dat。
///
/// 手写 protobuf wire format 而不是塞假数据：走的是与生产完全相同的
/// 解析路径，否则这些用例证明不了线上行为。
mod fixture {
    fn varint(v: u64, out: &mut Vec<u8>) {
        let mut v = v;
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }
    fn field(num: u32, wire: u8, out: &mut Vec<u8>) {
        varint(((num as u64) << 3) | wire as u64, out);
    }
    fn delimited(num: u32, payload: &[u8], out: &mut Vec<u8>) {
        field(num, 2, out);
        varint(payload.len() as u64, out);
        out.extend_from_slice(payload);
    }

    /// GeoSiteList：每个类别带若干 Domain.Type=Domain（后缀）条目。
    pub fn geosite(entries: &[(&str, &[&str])]) -> Vec<u8> {
        let mut list = Vec::new();
        for (code, domains) in entries {
            let mut site = Vec::new();
            delimited(1, code.as_bytes(), &mut site);
            for value in *domains {
                let mut dom = Vec::new();
                field(1, 0, &mut dom);
                varint(2, &mut dom); // Domain.Type = Domain
                delimited(2, value.as_bytes(), &mut dom);
                delimited(2, &dom, &mut site);
            }
            delimited(1, &site, &mut list);
        }
        list
    }

    /// GeoIPList：每个类别带若干 (ipv4, prefix) CIDR。
    ///
    /// 别名是为了避开 clippy::type_complexity —— 裸写嵌套会被拦下
    /// （与 wsieve-geo/src/ip.rs 的测试同一处理）。
    pub type CidrSpec = ([u8; 4], u32);
    pub type GeoIpSpec<'a> = (&'a str, &'a [CidrSpec]);

    pub fn geoip(entries: &[GeoIpSpec<'_>]) -> Vec<u8> {
        let mut list = Vec::new();
        for (code, cidrs) in entries {
            let mut gi = Vec::new();
            delimited(1, code.as_bytes(), &mut gi);
            for (ip, prefix) in *cidrs {
                let mut c = Vec::new();
                delimited(1, ip, &mut c);
                field(2, 0, &mut c);
                varint(*prefix as u64, &mut c);
                delimited(2, &c, &mut gi);
            }
            delimited(1, &gi, &mut list);
        }
        list
    }
}

/// 落地一对真实 .dat 并返回指向它们的 GeoDb。
fn geo_real(tag: &str) -> GeoDb {
    let dir = std::env::temp_dir().join(format!("wsieve-route-geo-{tag}"));
    std::fs::create_dir_all(&dir).unwrap();
    let site = dir.join("geosite.dat");
    let ip = dir.join("geoip.dat");
    std::fs::write(&site, fixture::geosite(&[("cn", &["baidu.com", "qq.com"])])).unwrap();
    std::fs::write(&ip, fixture::geoip(&[("cn", &[([1, 2, 0, 0], 16)])])).unwrap();
    GeoDb::new(ip, site)
}

#[test]
fn typo_in_geosite_class_is_reported_with_its_line_number() {
    // 这正是 I5 的场景：写了 GEOSITE,cnn 而非 cn。规则语法完全合法，
    // 加载不报错，判决时也只是「不命中」—— 与压根没写这条规则毫无区别。
    // 不告警的话，用户会一直以为自己配了条分流规则。
    let set = rs(
        &[
            "# 先走直连",
            "GEOSITE,cn,DIRECT",
            "GEOSITE,cnn,PROXY",
            "MATCH,PROXY",
        ],
        &["PROXY"],
    );
    let w = set.check_geo(&geo_real("typo"));
    assert_eq!(w.len(), 1, "只有 cnn 是错的，cn 存在：{w:?}");
    match &w[0] {
        wsieve_route::GeoWarning::UnknownClass { line, code, .. } => {
            // 行号要含注释行 —— UI 拿它去标红，差一行就标错规则
            assert_eq!(*line, 3, "行号应是 3（注释占号），实为 {line}");
            assert_eq!(code, "cnn");
        }
        other => panic!("应是 UnknownClass，实为 {other:?}"),
    }
    // 告警文案要能让用户直接定位
    let text = w[0].to_string();
    assert!(text.contains("cnn"), "要点名冒犯的类别：{text}");
    assert!(text.contains("GEOSITE"), "要用用户写的那个词：{text}");
    assert!(text.contains('3'), "要给出行号：{text}");
}

#[test]
fn geoip_class_typo_is_caught_too() {
    let set = rs(&["GEOIP,zz,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let w = set.check_geo(&geo_real("ipclass"));
    assert_eq!(w.len(), 1, "{w:?}");
    let text = w[0].to_string();
    assert!(text.contains("zz") && text.contains("GEOIP"), "{text}");
}

#[test]
fn correct_geo_classes_produce_no_warnings() {
    // 假阳性和漏报一样有害：天天弹一条无意义的告警，用户很快就不看了
    let set = rs(
        &["GEOSITE,cn,DIRECT", "GEOIP,cn,DIRECT", "MATCH,PROXY"],
        &["PROXY"],
    );
    assert!(set.check_geo(&geo_real("clean")).is_empty());
}

#[test]
fn missing_geo_file_is_reported_separately_from_a_typo() {
    // 两者的修复方向完全不同：缺文件是环境问题（下载即可），
    // 类别写错是配置笔误（换多少个文件都没用）。混报会把用户引偏。
    let set = rs(&["GEOSITE,cn,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let w = set.check_geo(&geo_stub());
    assert_eq!(w.len(), 1, "{w:?}");
    match &w[0] {
        wsieve_route::GeoWarning::DbUnavailable { affected, reason, .. } => {
            assert_eq!(*affected, 1);
            // 原始原因不能被吞掉 —— 要指明是哪个文件
            assert!(reason.contains("geosite.dat"), "要指明是哪个文件：{reason}");
        }
        other => panic!("缺文件不该报成类别不存在，实为 {other:?}"),
    }
}

#[test]
fn an_unavailable_db_yields_one_warning_not_one_per_rule() {
    // 缺一个文件却刷十条一模一样的告警，是在用噪音淹没信号
    let set = rs(
        &[
            "GEOSITE,cn,DIRECT",
            "GEOSITE,google,PROXY",
            "GEOSITE,netflix,PROXY",
            "MATCH,PROXY",
        ],
        &["PROXY"],
    );
    let w = set.check_geo(&geo_stub());
    assert_eq!(w.len(), 1, "应汇总成一条：{w:?}");
    match &w[0] {
        wsieve_route::GeoWarning::DbUnavailable { affected, .. } => {
            assert_eq!(*affected, 3, "但要如实报出受影响的条数")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_broken_geosite_does_not_suppress_geoip_checking() {
    // 两个库互相独立。geosite 缺失时若连 geoip 的校验也一并放弃，
    // 用户修好前者之后才会看到后者的问题 —— 平白多一轮往返。
    let dir = std::env::temp_dir().join("wsieve-route-geo-halfbroken");
    std::fs::create_dir_all(&dir).unwrap();
    let ip = dir.join("geoip.dat");
    std::fs::write(&ip, fixture::geoip(&[("cn", &[([1, 2, 0, 0], 16)])])).unwrap();
    let geo = GeoDb::new(ip, PathBuf::from("/nonexistent/geosite.dat"));

    let set = rs(&["GEOSITE,cn,DIRECT", "GEOIP,zz,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let w = set.check_geo(&geo);
    assert_eq!(w.len(), 2, "两个库应各报各的：{w:?}");
    assert!(
        w.iter().any(|x| matches!(x, wsieve_route::GeoWarning::DbUnavailable { .. })),
        "geosite 缺失要报：{w:?}"
    );
    assert!(
        w.iter().any(|x| matches!(x, wsieve_route::GeoWarning::UnknownClass { .. })),
        "geoip 的类别笔误照样要报：{w:?}"
    );
}

#[test]
fn a_ruleset_without_geo_rules_never_touches_the_files() {
    // check_geo 不该为一份不含 GEO 规则的配置去解析 11MB 的 geosite.dat。
    // 用不存在的路径来断言：真去读了就会产生告警。
    let set = rs(&["DOMAIN,a.com,PROXY", "IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    assert!(set.check_geo(&geo_stub()).is_empty(), "无 GEO 规则不该碰文件");
}

#[test]
fn geo_warnings_never_block_evaluation() {
    // §12 的纪律：跳过 + 告警，绝不阻断。告警存在，判决照常给出。
    let set = rs(&["GEOSITE,cnn,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let geo = geo_real("nonblocking");
    assert_eq!(set.check_geo(&geo).len(), 1);
    assert_eq!(
        decided(set.evaluate(&domain("baidu.com", 443), None, &geo)),
        Decision::Outbound("PROXY".into()),
        "写错类别的规则应被跳过，流程继续走到 MATCH"
    );
}


// ── 判决出处：交给阶段 4/5 的数据契约 ──
//
// 流量视图的桑基图中层要显示「是哪条规则把这条流送去了这个出站」。
// 这几条锁住的是：命中的规则原文与行号能被带出判决路径。判决完就丢掉的话，
// 前端再也拿不到 —— 事后补采集要么做不到，要么要把判决重跑一遍。

#[test]
fn a_decision_carries_the_rule_that_made_it() {
    let set = rs(
        &[
            "# 注释也占行号",
            "DOMAIN-SUFFIX,google.com,日本节点",
            "MATCH,DIRECT",
        ],
        &["日本节点"],
    );
    let e = set.evaluate_explained(&domain("www.google.com", 443), None, &geo_stub());
    assert_eq!(decided(e.verdict), Decision::Outbound("日本节点".into()));
    let hit = e.hit.expect("命中规则必须带出出处");
    assert_eq!(hit.text, "DOMAIN-SUFFIX,google.com,日本节点");
    assert_eq!(hit.line, 2, "行号按用户看到的算，注释也占号");
}

#[test]
fn the_fallback_match_is_itself_a_rule_hit() {
    // MATCH 是用户显式写下的一行，它做出的判决同样要能反查到出处。
    let set = rs(&["DOMAIN,a.com,DIRECT", "MATCH,日本节点"], &["日本节点"]);
    let e = set.evaluate_explained(&domain("nothing-matches.example", 443), None, &geo_stub());
    assert_eq!(decided(e.verdict), Decision::Outbound("日本节点".into()));
    assert_eq!(e.hit.expect("MATCH 也是规则").text, "MATCH,日本节点");
}

#[test]
fn mode_shortcuts_report_no_rule_hit() {
    // global / direct 模式下没有任何规则被执行。硬塞一条进去，
    // UI 就会显示一条其实没跑过的规则 —— 那是在编造证据。
    let known: HashSet<String> = ["日本节点"].iter().map(|s| s.to_string()).collect();
    for mode in [Mode::Global, Mode::Direct] {
        let set = RuleSet::build(
            &["DOMAIN,a.com,DIRECT".to_string(), "MATCH,日本节点".to_string()],
            mode,
            "",
            &known,
        )
        .unwrap();
        let e = set.evaluate_explained(&domain("a.com", 443), None, &geo_stub());
        assert!(e.hit.is_none(), "{mode:?} 模式不该报告规则命中：{:?}", e.hit);
    }
}

#[test]
fn need_resolve_is_not_a_rule_hit() {
    // NeedResolve 时还没有判决，那条 IP 规则只是**触发**了解析而非命中它。
    let set = rs(&["GEOIP,CN,DIRECT", "MATCH,日本节点"], &["日本节点"]);
    let e = set.evaluate_explained(&domain("a.com", 443), None, &geo_stub());
    assert!(matches!(e.verdict, Verdict::NeedResolve { .. }));
    assert!(e.hit.is_none(), "尚未判决，不能算命中");
}

#[test]
fn explained_and_plain_evaluate_never_disagree() {
    // evaluate 现在是 evaluate_explained 的薄包装。若哪天有人给其中一条
    // 加了分支而忘了另一条，判决与「规则试算」就会开始给出不同答案。
    let set = rs(
        &[
            "DST-PORT,22,REJECT",
            "DOMAIN-KEYWORD,ads,REJECT",
            "IP-CIDR,10.0.0.0/8,DIRECT,no-resolve",
            "MATCH,日本节点",
        ],
        &["日本节点"],
    );
    let geo = geo_stub();
    let cases = [
        domain("x.com", 22),
        domain("some-ads-host.com", 443),
        domain("plain.example", 443),
        ipv4([10, 1, 2, 3], 443),
        ipv4([1, 1, 1, 1], 80),
    ];
    for t in cases {
        assert_eq!(
            set.evaluate(&t, None, &geo),
            set.evaluate_explained(&t, None, &geo).verdict,
            "两条路径对 {} 给出了不同判决",
            t.display()
        );
    }
}

#[test]
fn resolving_rules_are_counted_so_the_gap_can_be_reported() {
    // 没接解析器时，这些规则对**域名**目标一条都不会命中。调用方据此告警
    // —— 静默的覆盖面缺失比报错更危险：用户会以为 GEOIP,CN,DIRECT 在生效。
    let set = rs(
        &[
            "DOMAIN,a.com,DIRECT",              // 不需要解析
            "GEOIP,CN,DIRECT",                  // 需要
            "IP-CIDR,1.0.0.0/8,DIRECT",         // 需要
            "IP-CIDR,10.0.0.0/8,DIRECT,no-resolve", // 明确说了不解析，不算
            "DST-PORT,22,REJECT",               // 不需要
            "MATCH,PROXY",
        ],
        &["PROXY"],
    );
    assert_eq!(set.resolving_rule_count(), 2);

    let none = rs(&["DOMAIN,a.com,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    assert_eq!(none.resolving_rule_count(), 0, "无 IP 类规则时不该告警");
}
