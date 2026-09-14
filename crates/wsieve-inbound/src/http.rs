//! HTTP 代理入口：CONNECT 隧道与普通请求转发。
//!
//! ponytail: 普通 HTTP 请求（非 CONNECT）只处理连接上的**第一个**请求，
//! 并在转发时注入 `Connection: close`。
//! 上限：代理侧不支持 keep-alive 复用，每个普通 HTTP 请求一条连接。
//! 为什么可接受：现代工具对代理几乎一律走 CONNECT（https 是默认），
//! 普通 HTTP 主要来自 curl/apt 这类简单场景，连接开销可忽略。
//! 升级路径：要支持 keep-alive 就得完整解析每一轮请求（后续请求
//! 仍是绝对 URI 形式），届时引入 httparse 而不是手写。

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use wsieve_proto::addr::{AddrPort, TargetAddr};

/// 请求头（含请求行）总字节上限。
///
/// 混合端口对内网开放，任何人都能连上来。没有上限的话，一个只发头
/// 不发空行的客户端能把内存喂到 OOM —— 一条连接拖垮整个进程。
pub const MAX_HEAD_BYTES: usize = 64 * 1024;

/// 读完整个请求头的时间上限。
///
/// 与 [`crate::sniff::DEFAULT_SNIFF_TIMEOUT`] 是两道不同的闸：嗅探那道
/// 只保证「说了第一个字节」，这道保证「把头说完了」。慢速头攻击
/// （Slowloris）正是卡在两者之间 —— 每隔几秒挤出一个字节。
pub const HEAD_READ_TIMEOUT: Duration = Duration::from_secs(30);


#[derive(Debug, PartialEq, Eq)]
pub enum HttpRequest {
    Connect {
        host: String,
        port: u16,
    },
    Plain {
        host: String,
        port: u16,
        /// 请求行已改写为 origin-form（上游服务器要的形态）
        rewritten: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("请求行格式非法：{0}")]
    BadRequestLine(String),
    #[error("不是代理请求：请求行使用了 origin-form，代理需要绝对 URI")]
    NotAProxyRequest,
    #[error("非法端口：{0}")]
    BadPort(String),
}

pub fn parse_request_line(line: &str) -> Result<HttpRequest, HttpError> {
    let mut parts = line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| HttpError::BadRequestLine(line.into()))?;
    let uri = parts
        .next()
        .ok_or_else(|| HttpError::BadRequestLine(line.into()))?;
    let version = parts.next().unwrap_or("HTTP/1.1");

    if method.eq_ignore_ascii_case("CONNECT") {
        // CONNECT 的目标形如 host:port，端口省略时默认 443
        let (host, port) = split_host_port(uri, 443)?;
        return Ok(HttpRequest::Connect { host, port });
    }

    // 其余方法必须是绝对 URI（代理请求的形态）
    let (scheme, rest) = uri.split_once("://").ok_or(HttpError::NotAProxyRequest)?;
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "https" => 443,
        _ => 80,
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"), // 没有路径时补 "/"，空串会让上游 400
    };
    let (host, port) = split_host_port(authority, default_port)?;
    Ok(HttpRequest::Plain {
        host,
        port,
        rewritten: format!("{method} {path} {version}"),
    })
}

fn split_host_port(s: &str, default: u16) -> Result<(String, u16), HttpError> {
    // IPv6 字面量形如 [::1]:8080
    if let Some(rest) = s.strip_prefix('[') {
        let (h, tail) = rest
            .split_once(']')
            .ok_or_else(|| HttpError::BadRequestLine(s.into()))?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| HttpError::BadPort(p.into()))?,
            None => default,
        };
        if h.is_empty() {
            return Err(HttpError::BadRequestLine(s.into()));
        }
        return Ok((h.to_string(), port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => Ok((
            h.to_string(),
            p.parse().map_err(|_| HttpError::BadPort(p.into()))?,
        )),
        // 主机名为空（":443"）或整串为空：都不是一个能连的目标。
        // 不能落到下面的默认分支——那会把 ":443" 整个当成主机名，
        // 于是一个明显非法的请求被伪装成「看起来像样」的目标送进判决层。
        Some(_) => Err(HttpError::BadRequestLine(s.into())),
        None => {
            if s.is_empty() {
                return Err(HttpError::BadRequestLine(s.into()));
            }
            Ok((s.to_string(), default))
        }
    }
}

