//! 上游地址解析：把配置里的 nameserver 字符串变成 hickory 的 NameServerConfig。
//!
//! 纪律②（设计文档 §7.2）：**DoH 上游一律用 IP 字面量**。否则「DoH 服务器
//! 自己的域名由谁解析」就成了一个先有鸡还是先有蛋的问题。本模块把这条纪律
//! 变成**解析期的硬错误**——写了域名的配置根本加载不进来，而不是运行时才炸。

use std::net::IpAddr;
use std::sync::Arc;

use hickory_resolver::config::{ConnectionConfig, NameServerConfig, ProtocolConfig};

#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error("nameserver「{0}」为空")]
    Empty(String),
    #[error(
        "nameserver「{spec}」的主机部分「{host}」不是 IP 字面量。\
         DoH/DoT 上游必须写成 IP（如 https://1.1.1.1/dns-query），\
         否则解析这台 DNS 服务器的域名本身又需要一次 DNS 查询，\
         形成先有鸡还是先有蛋的死循环（设计文档 §7.2 纪律②）"
    )]
    NotAnIpLiteral { spec: String, host: String },
    #[error("nameserver「{spec}」的端口「{port}」非法")]
    BadPort { spec: String, port: String },
    #[error("nameserver「{spec}」使用了不支持的协议前缀。支持：https:// · tls:// · udp:// · tcp:// · 裸 IP")]
    UnknownScheme { spec: String },
}

/// 把一条配置字符串解析成 hickory 的上游配置。
///
/// 支持的写法：
/// - `https://1.1.1.1/dns-query`  → DoH（默认 443 端口）
/// - `tls://1.1.1.1`             → DoT（默认 853 端口）
/// - `udp://1.1.1.1` / `1.1.1.1` → 明文 UDP+TCP（默认 53 端口）
/// - `tcp://1.1.1.1`             → 明文 TCP
/// - 端口可显式覆盖：`https://1.1.1.1:8443/dns-query`
/// - IPv6 用方括号：`https://[2606:4700:4700::1111]/dns-query`
pub fn parse_nameserver(spec: &str) -> Result<NameServerConfig, UpstreamError> {
    let s = spec.trim();
    if s.is_empty() {
        return Err(UpstreamError::Empty(spec.to_string()));
    }

    let (scheme, rest) = match s.split_once("://") {
        Some((sc, r)) => (sc.to_ascii_lowercase(), r),
        // 裸 IP 视为 udp
        None => ("udp".to_string(), s),
    };

    // 先切掉路径，再切端口
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (rest, None),
    };
    let (host, port) = split_host_port(authority).map_err(|port| UpstreamError::BadPort {
        spec: spec.to_string(),
        port,
    })?;

    // 纪律②：主机必须是 IP 字面量
    let ip: IpAddr = host.parse().map_err(|_| UpstreamError::NotAnIpLiteral {
        spec: spec.to_string(),
        host: host.to_string(),
    })?;

    // server_name 就用 IP 的字符串形式：证书里 1.1.1.1 / 8.8.8.8 / 9.9.9.9
    // 都带有 IP SAN，实测可通过校验。
    let sni: Arc<str> = Arc::from(ip.to_string().as_str());

    let protocol = match scheme.as_str() {
        "https" | "h2" => ProtocolConfig::Https {
            server_name: sni,
            // 路径缺省用 /dns-query，与 RFC 8484 的惯例一致
            path: Arc::from(
                match path {
                    Some(p) if !p.is_empty() => format!("/{p}"),
                    _ => "/dns-query".to_string(),
                }
                .as_str(),
            ),
        },
        "tls" | "dot" => ProtocolConfig::Tls { server_name: sni },
        "udp" => ProtocolConfig::Udp,
        "tcp" => ProtocolConfig::Tcp,
        _ => {
            return Err(UpstreamError::UnknownScheme {
                spec: spec.to_string(),
            })
        }
    };

    let mut conn = ConnectionConfig::new(protocol);
    if let Some(p) = port {
        conn.port = p;
    }

    // udp 额外配一条 tcp：被截断的应答（TC 位）要能退回 TCP 重问。
    let connections = if scheme == "udp" {
        let mut tcp = ConnectionConfig::new(ProtocolConfig::Tcp);
        if let Some(p) = port {
            tcp.port = p;
        }
        vec![conn, tcp]
    } else {
        vec![conn]
    };

    Ok(NameServerConfig::new(ip, true, connections))
}

