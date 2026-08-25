//! 本地条带的启动编排：清残留 → 解析 → 起转发器 → 写 hosts，以及失败时的降级。
//!
//! 顺序是关键（见 `plan` 的注释）：**清残留在最前面且无条件执行**，
//! 然后才解析、起转发器、最后写 hosts。任何一步失败都降级到单会话继续跑，
//! 而不是拒绝启动——与 mux 协商失败的处理一致（优先建立连接 + 警告日志）。
//!
//! hosts 条目的托管本身归 `crate::custody`（设计文档 §10），本模块只管编排。

use std::sync::Arc;

use crate::custody::hosts::{HostsCustody, HostsFile};
use crate::custody::{CustodyGuard, ManagedSystemState};
use crate::shard::{self, ShardGuard};

/// 条带编排结果。
pub struct ShardPlan {
    /// 主 WebView 应加载的 URL（劫持生效时是本地端口）。
    pub page_url: String,
    /// 各会话的请求基址；`None` 表示该会话用相对路径（同源，即会话 0）。
    pub session_bases: Vec<Option<String>>,
    /// 持有转发器与 hosts 清理职责；drop 即摘除 hosts 条目。
    pub guard: Option<ShardGuard>,
}

impl ShardPlan {
    /// 降级：不劫持、单会话、页面就是原始 URL。
    fn degraded(server_url: &str) -> Self {
        Self {
            page_url: server_url.to_string(),
            session_bases: vec![None],
            guard: None,
        }
    }