/// 把 `host:port` 转成下游判决层要的 [`AddrPort`]。
///
/// 主机是 IP 字面量时转成 V4/V6，否则当域名。域名长度受 SOCKS5 地址
/// 编码限制（一字节长度前缀），超过 255 的直接拒绝而不是截断 ——
/// 截断出来的域名会指向一个完全不同的地方。
pub fn to_addr_port(host: &str, port: u16) -> Result<AddrPort, HttpError> {
    let addr = match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => TargetAddr::V4(v4.octets()),
        Ok(std::net::IpAddr::V6(v6)) => TargetAddr::V6(v6.octets()),
        Err(_) => {
            if host.is_empty() || host.len() > 255 {
                return Err(HttpError::BadRequestLine(host.into()));
            }
            TargetAddr::Domain(host.to_string())
        }
    };
    Ok(AddrPort { addr, port })
}

/// 读到 `\r\n\r\n` 为止的请求头。
///
/// 返回 (头部字节, 头部之后已经读到的字节)。第二项不能丢：客户端常常
/// 把头和紧随其后的载荷放在同一个 TCP 分段里发出（CONNECT 之后抢跑的
/// TLS ClientHello、POST 的请求体）。丢掉它请求就会莫名其妙地卡住。
async fn read_head(tcp: &mut TcpStream) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(i) = find_head_end(&buf) {
            let rest = buf.split_off(i);
            return Ok((buf, rest));
        }
        if buf.len() >= MAX_HEAD_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("请求头超过上限 {MAX_HEAD_BYTES} 字节仍未结束"),
            ));
        }
        let n = match tokio::time::timeout(HEAD_READ_TIMEOUT, tcp.read(&mut chunk)).await {
            Ok(r) => r?,
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("读取请求头超过 {HEAD_READ_TIMEOUT:?} 未完成"),
                ))
            }
        };
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "请求头未结束对端即关闭",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// 找到 `\r\n\r\n` 之后的位置。返回 None 表示头还没读完。
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// 把头部拆成请求行与其余头字段行。
fn split_head(head: &str) -> Option<(&str, Vec<&str>)> {
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    Some((request_line, lines.filter(|l| !l.is_empty()).collect()))
}

