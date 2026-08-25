//! 内部解析器：路由引擎两阶段求值的第二轮供料方。
//!
//! 这一层**不监听任何端口、不需要 root**（设计文档 §7.1 第一层）。
//! 唯一的消费者是路由引擎——SOCKS5 与 HTTP CONNECT 本就把域名原样递过来，
//! 代理模式下客户端无需解析即可转发；解析只在一处被需要：让 GEOIP / IP-CIDR
//! 这类规则对域名目标生效。
//!
//! 三条纪律（设计文档 §7.2 / §7.3）在本模块的落点：
//! - 纪律①：出站服务器域名走 `bootstrap()`，绝不经过本解析器
//! - 纪律②：上游必须是 IP 字面量，由 `upstream::parse_nameserver` 在加载期拦下
//! - 纪律③：解析超时/失败**返回空切片而非错误**，判决继续往下走

use std::net::IpAddr;
use std::time::Duration;

use hickory_resolver::config::{ResolveHosts, ResolverConfig, ResolverOpts};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::{Resolver, TokioResolver};

use crate::upstream::{parse_nameserver, UpstreamError};

#[derive(Debug, thiserror::Error)]
pub enum ResolverError {
    #[error("nameserver 配置有误：{0}")]
    Upstream(#[from] UpstreamError),
    #[error(
        "dns.nameserver 为空。内部解析器至少需要一个上游，\
             例如：https://1.1.1.1/dns-query"
    )]
    NoNameservers,
    #[error("构建解析器失败：{0}")]
    Build(String),
    #[error("读取系统 DNS 配置失败：{0}。bootstrap 解析器依赖它来解析出站服务器域名")]
    SystemConf(String),
}

/// 内部解析器。判决路径专用。
///
/// 缓存**不自建**：hickory 内置 `ResponseCache`（moka，按 TTL 过期）已同时
/// 覆盖设计文档 §7.3 要求的「遵循 TTL 的 LRU」与「失败必须负缓存」两项——
/// NXDOMAIN 走 `NoRecordsFound` 分支入缓存，TTL 由 `negative_min_ttl` /
/// `negative_max_ttl` 夹取。自己再套一层只会与内置缓存的 TTL 记账打架。
///
/// `Debug` 不是装饰：测试里对 `Result<DnsResolver, _>` 调 `.unwrap_err()`
/// 要求 `T: Debug`，少了它本 Task 的构造校验测试直接编译不过。
/// hickory 的 `Resolver` 未实现 `Debug`，故手写而非 derive。
pub struct DnsResolver {
    inner: TokioResolver,
    /// 判决路径上的硬超时。**必须由外层 tokio::time::timeout 施加**，
    /// 见 `lookup_for_routing` 的注释。
    hard_timeout: Duration,
}

impl std::fmt::Debug for DnsResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不打印上游列表：那属于用户配置，日志里不该有。
        f.debug_struct("DnsResolver")
            .field("hard_timeout", &self.hard_timeout)
            .finish_non_exhaustive()
    }
}

impl DnsResolver {
    /// 用配置里的 `dns.nameserver` 建立解析器。
    ///
    /// - `nameservers`：上游列表，必须是 IP 字面量（纪律②）
    /// - `timeout`：判决路径硬超时（设计文档 §7.3 定为 2s）
    /// - `cache_max` / `negative_ttl`：对应配置的 `dns.cache.{max,negative-ttl-s}`
    pub fn new(
        nameservers: &[String],
        timeout: Duration,
        cache_max: u64,
        negative_ttl: Duration,
    ) -> Result<Self, ResolverError> {
        if nameservers.is_empty() {
            return Err(ResolverError::NoNameservers);
        }
        let mut servers = Vec::with_capacity(nameservers.len());
        for spec in nameservers {
            servers.push(parse_nameserver(spec)?);
        }

        let mut opts = ResolverOpts::default();
        // 这个 timeout 只约束「池内单轮」，不等于端到端墙钟上限——真正的
        // 硬超时在 lookup_for_routing 里用 tokio::time::timeout 施加。
        // 这里仍然设小，是为了让池尽早放弃一台坏上游去试下一台。
        opts.timeout = timeout;
        // 判决路径上不做重试：重试的时间预算还不如直接让 IP 规则不匹配，
        // 流程继续往下（纪律③）。attempts 默认为 2，必须显式压到 1。
        opts.attempts = 1;
        opts.cache_size = cache_max;
        // 负缓存下限：服务端给的 NXDOMAIN TTL 可能是 0，那样等于没有负缓存，
        // 不存在的域名会被反复查询（设计文档 §7.3）。
        opts.negative_min_ttl = Some(negative_ttl);
        // 判决只关心「这批 IP 落在哪个网段」，中间的 CNAME 记录一概不需要，
        // 留着只会占缓存容量。
        opts.preserve_intermediates = false;
        // 关键：**绝不读系统 hosts 文件**。shard.rs 会把出站服务器域名写成
        // `127.0.0.1 <域名> # wsieve-managed`；若解析器读了它，路由判决会
        // 认为该域名是环回地址，从而命中内网直连规则。判决必须看到真实 IP。
        // 默认值是 Auto，**会读** —— 见 tests/resolver.rs 的 hosts 用例。
        opts.use_hosts_file = ResolveHosts::Never;

        let inner = Resolver::builder_with_config(
            ResolverConfig::from_parts(None, vec![], servers),
            TokioRuntimeProvider::default(),
        )
        .with_options(opts)
        .build()
        .map_err(|e| ResolverError::Build(e.to_string()))?;

        Ok(Self {
            inner,
            hard_timeout: timeout,
        })
    }

