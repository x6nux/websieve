//! HTTP 代理入口：CONNECT 隧道与普通请求转发。
//!
//! ponytail: 普通 HTTP 请求（非 CONNECT）只处理连接上的**第一个**请求，
//! 并在转发时注入 `Connection: close`。
//! 上限：代理侧不支持 keep-alive 复用，每个普通 HTTP 请求一条连接。
//! 为什么可接受：现代工具对代理几乎一律走 CONNECT（https 是默认），
//! 普通 HTTP 主要来自 curl/apt 这类简单场景，连接开销可忽略。
//! 升级路径：要支持 keep-alive 就得完整解析每一轮请求（后续请求
//! 仍是绝对 URI 形式），届时引入 httparse 而不是手写。

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