    /// 实际可用的会话数。
    #[cfg(test)]
    pub fn session_count(&self) -> usize {
        self.session_bases.len()
    }
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

/// 域名是否值得劫持。IP 字面量与环回主机不劫持：IP 没有「同域名不同端口
/// 仍匹配证书」这一前提可依赖，环回则本来就没有跨网链路可并行。
fn hijackable(host: &str) -> bool {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    !matches!(host, "localhost" | "localhost.")
}

/// 编排本地条带。`extra_sessions` 为 0 时直接走单会话原路径。
pub async fn plan(
    server_url: &str,
    shard_base_port: u16,
    extra_sessions: usize,
    hosts_path: std::path::PathBuf,
) -> ShardPlan {
    let hosts = Arc::new(HostsFile::new(hosts_path));

    // 0) 无条件先清残留 —— 在任何早退分支之前。
    //
    // 这一步与「本次要不要劫持」无关：上一次运行被 SIGKILL 时条目留在了
    // hosts 里，而本次可能因为条带被关掉、URL 变成 IP、服务端换了域名等
    // 任何理由直接降级。清理只发生在「本次也打算劫持」的路径上的话，
    // 残留就会一直留着，那个域名从此永远指向一个不在跑的转发器。
    // 这正是 §10 里 clear_stale 与 apply 分开的理由。
    if let Err(e) = HostsCustody::cleanup_only(hosts.clone()).clear_stale() {
        // 不可写属常态（没管理员权限），降到 debug；其余照常告警。
        if hosts.writable() {
            tracing::warn!("清理 hosts 残留失败（{e:#}）——若上次异常退出，条目可能仍在");
        } else {
            tracing::debug!("hosts 不可写，跳过残留清理：{e:#}");
        }
    }

    if extra_sessions == 0 {
        return ShardPlan::degraded(server_url);
    }
    let sessions = extra_sessions + 1;

    let (scheme, host, port) = match split_url(server_url) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("条带禁用：服务端 URL 解析失败（{e}）——退回单会话");
            return ShardPlan::degraded(server_url);
        }
    };
    if !hijackable(&host) {
        tracing::warn!(
            "条带禁用：主机 {host} 是 IP 或环回，不做 hosts 劫持——退回单会话"
        );
        return ShardPlan::degraded(server_url);
    }

    if !hosts.writable() {
        tracing::warn!(
            "条带禁用：{} 不可写——退回单会话。要启用多 TCP 条带，请以管理员身份运行，\
             或手动加一行：127.0.0.1 {host} # wsieve-managed",
            hosts.path().display()
        );
        return ShardPlan::degraded(server_url);
    }

    // 1) 解析真实地址。残留已在步骤 0 清掉，此刻系统解析器不受我们污染
    //    ——否则会拿到环回地址，转发器就转给自己。
    let upstream = match shard::resolve_upstream(&host, port).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("条带禁用：解析 {host}:{port} 失败（{e}）——退回单会话");
            return ShardPlan::degraded(server_url);
        }
    };
    if shard::is_loopback(&upstream) {
        // 清了残留还解析到环回，说明是别处（用户自己的 hosts 行/DNS）指过来的，
        // 劫持只会形成自环。
        tracing::warn!("条带禁用：{host} 解析到环回地址 {upstream}——退回单会话");
        return ShardPlan::degraded(server_url);
    }

    // 2) 起转发器，成功后才写 hosts——反过来的话，转发器起不来时域名已被
    //    指向本地，本机访问该域名会全部失败。
    let forwarder = match shard::spawn(shard_base_port, sessions, upstream).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("条带禁用：本地转发器启动失败（{e}）——退回单会话");
            return ShardPlan::degraded(server_url);
        }
    };
    // 3) 写 hosts。托管交给 CustodyGuard：持有即生效，drop 即摘除，
    //    崩溃残留由下次启动的步骤 0 兜底（§10）。
    let custody = match CustodyGuard::acquire(HostsCustody::new(
        hosts.clone(),
        "127.0.0.1".into(),
        vec![host.clone()],
    )) {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("条带禁用：写 hosts 失败（{e:#}）——退回单会话");
            return ShardPlan::degraded(server_url);
        }
    };

    let ports = forwarder.ports().to_vec();
    let origin = |p: u16| format!("{scheme}://{host}:{p}");
    // 会话 0 用相对路径：页面本身就加载自该 origin，保持完全同源
    // （spec §6.7），只有额外会话才跨源。
    let mut session_bases = vec![None];
    session_bases.extend(ports.iter().skip(1).map(|p| Some(origin(*p))));

    tracing::info!(
        "本地条带就绪：{} 个会话，端口 {:?} → {upstream}",
        sessions,
        ports
    );
    ShardPlan {
        page_url: format!("{}/", origin(ports[0])),
        session_bases,
        guard: Some(ShardGuard::new(forwarder, custody)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn zero_extra_sessions_keeps_original_url() {
        let p = plan("https://x.com/", 18443, 0, "/nonexistent".into()).await;
        assert_eq!(p.page_url, "https://x.com/");
        assert_eq!(p.session_count(), 1);
        assert!(p.guard.is_none());
    }

    #[tokio::test]
    async fn unwritable_hosts_degrades_instead_of_failing() {
        // 关键降级路径：没有管理员权限时必须还能连上，只是没有条带。
        let p = plan(
            "https://x.com/",
            18443,
            3,
            "/proc/definitely-not-writable/hosts".into(),
        )
        .await;
        assert_eq!(p.page_url, "https://x.com/");
        assert_eq!(p.session_count(), 1);
        assert!(p.guard.is_none());
    }

    #[tokio::test]
    async fn ip_server_url_degrades() {
        let p = temp_hosts("ip-degrade", "127.0.0.1 localhost\n");
        let plan0 = plan("https://127.0.0.1:8443/", 18443, 3, p.clone()).await;
        assert_eq!(plan0.session_count(), 1);
        assert!(plan0.guard.is_none());
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
        let plan0 = plan("https://x.com/", 18443, 0, p.clone()).await;
        assert!(plan0.guard.is_none(), "条带关掉时不该有 guard");
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
        let plan0 = plan("https://127.0.0.1:8443/", 18443, 3, p.clone()).await;
        assert!(plan0.guard.is_none());
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(!s.contains("old.com"), "IP 服务端也必须清掉残留：{s}");
        std::fs::remove_file(&p).ok();
    }
}
