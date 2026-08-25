//! fake-ip DNS 判决（设计文档 §7.1 第二层的**纯逻辑**部分）。
//!
//! 只做一件事：A 查询 → 分配 fake-ip 并**立即**应答（零网络往返）。
//! 真实解析推迟到出站层 —— 服务端离目标更近，且不受本地污染（§6.3）。
//!
//! 三类查询不给 fake-ip：
//!   1. `fake-ip-filter` 命中（含自动并入的出站服务器域名，§7.2 纪律①）
//!   2. AAAA —— 返回空应答（NOERROR + 0 answer），逼客户端退回 IPv4。
//!      给 IPv6 也发假地址意味着要再开一个 fake 段并让 netstack 双栈，
//!      收益为零
//!   3. 非 A/AAAA（MX / TXT / SRV…）—— 交给真实解析器
//!
//! # 为什么判决与 IO 分开
//!
//! 本模块**不碰网络**，因此每一条判决都能穷举单测；IO 在 `dns_server`。
//! 这与 `startup` / `managed` 把特权操作藏在 trait 后面是同一个理由。
//!
//! # `198.18.0.0/15` 是双向的边界
//!
//! 向下（我们发出去的应答）：段内地址由本池独占分配。
//! 向上（上游发回来的应答）：**任何**落在段内的 A 记录都是污染 ——
//! 见 [`FakeDns::screen_upstream`]。2026-08-25 在开发机上实测本机 DNS 被
//! 链路层劫持，`example.com` 得到 `198.18.0.207`，正落在本段内。这不是
//! 假想威胁。
//!
//! **hickory 0.26 API 提示**：`Message` 的字段是公开的，0.24 时代的
//! `set_authoritative()` / `id()` 之类的存取器已移除，直接读写
//! `msg.metadata.*` / `msg.queries` / `msg.answers`。

use std::net::Ipv4Addr;
use std::sync::Arc;

use hickory_proto::op::{Message, MessageType, OpCode, ResponseCode};
use hickory_proto::rr::rdata::A;
use hickory_proto::rr::{Name, RData, Record, RecordType};

use crate::fakeip::{AllocError, FakeIpPool};

/// fake-ip 记录的 TTL。取 1s：映射本就在本地，客户端缓存久了只会让
/// 我们无法及时回收；1s 也足以避免同一次页面加载重复查询。
const FAKE_TTL: u32 = 1;

/// 一次查询的处置结果。把「决定」与「发包」分开，判决逻辑因此可穷举单测。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// 直接以 fake-ip 应答。
    Fake(Ipv4Addr),
    /// 空应答（NOERROR，0 条记录）。AAAA 走这里。
    Empty,
    /// 交给上游真实解析器（filter 命中的 A 查询、以及其余记录类型）。
    Upstream,
    /// 分配失败且**不是** filter 命中 —— 这是故障，必须以 SERVFAIL 显式失败。
    ///
    /// 为什么不退回 `Upstream`：TUN 模式下退回上游意味着客户端拿到**真实 IP**，
    /// 该 IP 被 TUN 捕获后 `lookup()` 必然落空，域名就此丢失，连接以一种
    /// 完全静默的方式走错路。宁可让这一次查询响亮地失败。
    Failed(AllocError),
}

/// 上游应答的筛查结论。
///
/// 前提：TUN 模式下 `198.18.0.0/15` 由本池**独占**（该段是 RFC 2544 基准测试
/// 保留段，公网上不存在真实主机，且我们把整段路由进了 TUN 设备）。因此上游
/// 应答里出现段内地址只有两种可能，两种都不能放行：
///   - 链路层/解析器劫持（开发机实测即是此种）
///   - 上游把我们自己发出的 fake-ip 回灌了回来
///
/// 放行的后果是确定性的：客户端拿着段内地址去连，包进 TUN，反查落空，
/// 没有域名可供判决 —— 连接必死，且日志上看不出所以然。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    /// 干净，原样转给客户端。
    Clean,
    /// 部分 A 记录中毒，已剔除；`resp` 是重编码后的报文。
    Cleaned {
        resp: Vec<u8>,
        removed: Vec<Ipv4Addr>,
    },
    /// 应答里**所有** A 记录都中毒，没有任何可用地址剩下。
    /// 调用方必须回 SERVFAIL：客户端会去问下一个解析器，而不是拿着
    /// 一个注定连不通的地址空等。
    AllPoisoned { removed: Vec<Ipv4Addr> },
}

