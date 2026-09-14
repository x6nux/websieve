//! 本地条带的启动编排：清残留 → 解析 → 起转发器 → 让 WebView 找到转发器，
//! 以及失败时的降级。
//!
//! 顺序是关键（见 `plan_many` 的注释）：**清残留在最前面且无条件执行**，
//! 然后才解析、起转发器、最后把域名指过去。任何一步失败都降级到单会话继续跑，
//! 而不是拒绝启动——与 mux 协商失败的处理一致（优先建立连接 + 警告日志）。
//!
//! # 两条「让 WebView 找到转发器」的路
//!
//! 转发器在 `127.0.0.1:base..base+N` 上监听，而 WebView 要连的是
//! `https://域名:base+i/`。把前者接到后者有两种做法，差别**只在这一步**，
//! 解析、起转发器、算 session_bases 全都一模一样：
//!
//! - [`Reach::Proxy`]（首选）：给 WebView 设一个本地 SOCKS5 代理，代理按
//!   `(host, port)` 查表改写到 `127.0.0.1:port`。不要管理员权限、只作用于
//!   我们自己的 WebView、**纯 IP 服务端也能用**。见 [`crate::webview_proxy`]。
//! - [`Reach::Hosts`]（兜底）：把域名写进 `/etc/hosts` 指向 127.0.0.1。要
//!   管理员权限、全系统生效、IP 服务端用不了（hosts 映射不了「IP→IP」）。
//!
//! hosts 条目的托管本身归 `crate::custody`（设计文档 §10），本模块只管编排。
//!
//! # 为什么是「一批」而不是「一个」（运行时接入 config.yaml Task 3）
//!
//! `HostsFile::set_managed` 是**整体替换**语义：它先清掉全部带
//! `wsieve-managed` 标记的行，再写入调用方给的那份列表。若给每个出站各调
//! 一次单出站版本的 `plan`，第二个出站的 `plan` 一开始就会把第一个出站刚写
//! 好的那一行连同其余托管行一起清掉，然后只写回它自己那一个域名——表现为
//! 「配置了两个出站，过一会儿第一个的域名劫持就悄悄失效了」，而且没有任何
//! 报错，因为从 hosts 文件的视角看，这就是一次正常的「设为只有这些条目」。
//!
//! 因此本模块只提供**批量**入口 `plan_many`：清残留只做一次，覆盖整批目标；
//! 每个目标各自解析、起转发器（互不影响，一个失败只降级它自己）；全部目标
//! 处理完之后，只对**成功需要劫持的那些**域名做**一次**原子的
//! `CustodyGuard::acquire`，覆盖全部域名的并集。若这唯一一次写入失败，
//! 全部原本该被劫持的目标一起退回单会话——不存在「有的目标享受批量写入的
//! 结果、有的目标看到另一份」这种分裂状态。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use crate::custody::hosts::{HostsCustody, HostsFile};
use crate::custody::{CustodyGuard, ManagedSystemState};
use crate::shard::{self, Forwarder, ShardGuard};

/// 解析出服务器真实 IP 之后、起转发器之前的回调。
///
/// **这是 §8.3.2 第 2 步「写 bypass 路由」的挂点**，位置不能挪：
///   - 挪到解析**之前**：那时还不知道服务器 IP，无从写起
///   - 挪到起转发器**之后**：转发器建立的第一条连接就已经在 TUN 覆盖下裸奔，
///     而 TUN 此刻可能已经被上一次运行的残留路由接管
///
/// 返回 `Err` 时 `plan_many` **不中止该目标**：转发器与 hosts 归混合端口
/// 入口用，与 TUN 无关，没道理因为 TUN 起不来就把这个出站整个关掉。错误被
/// 记进 [`ShardPlanEntry::bypass_error`]，由调用方据此**拒绝拉起 TUN**。
pub type UpstreamHook = Arc<dyn Fn(SocketAddr) -> anyhow::Result<()> + Send + Sync>;

/// 让 WebView 找到本地转发器的方式。见模块文档「两条路」。
pub enum Reach<'a> {
    /// 经 WebView 自己的 SOCKS5 代理改写（免提权，首选）。
    Proxy(&'a crate::webview_proxy::WebviewProxy),
    /// 写 `/etc/hosts` 劫持域名（要管理员权限，代理不可用时的兜底）。
    Hosts,
}

impl Reach<'_> {
    /// 这条路要不要求「主机名可劫持」。
    ///
    /// 只有 hosts 路要求：它靠「域名→IP」的映射，对 IP 字面量无能为力
    /// （映射不了「IP→IP」），对环回也没有跨网链路可并行。代理路不做名字
    /// 解析，只按 (host, port) 查表，因此纯 IP 服务端一样拿得到多会话。
    fn needs_hijackable_host(&self) -> bool {
        matches!(self, Reach::Hosts)
    }
}

/// 一个待编排的出站：它的服务端 URL、本地转发端口起点、额外会话数、
/// 以及（若 TUN 开启）解析出真实 IP 后要调用的 bypass 钩子。
pub struct ShardTarget {
    pub server_url: String,
    pub base_port: u16,
    pub extra_sessions: usize,
    pub on_upstream: Option<UpstreamHook>,
}