    /// 判决路径专用的解析。**永不返回错误**。
    ///
    /// 设计文档 §6.2 与 §12 的硬要求：超时或失败一律视为「该 IP 规则不匹配」，
    /// 流程继续往下走，绝不因一次 DNS 故障阻断整条连接。因此签名是
    /// `-> Vec<IpAddr>`，调用方把它原样传给 `evaluate(target, Some(&ips), ..)`，
    /// 空切片即不匹配。
    ///
    /// **硬超时必须由外层 `tokio::time::timeout` 施加，不能只靠
    /// `ResolverOpts::timeout`。** 已实测：把 `opts.timeout` 设成 500ms、
    /// 上游指向黑洞地址（192.0.2.1，RFC 5737 文档段），单次 `lookup_ip`
    /// 实际耗时 **15.03 秒**。原因是 `opts.timeout` 只在名字服务器池的
    /// **轮与轮之间**检查 deadline（`name_server_pool.rs:290`），而 TCP/TLS
    /// 建连本身卡在系统 connect 超时里，一轮都没走完。外层包一层之后实测
    /// 稳定在设定值。
    pub async fn lookup_for_routing(&self, domain: &str) -> Vec<IpAddr> {
        match tokio::time::timeout(self.hard_timeout, self.inner.lookup_ip(domain)).await {
            Ok(Ok(lookup)) => lookup.iter().collect(),
            Ok(Err(e)) => {
                // 不静默吞掉：解析失败是排查「为什么这个域名没走对路」的关键线索。
                tracing::debug!("解析 {domain} 失败，该 IP 规则按不匹配处理：{e}");
                Vec::new()
            }
            Err(_) => {
                tracing::warn!(
                    "解析 {domain} 超过硬超时 {:?}，该 IP 规则按不匹配处理，连接继续",
                    self.hard_timeout
                );
                Vec::new()
            }
        }
    }
}

/// **bootstrap 解析器：专解出站服务器域名，绝不经过我们自己的 DNS。**
///
/// 这是设计文档 §7.2 纪律①的落点，也是本阶段最要紧的一条。`shard.rs` 的
/// `resolve_upstream` 必须拿到**真实 IP**：一旦阶段 6 的 fake-ip 生效，
/// 走我们自己的 DNS 只会拿到 `198.18.x.x`，转发器于是连向虚空——而且是
/// 静默失败，表现为「握手一直不成功」，极难排查。
///
/// 实现上刻意用一个**独立的 Resolver 实例**、读系统配置（`/etc/resolv.conf`
/// 或 Windows 注册表），与 `DnsResolver` 没有任何共享状态：没有共享缓存、
/// 没有共享上游、没有共享 fake-ip 映射表。隔离靠的是「压根是两个对象」，
/// 而不是靠某个 if 分支——分支会被改错，两个对象不会。
///
/// 对应配置项 `dns.proxy-server-nameserver: [system]`（沿用 Clash 字段名，
/// 语义恰好是「专门用来解析代理服务器域名的」，见设计文档 §5.3 取舍④）。
///
/// **注意这里保留系统 hosts 的读取（不设 Never）**：`builder_tokio()` 走
/// 系统配置，用户自己在 hosts 里写的条目属于「用户明示的意图」，应当尊重。
/// 我们自己写的 `# wsieve-managed` 行由 `shard_setup.rs` 的既有顺序防线
/// 处理 —— 它在 `clear_managed()` 之后才解析（见 Task 6）。
pub fn bootstrap() -> Result<TokioResolver, ResolverError> {
    Resolver::builder_tokio()
        .map_err(|e| ResolverError::SystemConf(e.to_string()))?
        .build()
        .map_err(|e| ResolverError::Build(e.to_string()))
}

/// 用显式上游建立 bootstrap 解析器，供 `proxy-server-nameserver` 写了具体
/// 地址（而非 `system`）时使用。
///
/// 注意这里**不设负缓存下限也不压 attempts**：bootstrap 服务的是「建立出站
/// 连接」这条路径，宁可多等一会儿也要拿到真实 IP；而判决路径的纪律是宁可
/// 不匹配也不阻塞。两条路径的取舍方向相反，所以不共用配置。
pub fn bootstrap_with(nameservers: &[String]) -> Result<TokioResolver, ResolverError> {
    if nameservers.is_empty() {
        return Err(ResolverError::NoNameservers);
    }
    let mut servers = Vec::with_capacity(nameservers.len());
    for spec in nameservers {
        servers.push(parse_nameserver(spec)?);
    }
    let mut opts = ResolverOpts::default();
    // 显式上游意味着用户绕开了系统配置，此时 hosts 也一并绕开 —— 出站域名
    // 正是被我们写进 hosts 的那一个，读它只会拿到 127.0.0.1。
    opts.use_hosts_file = ResolveHosts::Never;
    Resolver::builder_with_config(
        ResolverConfig::from_parts(None, vec![], servers),
        TokioRuntimeProvider::default(),
    )
    .with_options(opts)
    .build()
    .map_err(|e| ResolverError::Build(e.to_string()))
}