pub struct FakeDns {
    pool: Arc<FakeIpPool>,
}

impl FakeDns {
    pub fn new(pool: Arc<FakeIpPool>) -> Self {
        Self { pool }
    }

    /// 借出底层的池。TUN 入站层要靠它反查域名。
    pub fn pool(&self) -> &Arc<FakeIpPool> {
        &self.pool
    }

    /// 判决单条查询。纯函数，不碰网络。
    pub fn decide(&self, name: &str, qtype: RecordType) -> Answer {
        match qtype {
            RecordType::A => match self.pool.allocate(name) {
                Ok(ip) => Answer::Fake(ip),
                // filter 命中：必须走真实解析。出站服务器域名走的正是这条，
                // 拿到假 IP 的话转发器会连向虚空（§7.2 纪律①）。这是**正常**路径。
                Err(AllocError::Filtered) => Answer::Upstream,
                // 池耗尽是**故障**，不是正常路径。两者共用一个返回值的话，
                // 13 万个映射全在用时会静默退化成「所有域名都走上游」。
                Err(e) => Answer::Failed(e),
            },
            // fake-ip 只发 IPv4。空应答让客户端退回 A 查询。
            RecordType::AAAA => Answer::Empty,
            _ => Answer::Upstream,
        }
    }

    /// 把请求报文变成应答报文。返回 `Ok(None)` 表示本层不应答，
    /// 调用方转发给上游解析器。
    ///
    /// 报文解析失败**不静默丢弃**：返回错误让调用方记日志，否则客户端
    /// 要等到超时，表现为「莫名其妙全网卡顿」而不是一个可见的故障。
    pub fn respond(&self, req: &[u8]) -> Result<Option<Vec<u8>>, DnsError> {
        let msg = Message::from_vec(req).map_err(|_| DnsError::Malformed)?;

        // 收到的若本身是一条**应答**，绝不能再回一条应答 —— 两个 UDP 服务
        // 互指时会打成无限包风暴。报错让调用方记日志，但不发任何包。
        if msg.metadata.message_type == MessageType::Response {
            return Err(DnsError::NotAQuery);
        }

        // UPDATE / NOTIFY 等非 Query 操作码：`queries` 字段在 UPDATE 报文里
        // 装的是 zone 而不是查询，当成域名去分配 fake-ip 是彻头彻尾的误判。
        if msg.metadata.op_code != OpCode::Query {
            return Ok(Some(encode(&Message::error_msg(
                msg.metadata.id,
                msg.metadata.op_code,
                ResponseCode::NotImp,
            ))?));
        }

        let Some(q) = msg.queries.first() else {
            let err = Message::error_msg(msg.metadata.id, OpCode::Query, ResponseCode::FormErr);
            return Ok(Some(encode(&err)?));
        };
        let name = q.name().to_ascii();
        match self.decide(&name, q.query_type()) {
            Answer::Upstream => Ok(None),
            Answer::Empty => Ok(Some(self.build(&msg, None)?)),
            Answer::Fake(ip) => Ok(Some(self.build(&msg, Some(ip))?)),
            Answer::Failed(e) => {
                // 不静默：池耗尽必须能在日志里看见，否则表现为「网突然全挂」。
                tracing::error!("为 {name} 分配 fake-ip 失败，回 SERVFAIL：{e}");
                Ok(Some(servfail_for(&msg)?))
            }
        }
    }

