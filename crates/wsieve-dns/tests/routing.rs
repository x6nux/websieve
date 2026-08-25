//! 解析注入 + 规则命中（设计文档 §14 阶段 3 的验证方式）。
//!
//! **这里没有假解析器。** 断言的是真链路：真的 `DnsResolver`（真 UDP、
//! 真报文编解码、真硬超时、真缓存）对着一台**跑在 127.0.0.1 上的真 DNS
//! 服务器**发查询，服务器按本文件给的应答表回话。
//!
//! 之所以不用「返回固定表的假解析器」：那种东西绕过了报文编解码、超时、
//! 缓存与 hosts 开关，测的是自己写的 HashMap 而非产品代码 —— 与项目
//! 「严禁 mock 代替真实实现」的纪律相悖。起一台本地 DNS 服务器成本极低
//! （下面 `TestDnsServer` 不到 80 行），换来的是：链路上每一环都是真的，
//! 而结果依然确定、脱外网、毫秒级。
//!
//! 服务器还**记录被查询过的域名**，这让「不该解析时确实一次 DNS 都没发」
//! 成为可断言的事实 —— 而且是在**线缆层面**断言，比问解析器「你被调用了
//! 吗」强得多：后者答"没有"也可能是它自己走了缓存。

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hickory_resolver::proto::op::{Message, MessageType, OpCode, ResponseCode};
use hickory_resolver::proto::rr::rdata::{A, AAAA};
use hickory_resolver::proto::rr::{Name, RData, Record, RecordType};
use tokio::net::UdpSocket;

use wsieve_dns::{decide, DnsResolver};
use wsieve_geo::GeoDb;
use wsieve_proto::addr::{AddrPort, TargetAddr};
use wsieve_route::{Decision, Mode, RuleSet};

// ── 真 DNS 服务器 ────────────────────────────────────────────

/// 一台跑在 127.0.0.1 随机端口上的真 UDP DNS 服务器。
///
/// 只做一件事：按 `table` 回答 A/AAAA 查询，表里没有的域名回 NXDOMAIN。
/// 这足以覆盖判决链路需要的全部情形 —— 解析成功、解析失败，二者而已。
struct TestDnsServer {
    port: u16,
    /// 线缆层面的证据：服务器真正收到过哪些查询（按到达顺序）。
    /// 「零 DNS 泄漏」这条纪律要在这里验，而不是问解析器。
    seen: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl TestDnsServer {
    /// `table`：域名 → IP 列表。未列出的域名一律 NXDOMAIN
    /// （等价于生产中的「域名不存在」，触发纪律③的空切片路径）。
    async fn start(table: &[(&str, &[&str])]) -> Self {
        Self::spawn(table, Silence::Answer).await
    }

    /// 一台**收下查询但永不回话**的服务器。
    ///
    /// 这是比 NXDOMAIN 更狠的一档故障：上游黑洞。用本地静默服务器而不是
    /// RFC 5737 的 `192.0.2.1`，是因为后者**在真实机器上并不可靠** ——
    /// 本机实测 `udp://192.0.2.1` 5ms 就返回了 `[fc00::d1, 198.18.0.211]`，
    /// 那是链路上某处的 DNS 拦截给的 fake-ip（198.18.0.0/15 正是 fake-ip 段）。
    /// 拿它当黑洞，测试会在别人的机器上随机变红。本地静默端口没有这个问题：
    /// 它是我们自己绑的，没有任何中间人能替它回话。
    async fn start_silent() -> Self {
        Self::spawn(&[], Silence::NeverAnswer).await
    }

    async fn spawn(table: &[(&str, &[&str])], silence: Silence) -> Self {
        let table: HashMap<String, Vec<IpAddr>> = table
            .iter()
            .map(|(d, ips)| {
                let ips = ips
                    .iter()
                    .map(|s| s.parse().unwrap_or_else(|_| panic!("非法 IP：{s}")))
                    .collect();
                (d.trim_end_matches('.').to_ascii_lowercase(), ips)
            })
            .collect();

        let sock = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("绑定本地 UDP 端口失败");
        let port = sock.local_addr().expect("取本地端口失败").port();
        let seen = Arc::new(Mutex::new(Vec::new()));

        let task = tokio::spawn(serve(sock, table, Arc::clone(&seen), silence));
        Self { port, seen, task }
    }

    /// 服务器**在线缆上**收到过的查询域名（按顺序，同名多次会重复出现）。
    fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("测试服务器状态锁中毒").clone()
    }

