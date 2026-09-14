//! DNS 服务器的监听与转发（设计文档 §7.1 第二层的 IO 部分）。
//!
//! 判决逻辑全在 `fakedns::FakeDns`（纯函数、可穷举单测）。本模块只负责：
//! 收包 → 问判决 → 要么直接回、要么转给上游解析器再回。
//!
//! **端口选择**：监听 `127.0.0.1:53` 需要 root（<1024）。但 TUN 模式本就
//! 要 root，二者同生共死，因此不额外引入权限需求。测试一律用 `:0`。
//!
//! # 为什么不复用 `wsieve-dns::DnsResolver`
//!
//! 两者服务的是**要求相反**的两条路径，共用一个对象只会把其中一条弄坏：
//!
//! | | `wsieve-dns`（判决路径） | 本模块（对外转发） |
//! |---|---|---|
//! | 上游故障时 | 返回空切片，判决继续往下（纪律③） | **必须 SERVFAIL** |
//! | 记录类型 | 只要 A/AAAA 的 IP | MX/TXT/SRV/CNAME… 原样转 |
//! | 报文 | 已解析成 `Vec<IpAddr>` | 要保 id、flags、rcode、附加段 |
//!
//! 第一行是致命的：把「上游超时」翻译成 NOERROR + 0 条答案，等于告诉客户端
//! **这个域名没有 A 记录**，客户端把这条否定结论存进负缓存，接下来几分钟
//! 都不会再问 —— 一次上游抖动变成一段时间的「这个网站打不开」，且日志上
//! 看不出所以然。转发器必须 SERVFAIL，客户端才会转头去问下一个解析器。
//!
//! 第三行同样不可调和：`Resolver::lookup` 交出的是记录集合，rcode（NXDOMAIN
//! 与 NODATA 的区别）、否定缓存要用的 SOA、CNAME 链（判决路径刻意用
//! `preserve_intermediates = false` 丢掉了）全都不在里面。整机 DNS 从这里过，
//! 丢这些保真度是不能接受的。
//!
//! **复用的是那条教训而不是那个对象**：硬超时必须由外层
//! `tokio::time::timeout` 施加。`wsieve-dns` 实测过 `ResolverOpts::timeout`
//! 挡不住卡在建连里的那一轮（设了 500ms，实际 15.03 秒）。本模块的
//! [`UdpUpstream::query`] 把墙钟上限钉死在同一个位置。
//!
//! # 上游走明文 UDP，且它自己会进 TUN
//!
//! `ponytail:` 上游只支持明文 UDP DNS（一个 `SocketAddr`）。
//! **上限**：配置里若写了 DoH（`https://1.1.1.1/dns-query`），本模块用不了 ——
//! 编排层必须挑一个 UDP 上游喂进来，通常是系统解析器。
//! **升级路径**：`wsieve-dns::parse_nameserver` 已经能把配置串解析成
//! hickory 的 `NameServerConfig`；要支持 DoH 就在这里加一个枚举分支，
//! 转发时改走 hickory 的客户端，判决与筛查两层都不用动。
//!
//! 还有一条**留给编排层（Task 9/10）的约束**：TUN 拉起后，本模块发往上游的
//! UDP:53 同样会被 TUN 捕获。上游若是局域网网关（`192.168.x.x`）没问题，
//! 那条路由本就不进 TUN；上游若是公网地址（`8.8.8.8`），它必须像出站服务器 IP
//! 一样进 `BypassSet`，否则 DNS 转发自己就成了陷阱 1 的又一个环。

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use hickory_proto::op::Message;
use tokio::net::UdpSocket;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::fakedns::{servfail_from_bytes, DnsError, FakeDns, Screen};

/// 收发缓冲区大小。
///
/// 经典上限是 512，DNS flag day 2020 建议 1232，但带 EDNS0 的客户端可以
/// 宣称 4096，上游也就可能真的发这么大。缓冲区小于实际报文时 `recv_from`
/// 会**静默截断**，我们转出去的就是一个残缺报文 —— 客户端只会看到解析
/// 失败，看不到原因。取 4096 把这个窗口关上。
const MAX_DNS: usize = 4096;