/// 单个出站的编排结果（不含 guard——guard 是整批共享的一份，见
/// [`ShardManyPlan::guard`]）。
pub struct ShardPlanEntry {
    /// 各会话的请求基址；`None` 表示该会话用相对路径（同源，即会话 0）。
    pub session_bases: Vec<Option<String>>,
    /// 解析到的服务器真实地址。`None` 表示本次走了降级路径，压根没解析。
    ///
    /// **拉起 TUN 的前提**：没有真实 IP 就没有 bypass，而没有 bypass 的 TUN
    /// 是确定性的环路（§8.3.1）。
    pub upstream: Option<SocketAddr>,
    /// bypass 钩子的失败原因。
    ///
    /// `Some` ⇒ **绝不能拉起 TUN**。字符串而非 `anyhow::Error`：这里只用于
    /// 报给用户，而这个结构要能跨 await 点搬来搬去。
    pub bypass_error: Option<String>,
}

impl ShardPlanEntry {
    /// 降级：不劫持、单会话，会话 0 用原始 URL 的 origin。
    fn degraded(server_url: &str) -> Self {
        Self {
            // 会话 0 也用显式绝对 origin。它曾经吃相对路径，前提是「承载页
            // 就是它的 origin」——承载页挪到本机 http 壳之后这个前提没了。
            //
            // 取不出 origin 时退回原样：走到这条路径的 URL 有一部分正是因为
            // `split_url` 失败才降级的，此处再制造一个失败点没有意义。真正
            // 挡住这种坏 URL 的安全网是 `runtime_state::build_startup_plan`——
            // 它对 `config.proxies[].url`（与这里的 `server_url` 是同一个
            // 字符串）逐个再跑一遍 `origin_of` 并拒绝非法 URL。这道网立在
            // 另一个模块里，两边一旦失步，这里退回原样就会悄悄拼出一个指向
            // 承载页 origin 的相对 URL——脆，但今天真实拦截它的就是这道网。
            session_bases: vec![Some(
                origin_of(server_url).unwrap_or_else(|_| server_url.to_string()),
            )],
            upstream: None,
            bypass_error: None,
        }
    }

    /// 本次是否可以安全地拉起 TUN。
    ///
    /// 两个条件缺一不可：解析到了真实 IP、且 bypass 路由确实写进去了。
    /// 做成方法而不是让调用方各自判两个字段 —— 漏判任何一个都是环路，
    /// 而环路是静默的。
    pub fn tun_may_start(&self) -> bool {
        self.upstream.is_some() && self.bypass_error.is_none()
    }

    /// 实际可用的会话数。
    #[cfg(test)]
    pub fn session_count(&self) -> usize {
        self.session_bases.len()
    }
}

/// `plan_many` 的返回：每个目标各自的结果（与入参 `targets` 按下标一一
/// 对应），加上整批共享的一份 guard（覆盖全部真正被劫持的域名 + 全部为它们
/// 建的转发器；没有任何目标被劫持时是 `None`）。
pub struct ShardManyPlan {
    pub entries: Vec<ShardPlanEntry>,
    pub guard: Option<ShardGuard>,
}

/// 从 URL 取出 (scheme, host, port)。仅支持 http/https。
fn split_url(url: &str) -> anyhow::Result<(String, String, u16)> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| anyhow::anyhow!("URL 缺少 scheme: {url}"))?;
    let default_port = match scheme {
        "https" => 443u16,
        "http" => 80,
        other => anyhow::bail!("不支持的 scheme: {other}"),
    };
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() {
        anyhow::bail!("URL 缺少主机: {url}");
    }
    // IPv6 字面量形如 [::1]:443
    let (host, port) = if let Some(end) = authority.strip_prefix('[') {
        let (h, tail) = end
            .split_once(']')
            .ok_or_else(|| anyhow::anyhow!("IPv6 字面量不完整: {url}"))?;
        (
            h.to_string(),
            tail.strip_prefix(':')
                .map(|p| p.parse())
                .transpose()?
                .unwrap_or(default_port),
        )
    } else {
        match authority.split_once(':') {
            Some((h, p)) => (h.to_string(), p.parse()?),
            None => (authority.to_string(), default_port),
        }
    };
    Ok((scheme.to_string(), host, port))
}

/// 从 URL 取出 origin（`scheme://authority`，无路径、无尾斜杠）。
///
/// 与 `split_url` 的区别是**不归一默认端口**：`https://a.com:443` 与
/// `https://a.com` 对 fetch 等价，但保留用户写的原样能让日志里的基址和配置
/// 文件对得上。数据面基址要给 WebView 直接拼路径用，所以走这一个。
///
/// `pub(crate)`：`runtime_state::build_startup_plan` 复用它来校验
/// `config.proxies[].url`。这道校验以前长在 `outbound::carrier::validate`
/// 里，承载计划不再摸出站 URL 之后无处可挂，只能搬家；`outbound::carrier`
/// 那边本来也有一份 `origin_of`（已随 `validate` 一起删掉），两份重复实现
/// 不该再添第三份，直接复用这一份。
pub(crate) fn origin_of(url: &str) -> anyhow::Result<String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| anyhow::anyhow!("URL 缺少 scheme: {url}"))?;
    if scheme != "http" && scheme != "https" {
        anyhow::bail!("不支持的 scheme: {scheme}");
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        anyhow::bail!("URL 缺少主机: {url}");
    }
    // 用户名密码写进 origin 会让它不再是合法 origin，也会把凭据泄进
    // session_bases 与 tracing 日志。
    if authority.contains('@') {
        anyhow::bail!("URL 不接受 userinfo: {url}");
    }
    Ok(format!("{scheme}://{authority}"))
}

