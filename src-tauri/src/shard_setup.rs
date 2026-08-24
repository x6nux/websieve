//! 本地条带的启动编排：解析 → 写 hosts → 起转发器，以及失败时的降级。
//!
//! 顺序是关键（见 `plan` 的注释）：先清残留、再解析、最后写 hosts。任何一步
//! 失败都降级到单会话继续跑，而不是拒绝启动——与 mux 协商失败的处理一致
//! （优先建立连接 + 警告日志）。

use std::sync::Arc;

use crate::hosts::HostsFile;
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

    let hosts = Arc::new(HostsFile::new(hosts_path));
    if !hosts.writable() {
        tracing::warn!(
            "条带禁用：{} 不可写——退回单会话。要启用多 TCP 条带，请以管理员身份运行，\
             或手动加一行：127.0.0.1 {host} # wsieve-managed",
            hosts.path().display()
        );
        return ShardPlan::degraded(server_url);
    }

    // 1) 先清残留：上一次运行若非正常退出，hosts 里还指着 127.0.0.1，
    //    此时解析会拿到环回地址，转发器就会转给自己。
    if let Err(e) = hosts.clear_managed() {
        tracing::warn!("条带禁用：清理 hosts 残留失败（{e}）——退回单会话");
        return ShardPlan::degraded(server_url);
    }

    // 2) 再解析真实地址（此刻系统解析器已不受我们污染）。
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

    // 3) 起转发器，成功后才写 hosts——反过来的话，转发器起不来时域名已被
    //    指向本地，本机访问该域名会全部失败。
    let forwarder = match shard::spawn(shard_base_port, sessions, upstream).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("条带禁用：本地转发器启动失败（{e}）——退回单会话");
            return ShardPlan::degraded(server_url);
        }
    };
    if let Err(e) = hosts.set_managed("127.0.0.1", &[host.clone()]) {
        tracing::warn!("条带禁用：写 hosts 失败（{e}）——退回单会话");
        return ShardPlan::degraded(server_url);
    }

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
        guard: Some(ShardGuard::new(forwarder, hosts)),
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
        let p = plan("https://127.0.0.1:8443/", 18443, 3, "/tmp/x".into()).await;
        assert_eq!(p.session_count(), 1);
        assert!(p.guard.is_none());
    }
}