/// 上游硬超时。§7.3 定的 2s。超时即 SERVFAIL，绝不把查询吊着。
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(2);

/// 同时在飞的上游查询数上限。
///
/// 每条在飞查询占一个临时 UDP 套接字；不设上限的话，本机上一个循环发查询的
/// 进程就能把文件描述符耗光，**整个 app 跟着死**，不只是 DNS。饱和时立刻
/// 回 SERVFAIL 而不是排队 —— 排队正是「DNS 服务器一卡，全机跟着卡」的那条路。
const MAX_INFLIGHT: usize = 256;

/// 运行中的 DNS 服务器。
///
/// 持有句柄而不是把任务 detach 掉：TUN 关闭再打开时要能重新绑定 53 端口，
/// 一个detach 的任务会一直攥着它，第二次启动直接 `AddrInUse`。
pub struct DnsHandle {
    local: SocketAddr,
    task: JoinHandle<()>,
}

impl DnsHandle {
    /// 实际绑定的地址（传 `:0` 时才知道端口）。
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// 停下服务并等它真的停了。
    pub async fn shutdown(mut self) {
        self.task.abort();
        // 被 abort 的任务返回 Cancelled，那是预期结果，不是错误。
        // 这里只可变借用不移动 —— `DnsHandle` 有 `Drop`，移出字段编译不过。
        let _ = (&mut self.task).await;
    }
}

impl Drop for DnsHandle {
    fn drop(&mut self) {
        // 句柄没了还留着监听端口，等于把 53 端口漏掉。
        self.task.abort();
    }
}

/// 明文 UDP 上游。
pub struct UdpUpstream {
    addr: SocketAddr,
}

impl UdpUpstream {
    pub fn new(addr: SocketAddr) -> Self {
        Self { addr }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 把请求原样发给上游并取回应答。
    ///
    /// 三条防线：
    /// 1. **硬超时**由外层 `tokio::time::timeout` 施加（见模块头）
    /// 2. 临时套接字 `connect()` 到上游，内核只放行该地址来的包
    /// 3. 仍然核对应答的 **id**：`connect()` 挡不住知道我们端口的链路上攻击者，
    ///    id 不符的包丢弃并继续等，直到超时
    pub async fn query(&self, req: &[u8], id: u16) -> Result<Vec<u8>, UpstreamError> {
        // 绑定地址族要跟上游一致，否则 IPv6 上游直接 connect 失败。
        let bind: SocketAddr = match self.addr {
            SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
        };
        let sock = UdpSocket::bind(bind)
            .await
            .map_err(|e| UpstreamError::Io(e.to_string()))?;
        sock.connect(self.addr)
            .await
            .map_err(|e| UpstreamError::Io(e.to_string()))?;
        sock.send(req)
            .await
            .map_err(|e| UpstreamError::Io(e.to_string()))?;

        let deadline = tokio::time::Instant::now() + UPSTREAM_TIMEOUT;
        let mut buf = vec![0u8; MAX_DNS];
        loop {
            let n = tokio::time::timeout_at(deadline, sock.recv(&mut buf))
                .await
                .map_err(|_| UpstreamError::Timeout {
                    upstream: self.addr,
                    after: UPSTREAM_TIMEOUT,
                })?
                .map_err(|e| UpstreamError::Io(e.to_string()))?;

            if n == buf.len() {
                // 只能警告：UDP 收到的就是被内核截掉之后的东西，补不回来。
                tracing::warn!("上游 {} 的应答达到 {MAX_DNS} 字节上限，可能已被截断", self.addr);
            }
            // id 不符：不是我们要的那条应答（乱入包或投毒尝试），丢弃再等。
            match Message::from_vec(&buf[..n]) {
                Ok(m) if m.metadata.id == id => {
                    buf.truncate(n);
                    return Ok(buf);
                }
                Ok(m) => {
                    tracing::debug!("丢弃上游 {} 的乱序应答：id {} ≠ {id}", self.addr, m.metadata.id);
                }
                Err(_) => {
                    tracing::debug!("丢弃上游 {} 的不可解析应答（{n} 字节）", self.addr);
                }
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error("上游 {upstream} 解析超时（{after:?}）")]
    Timeout {
        upstream: SocketAddr,
        after: Duration,
    },
    #[error("与上游通信失败：{0}")]
    Io(String),
}

pub struct DnsServer {
    fake: Arc<FakeDns>,
    upstream: UdpUpstream,
    /// 在飞上游查询的配额。见 [`MAX_INFLIGHT`]。
    inflight: Arc<Semaphore>,
}

impl DnsServer {
    pub fn new(fake: Arc<FakeDns>, upstream: SocketAddr) -> Self {
        Self {
            fake,
            upstream: UdpUpstream::new(upstream),
            inflight: Arc::new(Semaphore::new(MAX_INFLIGHT)),
        }
    }

    /// 把在飞配额压到 `n`，让饱和路径能被确定性地测到。
    ///
    /// 不是替身：被测的仍是 `spawn_forward` 的真实实现，只是把「凑够 256 条
    /// 卡住的查询」这一步省掉了 —— 真凑一遍要 256 个套接字和 2 秒挂起，
    /// 且与被测行为无关。同 `FakeIpPool::exhaust_for_test` 的手法。
    #[cfg(test)]
    pub(crate) fn with_inflight_limit(mut self, n: usize) -> Self {
        self.inflight = Arc::new(Semaphore::new(n));
        self
    }

    /// 在 `listen` 上跑起来。
    pub async fn serve(self: Arc<Self>, listen: SocketAddr) -> std::io::Result<DnsHandle> {
        let sock = Arc::new(UdpSocket::bind(listen).await?);
        let local = sock.local_addr()?;
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_DNS];
            loop {
                let (n, from) = match sock.recv_from(&mut buf).await {
                    Ok(v) => v,
                    Err(e) => {
                        // 不静默：DNS 悄悄失败的表现是「整机网络莫名其妙变慢」。
                        tracing::warn!("DNS 收包失败: {e}");
                        continue;
                    }
                };
                self.dispatch(&sock, &buf[..n], from).await;
            }
        });
        Ok(DnsHandle { local, task })
    }