/// 域名是否值得劫持。IP 字面量与环回主机不劫持：IP 没有「同域名不同端口
/// 仍匹配证书」这一前提可依赖，环回则本来就没有跨网链路可并行。
fn hijackable(host: &str) -> bool {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    !matches!(host, "localhost" | "localhost.")
}

/// 一个目标成功走完「解析 + 起转发器」之后、等待批量写 hosts 的中间态。
struct Prepared {
    idx: usize,
    original_url: String,
    host: String,
    forwarder: Forwarder,
    session_bases: Vec<Option<String>>,
    upstream: SocketAddr,
    bypass_error: Option<String>,
}

/// 编排一批出站的本地条带。`extra_sessions` 为 0 的目标直接走单会话原路径。
///
/// `on_upstream` 在**解析之后、起转发器之前**被调用一次（见 [`UpstreamHook`]）。
/// TUN 关闭时传 `None`。
///
/// 每个目标独立解析、独立起转发器——一个目标的失败只降级它自己，不影响其他
/// 目标。但 hosts 的写入是**批量的、原子的一次**：见模块文档「为什么是一批」。
///
/// `reach` 决定最后一步怎么把域名接到转发器上（见模块文档「两条路」）。
/// 无论走哪条，**步骤 0 的 hosts 残留清理都照常执行**：上一次运行可能走的
/// 是 hosts 路且被 SIGKILL，那行残留必须清掉，否则那个域名会一直指向一个
/// 早已不在跑的转发器——而且是全系统生效。
pub async fn plan_many(
    targets: Vec<ShardTarget>,
    hosts_path: PathBuf,
    reach: Reach<'_>,
) -> ShardManyPlan {
    let hosts = Arc::new(HostsFile::new(hosts_path));

    // 0) 无条件先清残留 —— 在任何目标的处理之前，覆盖整批。
    //
    // 这一步与「本次要不要劫持」无关：上一次运行被 SIGKILL 时条目留在了
    // hosts 里，而本次可能因为条带被关掉、URL 变成 IP、服务端换了域名等
    // 任何理由直接降级。清理只发生在「本次也打算劫持」的路径上的话，
    // 残留就会一直留着，那个域名从此永远指向一个不在跑的转发器。
    if let Err(e) = HostsCustody::cleanup_only(hosts.clone()).clear_stale() {
        // 不可写属常态（没管理员权限），降到 debug；其余照常告警。
        if hosts.writable() {
            tracing::warn!("清理 hosts 残留失败（{e:#}）——若上次异常退出，条目可能仍在");
        } else {
            tracing::debug!("hosts 不可写，跳过残留清理：{e:#}");
        }
    }

    let total = targets.len();
    let mut entries: Vec<Option<ShardPlanEntry>> = (0..total).map(|_| None).collect();
    let mut prepared: Vec<Prepared> = Vec::new();

    for (idx, t) in targets.into_iter().enumerate() {
        if t.extra_sessions == 0 {
            entries[idx] = Some(ShardPlanEntry::degraded(&t.server_url));
            continue;
        }
        let sessions = t.extra_sessions + 1;

        let (scheme, host, port) = match split_url(&t.server_url) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("条带禁用：服务端 URL 解析失败（{e}）——退回单会话");
                entries[idx] = Some(ShardPlanEntry::degraded(&t.server_url));
                continue;
            }
        };
        // 下面两道门只对 hosts 路成立。代理路不做名字解析、不写系统文件，
        // 因此 IP 服务端与无管理员权限这两种情形在代理路下都不是阻碍——
        // 这正是它存在的主要理由。
        if reach.needs_hijackable_host() && !hijackable(&host) {
            tracing::warn!("条带禁用：主机 {host} 是 IP 或环回，不做 hosts 劫持——退回单会话");
            entries[idx] = Some(ShardPlanEntry::degraded(&t.server_url));
            continue;
        }
        if reach.needs_hijackable_host() && !hosts.writable() {
            tracing::warn!(
                "条带禁用：{} 不可写——退回单会话。要启用多 TCP 条带，请以管理员身份运行，\
                 或手动加一行：127.0.0.1 {host} # wsieve-managed",
                hosts.path().display()
            );
            entries[idx] = Some(ShardPlanEntry::degraded(&t.server_url));
            continue;
        }

        // 1) 解析真实地址。残留已在步骤 0 清掉，此刻解析器不受我们污染
        //    ——否则会拿到环回地址，转发器就转给自己。
        //
        //    走 bootstrap 解析器而非系统 `lookup_host`：阶段 6 的 fake-ip 生效后，
        //    系统查询会被劫持成 198.18.x.x，转发器连向虚空且完全静默
        //    （设计文档 §7.2 纪律①）。今天两者行为等价，趁改动无风险时先换掉。
        let boot = match wsieve_dns::bootstrap() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("条带禁用：bootstrap 解析器建立失败（{e}）——退回单会话");
                entries[idx] = Some(ShardPlanEntry::degraded(&t.server_url));
                continue;
            }
        };
        let upstream = match shard::resolve_upstream(&boot, &host, port).await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("条带禁用：解析 {host}:{port} 失败（{e}）——退回单会话");
                entries[idx] = Some(ShardPlanEntry::degraded(&t.server_url));
                continue;
            }
        };
        if shard::is_loopback(&upstream) {
            // 清了残留还解析到环回，说明是别处（用户自己的 hosts 行/DNS）指过来的，
            // 劫持只会形成自环。
            tracing::warn!("条带禁用：{host} 解析到环回地址 {upstream}——退回单会话");
            entries[idx] = Some(ShardPlanEntry::degraded(&t.server_url));
            continue;
        }

        // 2) **写 bypass 路由**（§8.3.2 第 2 步）。必须夹在解析与起转发器之间：
        //    转发器的第一条连接就要走这条路由绕开 TUN，晚一步就是裸奔。
        //
        //    失败**不中止**这个目标：转发器与 hosts 服务的是混合端口入口，与
        //    TUN 无关。但错误要原样带回去，调用方据此拒绝拉起 TUN —— 没有
        //    bypass 的 TUN 是确定性的环路，绝不能靠「大概写上了」蒙混过去。
        let mut bypass_error = None;
        if let Some(hook) = &t.on_upstream {
            if let Err(e) = hook(upstream) {
                tracing::error!("写 bypass 路由失败（{e:#}）——本次不会拉起 TUN");
                bypass_error = Some(format!("{e:#}"));
            }
        }

        // 3) 起转发器。hosts 要等这一批全部目标处理完才**一次性**写
        //    （见模块文档），因此这里先只把转发器立起来，暂不写 hosts。
        let forwarder = match shard::spawn(t.base_port, sessions, upstream).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("条带禁用：本地转发器启动失败（{e}）——退回单会话");
                entries[idx] = Some(ShardPlanEntry::degraded(&t.server_url));
                continue;
            }
        };

        let ports = forwarder.ports().to_vec();
        // 数据面的 origin。**与承载页的 scheme 无关**：承载页是本机的 http
        // 壳，而数据面必须留在 https 才有真实 TLS 指纹。两者共用一个闭包，
        // 下一次有人改承载页 scheme 就会把数据面一起带歪。
        let data_origin = |p: u16| {
            // IPv6 字面量在 URL 的 authority 里**必须加方括号**（RFC 3986
            // §3.2.2），否则地址自带的冒号会被当成端口分隔符：
            // `https://2001:db8::1:18443` 被解析成 host=2001。
            //
            // 后果是彻底的而非降级的：每条会话基址都成了连不上的 URL，而
            // 改写表的 key 又永远匹配不上，出站完全不可用且不报错。
            // `runtime_state::build_startup_plan` 校验的是 `config.proxies[].url`
            // （经 `origin_of`，那里会正确加括号），**不校验这里派生出来的
            // 基址**，所以这条路上没有任何别的关卡。
            if host.parse::<std::net::Ipv6Addr>().is_ok() {
                format!("{scheme}://[{host}]:{p}")
            } else {
                format!("{scheme}://{host}:{p}")
            }
        };
        // 每条会话都用显式绝对 URL，**含会话 0**——见 degraded 上的注释。
        let session_bases: Vec<Option<String>> =
            ports.iter().map(|p| Some(data_origin(*p))).collect();

        prepared.push(Prepared {
            idx,
            original_url: t.server_url,
            host,
            forwarder,
            session_bases,
            upstream,
            bypass_error,
        });
    }

    if prepared.is_empty() {
        return ShardManyPlan {
            entries: entries.into_iter().map(|e| e.expect("每项都已填充")).collect(),
            guard: None,
        };
    }

    // 4) 把域名接到转发器上。两条路见模块文档。
    let proxy = match reach {
        Reach::Proxy(p) => Some(p),
        Reach::Hosts => None,
    };
    if let Some(proxy) = proxy {
        // 代理路：把 `(域名, 转发端口) -> 127.0.0.1:转发端口` 灌进改写表。
        //
        // 一次性整体替换而非逐条追加——上一代的端口映射必须随这次换代一起
        // 消失，否则会留下指向已 abort 转发器的死映射（见 `set_routes`）。
        // 这与 hosts 路「一次原子写入覆盖全部域名」是同一个理由。
        let mut routes = crate::webview_proxy::Routes::new();
        for p in &prepared {
            for port in p.forwarder.ports() {
                routes.insert(
                    (p.host.to_ascii_lowercase(), *port),
                    SocketAddr::from(([127, 0, 0, 1], *port)),
                );
            }
        }
        let n = routes.len();
        proxy.set_routes(routes);
        let names: Vec<&str> = prepared.iter().map(|p| p.host.as_str()).collect();
        tracing::info!(
            "本地条带就绪（经 WebView 代理，无需管理员权限）：{} 个出站、{n} 条改写，域名 {names:?}",
            prepared.len()
        );
        let mut forwarders = Vec::with_capacity(prepared.len());
        for p in prepared {
            entries[p.idx] = Some(ShardPlanEntry {
                session_bases: p.session_bases,
                upstream: Some(p.upstream),
                bypass_error: p.bypass_error,
            });
            forwarders.push(p.forwarder);
        }
        return ShardManyPlan {
            entries: entries.into_iter().map(|e| e.expect("每项都已填充")).collect(),
            guard: Some(ShardGuard::without_hosts(forwarders)),
        };
    }

    // hosts 路：一次 `CustodyGuard::acquire` 覆盖全部需要劫持的域名。
    // 托管交给 CustodyGuard：持有即生效，drop 即摘除，崩溃残留由下次
    // 启动的步骤 0 兜底（§10）。
    let all_hosts: Vec<String> = prepared.iter().map(|p| p.host.clone()).collect();
    match CustodyGuard::acquire(HostsCustody::new(
        hosts.clone(),
        "127.0.0.1".into(),
        all_hosts.clone(),
    )) {
        Ok(hosts_guard) => {
            tracing::info!(
                "本地条带就绪：{} 个出站参与劫持，域名 {all_hosts:?}",
                prepared.len()
            );
            let mut forwarders = Vec::with_capacity(prepared.len());
            for p in prepared {
                entries[p.idx] = Some(ShardPlanEntry {
                    session_bases: p.session_bases,
                    upstream: Some(p.upstream),
                    bypass_error: p.bypass_error,
                });
                forwarders.push(p.forwarder);
            }
            ShardManyPlan {
                entries: entries.into_iter().map(|e| e.expect("每项都已填充")).collect(),
                guard: Some(ShardGuard::new(forwarders, hosts_guard)),
            }
        }
        Err(e) => {
            // 唯一一次批量写入失败：本轮原本该被劫持的目标**全部**退回单会话
            // —— 不存在「有的看到写入结果、有的看不到」这种分裂状态。
            tracing::warn!("条带禁用：写 hosts 失败（{e:#}）——本批次全部退回单会话");
            for p in prepared {
                // 转发器在这里被丢弃：`Forwarder::drop` 会 abort 掉它的监听任务。
                drop(p.forwarder);
                entries[p.idx] = Some(ShardPlanEntry::degraded(&p.original_url));
            }
            ShardManyPlan {
                entries: entries.into_iter().map(|e| e.expect("每项都已填充")).collect(),
                guard: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(url: &str, base_port: u16, extra_sessions: usize) -> ShardTarget {
        ShardTarget {
            server_url: url.to_string(),
            base_port,
            extra_sessions,
            on_upstream: None,
        }
    }

    #[test]
    fn degraded_gives_session_zero_an_explicit_absolute_origin() {
        // 会话 0 曾经吃相对路径，前提是「承载页就是它的 origin」。
        // 承载页已经挪到本机 http 壳上，这个前提不再成立，
        // 因此降级路径也必须显式写死数据面 origin。
        let e = ShardPlanEntry::degraded("https://a.example/some/path");
        assert_eq!(
            e.session_bases,
            vec![Some("https://a.example".to_string())],
            "降级路径的会话 0 必须是绝对 origin，且路径要被剥掉"
        );
    }

    #[test]
    fn degraded_keeps_an_explicit_port_verbatim() {
        // 不归一默认端口：https://a:443 与 https://a 对 fetch 等价，
        // 但保留原样能让日志里的基址和配置文件对得上。
        let e = ShardPlanEntry::degraded("https://a.example:8443/");
        assert_eq!(e.session_bases, vec![Some("https://a.example:8443".to_string())]);
    }

    #[tokio::test]
    async fn no_session_base_is_ever_relative() {
        // 钉死「没有任何一项是 None」——None 意味着相对路径，而承载页
        // 已经不是任何出站的 origin 了。
        //
        // 走降级路径：劫持路径要真实 DNS 解析 + 真的起转发器，单测里既慢
        // 又不确定（离线环境直接降级），那条腿交给 Task 5 的真机走查。
        // 不变量本身两条路径是同一条。
        let r = plan_many(
            vec![target("https://x.com/", 18443, 0)],
            "/nonexistent".into(),
            Reach::Hosts,
        )
        .await;
        for (i, b) in r.entries[0].session_bases.iter().enumerate() {
            let b = b.as_ref().unwrap_or_else(|| panic!("会话 {i} 的基址是 None"));
            assert!(b.starts_with("https://"), "会话 {i} 的基址必须是 https：{b}");
        }
    }

    // ── 代理路与 hosts 路的分水岭 ──

    /// **纯 IP 服务端在代理路下也能拿到多会话。**
    ///
    /// 这是代理路存在的主要理由，也是 hosts 路做不到的事：hosts 只能映射
    /// 「域名→IP」，映射不了「IP→IP」，所以 `hijackable` 直接拒绝 IP 字面量。
    /// 代理路不做名字解析，只按 (host, port) 查表改写，IP 一样成立。
    ///
    /// 与下面那条 hosts 路的对照测试成对存在——单看这一条无法分辨「代理路
    /// 真的放行了」还是「这个 URL 本来就不会被拒」。
    ///
    /// 用 RFC 5737 的 TEST-NET-3（`203.0.113.x`）：保证不是环回（否则会被
    /// `is_loopback` 拦下），也保证不会真连到谁身上。
    #[tokio::test]
    async fn an_ip_literal_server_still_gets_multiple_sessions_via_the_proxy() {
        let proxy = crate::webview_proxy::WebviewProxy::spawn().await.unwrap();
        let r = plan_many(
            vec![target("https://203.0.113.7/", 18661, 3)],
            "/nonexistent".into(),
            Reach::Proxy(&proxy),
        )
        .await;
        assert_eq!(
            r.entries[0].session_count(),
            4,
            "IP 服务端在代理路下必须拿到 extra_sessions+1 条会话；\
             退化成 1 说明 needs_hijackable_host 那道门把代理路也拦了"
        );
        // 每条会话一个不同端口 = 不同 origin，这才是多 TCP 的来源
        let bases: Vec<&str> = r.entries[0]
            .session_bases
            .iter()
            .map(|b| b.as_deref().expect("基址不得为 None"))
            .collect();
        for (i, b) in bases.iter().enumerate() {
            assert_eq!(
                *b,
                format!("https://203.0.113.7:{}", 18661 + i),
                "会话 {i} 的基址端口不对——端口相同就是同一个 origin，条带归零"
            );
        }
        // 改写表要覆盖全部会话端口，否则 WebView 连过来查不到、直连一个
        // 没人监听的假端口
        assert_eq!(proxy.route_count(), 4, "改写表要覆盖每一条会话");
    }

    /// IPv6 字面量的每条会话基址都必须是**可解析的 URL**。
    ///
    /// 这条与上面那条 IPv4 的不是重复：IPv4 的 `1.2.3.4:443` 拼进 authority
    /// 天然无歧义，IPv6 的 `2001:db8::1:443` 则会被浏览器按最后一个冒号切开、
    /// 把 `2001` 当主机。少了方括号不是「基址不好看」，是**四条会话全部连不
    /// 上且不报错**——改写表的 key 存的是裸地址，WebView 拿着 host=2001
    /// 连过来永远查不到。
    #[tokio::test]
    async fn an_ipv6_literal_server_gets_bracketed_session_bases() {
        let proxy = crate::webview_proxy::WebviewProxy::spawn().await.unwrap();
        let r = plan_many(
            vec![target("https://[2001:db8::1]/", 18681, 3)],
            "/nonexistent".into(),
            Reach::Proxy(&proxy),
        )
        .await;
        assert_eq!(r.entries[0].session_count(), 4, "IPv6 服务端也该拿到多会话");
        for (i, b) in r.entries[0].session_bases.iter().enumerate() {
            let b = b.as_deref().expect("基址不得为 None");
            assert_eq!(
                b,
                format!("https://[2001:db8::1]:{}", 18681 + i),
                "会话 {i} 的 IPv6 基址缺方括号——浏览器会把 2001 当主机"
            );
            // 再把基址喂回 `split_url`（改写表的 key 就是从它来的）：解出来的
            // 主机必须是**裸地址**、端口必须是这条会话的端口。这一步才是
            // 「基址和改写表能不能对上」的判据——只比字符串的话，两边同时
            // 少括号也照样通过。
            let (_, h, p) = split_url(b).expect("基址必须能被自己的解析器解回");
            assert_eq!(h, "2001:db8::1", "解回的主机必须是裸地址");
            assert_eq!(p, 18681 + i as u16);
        }
        // 改写表的 key 是裸地址（SOCKS5 的 ATYP_IPV6 就是这么发的），
        // 与上面带括号的 URL 形式必须能对上。
        assert_eq!(proxy.route_count(), 4, "改写表要覆盖每一条 IPv6 会话");
    }

    /// 对照：同一个 IP 服务端走 hosts 路必然退回单会话。
    ///
    /// 这条不是重复——它保证上面那条测的是「代理路放行」而不是「这个 URL
    /// 谁都不会拒」。两条一起才把分水岭钉死。
    #[tokio::test]
    async fn the_same_ip_literal_server_degrades_on_the_hosts_path() {
        let r = plan_many(
            vec![target("https://203.0.113.7/", 18671, 3)],
            "/nonexistent".into(),
            Reach::Hosts,
        )
        .await;
        assert_eq!(
            r.entries[0].session_count(),
            1,
            "hosts 路对 IP 字面量无能为力，必须退回单会话"
        );
    }

    /// 代理路**一个字节都不写 hosts**。
    ///
    /// 写了的话免提权就是假的：没有管理员权限时那次写入会失败，整批目标
    /// 按现有逻辑一起退回单会话——表现为「装了新版本还是要管理员权限」。
    #[tokio::test]
    async fn the_proxy_path_never_touches_the_hosts_file() {
        // 与 custody::hosts 的测试同一套造法：临时目录 + 唯一名，不引新依赖。
        let hosts = std::env::temp_dir().join(format!(
            "wsieve-proxy-path-hosts-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let original = "127.0.0.1 localhost\n# 用户自己的行\n";
        std::fs::write(&hosts, original).unwrap();

        let proxy = crate::webview_proxy::WebviewProxy::spawn().await.unwrap();
        let r = plan_many(
            vec![target("https://proxy-path.example/", 18681, 2)],
            hosts.clone(),
            Reach::Proxy(&proxy),
        )
        .await;
        // 这个域名解析不出来 ⇒ 走降级；解析得出来 ⇒ 走代理路。无论哪条，
        // hosts 文件都不该被碰过一个字节。
        let _ = r;
        let after = std::fs::read_to_string(&hosts).unwrap();
        let _ = std::fs::remove_file(&hosts);
        assert_eq!(after, original, "代理路不得改动 hosts 文件");
    }

    #[test]
    fn url_split_covers_real_forms() {
        assert_eq!(
            split_url("https://x.com/").unwrap(),
            ("https".into(), "x.com".into(), 443)
        );
        assert_eq!(
            split_url("https://x.com:8443/path").unwrap(),
            ("https".into(), "x.com".into(), 8443)
        );
        assert_eq!(
            split_url("http://x.com").unwrap(),
            ("http".into(), "x.com".into(), 80)
        );
        assert_eq!(
            split_url("https://[::1]:9443/").unwrap(),
            ("https".into(), "::1".into(), 9443)
        );
        assert!(split_url("x.com").is_err());
        assert!(split_url("ftp://x.com").is_err());
    }

    #[test]
    fn ip_and_loopback_hosts_are_not_hijacked() {
        // IP 字面量没有「同域名不同端口仍匹配证书」这一前提。
        assert!(!hijackable("127.0.0.1"));
        assert!(!hijackable("::1"));
        assert!(!hijackable("localhost"));
        assert!(hijackable("x.com"));
    }

    #[tokio::test]
    async fn zero_extra_sessions_degrades_to_a_single_absolute_origin() {
        let r = plan_many(
            vec![target("https://x.com/", 18443, 0)],
            "/nonexistent".into(),
            Reach::Hosts,
        )
        .await;
        assert_eq!(
            r.entries[0].session_bases,
            vec![Some("https://x.com".to_string())]
        );
        assert_eq!(r.entries[0].session_count(), 1);
        assert!(r.guard.is_none());
    }

    #[tokio::test]
    async fn unwritable_hosts_degrades_instead_of_failing() {
        // 关键降级路径：没有管理员权限时必须还能连上，只是没有条带。
        let r = plan_many(
            vec![target("https://x.com/", 18443, 3)],
            "/proc/definitely-not-writable/hosts".into(),
            Reach::Hosts,
        )
        .await;
        assert_eq!(
            r.entries[0].session_bases,
            vec![Some("https://x.com".to_string())]
        );
        assert_eq!(r.entries[0].session_count(), 1);
        assert!(r.guard.is_none());
    }

    #[tokio::test]
    async fn ip_server_url_degrades() {
        let p = temp_hosts("ip-degrade", "127.0.0.1 localhost\n");
        let r = plan_many(
            vec![target("https://127.0.0.1:8443/", 18443, 3)],
            p.clone(),
            Reach::Hosts,
        )
        .await;
        assert_eq!(r.entries[0].session_count(), 1);
        assert!(r.guard.is_none());
        std::fs::remove_file(&p).ok();
    }

    /// 每个测试一份独立临时 hosts —— **绝不指向真实 /etc/hosts**。
    fn temp_hosts(tag: &str, content: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "wsieve-shard-setup-{}-{tag}",
            std::process::id()
        ));
        std::fs::write(&p, content).unwrap();
        p
    }

    /// 崩溃恢复的关键回归：**降级路径也必须清残留**。
    ///
    /// 场景：上次运行劫持了 old.com 后被 SIGKILL；这次用户把条带关了
    /// （extra_sessions=0）。旧实现在这条分支上第一行就 return，
    /// clear 永远跑不到，old.com 从此一直指向一个不在跑的转发器。
    #[tokio::test]
    async fn stale_entry_is_cleared_even_when_this_run_degrades() {
        let p = temp_hosts(
            "degrade",
            "127.0.0.1 localhost\n127.0.0.1 old.com # wsieve-managed\n",
        );
        let r = plan_many(vec![target("https://x.com/", 18443, 0)], p.clone(), Reach::Hosts).await;
        assert!(r.guard.is_none(), "条带关掉时不该有 guard");
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(!s.contains("old.com"), "降级路径也必须清掉上次的残留：{s}");
        assert_eq!(s, "127.0.0.1 localhost\n", "用户条目必须字节还原");
        std::fs::remove_file(&p).ok();
    }

    /// 同一个洞的另一条腿：服务端换成了 IP，于是本次不劫持，
    /// 但上次留下的域名条目照样得清。
    #[tokio::test]
    async fn stale_entry_is_cleared_when_host_is_no_longer_hijackable() {
        let p = temp_hosts(
            "not-hijackable",
            "127.0.0.1 localhost\n127.0.0.1 old.com # wsieve-managed\n",
        );
        let r = plan_many(
            vec![target("https://127.0.0.1:8443/", 18443, 3)],
            p.clone(),
            Reach::Hosts,
        )
        .await;
        assert!(r.guard.is_none());
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(!s.contains("old.com"), "IP 服务端也必须清掉残留：{s}");
        std::fs::remove_file(&p).ok();
    }

    /// **降级路径不得让 TUN 起来。**
    ///
    /// 降级意味着没解析、没写 bypass。此时若 TUN 照拉，转发器的第一条
    /// 出网连接就会被捕获、判「走代理」、绕回本机 —— §8.3.1 的环路，
    /// 而且是静默的。
    #[tokio::test]
    async fn a_degraded_plan_never_lets_tun_start() {
        let r = plan_many(
            vec![target("https://x.com/", 18443, 0)],
            "/nonexistent".into(),
            Reach::Hosts,
        )
        .await;
        assert!(r.entries[0].upstream.is_none(), "降级路径压根没解析");
        assert!(
            !r.entries[0].tun_may_start(),
            "没有真实 IP 就没有 bypass —— 拉起 TUN 必成环路"
        );
    }

    /// 钩子失败 ⇒ **条带照常，TUN 不起**。
    ///
    /// 两件事必须分开：转发器与 hosts 服务的是混合端口入口，没道理因为
    /// TUN 起不来就把代理整个关掉；但 bypass 没写成也绝不能拉 TUN。
    #[test]
    fn a_failing_bypass_hook_is_recorded_rather_than_swallowed() {
        let e = ShardPlanEntry {
            session_bases: vec![None],
            upstream: Some("203.0.113.7:443".parse().unwrap()),
            bypass_error: Some("must be root to alter routing table".into()),
        };
        assert!(!e.tun_may_start(), "bypass 写失败却允许拉 TUN —— 这正是环路");
        // 解析成功且钩子也成功时才放行。
        let ok = ShardPlanEntry {
            bypass_error: None,
            ..e
        };
        assert!(ok.tun_may_start());
    }

    /// 钩子**排在解析之后**：还没解析就返回的路径上，它一次都不该被调用。
    ///
    /// 这是 §8.3.2 第 2 步位置正确的直接证据 —— 钩子若被挪到解析之前，
    /// 它根本拿不到地址可写，只能写进一条指向虚空的 bypass 路由。
    #[tokio::test]
    async fn the_hook_is_not_called_on_paths_that_never_resolve() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<SocketAddr>::new()));
        let s = seen.clone();
        let hook: UpstreamHook = Arc::new(move |addr| {
            s.lock().unwrap().push(addr);
            Ok(())
        });
        // extra_sessions = 0 ⇒ 在解析之前就返回。
        let r = plan_many(
            vec![ShardTarget {
                server_url: "https://x.com/".into(),
                base_port: 18443,
                extra_sessions: 0,
                on_upstream: Some(hook),
            }],
            "/nonexistent".into(),
            Reach::Hosts,
        )
        .await;
        assert!(
            seen.lock().unwrap().is_empty(),
            "还没解析就调钩子的话，它拿不到任何地址可写"
        );
        assert!(!r.entries[0].tun_may_start());
    }

    /// **本模块存在的核心动机**：多个目标各自独立降级，下标不能互相串。
    ///
    /// 三个目标全部走降级路径（0 个目标真的需要写 hosts），刻意用不同的
    /// 降级触发原因（零会话 / IP 字面量 / 畸形 URL），确认 `entries[idx]`
    /// 的下标对齐不会因为中间有目标提前 `continue` 而错位。
    #[tokio::test]
    async fn independent_targets_degrade_without_cross_contaminating_indices() {
        let p = temp_hosts("no-cross-contam", "127.0.0.1 localhost\n");
        let r = plan_many(
            vec![
                target("https://a.example/", 18443, 0), // 零会话
                target("https://127.0.0.1:8443/", 18450, 3), // IP 字面量
                target("not-a-url", 18460, 3),           // 畸形 URL
            ],
            p.clone(),
            Reach::Hosts,
        )
        .await;
        assert!(r.guard.is_none(), "没有任何目标真的需要劫持");
        assert_eq!(r.entries.len(), 3);
        assert_eq!(
            r.entries[0].session_bases,
            vec![Some("https://a.example".to_string())]
        );
        assert_eq!(
            r.entries[1].session_bases,
            vec![Some("https://127.0.0.1:8443".to_string())]
        );
        assert_eq!(
            r.entries[2].session_bases,
            vec![Some("not-a-url".to_string())]
        );
        for e in &r.entries {
            assert_eq!(e.session_count(), 1);
            assert!(e.upstream.is_none());
        }
        std::fs::remove_file(&p).ok();
    }
}
