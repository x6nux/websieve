//! **TUN 环路回归专项**（设计文档 §13 最后一行）。
//!
//! 验的是 §8.3.1 那条死循环：
//!
//! ```text
//! WebView → 127.0.0.1:18443（环回，不过 TUN ✓）
//!   → 转发器 → 真实服务器 IP:443
//!       → 被 TUN 捕获 → 路由层判「走代理」
//!           → 出站 → WebView → 127.0.0.1:18443 → ↺ 死循环
//! ```
//!
//! # 本文件与各模块单测的分工
//!
//! `inbound.rs` / `bypass.rs` / `fakedns.rs` 的单测各自钉住**一个模块内部**
//! 的行为。环路却是**跨模块**才成立的东西：它要 DNS 发对地址、池反查对得上、
//! bypass 名单收得住、路由表写得对，四件事**同时**成立才不闭合。任何一处
//! 单独看都是绿的，合起来仍然可能环。本文件因此一律用**多个真实模块串起来**
//! 的路径，不复述单模块断言。
//!
//! # 哪些是真跑，哪些是结构断言
//!
//! **真跑**（这些代码路径在测试里被实际执行）：
//!   - `FakeDns::respond()` 吃真实 DNS 线格式字节、吐真实应答字节
//!   - `FakeIpPool` 的分配与反查
//!   - `TunInbound::classify()` 的判定
//!   - `BypassSet` 的增删与引用计数
//!   - `startup::bring_up()` 的六步编排
//!   - `TunRoutes::sync_bypass()` / `apply()` 与 `routes::argv()` 构造出的
//!     **真实 `route`(8) 参数向量**（由测试内的后端记录下来断言）
//!
//! **结构断言**（无法在无 root 环境里执行，只能断言构造出来的东西是对的）：
//!   - `route`(8) 命令**没有真的执行** —— 实测即使加 `-t` 也要 root（rc=77）
//!   - utun 设备**没有真的创建**，因此「TUN 捕获」被建模为**判决函数**：
//!     凡是走到 `classify()` 的目标，就是被 TUN 捕获了的
//!   - 内核的最长前缀匹配**没有真的跑** —— `-host` 比 `/1` 更具体这件事
//!     是 IP 路由的定义，测试只能断言我们确实写了 `-host`
//!
//! 真设备上的验证是手工步骤（计划文档「手工验证清单」M4 / M6），**尚未做**。
//! 本文件全绿**不等于**真机上环路不成立。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use hickory_proto::op::{Message, MessageType, OpCode, Query};
use hickory_proto::rr::rdata::A;
use hickory_proto::rr::{Name, RData, Record, RecordType};

use wsieve_tun::bypass::BypassSet;
use wsieve_tun::fakedns::{Answer, FakeDns, Screen};
use wsieve_tun::fakeip::FakeIpPool;
use wsieve_tun::inbound::{TunInbound, TunTarget};
use wsieve_tun::managed::{RouteBackend, TunRoutes};
use wsieve_tun::routes::{argv, RouteEntry, RouteOp};
use wsieve_tun::startup::{bring_up, FakeIpRangeOwner, Step, Timeline, TunStage};

/// 出站服务器的域名。转发器要连的就是它解析出来的地址。
const SERVER_DOMAIN: &str = "srv.example.com";
/// 转发器的目的地。真实实现里由 `shard::resolve_upstream` 在运行时得到。
const SERVER_IP: &str = "203.0.113.7";
/// 物理网关。bypass 路由必须指向它。
const PHYS_GW: &str = "10.0.0.1";
/// TUN 自己的地址，两条 `/1` 默认路由指向它。
const TUN_GW: &str = "198.18.0.1";

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