    /// 分派一个请求。**本层能答的就地答**，零网络往返也就没有 spawn 的理由；
    /// 只有要转上游的才切出去，免得一条慢查询把整条收包循环堵住。
    async fn dispatch(self: &Arc<Self>, sock: &Arc<UdpSocket>, req: &[u8], from: SocketAddr) {
        match self.fake.respond(req) {
            Ok(Some(resp)) => send(sock, &resp, from).await,
            Ok(None) => self.spawn_forward(sock, req, from),
            // 报文根本读不懂：连 id 都拿不到，发什么都认不出，只能记日志。
            Err(DnsError::Malformed) => {
                tracing::warn!("丢弃来自 {from} 的畸形 DNS 报文（{} 字节）", req.len());
            }
            // 收到的是应答：回应它会和对面打成无限包风暴。只记不发。
            Err(DnsError::NotAQuery) => {
                tracing::warn!("丢弃来自 {from} 的 DNS 应答报文 —— 本端口只收查询");
            }
            Err(e) => {
                tracing::error!("构造给 {from} 的应答失败：{e}");
                if let Some(sf) = servfail_from_bytes(req) {
                    send(sock, &sf, from).await;
                }
            }
        }
    }

    fn spawn_forward(self: &Arc<Self>, sock: &Arc<UdpSocket>, req: &[u8], from: SocketAddr) {
        // 饱和时立刻失败，不排队 —— 排队就是把「DNS 慢」传染给整机。
        let Ok(permit) = self.inflight.clone().try_acquire_owned() else {
            tracing::warn!(
                "在飞上游查询已达上限 {MAX_INFLIGHT}，对 {from} 回 SERVFAIL（不排队，排队会拖垮整机）"
            );
            let sock = sock.clone();
            if let Some(sf) = servfail_from_bytes(req) {
                tokio::spawn(async move { send(&sock, &sf, from).await });
            }
            return;
        };
        let this = self.clone();
        let sock = sock.clone();
        let req = req.to_vec();
        tokio::spawn(async move {
            let _permit = permit;
            this.forward(&sock, &req, from).await;
        });
    }