/// 转发给上游的头字段：剔除逐跳字段，注入 `Connection: close`。
///
/// `Proxy-Connection` / `Proxy-Authorization` 是代理与客户端之间的事，
/// 原样透传给源站等于把代理的存在泄漏出去。
fn rebuild_headers(fields: &[&str]) -> String {
    let mut out = String::new();
    for line in fields {
        let name = line.split(':').next().unwrap_or("").trim();
        if name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("proxy-connection")
            || name.eq_ignore_ascii_case("proxy-authorization")
            || name.eq_ignore_ascii_case("keep-alive")
        {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    // ponytail 的上限在此显形：一条连接一个请求，所以显式 close
    out.push_str("Connection: close\r\n");
    out
}

/// 在单条已建立的连接上处理一个 HTTP 代理请求。
///
/// 失败一律给客户端一个合乎 HTTP 语义的响应再关闭，而不是静默断开 ——
/// 设计文档 §6.4：拒绝要让客户端明确知道。
pub async fn serve_conn(mut tcp: TcpStream, dispatch: crate::Dispatch) -> anyhow::Result<()> {
    let (head, mut leftover) = match read_head(&mut tcp).await {
        Ok(v) => v,
        Err(e) => {
            // 头都没读完，能回什么取决于错在哪；一律给 400 好过什么都不说
            let _ = tcp.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            return Err(e.into());
        }
    };

    let head_str = match std::str::from_utf8(&head) {
        Ok(s) => s,
        Err(_) => {
            let _ = tcp.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            anyhow::bail!("请求头不是合法 UTF-8");
        }
    };
    let Some((request_line, fields)) = split_head(head_str) else {
        let _ = tcp.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
        anyhow::bail!("请求头为空");
    };

    let parsed = match parse_request_line(request_line) {
        Ok(p) => p,
        Err(e) => {
            let _ = tcp.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            return Err(e.into());
        }
    };

    let (target, upstream_prelude) = match parsed {
        HttpRequest::Connect { host, port } => (to_addr_port(&host, port), None),
        HttpRequest::Plain {
            host,
            port,
            rewritten,
        } => {
            let mut prelude = String::with_capacity(head_str.len() + 32);
            prelude.push_str(&rewritten);
            prelude.push_str("\r\n");
            prelude.push_str(&rebuild_headers(&fields));
            prelude.push_str("\r\n");
            (to_addr_port(&host, port), Some(prelude))
        }
    };
    let target = match target {
        Ok(t) => t,
        Err(e) => {
            let _ = tcp.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            return Err(e.into());
        }
    };
    let is_connect = upstream_prelude.is_none();

    let mut upstream = match dispatch(target).await {
        Ok(s) => s,
        Err(e) => {
            // 判决为拒绝、或出站不可用。§6.4：拒绝而非静默回退，
            // 且要让客户端知道是代理侧拒绝的，不是网络抽风。
            tcp.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await?;
            return Err(e.into());
        }
    };

    if is_connect {
        tcp.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
    } else if let Some(prelude) = upstream_prelude {
        upstream.write_all(prelude.as_bytes()).await?;
    }

    // 头之后已读到的字节要补给上游：CONNECT 场景是客户端抢跑的载荷，
    // 普通请求场景是请求体。这两类都不在 copy_bidirectional 的视野里，
    // 因为它们早已离开了 socket 缓冲区。
    if !leftover.is_empty() {
        upstream.write_all(&leftover).await?;
        leftover.clear();
    }

    tokio::io::copy_bidirectional(&mut tcp, &mut upstream).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_target_is_parsed() {
        let r = parse_request_line("CONNECT example.com:443 HTTP/1.1").unwrap();
        assert!(matches!(r, HttpRequest::Connect { ref host, port: 443 } if host == "example.com"));
    }

    #[test]
    fn connect_without_port_defaults_to_443() {
        let r = parse_request_line("CONNECT example.com HTTP/1.1").unwrap();
        assert!(matches!(r, HttpRequest::Connect { port: 443, .. }));
    }

    #[test]
    fn absolute_uri_is_split_into_target_and_origin_form() {
        let r = parse_request_line("GET http://example.com/a/b?c=1 HTTP/1.1").unwrap();
        match r {
            HttpRequest::Plain {
                host,
                port,
                rewritten,
            } => {
                assert_eq!(host, "example.com");
                assert_eq!(port, 80, "http 默认 80");
                assert_eq!(rewritten, "GET /a/b?c=1 HTTP/1.1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn absolute_uri_with_explicit_port() {
        let r = parse_request_line("GET http://example.com:8080/x HTTP/1.1").unwrap();
        assert!(matches!(r, HttpRequest::Plain { port: 8080, .. }));
    }

    #[test]
    fn root_path_becomes_slash_not_empty() {
        // http://example.com → 路径是 "/"，不是空串（空串会让上游 400）
        let r = parse_request_line("GET http://example.com HTTP/1.1").unwrap();
        match r {
            HttpRequest::Plain { rewritten, .. } => assert_eq!(rewritten, "GET / HTTP/1.1"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn origin_form_without_absolute_uri_is_rejected() {
        // 直连服务器的请求形态，不是代理请求 —— 明确报错好过转发到虚空
        assert!(parse_request_line("GET /a/b HTTP/1.1").is_err());
    }

    #[test]
    fn https_absolute_uri_defaults_to_443() {
        let r = parse_request_line("GET https://example.com/x HTTP/1.1").unwrap();
        assert!(matches!(r, HttpRequest::Plain { port: 443, .. }));
    }

    #[test]
    fn malformed_lines_are_rejected() {
        assert!(parse_request_line("").is_err());
        assert!(parse_request_line("GET").is_err());
        assert!(parse_request_line("CONNECT").is_err());
    }

    // —— 以下几组是解析层的垃圾输入边界 ——

    #[test]
    fn bad_ports_are_rejected_not_silently_defaulted() {
        // 端口非数字 / 越界都必须报错。静默回落到默认端口会把请求
        // 送到一个客户端从未要求过的地方 —— 那比失败更糟。
        for line in [
            "CONNECT example.com:http HTTP/1.1",
            "CONNECT example.com: HTTP/1.1",
            "CONNECT example.com:65536 HTTP/1.1",
            "CONNECT example.com:-1 HTTP/1.1",
            "GET http://example.com:99999/x HTTP/1.1",
        ] {
            assert!(
                matches!(parse_request_line(line), Err(HttpError::BadPort(_))),
                "应报 BadPort：{line}"
            );
        }
    }

    #[test]
    fn ipv6_literals_are_parsed() {
        let r = parse_request_line("CONNECT [::1]:8080 HTTP/1.1").unwrap();
        assert!(matches!(r, HttpRequest::Connect { ref host, port: 8080 } if host == "::1"));

        // 无端口时用默认
        let r = parse_request_line("CONNECT [2001:db8::1] HTTP/1.1").unwrap();
        assert!(matches!(r, HttpRequest::Connect { port: 443, .. }));

        // 绝对 URI 里的 IPv6
        let r = parse_request_line("GET http://[::1]:8080/x HTTP/1.1").unwrap();
        match r {
            HttpRequest::Plain {
                host,
                port,
                rewritten,
            } => {
                assert_eq!(host, "::1");
                assert_eq!(port, 8080);
                assert_eq!(rewritten, "GET /x HTTP/1.1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn malformed_authority_is_rejected() {
        // 缺右括号的 IPv6、空主机名 —— 都不能变成一个「看起来像样」的目标
        assert!(parse_request_line("CONNECT [::1:8080 HTTP/1.1").is_err());
        assert!(parse_request_line("CONNECT []:80 HTTP/1.1").is_err());
        assert!(parse_request_line("CONNECT :443 HTTP/1.1").is_err());
        assert!(parse_request_line("GET http:// HTTP/1.1").is_err());
        assert!(parse_request_line("GET http:///path HTTP/1.1").is_err());
    }

    #[test]
    fn method_case_and_unknown_schemes() {
        // CONNECT 大小写不敏感（RFC 上方法名区分大小写，但这里宽容处理不会引入歧义）
        assert!(matches!(
            parse_request_line("connect example.com:443 HTTP/1.1").unwrap(),
            HttpRequest::Connect { port: 443, .. }
        ));
        // 未知 scheme 回落到 80，而不是报错 —— 目标主机仍然是明确的
        let r = parse_request_line("GET ftp://example.com/x HTTP/1.1").unwrap();
        assert!(matches!(r, HttpRequest::Plain { port: 80, .. }));
    }

    #[test]
    fn version_is_preserved_in_rewritten_line() {
        // 改写只动 URI，版本必须原样带过去；HTTP/1.0 客户端不能被悄悄升级
        let r = parse_request_line("GET http://example.com/x HTTP/1.0").unwrap();
        match r {
            HttpRequest::Plain { rewritten, .. } => assert_eq!(rewritten, "GET /x HTTP/1.0"),
            other => panic!("{other:?}"),
        }
    }
}