    /// 筛查上游应答，拦下落在 fake-ip 段内的 A 记录。
    ///
    /// 纯函数，不需要池的状态：这一侧的判据是**段归属**（`in_segment`，整个
    /// `198.18.0.0/15`）而非池记录、也不是可分配范围（`in_range`）。
    /// 与 `fakeip::in_range` 文档里那条「段内 ≠ 出自本池」并不矛盾 ——
    /// 两个方向的问题不同：
    ///   - 入站方向（TUN 拿到目的 IP）问的是「这个地址是不是我发出去的」，
    ///     只有 `lookup()` 能回答
    ///   - 上游方向（这里）问的是「上游有没有资格给出这个地址」，答案永远是
    ///     没有 —— 整段都归我们，上游给出段内地址一定是污染
    ///
    /// 为什么必须是整段而不是可分配范围：整段都被路由进 TUN 设备
    /// （`device.rs` 用 `/15` 配接口）。上游若回一个 `198.18.0.2` 这种落在
    /// 段内却在池外的地址，客户端连出去照样被捕获、照样反查落空 —— 放行它
    /// 等于放行一个静默黑洞。
    pub fn screen_upstream(resp: &[u8]) -> Result<Screen, DnsError> {
        let mut msg = Message::from_vec(resp).map_err(|_| DnsError::Malformed)?;

        let mut removed = Vec::new();
        let mut kept_a = 0usize;
        msg.answers.retain(|r| match r.data {
            RData::A(A(ip)) if FakeIpPool::in_segment(ip) => {
                removed.push(ip);
                false
            }
            RData::A(_) => {
                kept_a += 1;
                true
            }
            _ => true,
        });

        if removed.is_empty() {
            return Ok(Screen::Clean);
        }
        if kept_a == 0 {
            return Ok(Screen::AllPoisoned { removed });
        }
        Ok(Screen::Cleaned {
            resp: encode(&msg)?,
            removed,
        })
    }

    fn build(&self, req: &Message, ip: Option<Ipv4Addr>) -> Result<Vec<u8>, DnsError> {
        let mut resp = Message::response(req.metadata.id, OpCode::Query);
        resp.add_queries(req.queries.iter().cloned());
        // 我们就是这些名字的权威：映射由本进程分配，不存在更上级来源。
        resp.metadata.authoritative = true;
        resp.metadata.recursion_desired = req.metadata.recursion_desired;
        resp.metadata.recursion_available = true;
        if let (Some(ip), Some(q)) = (ip, req.queries.first()) {
            let name: Name = q.name().clone();
            resp.add_answer(Record::from_rdata(name, FAKE_TTL, RData::A(A(ip))));
        }
        encode(&resp)
    }
}

/// 为一条请求构造 SERVFAIL 应答（回带 id 与问题段，客户端才认得出）。
///
/// 独立成函数是因为三处要用：池耗尽（本模块）、上游超时、上游应答全中毒
/// （后两处在 `dns_server`）。
pub fn servfail_for(req: &Message) -> Result<Vec<u8>, DnsError> {
    let mut resp = Message::error_msg(req.metadata.id, OpCode::Query, ResponseCode::ServFail);
    resp.add_queries(req.queries.iter().cloned());
    resp.metadata.recursion_desired = req.metadata.recursion_desired;
    resp.metadata.recursion_available = true;
    encode(&resp)
}

/// 从原始请求字节构造 SERVFAIL。请求连解析都失败时返回 `None` ——
/// 连 id 都不知道，发出去的应答客户端也认不出，不如不发。
pub fn servfail_from_bytes(req: &[u8]) -> Option<Vec<u8>> {
    let msg = Message::from_vec(req).ok()?;
    servfail_for(&msg).ok()
}