    /// 转给上游，筛查过后再回客户端。
    ///
    /// **失败一律 SERVFAIL，绝不沉默。** 沉默的代价是客户端把查询吊到它自己的
    /// 超时（常见 5s），一次页面加载几十条查询叠上去就是整机可感的卡顿，
    /// 而日志上什么都没有。
    async fn forward(&self, sock: &Arc<UdpSocket>, req: &[u8], from: SocketAddr) {
        let id = match Message::from_vec(req) {
            Ok(m) => m.metadata.id,
            Err(_) => {
                // dispatch 已经解析成功过一次，走不到这里；真到了也不能瞎发包。
                tracing::error!("转发前重新解析来自 {from} 的请求失败");
                return;
            }
        };

        let raw = match self.upstream.query(req, id).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("转发 {from} 的查询到 {} 失败：{e}", self.upstream.addr());
                if let Some(sf) = servfail_from_bytes(req) {
                    send(sock, &sf, from).await;
                }
                return;
            }
        };

        // 上游应答里出现 fake-ip 段内地址只有两种可能：本机 DNS 被劫持，
        // 或上游把我们自己发的假地址回灌了回来。两种都不能放行 —— 客户端
        // 拿着它去连，包进 TUN，反查落空，连接必死且无从排查。
        // 2026-08-25 在开发机上实测：本机 DNS 确实被链路层劫持，
        // example.com 得到 198.18.0.207。这不是假想威胁。
        let out = match FakeDns::screen_upstream(&raw) {
            Ok(Screen::Clean) => raw,
            Ok(Screen::Cleaned { resp, removed }) => {
                tracing::warn!(
                    "上游 {} 的应答含 {} 条 fake-ip 段内地址（{removed:?}），已剔除后转出",
                    self.upstream.addr(),
                    removed.len()
                );
                resp
            }
            Ok(Screen::AllPoisoned { removed }) => {
                tracing::error!(
                    "上游 {} 的应答**全部**落在 fake-ip 段内（{removed:?}）—— \
                     本机 DNS 很可能被劫持。回 SERVFAIL，让客户端去问别家",
                    self.upstream.addr()
                );
                if let Some(sf) = servfail_from_bytes(req) {
                    send(sock, &sf, from).await;
                }
                return;
            }
            Err(e) => {
                tracing::warn!("上游 {} 的应答无法解析（{e}），回 SERVFAIL", self.upstream.addr());
                if let Some(sf) = servfail_from_bytes(req) {
                    send(sock, &sf, from).await;
                }
                return;
            }
        };
        send(sock, &out, from).await;
    }
}