/// 拆 `host[:port]`，IPv6 用 `[...]` 包裹。
/// 返回 Err(端口原文) 表示端口段存在但解析失败。
fn split_host_port(authority: &str) -> Result<(&str, Option<u16>), String> {
    if let Some(rest) = authority.strip_prefix('[') {
        // IPv6 字面量
        let (host, tail) = rest.split_once(']').ok_or_else(|| authority.to_string())?;
        let port = match tail.strip_prefix(':') {
            Some(p) => Some(p.parse::<u16>().map_err(|_| p.to_string())?),
            None => None,
        };
        return Ok((host, port));
    }
    // IPv4 或裸 IPv6（无端口）。裸 IPv6 含多个冒号，不能当端口分隔符。
    match authority.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => {
            Ok((h, Some(p.parse::<u16>().map_err(|_| p.to_string())?)))
        }
        _ => Ok((authority, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(spec: &str) -> NameServerConfig {
        parse_nameserver(spec).unwrap_or_else(|e| panic!("解析 {spec:?} 失败：{e}"))
    }

    #[test]
    fn doh_ip_literal_is_accepted() {
        let ns = ok("https://1.1.1.1/dns-query");
        assert_eq!(ns.ip, "1.1.1.1".parse::<IpAddr>().unwrap());
        assert_eq!(ns.connections.len(), 1);
        assert_eq!(ns.connections[0].port, 443);
        match &ns.connections[0].protocol {
            ProtocolConfig::Https { server_name, path } => {
                assert_eq!(&**server_name, "1.1.1.1", "SNI 必须是 IP 本身");
                assert_eq!(&**path, "/dns-query");
            }
            other => panic!("应是 Https，实为 {other:?}"),
        }
    }

    #[test]
    fn doh_with_hostname_is_rejected_naming_the_host() {
        // 纪律②：这是本模块存在的理由，必须在加载期就炸
        let e = parse_nameserver("https://cloudflare-dns.com/dns-query")
            .unwrap_err()
            .to_string();
        assert!(e.contains("cloudflare-dns.com"), "要点名冒犯的主机：{e}");
        assert!(e.contains("IP"), "要说明为什么：{e}");
    }

    #[test]
    fn plain_ip_defaults_to_udp_plus_tcp() {
        let ns = ok("1.1.1.1");
        assert_eq!(ns.connections.len(), 2, "UDP 要配一条 TCP 兜截断应答");
        assert_eq!(ns.connections[0].port, 53);
        assert!(matches!(ns.connections[0].protocol, ProtocolConfig::Udp));
        assert!(matches!(ns.connections[1].protocol, ProtocolConfig::Tcp));
    }

    #[test]
    fn dot_uses_853() {
        let ns = ok("tls://9.9.9.9");
        assert_eq!(ns.connections[0].port, 853);
    }

    #[test]
    fn explicit_port_overrides_default() {
        let ns = ok("https://1.1.1.1:8443/dns-query");
        assert_eq!(ns.connections[0].port, 8443);
    }

    #[test]
    fn ipv6_literal_in_brackets() {
        let ns = ok("https://[2606:4700:4700::1111]/dns-query");
        assert_eq!(ns.ip, "2606:4700:4700::1111".parse::<IpAddr>().unwrap());
        assert_eq!(ns.connections[0].port, 443);
    }

    #[test]
    fn bare_ipv6_without_port_is_not_mistaken_for_host_colon_port() {
        // 裸 IPv6 有一堆冒号，不能把最后一段当端口
        let ns = ok("2606:4700:4700::1111");
        assert_eq!(ns.ip, "2606:4700:4700::1111".parse::<IpAddr>().unwrap());
        assert_eq!(ns.connections[0].port, 53);
    }

    #[test]
    fn missing_path_defaults_to_dns_query() {
        let ns = ok("https://8.8.8.8");
        match &ns.connections[0].protocol {
            ProtocolConfig::Https { path, .. } => assert_eq!(&**path, "/dns-query"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn custom_path_is_kept() {
        let ns = ok("https://8.8.8.8/resolve");
        match &ns.connections[0].protocol {
            ProtocolConfig::Https { path, .. } => assert_eq!(&**path, "/resolve"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_scheme_is_rejected() {
        assert!(parse_nameserver("quic://1.1.1.1").is_err());
        assert!(parse_nameserver("ftp://1.1.1.1").is_err());
    }

    #[test]
    fn empty_is_rejected() {
        assert!(parse_nameserver("   ").is_err());
    }

    #[test]
    fn bad_port_is_rejected_not_silently_defaulted() {
        assert!(parse_nameserver("https://1.1.1.1:99999/dns-query").is_err());
        assert!(parse_nameserver("https://1.1.1.1:abc/dns-query").is_err());
    }
}