/// ponytail: 应答不回带 EDNS0 OPT。**上限**：RFC 6891 §6.1.1 说收到 OPT 的
/// responder「MUST」回带 OPT，不回带等于告诉客户端本服务器不支持 EDNS，
/// 客户端于是按 512 字节上限看待应答 —— 我们的 fake-ip 应答只有几十字节，
/// 够不着；DO 位也随之丢失，但 fake-ip 本就是伪造的，签不了名也没意义。
/// **升级路径**：`build()` 里把 `req.edns` 克隆过去并夹取 payload size。
fn encode(msg: &Message) -> Result<Vec<u8>, DnsError> {
    msg.to_vec().map_err(|_| DnsError::Encode)
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DnsError {
    #[error("DNS 报文无法解析")]
    Malformed,
    #[error("DNS 应答编码失败")]
    Encode,
    #[error("收到的是 DNS 应答而非查询，已丢弃（回应它会打成包风暴）")]
    NotAQuery,
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::op::Query;

    fn dns(filter: Vec<String>) -> FakeDns {
        FakeDns::new(Arc::new(FakeIpPool::new(filter)))
    }

    fn query_bytes(name: &str, qtype: RecordType) -> Vec<u8> {
        let mut m = Message::new(0x1234, MessageType::Query, OpCode::Query);
        m.add_query(Query::query(Name::from_ascii(name).unwrap(), qtype));
        m.metadata.recursion_desired = true;
        m.to_vec().unwrap()
    }

    /// 造一条上游应答：给定域名与若干 A 地址。
    fn upstream_a(name: &str, ips: &[&str]) -> Vec<u8> {
        let mut m = Message::response(0x4321, OpCode::Query);
        let n = Name::from_ascii(name).unwrap();
        m.add_query(Query::query(n.clone(), RecordType::A));
        for ip in ips {
            m.add_answer(Record::from_rdata(
                n.clone(),
                60,
                RData::A(A(ip.parse().unwrap())),
            ));
        }
        m.to_vec().unwrap()
    }

    #[test]
    fn a_query_gets_fake_ip() {
        let d = dns(vec![]);
        match d.decide("example.com.", RecordType::A) {
            Answer::Fake(ip) => assert!(FakeIpPool::in_range(ip)),
            other => panic!("应发 fake-ip，实得 {other:?}"),
        }
    }

    #[test]
    fn filtered_a_query_goes_upstream_not_fake() {
        // 这条测试就是环路防线的 DNS 一侧：出站服务器域名绝不能拿假 IP。
        let d = dns(vec!["srv.example.com".into()]);
        assert_eq!(d.decide("srv.example.com.", RecordType::A), Answer::Upstream);
    }

    #[test]
    fn aaaa_returns_empty_not_fake() {
        let d = dns(vec![]);
        assert_eq!(d.decide("example.com.", RecordType::AAAA), Answer::Empty);
    }

    #[test]
    fn other_record_types_go_upstream() {
        let d = dns(vec![]);
        for t in [
            RecordType::MX,
            RecordType::TXT,
            RecordType::SRV,
            RecordType::CNAME,
        ] {
            assert_eq!(d.decide("example.com.", t), Answer::Upstream, "{t} 应走上游");
        }
    }

    #[test]
    fn response_echoes_id_and_question() {
        let d = dns(vec![]);
        let out = d
            .respond(&query_bytes("example.com.", RecordType::A))
            .unwrap()
            .unwrap();
        let m = Message::from_vec(&out).unwrap();
        assert_eq!(m.metadata.id, 0x1234, "应答必须回带请求 id，否则客户端认不出");
        assert_eq!(m.metadata.message_type, MessageType::Response);
        assert_eq!(m.queries.len(), 1);
        assert_eq!(m.answers.len(), 1);
        assert_eq!(m.metadata.response_code, ResponseCode::NoError);
    }

    #[test]
    fn response_answer_is_in_fake_range_and_reversible() {
        let pool = Arc::new(FakeIpPool::new(vec![]));
        let d = FakeDns::new(pool.clone());
        let out = d
            .respond(&query_bytes("example.com.", RecordType::A))
            .unwrap()
            .unwrap();
        let m = Message::from_vec(&out).unwrap();
        let RData::A(A(ip)) = m.answers[0].data else {
            panic!("应答记录不是 A");
        };
        assert!(FakeIpPool::in_range(ip));
        // 反查回域名 —— TUN 路由判决的全部依据。
        assert_eq!(pool.lookup(ip).as_deref(), Some("example.com"));
    }

    #[test]
    fn aaaa_response_has_zero_answers_and_noerror() {
        // NXDOMAIN 会让客户端认为域名不存在，连 A 查询都不发了。必须是
        // NOERROR + 空答案，语义是「这个名字没有 AAAA」。
        let d = dns(vec![]);
        let out = d
            .respond(&query_bytes("example.com.", RecordType::AAAA))
            .unwrap()
            .unwrap();
        let m = Message::from_vec(&out).unwrap();
        assert_eq!(m.answers.len(), 0);
        assert_eq!(m.metadata.response_code, ResponseCode::NoError);
    }

    #[test]
    fn filtered_query_returns_none_so_caller_forwards() {
        let d = dns(vec!["srv.example.com".into()]);
        assert_eq!(
            d.respond(&query_bytes("srv.example.com.", RecordType::A))
                .unwrap(),
            None
        );
    }

    #[test]
    fn malformed_packet_is_reported_not_swallowed() {
        let d = dns(vec![]);
        assert_eq!(d.respond(&[0xff, 0x00, 0x01]), Err(DnsError::Malformed));
    }

    #[test]
    fn query_without_question_gets_formerr() {
        let d = dns(vec![]);
        let m = Message::new(7, MessageType::Query, OpCode::Query);
        let out = d.respond(&m.to_vec().unwrap()).unwrap().unwrap();
        let back = Message::from_vec(&out).unwrap();
        assert_eq!(back.metadata.response_code, ResponseCode::FormErr);
        assert_eq!(back.metadata.id, 7);
    }

    // ---- 故障与滥用：都必须显式，不得静默 ----

    #[test]
    fn pool_exhaustion_is_not_disguised_as_upstream() {
        // 耗尽退回上游 = 客户端拿真实 IP → TUN 反查落空 → 域名丢失。
        // 必须是一条独立的、可见的失败。
        let pool = Arc::new(FakeIpPool::new(vec![]));
        pool.exhaust_for_test();
        let d = FakeDns::new(pool);
        match d.decide("example.com.", RecordType::A) {
            Answer::Failed(AllocError::Exhausted { capacity }) => {
                assert_eq!(capacity, FakeIpPool::capacity());
            }
            other => panic!("耗尽必须是 Failed，实得 {other:?}"),
        }
    }

    #[test]
    fn exhaustion_answers_servfail_carrying_the_question() {
        let pool = Arc::new(FakeIpPool::new(vec![]));
        pool.exhaust_for_test();
        let d = FakeDns::new(pool);
        let out = d
            .respond(&query_bytes("example.com.", RecordType::A))
            .unwrap()
            .unwrap();
        let m = Message::from_vec(&out).unwrap();
        assert_eq!(m.metadata.response_code, ResponseCode::ServFail);
        assert_eq!(m.metadata.id, 0x1234);
        assert_eq!(m.queries.len(), 1, "SERVFAIL 也要回带问题段");
        assert_eq!(m.answers.len(), 0);
    }

    #[test]
    fn a_response_arriving_at_the_query_port_is_never_answered() {
        // 回应一条应答 = 两个 UDP 服务互指打成包风暴。报错但不发包。
        let d = dns(vec![]);
        let mut m = Message::response(1, OpCode::Query);
        m.add_query(Query::query(
            Name::from_ascii("example.com.").unwrap(),
            RecordType::A,
        ));
        assert_eq!(d.respond(&m.to_vec().unwrap()), Err(DnsError::NotAQuery));
    }

    #[test]
    fn non_query_opcode_gets_notimp_instead_of_a_fake_ip() {
        // UPDATE 报文的 `queries` 装的是 zone；当成域名分配 fake-ip 是误判。
        let d = dns(vec![]);
        let mut m = Message::new(5, MessageType::Query, OpCode::Update);
        m.add_query(Query::query(
            Name::from_ascii("example.com.").unwrap(),
            RecordType::SOA,
        ));
        let out = d.respond(&m.to_vec().unwrap()).unwrap().unwrap();
        let back = Message::from_vec(&out).unwrap();
        assert_eq!(back.metadata.response_code, ResponseCode::NotImp);
        assert_eq!(back.metadata.id, 5);
        assert!(d.pool().is_empty(), "非 Query 操作码不得消耗池地址");
    }

    // ---- 上游应答筛查：本机 DNS 被劫持时的唯一防线 ----

    #[test]
    fn clean_upstream_answer_passes_through_untouched() {
        let r = FakeDns::screen_upstream(&upstream_a("srv.example.com.", &["1.2.3.4"])).unwrap();
        assert_eq!(r, Screen::Clean);
    }

    #[test]
    fn upstream_answer_inside_the_fake_range_is_caught() {
        // 2026-08-25 开发机实测：本机 DNS 被链路层劫持，example.com 得到
        // 198.18.0.207 —— 正落在我们独占的段里。放行它，客户端就会拿着
        // 一个进 TUN 后反查必然落空的地址去连。
        let r = FakeDns::screen_upstream(&upstream_a("example.com.", &["198.18.0.207"])).unwrap();
        assert_eq!(
            r,
            Screen::AllPoisoned {
                removed: vec!["198.18.0.207".parse().unwrap()]
            }
        );
    }

    /// 段内但**池外**的地址同样是污染。
    ///
    /// `198.18.0.2` 落在 `/15` 段里却在池的可分配范围之外（前 4 个地址不
    /// 分配）。判据若用 `in_range`（可分配范围），这条会被放行 —— 而整段
    /// 都路由进 TUN 设备，客户端拿它去连照样被捕获、照样反查落空。
    /// 判据必须是 `in_segment`（段归属）。段末的 `198.19.255.255` 同理。
    #[test]
    fn in_segment_but_out_of_pool_addresses_are_poison_too() {
        for a in ["198.18.0.0", "198.18.0.1", "198.18.0.2", "198.19.255.255"] {
            let r = FakeDns::screen_upstream(&upstream_a("example.com.", &[a])).unwrap();
            assert_eq!(
                r,
                Screen::AllPoisoned {
                    removed: vec![a.parse().unwrap()]
                },
                "{a} 在段内，上游没有资格给出它"
            );
        }
    }

    /// 段外的邻居必须**照常放行**：判据不能宽到误伤真实互联网地址。
    #[test]
    fn addresses_just_outside_the_segment_are_not_screened() {
        for a in ["198.17.255.255", "198.20.0.0"] {
            assert_eq!(
                FakeDns::screen_upstream(&upstream_a("example.com.", &[a])).unwrap(),
                Screen::Clean,
                "{a} 在段外，是真实可达地址"
            );
        }
    }

    #[test]
    fn partially_poisoned_answer_keeps_the_usable_addresses() {
        let raw = upstream_a("example.com.", &["198.18.0.207", "93.184.216.34"]);
        let Screen::Cleaned { resp, removed } = FakeDns::screen_upstream(&raw).unwrap() else {
            panic!("应是 Cleaned");
        };
        assert_eq!(removed, vec!["198.18.0.207".parse::<Ipv4Addr>().unwrap()]);
        let m = Message::from_vec(&resp).unwrap();
        assert_eq!(m.answers.len(), 1, "只剩下那条干净记录");
        let RData::A(A(ip)) = m.answers[0].data else {
            panic!("不是 A")
        };
        assert_eq!(ip.to_string(), "93.184.216.34");
        assert_eq!(m.metadata.id, 0x4321, "重编码不得丢 id");
        assert_eq!(m.queries.len(), 1, "重编码不得丢问题段");
    }

    #[test]
    fn screening_ignores_non_a_records() {
        // MX / TXT 之类不含 IPv4 地址，筛查不该动它们。
        let mut m = Message::response(9, OpCode::Query);
        let n = Name::from_ascii("example.com.").unwrap();
        m.add_query(Query::query(n.clone(), RecordType::TXT));
        m.add_answer(Record::from_rdata(
            n,
            60,
            RData::TXT(hickory_proto::rr::rdata::TXT::new(vec!["hi".into()])),
        ));
        assert_eq!(
            FakeDns::screen_upstream(&m.to_vec().unwrap()).unwrap(),
            Screen::Clean
        );
    }

    #[test]
    fn our_own_fake_ip_bounced_back_by_upstream_is_still_poison() {
        // 上游把我们发出去的假地址回灌回来，同样必须拦。判据是「段归属」，
        // 不是「是不是本池发的」—— 段整个归我们，上游没资格给出段内地址。
        let pool = Arc::new(FakeIpPool::new(vec![]));
        let mine = pool.allocate("mine.test").unwrap();
        let raw = upstream_a("other.test.", &[&mine.to_string()]);
        assert_eq!(
            FakeDns::screen_upstream(&raw).unwrap(),
            Screen::AllPoisoned { removed: vec![mine] }
        );
    }

    #[test]
    fn screening_a_malformed_upstream_answer_is_reported() {
        assert_eq!(
            FakeDns::screen_upstream(&[0x00, 0x01]),
            Err(DnsError::Malformed)
        );
    }

    #[test]
    fn servfail_from_bytes_gives_up_when_the_request_is_unparseable() {
        // 连 id 都读不出来时发应答毫无意义 —— 客户端认不出，只会当成噪声。
        assert!(servfail_from_bytes(&[0xff]).is_none());
        assert!(servfail_from_bytes(&query_bytes("example.com.", RecordType::A)).is_some());
    }
}