async fn send(sock: &UdpSocket, buf: &[u8], to: SocketAddr) {
    if let Err(e) = sock.send_to(buf, to).await {
        // 发不出去就是客户端拿不到应答，必须能在日志里看见。
        tracing::warn!("向 {to} 发送 DNS 应答失败：{e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakeip::FakeIpPool;
    use hickory_proto::op::{MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::rdata::A;
    use hickory_proto::rr::{Name, RData, Record, RecordType};

    fn query_bytes(name: &str, qtype: RecordType) -> Vec<u8> {
        let mut m = Message::new(99, MessageType::Query, OpCode::Query);
        m.add_query(Query::query(Name::from_ascii(name).unwrap(), qtype));
        m.to_vec().unwrap()
    }

    /// 真实的本地 DNS 服务器：收真报文、解真报文、发真报文。
    ///
    /// **不是 mock**：它按 RFC 1035 线格式收发，被测代码走的是完整的
    /// 编解码与 UDP 路径。之所以自己起一个而不是打公共解析器，是因为
    /// 本机 DNS 被劫持（2026-08-25 实测），查 `example.com` 会拿到
    /// `198.18.0.207` —— 一个落在我们 fake-ip 段里的地址，测试会因此
    /// 变成掷骰子。`answers` 决定它对每条查询回什么。
    struct LocalDns {
        addr: SocketAddr,
        task: JoinHandle<()>,
    }

    impl Drop for LocalDns {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    /// 起一个本地 DNS，对任何查询都回给定的这批 A 地址。
    async fn local_dns(answers: &'static [&'static str]) -> LocalDns {
        local_dns_with(move |req| {
            let mut resp = Message::response(req.metadata.id, OpCode::Query);
            resp.add_queries(req.queries.iter().cloned());
            if let Some(q) = req.queries.first() {
                for ip in answers {
                    resp.add_answer(Record::from_rdata(
                        q.name().clone(),
                        60,
                        RData::A(A(ip.parse().unwrap())),
                    ));
                }
            }
            Some(resp.to_vec().unwrap())
        })
        .await
    }

    /// 起一个本地 DNS，应答由 `f` 决定；返回 `None` 表示**不应答**（模拟黑洞）。
    async fn local_dns_with<F>(f: F) -> LocalDns
    where
        F: Fn(&Message) -> Option<Vec<u8>> + Send + 'static,
    {
        let s = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = s.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_DNS];
            while let Ok((n, from)) = s.recv_from(&mut buf).await {
                let Ok(req) = Message::from_vec(&buf[..n]) else {
                    continue;
                };
                if let Some(out) = f(&req) {
                    let _ = s.send_to(&out, from).await;
                }
            }
        });
        LocalDns { addr, task }
    }

    async fn start(pool: Arc<FakeIpPool>, upstream: SocketAddr) -> DnsHandle {
        Arc::new(DnsServer::new(Arc::new(FakeDns::new(pool)), upstream))
            .serve("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap()
    }

    async fn ask(server: SocketAddr, req: &[u8]) -> Message {
        let c = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        c.send_to(req, server).await.unwrap();
        let mut buf = vec![0u8; MAX_DNS];
        let n = tokio::time::timeout(Duration::from_secs(5), c.recv(&mut buf))
            .await
            .expect("应答超时")
            .unwrap();
        Message::from_vec(&buf[..n]).unwrap()
    }

    /// 发一条查询，`wait` 时间内没收到应答就返回 None。
    async fn ask_maybe(server: SocketAddr, req: &[u8], wait: Duration) -> Option<Message> {
        let c = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        c.send_to(req, server).await.unwrap();
        let mut buf = vec![0u8; MAX_DNS];
        let n = tokio::time::timeout(wait, c.recv(&mut buf)).await.ok()?.ok()?;
        Message::from_vec(&buf[..n]).ok()
    }

    fn first_a(m: &Message) -> Ipv4Addr {
        match m.answers.first().map(|r| &r.data) {
            Some(RData::A(A(ip))) => *ip,
            other => panic!("首条答案不是 A：{other:?}"),
        }
    }

    #[tokio::test]
    async fn normal_domain_gets_fake_ip_without_touching_upstream() {
        let pool = Arc::new(FakeIpPool::new(vec![]));
        // 故意给一个不存在的上游：若走了转发，本测试会超时失败。
        let h = start(pool.clone(), "127.0.0.1:1".parse().unwrap()).await;
        let m = ask(h.local_addr(), &query_bytes("example.com.", RecordType::A)).await;
        let ip = first_a(&m);
        assert!(FakeIpPool::in_range(ip), "普通域名必须拿 fake-ip，且零上游往返");
        assert_eq!(pool.lookup(ip).as_deref(), Some("example.com"));
    }

    /// **环路防线的 DNS 端到端验证**：出站服务器域名必须被转发到真实上游，
    /// 拿到真实 IP，绝不能拿 fake-ip。
    #[tokio::test]
    async fn server_domain_is_forwarded_and_gets_real_ip() {
        let up = local_dns(&["1.2.3.4"]).await;
        let pool =
            Arc::new(FakeIpPool::new(vec![]).with_server_domains(&["srv.example.com".into()]));
        let h = start(pool, up.addr).await;
        let m = ask(h.local_addr(), &query_bytes("srv.example.com.", RecordType::A)).await;
        assert_eq!(
            first_a(&m).to_string(),
            "1.2.3.4",
            "服务器域名必须走真实解析；拿到 fake-ip 就意味着转发器会连向虚空"
        );
        assert!(!FakeIpPool::in_range(first_a(&m)));
    }

    #[tokio::test]
    async fn mx_query_is_forwarded_upstream() {
        let up = local_dns(&["1.2.3.4"]).await;
        let h = start(Arc::new(FakeIpPool::new(vec![])), up.addr).await;
        let m = ask(h.local_addr(), &query_bytes("example.com.", RecordType::MX)).await;
        assert_eq!(m.metadata.id, 99);
        assert_eq!(m.answers.len(), 1, "非 A/AAAA 查询应由上游应答");
    }

    #[tokio::test]
    async fn aaaa_is_answered_locally_with_noerror_and_no_records() {
        // 若给 NXDOMAIN，客户端会认为域名不存在，连 A 查询都不发了。
        let h = start(
            Arc::new(FakeIpPool::new(vec![])),
            "127.0.0.1:1".parse().unwrap(),
        )
        .await;
        let m = ask(h.local_addr(), &query_bytes("example.com.", RecordType::AAAA)).await;
        assert_eq!(m.metadata.response_code, ResponseCode::NoError);
        assert!(m.answers.is_empty());
    }

    // ---- 上游不可达：整机 DNS 的生死线 ----

    /// 上游黑洞（收包但永不应答）时，服务器必须在硬超时后回 SERVFAIL。
    /// 沉默的话客户端要吊到它自己的超时，一次页面加载几十条查询叠起来
    /// 就是整机可感的卡顿，而日志上什么都没有。
    #[tokio::test]
    async fn blackhole_upstream_yields_servfail_within_the_hard_timeout() {
        let up = local_dns_with(|_| None).await;
        let pool = Arc::new(FakeIpPool::new(vec!["srv.test".into()]));
        let h = start(pool, up.addr).await;
        let t = std::time::Instant::now();
        let m = ask(h.local_addr(), &query_bytes("srv.test.", RecordType::A)).await;
        let elapsed = t.elapsed();
        assert_eq!(
            m.metadata.response_code,
            ResponseCode::ServFail,
            "上游不可达必须显式 SERVFAIL，绝不能沉默"
        );
        assert_eq!(m.metadata.id, 99, "SERVFAIL 也要回带 id，否则客户端认不出");
        assert_eq!(m.queries.len(), 1, "SERVFAIL 也要回带问题段");
        assert!(
            elapsed < UPSTREAM_TIMEOUT + Duration::from_secs(1),
            "硬超时没生效：耗时 {elapsed:?}"
        );
    }

    /// 上游端口上压根没人监听（ICMP port unreachable）时同样要 SERVFAIL，
    /// 而且要**快** —— 这条不该等满 2s。
    #[tokio::test]
    async fn unreachable_upstream_port_yields_servfail() {
        let pool = Arc::new(FakeIpPool::new(vec!["srv.test".into()]));
        // 绑一个端口再立刻释放，拿到一个几乎确定没人听的端口号。
        let dead = {
            let s = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
            s.local_addr().unwrap()
        };
        let h = start(pool, dead).await;
        let m = ask(h.local_addr(), &query_bytes("srv.test.", RecordType::A)).await;
        assert_eq!(m.metadata.response_code, ResponseCode::ServFail);
    }

    /// 上游挂了**不影响**本层能直接答的查询 —— fake-ip 是零往返的。
    /// 这条是「DNS 服务器一卡拖垮整机」的反面证据：即便上游全死，
    /// 普通域名照常秒回。
    #[tokio::test]
    async fn a_dead_upstream_does_not_stall_locally_answerable_queries() {
        let up = local_dns_with(|_| None).await;
        let pool = Arc::new(FakeIpPool::new(vec!["srv.test".into()]));
        let h = start(pool, up.addr).await;
        // 先打一条注定卡住的转发查询，再问一条本地能答的。
        let stuck = tokio::spawn({
            let addr = h.local_addr();
            async move { ask_maybe(addr, &query_bytes("srv.test.", RecordType::A), Duration::from_secs(5)).await }
        });
        let t = std::time::Instant::now();
        let m = ask(h.local_addr(), &query_bytes("fast.test.", RecordType::A)).await;
        assert!(FakeIpPool::in_range(first_a(&m)));
        assert!(
            t.elapsed() < Duration::from_millis(500),
            "本地可答的查询被上游拖慢了：{:?}",
            t.elapsed()
        );
        let _ = stuck.await;
    }

    // ---- 上游被劫持：开发机上每天都在发生 ----

    /// 上游回的是 fake-ip 段内地址（本机 DNS 被劫持的实测形态）时，
    /// 必须 SERVFAIL 而不是放行。放行等于让客户端拿一个进 TUN 后反查
    /// 必然落空的地址去连 —— 连接必死且无从排查。
    #[tokio::test]
    async fn hijacked_upstream_answer_in_fake_range_becomes_servfail() {
        // 198.18.0.207 是 2026-08-25 在本机实测查 example.com 得到的值。
        let up = local_dns(&["198.18.0.207"]).await;
        let pool = Arc::new(FakeIpPool::new(vec!["srv.example.com".into()]));
        let h = start(pool, up.addr).await;
        let m = ask(h.local_addr(), &query_bytes("srv.example.com.", RecordType::A)).await;
        assert_eq!(
            m.metadata.response_code,
            ResponseCode::ServFail,
            "被劫持的段内应答必须挡下 —— 尤其这是出站服务器域名"
        );
        assert!(m.answers.is_empty());
    }

    /// 部分中毒时保留干净的那几条，不因一条脏记录连累整次查询。
    #[tokio::test]
    async fn partially_hijacked_answer_keeps_the_clean_addresses() {
        let up = local_dns(&["198.18.0.207", "93.184.216.34"]).await;
        let pool = Arc::new(FakeIpPool::new(vec!["srv.example.com".into()]));
        let h = start(pool, up.addr).await;
        let m = ask(h.local_addr(), &query_bytes("srv.example.com.", RecordType::A)).await;
        assert_eq!(m.metadata.response_code, ResponseCode::NoError);
        assert_eq!(m.answers.len(), 1);
        assert_eq!(first_a(&m).to_string(), "93.184.216.34");
    }

    // ---- 报文层面的滥用 ----

    /// 上游乱回一条 id 不符的包（乱入或投毒尝试），不能当成应答转给客户端。
    #[tokio::test]
    async fn upstream_answer_with_mismatched_id_is_not_relayed() {
        let up = local_dns_with(|req| {
            // 故意把 id 改掉
            let mut resp = Message::response(req.metadata.id.wrapping_add(1), OpCode::Query);
            resp.add_queries(req.queries.iter().cloned());
            if let Some(q) = req.queries.first() {
                resp.add_answer(Record::from_rdata(
                    q.name().clone(),
                    60,
                    RData::A(A("6.6.6.6".parse().unwrap())),
                ));
            }
            Some(resp.to_vec().unwrap())
        })
        .await;
        let pool = Arc::new(FakeIpPool::new(vec!["srv.test".into()]));
        let h = start(pool, up.addr).await;
        let m = ask(h.local_addr(), &query_bytes("srv.test.", RecordType::A)).await;
        assert_eq!(
            m.metadata.response_code,
            ResponseCode::ServFail,
            "id 不符的上游应答必须丢弃，最终超时 SERVFAIL"
        );
        assert!(
            !m.answers.iter().any(|r| matches!(r.data, RData::A(A(ip)) if ip.to_string() == "6.6.6.6")),
            "绝不能把 id 不符的应答转给客户端"
        );
    }

    /// 畸形报文：拿不到 id 就发不出客户端认得出的应答，只能丢弃 + 记日志。
    /// 关键是**不能崩、不能卡住收包循环**——后一条查询照常。
    #[tokio::test]
    async fn malformed_request_is_dropped_without_killing_the_server() {
        let h = start(
            Arc::new(FakeIpPool::new(vec![])),
            "127.0.0.1:1".parse().unwrap(),
        )
        .await;
        assert!(
            ask_maybe(h.local_addr(), &[0xff, 0x00, 0x01], Duration::from_millis(300))
                .await
                .is_none(),
            "畸形报文不该收到应答"
        );
        // 服务器还活着
        let m = ask(h.local_addr(), &query_bytes("after.test.", RecordType::A)).await;
        assert!(FakeIpPool::in_range(first_a(&m)));
    }

    /// 把一条**应答**发到查询端口：绝不能回应，否则两个 UDP 服务互指
    /// 会打成无限包风暴。
    #[tokio::test]
    async fn a_response_sent_to_the_server_is_never_answered() {
        let h = start(
            Arc::new(FakeIpPool::new(vec![])),
            "127.0.0.1:1".parse().unwrap(),
        )
        .await;
        let mut m = Message::response(1, OpCode::Query);
        m.add_query(Query::query(
            Name::from_ascii("example.com.").unwrap(),
            RecordType::A,
        ));
        assert!(
            ask_maybe(h.local_addr(), &m.to_vec().unwrap(), Duration::from_millis(300))
                .await
                .is_none(),
            "回应一条应答就是在造包风暴"
        );
    }

    // ---- 生命周期 ----

    /// 在飞配额耗尽时**立刻** SERVFAIL，不排队。
    ///
    /// 排队才是「DNS 服务器一卡，全机跟着卡」的那条路：每条在飞查询占一个
    /// 临时套接字，本机上一个循环发查询的进程就能把 fd 耗光，整个 app
    /// 跟着死。饱和时快速失败让客户端转头去问下一个解析器。
    #[tokio::test]
    async fn saturated_inflight_quota_fails_fast_instead_of_queueing() {
        let up = local_dns_with(|_| None).await; // 黑洞：占住配额不放
        let pool = Arc::new(FakeIpPool::new(vec!["srv.test".into()]));
        let h = Arc::new(
            DnsServer::new(
                Arc::new(FakeDns::new(pool)),
                up.addr,
            )
            .with_inflight_limit(1),
        )
        .serve("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
        let addr = h.local_addr();

        // 第一条把唯一的配额占住（它会一直卡到 2s 硬超时）。
        let hog = tokio::spawn(async move {
            ask_maybe(addr, &query_bytes("srv.test.", RecordType::A), Duration::from_secs(5)).await
        });
        // 等它确实拿到配额
        tokio::time::sleep(Duration::from_millis(150)).await;

        let t = std::time::Instant::now();
        let m = ask(addr, &query_bytes("srv.test.", RecordType::A)).await;
        let elapsed = t.elapsed();
        assert_eq!(
            m.metadata.response_code,
            ResponseCode::ServFail,
            "配额饱和必须立刻失败"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "饱和时排了队：耗时 {elapsed:?}，说明没有快速失败"
        );
        let _ = hog.await;
    }

    /// 关掉之后端口必须能重新绑上。TUN 关了再开时走的正是这条路；
    /// 任务 detach 的话第二次启动会直接 AddrInUse。
    #[tokio::test]
    async fn port_is_released_after_shutdown_so_it_can_be_rebound() {
        let h = start(
            Arc::new(FakeIpPool::new(vec![])),
            "127.0.0.1:1".parse().unwrap(),
        )
        .await;
        let addr = h.local_addr();
        h.shutdown().await;
        // 同一个端口必须能再绑上
        let again = Arc::new(DnsServer::new(
            Arc::new(FakeDns::new(Arc::new(FakeIpPool::new(vec![])))),
            "127.0.0.1:1".parse().unwrap(),
        ))
        .serve(addr)
        .await;
        assert!(again.is_ok(), "端口没释放：{:?}", again.err());
    }

    /// 同一个域名并发查询必须收敛到同一个 fake-ip —— 否则同一个页面里
    /// 两条连接会被反查成不同结果。
    #[tokio::test]
    async fn concurrent_queries_for_one_domain_converge_on_one_ip() {
        let pool = Arc::new(FakeIpPool::new(vec![]));
        let h = start(pool.clone(), "127.0.0.1:1".parse().unwrap()).await;
        let addr = h.local_addr();
        let mut set = Vec::new();
        for _ in 0..16 {
            set.push(tokio::spawn(async move {
                first_a(&ask(addr, &query_bytes("shared.test.", RecordType::A)).await)
            }));
        }
        let mut ips = Vec::new();
        for t in set {
            ips.push(t.await.unwrap());
        }
        assert!(ips.iter().all(|ip| *ip == ips[0]), "并发查询拿到了不同的 fake-ip");
        assert_eq!(pool.len(), 1);
    }
}
