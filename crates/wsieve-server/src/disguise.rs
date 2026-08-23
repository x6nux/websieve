//! 伪装处理器（spec §8）：所有未通过认证的请求的唯一出口。
//! Task 13 范围：内嵌 nginx 欢迎页 + nginx 风格 404；上游反代在 Task 14 落地。
//!
//! 关键不变量：任何失败路径（解密失败/重放/无会话/版本不符）与普通请求
//! 走到这里，响应无差别——不给探测者任何判别面。

use axum::body::Body;
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;

/// 内嵌的 nginx 官方欢迎页副本。
const NGINX_INDEX: &str = include_str!("../assets/nginx/index.html");

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

/// 未认证请求统一处理：`GET /` → 欢迎页；其余 → 404。
pub async fn handle(method: &str, path: &str) -> Response<Body> {
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