    /// 指向这台服务器的真解析器。走明文 UDP：本地回环上没有中间人，
    /// 加 TLS 只会让测试依赖证书体系而测不出更多东西。
    fn resolver(&self) -> DnsResolver {
        DnsResolver::new(
            &[format!("udp://127.0.0.1:{}", self.port)],
            Duration::from_secs(2),
            4096,
            Duration::from_secs(30),
        )
        .expect("上游是 IP 字面量，应能建起来")
    }
}

impl Drop for TestDnsServer {
    fn drop(&mut self) {
        // 测试结束就收摊，别把任务留在 runtime 里
        self.task.abort();
    }
}

/// 服务器对收到的查询怎么处置。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Silence {
    /// 正常按表回话（表里没有则 NXDOMAIN）
    Answer,
    /// 收下、记账，但**永不回话** —— 模拟上游黑洞
    NeverAnswer,
}

async fn serve(
    sock: UdpSocket,
    table: HashMap<String, Vec<IpAddr>>,
    seen: Arc<Mutex<Vec<String>>>,
    silence: Silence,
) {
    let mut buf = vec![0u8; 1500];
    loop {
        let (n, from) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            // socket 关了就退出循环；测试结束时的正常路径
            Err(_) => return,
        };
        let Ok(req) = Message::from_vec(&buf[..n]) else {
            // 收到解不开的报文不是本测试关心的情形，但也不能静默丢：
            // 若真发生，说明解析器发的东西有问题，值得让测试看见。
            panic!("测试 DNS 服务器收到无法解析的报文（{n} 字节）");
        };
        let reply = answer(&req, &table, &seen);
        if silence == Silence::NeverAnswer {
            // 已经记完账（answer 里做的），到此为止，不发回应
            continue;
        }
        let Ok(bytes) = reply.to_vec() else {
            panic!("测试 DNS 服务器编码应答失败");
        };
        if sock.send_to(&bytes, from).await.is_err() {
            return;
        }
    }
}

fn answer(
    req: &Message,
    table: &HashMap<String, Vec<IpAddr>>,
    seen: &Mutex<Vec<String>>,
) -> Message {
    let mut resp = Message::response(req.metadata.id, OpCode::Query);
    resp.metadata.message_type = MessageType::Response;
    resp.metadata.recursion_desired = req.metadata.recursion_desired;
    resp.metadata.recursion_available = true;
    resp.metadata.authoritative = true;
    resp.add_queries(req.queries.iter().cloned());

    let Some(q) = req.queries.first() else {
        resp.metadata.response_code = ResponseCode::FormErr;
        return resp;
    };

    let name = q.name().to_ascii();
    let key = name.trim_end_matches('.').to_ascii_lowercase();
    seen.lock().expect("测试服务器状态锁中毒").push(key.clone());

    let Some(ips) = table.get(&key) else {
        // 表里没有 = 域名不存在。生产中这条路径通向纪律③的空切片。
        resp.metadata.response_code = ResponseCode::NXDomain;
        return resp;
    };

    // 只回与问题类型相符的记录；类型不符时回空 answer（NOERROR），
    // 这正是真实服务器对「有 A 无 AAAA」的域名的做法。
    for ip in ips {
        let rdata = match (q.query_type(), ip) {
            (RecordType::A, IpAddr::V4(v4)) => RData::A(A(*v4)),
            (RecordType::AAAA, IpAddr::V6(v6)) => RData::AAAA(AAAA(*v6)),
            _ => continue,
        };
        let owner: Name = q.name().clone();
        resp.add_answer(Record::from_rdata(owner, 60, rdata));
    }
    resp
}