fn sa(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

/// 记录 argv 的路由后端。**不执行**任何命令 —— `route`(8) 实测即使 `-t`
/// 也要 root，所以能验的只到「构造出来的参数向量对不对」为止。
#[derive(Default)]
struct RecordingBackend {
    /// 当前存在的托管路由。
    table: Mutex<Vec<RouteEntry>>,
    /// 依次执行过的 argv，含 add 与 delete。
    argvs: Mutex<Vec<Vec<String>>>,
}

impl RecordingBackend {
    fn argv_log(&self) -> Vec<Vec<String>> {
        self.argvs.lock().unwrap().clone()
    }
    fn table(&self) -> Vec<RouteEntry> {
        self.table.lock().unwrap().clone()
    }
}

impl RouteBackend for RecordingBackend {
    fn add(&self, e: &RouteEntry) -> anyhow::Result<()> {
        self.argvs.lock().unwrap().push(argv(RouteOp::Add, e));
        self.table.lock().unwrap().push(e.clone());
        Ok(())
    }
    fn delete(&self, e: &RouteEntry) -> anyhow::Result<()> {
        self.argvs.lock().unwrap().push(argv(RouteOp::Delete, e));
        self.table.lock().unwrap().retain(|x| x != e);
        Ok(())
    }
    fn list_managed(&self) -> anyhow::Result<Vec<RouteEntry>> {
        Ok(self.table())
    }
}

/// 造一条 DNS 查询的**线格式**字节。测试喂给 `FakeDns::respond` 的是它。
fn query_bytes(name: &str, qtype: RecordType) -> Vec<u8> {
    let mut m = Message::new(0x2b1c, MessageType::Query, OpCode::Query);
    m.add_query(Query::query(Name::from_ascii(name).unwrap(), qtype));
    m.metadata.recursion_desired = true;
    m.to_vec().unwrap()
}

/// 造一条上游应答的线格式字节。
fn upstream_a(name: &str, ips: &[&str]) -> Vec<u8> {
    let mut m = Message::response(0x2b1c, OpCode::Query);
    let n = Name::from_ascii(name).unwrap();
    m.add_query(Query::query(n.clone(), RecordType::A));
    for s in ips {
        m.add_answer(Record::from_rdata(
            n.clone(),
            60,
            RData::A(A(s.parse().unwrap())),
        ));
    }
    m.to_vec().unwrap()
}

/// 从应答字节里取出全部 A 记录地址。
fn a_records(resp: &[u8]) -> Vec<Ipv4Addr> {
    Message::from_vec(resp)
        .unwrap()
        .answers
        .iter()
        .filter_map(|r| match r.data {
            RData::A(A(v)) => Some(v),
            _ => None,
        })
        .collect()
}

// ───────────────────────────────────────────────────────────────────────────
// 一、环路的 IP 侧：转发器的出网连接
// ───────────────────────────────────────────────────────────────────────────

/// **本文件的主命题。** 转发器连向真实服务器的那条连接，一旦被 TUN 捕获，
/// 必须判成 `BypassLeak` 而**绝不**是任何可送进路由层的东西。
///
/// 送进路由层就会命中 `MATCH` 走代理 → 出站 → 回 WebView → 回转发器，
/// 环路当场闭合。
#[test]
fn the_forwarders_upstream_connection_never_becomes_a_routable_target() {
    let bypass = BypassSet::new();
    bypass.insert("jp", vec![ip(SERVER_IP)]);
    let t = TunInbound::new(Arc::new(FakeIpPool::new(vec![])), bypass);

    let dst = sa(&format!("{SERVER_IP}:443"));
    match t.classify(dst) {
        TunTarget::BypassLeak(a) => assert_eq!(a, dst),
        other => panic!("服务器 IP 被判成 {other:?} —— 这是环路的第一步"),
    }
}

/// **反例：没有 bypass 时环路怎么闭合的。**
///
/// 关键不在「结果等于 `Ip`」（那是 `inbound.rs` 已经钉过的），而在
/// **它与一个普通目标完全无法区分**：`is_anomaly()` 为假，于是调用方
/// 没有任何理由停下来看一眼。环路因此是**静默**的 —— 这正是必须动态
/// 维护名单、而不能指望下游发现异常的原因。
#[test]
fn without_bypass_the_server_ip_is_indistinguishable_from_ordinary_traffic() {
    let t = TunInbound::new(Arc::new(FakeIpPool::new(vec![])), BypassSet::new());
    let dst = sa(&format!("{SERVER_IP}:443"));
    let got = t.classify(dst);

    assert_eq!(got, TunTarget::Ip(dst));
    assert!(
        !got.is_anomaly(),
        "环路的致命之处是没有任何一层会报警 —— 若这条断言变红，说明有人给它加了警报，\
         那是好事，但请连同本测试一起改"
    );
    // 对照：有 bypass 时同一个地址是**可报警**的。
    let bypass = BypassSet::new();
    bypass.insert("jp", vec![ip(SERVER_IP)]);
    let guarded = TunInbound::new(Arc::new(FakeIpPool::new(vec![])), bypass);
    assert!(guarded.classify(dst).is_anomaly());
}

/// 多个出站各自的服务器 IP 都要绕行，一个都不能漏 —— 漏掉的那个单独进环路，
/// 表现是「某个节点一用就卡死，别的节点好好的」。
#[test]
fn every_outbound_gets_its_own_bypass_and_none_is_missed() {
    let bypass = BypassSet::new();
    let ips = ["203.0.113.7", "198.51.100.9", "192.0.2.5"];
    for (i, s) in ips.iter().enumerate() {
        bypass.insert(&format!("out{i}"), vec![ip(s)]);
    }
    let t = TunInbound::new(Arc::new(FakeIpPool::new(vec![])), bypass.clone());
    for s in &ips {
        assert!(
            t.classify(sa(&format!("{s}:443"))).is_anomaly(),
            "{s} 未被绕行 —— 该出站会单独进环路"
        );
    }
    assert_eq!(bypass.snapshot().len(), 3);
}

/// 服务器换 IP（DNS 轮转 / 迁移）后重解析：**名单与路由必须一起动**。
///
/// 这条走的是 `BypassSet` → `RouteDelta` → `TunRoutes::sync_bypass` 的完整
/// 链路，断言的是最终落到 `route`(8) 上的参数：新 IP 有 add，旧 IP 有
/// delete，且两者的网关都是**物理网关**。
#[test]
fn re_resolution_moves_both_the_bypass_and_the_route() {
    let backend = Arc::new(RecordingBackend::default());
    let routes = TunRoutes::new(backend.clone(), TUN_GW, PHYS_GW);
    let bypass = BypassSet::new();

    bypass.insert("jp", vec![ip(SERVER_IP)]);
    routes.sync_bypass(&bypass.snapshot()).unwrap();

    // 服务器迁走了。
    bypass.insert("jp", vec![ip("198.51.100.9")]);
    routes.sync_bypass(&bypass.snapshot()).unwrap();

    let t = TunInbound::new(Arc::new(FakeIpPool::new(vec![])), bypass);
    assert!(t.classify(sa("198.51.100.9:443")).is_anomaly(), "新 IP 必须绕行");
    assert!(
        !t.classify(sa(&format!("{SERVER_IP}:443"))).is_anomaly(),
        "旧 IP 不该再绕行，否则它永远逃过分流规则"
    );

    let log = backend.argv_log();
    assert!(
        log.contains(&vec![
            "-n".into(),
            "-q".into(),
            "add".into(),
            "-host".into(),
            "198.51.100.9".into(),
            PHYS_GW.into()
        ]),
        "新 IP 的 bypass 路由没写：{log:?}"
    );
    assert!(
        log.contains(&vec![
            "-n".into(),
            "-q".into(),
            "delete".into(),
            "-host".into(),
            SERVER_IP.into(),
            PHYS_GW.into()
        ]),
        "旧 IP 的 bypass 路由没删：{log:?}"
    );
    assert_eq!(
        routes.bypass_snapshot(),
        vec![ip("198.51.100.9")],
        "路由记账要与名单一致"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// 二、环路的 DNS 侧：服务器域名绝不能拿到假 IP
// ───────────────────────────────────────────────────────────────────────────

/// **环路的另一半。** 服务器域名一旦拿到 `198.18.x.x`，`resolve_upstream`
/// 就会把转发器指向虚空 —— 而且是静默的：包进 TUN、反查落空、连接挂到超时。
///
/// 这条走的是**真实 DNS 线格式**：造一条 A 查询字节喂进 `FakeDns::respond`，
/// 断言它返回 `None`（= 本层不答，交给上游做真实解析）。
#[test]
fn the_server_domain_is_answered_by_upstream_never_by_the_fake_pool() {
    let pool = Arc::new(
        FakeIpPool::new(vec![]).with_server_domains(&[SERVER_DOMAIN.to_string()]),
    );
    let dns = FakeDns::new(pool.clone());

    // 判决层与线格式层都要过 —— 只验 decide 会漏掉 respond 里的分支。
    assert_eq!(
        dns.decide(SERVER_DOMAIN, RecordType::A),
        Answer::Upstream,
        "服务器域名必须走真实解析"
    );
    assert_eq!(
        dns.respond(&query_bytes(SERVER_DOMAIN, RecordType::A)).unwrap(),
        None,
        "respond 必须交给上游，而不是就地编一个假 IP"
    );

    // 对照：其余域名照常拿假 IP，证明 filter 没有把整个池关掉。
    let resp = dns
        .respond(&query_bytes("www.example.com", RecordType::A))
        .unwrap()
        .expect("普通域名应就地应答");
    let got = a_records(&resp);
    assert_eq!(got.len(), 1);
    assert!(FakeIpPool::in_range(got[0]), "普通域名应得 fake-ip，实得 {got:?}");
}

/// 服务器域名的**子域**也不能拿假 IP。
///
/// 服务端多端口条带用的是同一个域名，但配置里若写了裸后缀，`api.srv.…`
/// 这类子域仍会被解析。漏掉子域等于给环路留一扇小门。
#[test]
fn subdomains_of_the_server_domain_are_filtered_too() {
    let pool = Arc::new(
        FakeIpPool::new(vec![]).with_server_domains(&[SERVER_DOMAIN.to_string()]),
    );
    let dns = FakeDns::new(pool);
    assert_eq!(
        dns.decide(&format!("api.{SERVER_DOMAIN}"), RecordType::A),
        Answer::Upstream
    );
    // 但**不能**误伤同后缀的无关域名。
    assert!(matches!(
        dns.decide("notsrv.example.com", RecordType::A),
        Answer::Fake(_)
    ));
}

/// **上游把服务器域名解析成段内地址时，必须响亮地失败。**
///
/// 病态但真实存在：开发机实测过链路层 DNS 劫持把 `example.com` 答成
/// `198.18.0.207`。若这种应答被放行，`resolve_upstream` 拿到的就是一个
/// 段内地址 —— 与「服务器域名拿了假 IP」后果完全一样。
///
/// `screen_upstream` 的判据是**段归属**，因此这条应答被整条判死，调用方
/// 回 SERVFAIL；启动编排随后因为「解析不到地址」拒绝拉起 TUN（见下面的
/// `startup` 组合测试）。宁可打不开，不要静默进环路。
#[test]
fn a_hijacked_answer_for_the_server_domain_is_killed_not_forwarded() {
    let resp = upstream_a(SERVER_DOMAIN, &["198.18.0.207"]);
    match FakeDns::screen_upstream(&resp).unwrap() {
        Screen::AllPoisoned { removed } => {
            assert_eq!(removed, vec!["198.18.0.207".parse::<Ipv4Addr>().unwrap()])
        }
        other => panic!("段内地址必须被判污染，实得 {other:?}"),
    }
    // 对照：真实 IP 原样放行，否则每次解析服务器域名都会失败。
    assert_eq!(
        FakeDns::screen_upstream(&upstream_a(SERVER_DOMAIN, &[SERVER_IP])).unwrap(),
        Screen::Clean
    );
}

// ───────────────────────────────────────────────────────────────────────────
// 三、fake-ip 的往返：DNS 发出去的地址，TUN 必须认得回来
// ───────────────────────────────────────────────────────────────────────────

/// **fake-ip 存在的全部理由，端到端跑一遍。**
///
/// DNS 侧发出的假 IP 从线格式里读出来，原样当作 TUN 捕获到的目的地址喂进
/// `classify()`，必须换回**同一个域名**。这条链路断在任何一处，域名规则
/// （GEOSITE / DOMAIN-SUFFIX）就全部失效 —— 而表现只是「规则莫名其妙不命中」。
#[test]
fn a_fake_ip_handed_out_by_dns_reverses_back_to_the_same_domain() {
    let pool = Arc::new(
        FakeIpPool::new(vec![]).with_server_domains(&[SERVER_DOMAIN.to_string()]),
    );
    let dns = FakeDns::new(pool.clone());
    // TUN 入站与 DNS **共用同一个池**，见下一条测试。
    let t = TunInbound::new(pool, BypassSet::new());

    let resp = dns
        .respond(&query_bytes("video.example.com", RecordType::A))
        .unwrap()
        .expect("应就地应答");
    let fake = a_records(&resp)[0];

    assert_eq!(
        t.classify(SocketAddr::new(IpAddr::V4(fake), 443)),
        TunTarget::Domain("video.example.com".into(), 443),
        "反查换不回域名，域名规则就全废了"
    );
}

/// **两个池 = 反查永远落空。** 这是接线时最容易犯的错。
///
/// DNS 一个池、TUN 入站另一个池，各自的单测都是绿的；合起来时 DNS 发出去的
/// 地址在入站那边查不到，每一条连接都变成 `StaleFakeIp` 被拒 —— 表现是
/// 「TUN 一开什么都打不开」，而两边日志都显示自己工作正常。
#[test]
fn two_separate_pools_break_every_reverse_lookup() {
    let dns_pool = Arc::new(FakeIpPool::new(vec![]));
    let dns = FakeDns::new(dns_pool);
    // 故意另建一个 —— 模拟「编排层 new 了两次」。
    let inbound_pool = Arc::new(FakeIpPool::new(vec![]));
    let t = TunInbound::new(inbound_pool, BypassSet::new());

    let resp = dns
        .respond(&query_bytes("video.example.com", RecordType::A))
        .unwrap()
        .unwrap();
    let fake = a_records(&resp)[0];

    let dst = SocketAddr::new(IpAddr::V4(fake), 443);
    assert_eq!(
        t.classify(dst),
        TunTarget::StaleFakeIp(dst),
        "池不共享时每一条连接都会被拒 —— 接线时必须传同一个 Arc"
    );
}

/// bypass **压过**反查：那个地址即使是有效的 fake-ip 映射也一样。
///
/// 病态但可能：服务器部署在段内（内网基准环境），或者 fake-ip 恰好撞上。
/// 顺序一旦写反，结果是 `Domain` —— 而 `Domain` 会被送进路由层，环路成立。
#[test]
fn bypass_outranks_a_successful_reverse_lookup() {
    let pool = Arc::new(FakeIpPool::new(vec![]));
    let fake = pool.allocate("some-site.example").unwrap();
    let bypass = BypassSet::new();
    bypass.insert("lab", vec![IpAddr::V4(fake)]);
    let t = TunInbound::new(pool.clone(), bypass);

    let dst = SocketAddr::new(IpAddr::V4(fake), 443);
    assert_eq!(t.classify(dst), TunTarget::BypassLeak(dst));
    // 池里那条映射还在 —— 判定不是靠「查不到」蒙对的。
    assert_eq!(pool.lookup(fake).as_deref(), Some("some-site.example"));
}

// ───────────────────────────────────────────────────────────────────────────
// 四、启动编排：TUN 拉起的那一刻，两道防线必须都已就位
// ───────────────────────────────────────────────────────────────────────────

/// 把真实的 `BypassSet` 与真实的 `TunRoutes` 装进 `TunStage`，
/// 在 `bring_up_tun()` 里当场检查防线在不在。
struct WiredStage {
    bypass: BypassSet,
    routes: TunRoutes,
    backend: Arc<RecordingBackend>,
    /// TUN 拉起时观察到的状态。`None` 表示还没拉起。
    at_tun_up: Mutex<Option<DefenseSnapshot>>,
}

/// TUN 拉起瞬间的防线快照。
#[derive(Debug, Clone, PartialEq, Eq)]
struct DefenseSnapshot {
    bypass_has_server_ip: bool,
    route_written_for_server_ip: bool,
    route_gateway: Option<String>,
}

impl WiredStage {
    fn new() -> Self {
        let backend = Arc::new(RecordingBackend::default());
        Self {
            bypass: BypassSet::new(),
            routes: TunRoutes::new(backend.clone(), TUN_GW, PHYS_GW),
            backend,
            at_tun_up: Mutex::new(None),
        }
    }
}

impl TunStage for WiredStage {
    fn fakeip_range_owner(&self) -> anyhow::Result<FakeIpRangeOwner> {
        Ok(FakeIpRangeOwner::Unclaimed)
    }

    fn resolve_upstream(&self, _host: &str) -> anyhow::Result<Vec<IpAddr>> {
        Ok(vec![ip(SERVER_IP)])
    }

    fn write_bypass_routes(&self, ips: &[IpAddr]) -> anyhow::Result<()> {
        // 真实链路：名单先收，路由跟着名单走。
        self.bypass.insert("jp", ips.to_vec());
        self.routes.sync_bypass(&self.bypass.snapshot())
    }

    fn start_forwarder(&self, _ips: &[IpAddr]) -> anyhow::Result<()> {
        Ok(())
    }

    fn write_hosts(&self, _host: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn bring_up_tun(&self) -> anyhow::Result<()> {
        // TUN 一拉起，转发器的下一条连接就会被捕获。此刻防线必须已经在。
        let server = ip(SERVER_IP);
        let entry = self
            .backend
            .table()
            .into_iter()
            .find(|e| e.dest == SERVER_IP);
        *self.at_tun_up.lock().unwrap() = Some(DefenseSnapshot {
            bypass_has_server_ip: self.bypass.contains(&server),
            route_written_for_server_ip: entry.is_some(),
            route_gateway: entry.map(|e| e.gateway),
        });
        Ok(())
    }
}

/// **陷阱 1 与陷阱 2 的交汇点。**
///
/// 单看顺序（`startup.rs` 的时间线测试）证明不了防线真的建立了 —— 顺序对而
/// 名单空、或者名单有而路由没写，环路照样成立。这条把真组件装进编排里，
/// 在 TUN 拉起的**那一瞬间**同时检查三件事：名单收了、路由写了、网关是
/// 物理网关。
#[test]
fn both_defenses_are_already_in_place_the_moment_tun_comes_up() {
    let stage = WiredStage::new();
    let timeline = Timeline::default();
    let ips = bring_up(&stage, SERVER_DOMAIN, &timeline).unwrap();

    assert_eq!(ips, vec![ip(SERVER_IP)]);
    assert_eq!(
        timeline.steps(),
        vec![
            Step::PrecheckFakeIpRange,
            Step::ResolveUpstream,
            Step::WriteBypassRoutes,
            Step::StartForwarder,
            Step::WriteHosts,
            Step::BringUpTun,
        ]
    );
    assert_eq!(
        stage.at_tun_up.lock().unwrap().clone(),
        Some(DefenseSnapshot {
            bypass_has_server_ip: true,
            route_written_for_server_ip: true,
            // 指向 TUN 网关就等于让 bypass 路由绕回 TUN 自己 —— 防线看着写了，
            // 实际一点用没有，且事后完全看不出来（路由都 add 成功了）。
            route_gateway: Some(PHYS_GW.to_string()),
        }),
        "TUN 拉起时防线不完整 —— 转发器的下一条连接就进环路"
    );
}

/// **解析不出地址时拒绝启动。** 空 bypass 拉起 TUN 必成环路。
///
/// 这一条是上面那条「上游应答被判全污染」的下游：解析失败 → 编排中止 →
/// TUN 根本没拉起来。整条链路因此是「宁可打不开，不要静默进环路」。
#[test]
fn an_empty_resolution_refuses_to_bring_up_tun_at_all() {
    struct NoAddress;
    impl TunStage for NoAddress {
        fn fakeip_range_owner(&self) -> anyhow::Result<FakeIpRangeOwner> {
            Ok(FakeIpRangeOwner::Unclaimed)
        }
        fn resolve_upstream(&self, _: &str) -> anyhow::Result<Vec<IpAddr>> {
            Ok(vec![])
        }
        fn write_bypass_routes(&self, _: &[IpAddr]) -> anyhow::Result<()> {
            panic!("空解析结果绝不该走到写路由");
        }
        fn start_forwarder(&self, _: &[IpAddr]) -> anyhow::Result<()> {
            unreachable!()
        }
        fn write_hosts(&self, _: &str) -> anyhow::Result<()> {
            unreachable!()
        }
        fn bring_up_tun(&self) -> anyhow::Result<()> {
            panic!("没有 bypass 就拉起 TUN = 环路");
        }
    }
    let timeline = Timeline::default();
    let e = bring_up(&NoAddress, SERVER_DOMAIN, &timeline).unwrap_err();
    assert!(e.to_string().contains("环路"), "错误要点明后果：{e}");
    assert_eq!(timeline.steps(), vec![Step::PrecheckFakeIpRange, Step::ResolveUpstream]);
}

// ───────────────────────────────────────────────────────────────────────────
// 五、路由表形态：环路防线落到 `route`(8) 上的样子
// ───────────────────────────────────────────────────────────────────────────

/// 完整 `apply()` 之后路由表该长什么样。
///
/// **结构断言**：命令没有真的执行（`route` 要 root），断言的是构造出来的
/// 参数向量。真机形态见手工验证清单 M3，**尚未验证**。
#[test]
fn the_applied_route_set_covers_everything_yet_leaves_default_alone() {
    let backend = Arc::new(RecordingBackend::default());
    let routes = TunRoutes::new(backend.clone(), TUN_GW, PHYS_GW);
    routes.sync_bypass(&[ip(SERVER_IP)]).unwrap();
    routes.apply().unwrap();

    let table = backend.table();
    let dests: Vec<&str> = table.iter().map(|e| e.dest.as_str()).collect();

    // 两条 /1 合起来盖住整个 IPv4 空间，但都指向 TUN。
    assert!(dests.contains(&"0.0.0.0/1"), "{dests:?}");
    assert!(dests.contains(&"128.0.0.0/1"), "{dests:?}");
    for e in table.iter().filter(|e| e.dest.ends_with("/1")) {
        assert_eq!(e.gateway, TUN_GW);
    }

    // **绝不动 `default`**：改它会冲掉原网关记录，崩溃后就恢复不了了。
    // 这是 M7（崩溃后路由可恢复）在无 root 环境下能验到的那一半。
    assert!(
        !dests.contains(&"default"),
        "写 default 会冲掉原网关记录，崩溃后无法恢复：{dests:?}"
    );

    // 服务器 IP 一条主机路由，指向**物理**网关 —— 环路防线本身。
    let srv = table.iter().find(|e| e.dest == SERVER_IP).expect("缺 bypass 路由");
    assert_eq!(srv.gateway, PHYS_GW);
    // 主机路由（无前缀）比 /1 更具体，因此内核会优先命中它。
    // **结构断言**：最长前缀匹配没有真的跑，这里只能断言我们确实写的是 -host。
    assert!(
        argv(RouteOp::Add, srv).contains(&"-host".to_string()),
        "bypass 必须是 -host；写成 -net 就可能被 route 的启发式推断改到别的地址上"
    );
}

/// **TUN 网关与物理网关相同 ⇒ 整项托管失败，一条路由都不写。**
///
/// 这是最坏的一类故障：bypass 路由指回 TUN 自己，防线在配置层面就失效了，
/// 而 `route add` 每一条都会成功 —— 事后从路由表上完全看不出问题。
#[test]
fn a_bypass_that_would_point_back_into_tun_refuses_to_apply() {
    let backend = Arc::new(RecordingBackend::default());
    let routes = TunRoutes::new(backend.clone(), TUN_GW, TUN_GW);
    routes.sync_bypass(&[ip(SERVER_IP)]).unwrap();

    let e = routes.apply().unwrap_err();
    assert!(e.to_string().contains("环路"), "错误要点明后果：{e}");
    assert!(
        !backend.table().iter().any(|x| x.dest.ends_with("/1")),
        "拒绝之后不该留下任何默认路由"
    );
}

/// 共享同一台服务器的两个出站，摘掉一个时那条 bypass 路由**必须留着**。
///
/// 盲目跟着「谁变了删谁」走，剩下那个出站当场进环路。这条走的是
/// `BypassSet::remove` → `snapshot` → `sync_bypass` 的真实链路，
/// 断言最终没有发出针对该 IP 的 delete。
#[test]
fn dropping_one_of_two_outbounds_on_the_same_host_keeps_the_route() {
    let backend = Arc::new(RecordingBackend::default());
    let routes = TunRoutes::new(backend.clone(), TUN_GW, PHYS_GW);
    let bypass = BypassSet::new();

    bypass.insert("jp", vec![ip(SERVER_IP)]);
    bypass.insert("sg", vec![ip(SERVER_IP)]);
    routes.sync_bypass(&bypass.snapshot()).unwrap();

    bypass.remove("jp");
    routes.sync_bypass(&bypass.snapshot()).unwrap();

    assert!(
        backend.table().iter().any(|e| e.dest == SERVER_IP),
        "sg 还在线，这条 bypass 路由删掉它就进环路"
    );
    assert!(
        !backend
            .argv_log()
            .iter()
            .any(|a| a.contains(&"delete".to_string()) && a.contains(&SERVER_IP.to_string())),
        "不该对共享 IP 发出 delete：{:?}",
        backend.argv_log()
    );

    // 最后一个持有者也走了，这时才该删。
    bypass.remove("sg");
    routes.sync_bypass(&bypass.snapshot()).unwrap();
    assert!(!backend.table().iter().any(|e| e.dest == SERVER_IP));
    let t = TunInbound::new(Arc::new(FakeIpPool::new(vec![])), bypass);
    assert!(
        !t.classify(sa(&format!("{SERVER_IP}:443"))).is_anomaly(),
        "没有出站持有时它就是个普通目标了"
    );
}

/// 崩溃残留必须能被清掉，且 `clear_stale` 幂等。
///
/// 残留的 `0/1` 指向一个已经消失的 utun，会把半个 IPv4 空间黑洞掉，而
/// 用户在任何界面上都看不出这跟本程序有关。**结构断言**：真机上的残留
/// 识别依赖 `netstat` 输出格式，见手工验证清单 M7，**尚未验证**。
#[test]
fn stale_routes_from_a_crash_are_cleared_before_anything_is_written() {
    let backend = Arc::new(RecordingBackend::default());
    // 上一次运行崩溃时留下的残骸。
    backend
        .add(&RouteEntry {
            dest: "0.0.0.0/1".into(),
            gateway: TUN_GW.into(),
        })
        .unwrap();

    let routes = TunRoutes::new(backend.clone(), TUN_GW, PHYS_GW);
    routes.clear_stale().unwrap();
    assert!(backend.table().is_empty(), "残留没清干净");
    routes.clear_stale().unwrap(); // 幂等：没得清也不能报错

    routes.apply().unwrap();
    assert_eq!(
        backend.table().iter().filter(|e| e.dest == "0.0.0.0/1").count(),
        1,
        "清完再写才不会攒出两份 —— 这正是 M7 第二段要在真机上数的那个数"
    );
}
