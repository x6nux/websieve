//! 伪装处理器（spec §8）：所有未通过认证的请求的唯一出口。
//!
//! 两种形态：
//! 1. 默认：内嵌 nginx 欢迎页 + nginx 风格 404，`Server: nginx` 头；
//! 2. `disguise.upstream`：配置后所有未认证请求（含 `/api/*` 上的无效请求）
//!    反向代理到上游站点——透传 method/path/query/常规头/body，回传上游
//!    状态码/头/body（缓存策略由 CDN 侧负责，服务端不代管，spec §8 ponytail 注记）。
//!
//! 关键不变量：任何失败路径（解密失败/重放/无会话/版本不符）与普通请求
//! 走同一出口，响应无差别——不给探测者任何判别面。

use axum::body::Body;
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use bytes::Bytes;

/// 内嵌的 nginx 官方欢迎页副本。
const NGINX_INDEX: &str = include_str!("../assets/nginx/index.html");

/// 透传到上游时需要剥掉的逐跳头（RFC 7230）。
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
];

/// nginx 风格 404 页（与 nginx 默认 404 body 同形）。
fn nginx_404_body() -> &'static str {
    "<html>\r\n<head><title>404 Not Found</title></head>\r\n<body>\r\n<center><h1>404 Not Found</h1></center>\r\n<hr><center>nginx</center>\r\n</body>\r\n</html>\r\n"
}

/// 给响应盖 `Server: nginx`（不带版本号）。
fn with_nginx_server(mut resp: Response<Body>) -> Response<Body> {
    resp.headers_mut().insert(
        header::SERVER,
        HeaderValue::from_static("nginx"),
    );
    resp
}

/// 未认证请求统一处理（内嵌页模式）：`GET /` → 欢迎页；其余 → 404。
pub async fn handle_static(method: &str, path: &str) -> Response<Body> {
    if method == "GET" && (path == "/" || path == "/index.html") {
        let resp = (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html")],
            NGINX_INDEX,
        )
            .into_response();
        with_nginx_server(resp)
    } else {
        let resp = (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/html")],
            nginx_404_body(),
        )
            .into_response();
        with_nginx_server(resp)
    }
}

/// 未认证请求统一处理（upstream 反代模式）：整包转发到上游。
/// 上游不可达/超时 → 回落到内嵌 nginx 404（可用性优先，不暴露上游故障细节）。
pub async fn handle_upstream(
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
    body: Bytes,
    base: &str,
) -> Response<Body> {
    let url = format!("{base}{uri}");
    let mut req = reqwest::Client::new()
        .request(
            reqwest::Method::from_bytes(method.as_bytes())
                .unwrap_or(reqwest::Method::GET),
            &url,
        )
        .body(body)
        .timeout(std::time::Duration::from_secs(15));
    for (k, v) in headers {
        if !HOP_BY_HOP.contains(&k.as_str()) {
            req = req.header(&k, &v);
        }
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(_) => {
            // 回落：与内嵌模式的未知路径同形
            return handle_static("X", "/").await;
        }
    };
    relay(resp).await
}

/// 上游响应 → axum 响应（状态/头/body 透传，剥逐跳头）。
async fn relay(resp: reqwest::Response) -> Response<Body> {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    for (name, value) in resp.headers().iter() {
        let n = name.as_str();
        if HOP_BY_HOP.contains(&n) || n == "content-length" {
            continue; // content-length 由 body 重新决定
        }
        if let Ok(v) = value.to_str() {
            builder = builder.header(n, v);
        }
    }
    let body = resp.bytes().await.unwrap_or_default();
    builder
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}