// ── 解析次数计数器 ───────────────────────────────────────────

/// 包住一个**真解析器**，如实转发每一次调用，顺带记账调用次数。
///
/// 这不是替身：解析工作全部由被包住的真解析器完成，本类型一个字节的
/// 应答都不伪造。之所以需要它，是因为「最多解析一次」这条纪律**无法**
/// 在线缆上验证 —— hickory 内置缓存会把第二次 `resolve()` 直接命中缓存，
/// 线缆上一个包都不多。实测过：在 `decide()` 里硬加一行第二次 resolve，
/// 只数线缆查询的断言照样通过。要盯住这条纪律，就必须数
/// **`decide()` 调了几次 `resolve()`**，而不是数发了几个包。
struct CountingResolver<R> {
    inner: R,
    calls: Arc<Mutex<Vec<String>>>,
}

impl<R: wsieve_dns::RoutingResolver> CountingResolver<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// `decide()` 实际发起过的解析调用（按顺序）。
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("计数器锁中毒").clone()
    }
}

impl<R: wsieve_dns::RoutingResolver> wsieve_dns::RoutingResolver for CountingResolver<R> {
    fn resolve<'a>(
        &'a self,
        domain: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<IpAddr>> + Send + 'a>> {
        self.calls
            .lock()
            .expect("计数器锁中毒")
            .push(domain.to_string());
        // 原样转发给真解析器，不做任何改写
        self.inner.resolve(domain)
    }
}

// ── 测试脚手架 ───────────────────────────────────────────────

fn geo_missing() -> GeoDb {
    // 指向不存在的文件：所有 GEO 查询都会失败，从而顺带验证
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
    .expect("测试规则应能构建")
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

// ── 不该解析的时候，一次 DNS 都不许发 ────────────────────────

#[tokio::test]
async fn domain_rule_hit_never_touches_dns() {
    let set = rs(
        &[
            "DOMAIN-SUFFIX,example.com,PROXY",
            "GEOIP,CN,DIRECT",
            "MATCH,REJECT",
        ],
        &["PROXY"],
    );
    let srv = TestDnsServer::start(&[("www.example.com", &["1.2.3.4"])]).await;
    let r = srv.resolver();
    let out = decide(&set, &domain("www.example.com", 443), &geo_missing(), &r).await;

    assert_eq!(out.decision, Decision::Outbound("PROXY".into()));
    assert!(!out.resolved, "域名规则命中，不该触发解析");
    assert!(
        srv.seen().is_empty(),
        "线缆上不该出现任何查询，实际收到：{:?}",
        srv.seen()
    );
}

#[tokio::test]
async fn ip_target_is_decided_without_resolution() {
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let srv = TestDnsServer::start(&[]).await;
    let r = srv.resolver();
    let out = decide(&set, &ipv4([10, 1, 2, 3], 443), &geo_missing(), &r).await;

    assert_eq!(out.decision, Decision::Direct);
    assert!(!out.resolved);
    assert!(srv.seen().is_empty(), "目标本就是 IP，无需解析");
}

#[tokio::test]
async fn no_resolve_flag_suppresses_the_query() {
    // 局域网段规则应默认带 no-resolve（设计文档 §6.1）
    let set = rs(
        &["IP-CIDR,192.168.0.0/16,DIRECT,no-resolve", "MATCH,PROXY"],
        &["PROXY"],
    );
    let srv = TestDnsServer::start(&[("a.com", &["192.168.1.1"])]).await;
    let r = srv.resolver();
    let out = decide(&set, &domain("a.com", 443), &geo_missing(), &r).await;

    assert_eq!(out.decision, Decision::Outbound("PROXY".into()));
    assert!(!out.resolved);
    assert!(
        srv.seen().is_empty(),
        "no-resolve 必须真的不发查询，实际：{:?}",
        srv.seen()
    );
}

// ── 该解析的时候，解析结果要真的进判决 ──────────────────────

#[tokio::test]
async fn resolved_ip_drives_the_cidr_rule() {
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let srv = TestDnsServer::start(&[("intranet.corp", &["10.7.7.7"])]).await;
    let r = srv.resolver();
    let out = decide(&set, &domain("intranet.corp", 443), &geo_missing(), &r).await;

    assert_eq!(out.decision, Decision::Direct, "解析到内网段应判直连");
    assert!(out.resolved);
    assert_eq!(
        out.ips,
        vec!["10.7.7.7".parse::<IpAddr>().unwrap()],
        "解析结果要如实带回"
    );
}

#[tokio::test]
async fn resolved_ip_outside_the_cidr_falls_through() {
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let srv = TestDnsServer::start(&[("a.com", &["93.184.216.34"])]).await;
    let r = srv.resolver();
    let out = decide(&set, &domain("a.com", 443), &geo_missing(), &r).await;

    assert_eq!(out.decision, Decision::Outbound("PROXY".into()));
    assert!(out.resolved);
}

#[tokio::test]
async fn any_one_of_several_ips_matching_is_enough() {
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let srv = TestDnsServer::start(&[("multi.com", &["93.184.216.34", "10.0.0.1"])]).await;
    let r = srv.resolver();
    let out = decide(&set, &domain("multi.com", 443), &geo_missing(), &r).await;

    assert_eq!(out.decision, Decision::Direct, "有一个 IP 落在段内即命中");
    assert_eq!(out.ips.len(), 2, "解析结果要如实带回，供 UI 展示");
}

#[tokio::test]
async fn resolution_happens_at_most_once_across_many_ip_rules() {
    // 第二轮从头重扫，会再次经过多条 IP 规则 —— 但解析只能发生一次。
    //
    // 这里数的是 **`decide()` 调了几次 `resolve()`**，不是线缆上的包数。
    // 后者验不了这条纪律：hickory 内置缓存会让第二次 resolve 直接命中，
    // 线缆上一个包都不多。实测过 —— 在 decide 里硬插一行第二次 resolve，
    // 只数包的断言照样绿。
    let set = rs(
        &[
            "IP-CIDR,10.0.0.0/8,DIRECT",
            "IP-CIDR,172.16.0.0/12,DIRECT",
            "GEOIP,CN,DIRECT",
            "MATCH,PROXY",
        ],
        &["PROXY"],
    );
    let srv = TestDnsServer::start(&[("a.com", &["8.8.8.8"])]).await;
    let r = CountingResolver::new(srv.resolver());
    let out = decide(&set, &domain("a.com", 443), &geo_missing(), &r).await;

    assert_eq!(out.decision, Decision::Outbound("PROXY".into()));
    assert_eq!(
        r.calls(),
        vec!["a.com".to_string()],
        "跨多条 IP 规则也只许解析一次"
    );
}

// ── 纪律③：解析失败绝不阻断连接 ────────────────────────────

#[tokio::test]
async fn resolution_failure_is_treated_as_no_match_not_as_an_error() {
    // 表里没有 = 服务器真的回 NXDOMAIN，走的是生产同一条失败路径
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let srv = TestDnsServer::start(&[]).await;
    let r = srv.resolver();
    let out = decide(&set, &domain("unreachable.example", 443), &geo_missing(), &r).await;

    assert_eq!(
        out.decision,
        Decision::Outbound("PROXY".into()),
        "解析失败必须继续往下走到 MATCH，而不是阻断连接"
    );
    assert!(out.resolved, "确实尝试过解析");
    assert!(out.ips.is_empty());
    assert!(!srv.seen().is_empty(), "确实发出了查询，只是没解析到");
}

#[tokio::test]
async fn an_upstream_that_never_answers_still_yields_a_decision() {
    // 比 NXDOMAIN 更狠的一档：上游收下查询却永不回话。判决链路必须照常
    // 产出结论，而不是把连接晾在那里等 DNS。
    //
    // **这个测试不是硬超时的回归防线，别把它当成那个。** 实测（把
    // `lookup_for_routing` 的外层 `tokio::time::timeout` 摘掉再跑）它照样
    // 通过：本用例走 UDP，没有 TCP/TLS 建连阶段，hickory 自己的
    // `opts.timeout` 在轮与轮之间就能生效。发现①的病灶**只在建连路径上**
    // —— 卡在系统 connect 超时里，一轮都走不完，deadline 没机会被检查。
    // 真正锁住硬超时的是 `tests/resolver.rs` 里走 DoH 的那两个用例。
    // 这里锁的是另一件事：`decide()` 在上游静默时不阻断、不虚构 IP。
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let srv = TestDnsServer::start_silent().await;
    let hard = Duration::from_millis(600);
    let r = DnsResolver::new(
        &[format!("udp://127.0.0.1:{}", srv.port)],
        hard,
        4096,
        Duration::from_secs(30),
    )
    .expect("上游是 IP 字面量");

    let started = std::time::Instant::now();
    let out = decide(&set, &domain("blackhole.example", 443), &geo_missing(), &r).await;
    let elapsed = started.elapsed();

    assert_eq!(
        out.decision,
        Decision::Outbound("PROXY".into()),
        "上游静默也必须产出判决，绝不阻断连接"
    );
    assert!(out.resolved);
    assert!(
        out.ips.is_empty(),
        "上游没回话，不该凭空多出 IP：{:?}",
        out.ips
    );
    assert!(!srv.seen().is_empty(), "查询确实发出去了，只是没人回");
    // 只防「无限期挂住」这一种退化，不是精确计时断言 —— 精确计时在 CI 上
    // 会因调度抖动变红。实测本用例稳定在 1.2s 附近（600ms × A/AAAA 两问）。
    assert!(
        elapsed < Duration::from_secs(5),
        "判决被 DNS 拖住了 {elapsed:?}，上游静默不该让连接等这么久"
    );
}

#[tokio::test]
async fn resolution_failure_still_reaches_a_reject_verdict_if_thats_the_fallback() {
    // 兜底是 REJECT 时也一样：判决照常产出，只是内容是拒绝
    let set = rs(&["GEOIP,CN,DIRECT", "MATCH,REJECT"], &[]);
    let srv = TestDnsServer::start(&[]).await;
    let r = srv.resolver();
    let out = decide(&set, &domain("nowhere.example", 443), &geo_missing(), &r).await;

    assert_eq!(out.decision, Decision::Reject);
    assert!(out.resolved);
}

#[tokio::test]
async fn geo_unavailable_plus_resolution_failure_still_decides() {
    // GEO 文件读不到 + 解析也失败：两种故障叠加，仍必须给出判决
    let set = rs(
        &["GEOSITE,cn,DIRECT", "GEOIP,CN,DIRECT", "MATCH,PROXY"],
        &["PROXY"],
    );
    let srv = TestDnsServer::start(&[]).await;
    let r = srv.resolver();
    let out = decide(&set, &domain("baidu.com", 443), &geo_missing(), &r).await;

    assert_eq!(out.decision, Decision::Outbound("PROXY".into()));
}

// ── 首命中顺序语义在跨越解析后依然成立 ──────────────────────

#[tokio::test]
async fn earlier_ip_rule_wins_over_later_one_after_resolution() {
    let set = rs(
        &[
            "IP-CIDR,8.8.8.0/24,REJECT",
            "IP-CIDR,8.0.0.0/8,DIRECT",
            "MATCH,PROXY",
        ],
        &["PROXY"],
    );
    let srv = TestDnsServer::start(&[("dns.example", &["8.8.8.8"])]).await;
    let r = srv.resolver();
    let out = decide(&set, &domain("dns.example", 443), &geo_missing(), &r).await;

    assert_eq!(out.decision, Decision::Reject, "两条都能匹配时取靠前的那条");
}

#[tokio::test]
async fn a_domain_rule_after_the_triggering_ip_rule_still_applies_in_pass_two() {
    // 第二轮从头重扫，靠后的域名规则在第一轮就已试过并未中；
    // 关键是第二轮不能因为「已经解析过」而跳过它
    let set = rs(
        &[
            "IP-CIDR,10.0.0.0/8,DIRECT",
            "DOMAIN-SUFFIX,special.com,REJECT",
            "MATCH,PROXY",
        ],
        &["PROXY"],
    );
    let srv = TestDnsServer::start(&[("a.special.com", &["93.184.216.34"])]).await;
    let r = srv.resolver();
    let out = decide(&set, &domain("a.special.com", 443), &geo_missing(), &r).await;

    assert_eq!(out.decision, Decision::Reject);
}

// ── 幂等性 ────────────────────────────────────────────────

#[tokio::test]
async fn the_same_input_yields_the_same_decision_every_time() {
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let srv = TestDnsServer::start(&[("a.com", &["10.0.0.9"])]).await;
    let r = srv.resolver();
    let t = domain("a.com", 443);

    let first = decide(&set, &t, &geo_missing(), &r).await;
    let second = decide(&set, &t, &geo_missing(), &r).await;
    assert_eq!(first, second, "同一输入必须得到同一判决");
}

// ── 阶段 2 的用法必须真的成立 ──────────────────────────────

#[tokio::test]
async fn decide_accepts_a_dyn_resolver_because_phase_two_holds_one() {
    // 计划的「交给阶段 2 的接口」一节写明出站管理器会持有
    // `Arc<dyn RoutingResolver>`。`decide` 的 `?Sized` 就是为它加的 ——
    // 这个测试保证那行签名不会被谁「顺手清理」掉。
    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let srv = TestDnsServer::start(&[("a.com", &["10.0.0.9"])]).await;
    let r: Arc<dyn wsieve_dns::RoutingResolver> = Arc::new(srv.resolver());

    let out = decide(&set, &domain("a.com", 443), &geo_missing(), r.as_ref()).await;
    assert_eq!(out.decision, Decision::Direct);
}

// ── 规则试算（§11.2）复用同一份判决 ────────────────────────

#[tokio::test]
async fn rule_test_without_resolution_can_tell_that_resolution_is_needed() {
    // UI 的 `rule_test(target, resolve: false)` 走的就是这条：只跑第一轮，
    // 拿到 NeedResolve 即可如实告诉用户「需解析才能确定」，全程零 DNS。
    // 与 `decide()` 共用同一个 evaluate，所以试算与真实判决永远一致。
    use wsieve_route::Verdict;

    let set = rs(&["IP-CIDR,10.0.0.0/8,DIRECT", "MATCH,PROXY"], &["PROXY"]);
    let srv = TestDnsServer::start(&[("a.com", &["10.0.0.9"])]).await;

    let first_pass = set.evaluate(&domain("a.com", 443), None, &geo_missing());
    assert_eq!(
        first_pass,
        Verdict::NeedResolve {
            domain: "a.com".into()
        }
    );
    assert!(srv.seen().is_empty(), "只跑第一轮时不该发任何查询");

    // resolve: true 时同一目标走 decide()，两者对同一份规则给出一致结论
    let r = srv.resolver();
    let out = decide(&set, &domain("a.com", 443), &geo_missing(), &r).await;
    assert_eq!(out.decision, Decision::Direct);
}
