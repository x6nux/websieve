//! 本地承载页 server（设计文档 2026-09-10 §5.1）。
//!
//! 存在的唯一理由是 origin 的 **scheme**：WKWebView 禁止 https 页面访问
//! custom scheme，Tauri 的 IPC raw body 快路径因此在远程承载页上根本发不
//! 出去（四格实测对照见 `scripts/spike-ipc-origin.sh`）。承载页换成
//! `http://127.0.0.1:{port}` 就能拿回 raw，省掉 base64 的 1.333 倍膨胀与
//! 一次全量编码——实测 48.8 MB/s @CPU 86% → 114.9 MB/s @CPU 34%。
//!
//! **它不碰数据面。** 页面加载完这个 server 就下班了：代理的每一个请求都由
//! WebView 用绝对 URL 直接发往真实服务端，TLS 仍由 WebKit 本体握手。一旦让
//! 数据面走进这里，握手就变成 rustls 发的，整个项目「用真实浏览器指纹」的
//! 前提当场作废——与 `crate::shard` 那条「绝不终结 TLS」同源。

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 承载页的 HTML。
///
/// **内容对伪装零影响**：它由本地直接响应，从不出网，网络上没有任何观察者
/// 能看到它。emitter 由 `initialization_script` 注入（见 `bootstrap::loader_js`），
/// 所以这里一个 `<script>` 都不需要。
#[allow(dead_code)] // Task 4 接线之前，回复内容还没有生产调用方在读它
const PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>websieve</title>";

/// 承载页 server 句柄：drop 即停（accept 任务随 abort 结束）。
pub struct CarrierPage {
    port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl CarrierPage {
    /// 实际监听到的端口。
    ///
    /// 逐项 `allow(dead_code)`：生产代码只用 `url()` 拼给 WebView 导航，
    /// 端口号本身没有调用方。此方法留给测试与排障 —— 查监听用端口号
    /// 比反解析 URL 直接。见 `carrier.rs::page_url_for` 上关于逐项 allow
    /// 而非整模块 allow 的说明。
    #[allow(dead_code)]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 承载 WebView 应加载的 URL。
    ///
    /// 带尾斜杠：拼路径的调用方不需要再判断要不要补，而 `origin_of` 一类的
    /// 解析都会把它吃掉。
    #[allow(dead_code)] // Task 4 才会把它接进建窗逻辑，现在只有测试在调
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }
}

impl Drop for CarrierPage {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// 在 `127.0.0.1` 上起承载页 server，**端口由 OS 分配**。
///
/// 端口既不能固定也不能复用 `shard_base_port` 段：
/// - 固定端口是本机指纹，别的进程扫到就知道装了什么；
/// - 条带段的每个端口都是 TCP 转发器，拿明文 HTTP 去打它等于把 HTTP 请求
///   塞给真实服务端的 TLS 端口。
#[allow(dead_code)] // Task 4 才会在启动序列里调用它，现在只有测试在调
pub async fn spawn() -> anyhow::Result<CarrierPage> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| anyhow::anyhow!("承载页 server 监听 127.0.0.1:0 失败: {e}"))?;
    let port = listener.local_addr()?.port();
    let task = tokio::spawn(accept_loop(listener));
    tracing::info!("承载页 server 就绪: http://127.0.0.1:{port}/");
    Ok(CarrierPage { port, task })
}

#[allow(dead_code)] // 只被 spawn() 调用，spawn() 接线前它随之一起是死代码
async fn accept_loop(listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                tokio::spawn(serve_one(stream));
            }
            Err(e) => {
                // accept 失败通常是 fd 耗尽一类的瞬时问题。让出一次再继续，
                // **绝不 return**：退出循环会让承载页从此再也加载不了，表现
                // 为「应用起来了但永远连不上」，而且没有一条能解释原因的日志。
                tracing::warn!("承载页 server accept 失败（继续监听）: {e}");
                tokio::task::yield_now().await;
            }
        }
    }
}

/// 读完请求头再回同一张页面。
///
/// 必须先读完：不读就写，客户端可能在写完请求之前收到 FIN/RST，
/// WebView 那边表现为导航失败。
///
/// `ponytail:` 不解析请求——任何路径都回同一张页面，解析出来的东西没有
/// 一处会被用到。`windows(4)` 的重复扫描是 O(n²)，对 8KB 上限无所谓。
/// 升级路径：真需要按路径分流时再上正经的 HTTP 处理。
#[allow(dead_code)] // 只被 accept_loop() 调用，同上，链路接通前一起是死代码
async fn serve_one(mut stream: TcpStream) {
    let mut buf = [0u8; 1024];
    let mut seen: Vec<u8> = Vec::new();
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => return,
            Ok(n) => {
                seen.extend_from_slice(&buf[..n]);
                if seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if seen.len() > 8192 {
                    // 请求头超过 8KB：不是浏览器发的正常请求，断开了事。
                    return;
                }
            }
            Err(_) => return,
        }
    }
    let resp = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        PAGE.len(),
        PAGE
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn serves_html_on_an_os_assigned_port() {
        let page = spawn().await.unwrap();
        assert_ne!(page.port(), 0, "端口必须是 OS 真的分配出来的");
        assert_eq!(page.url(), format!("http://127.0.0.1:{}/", page.port()));

        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", page.port()))
            .await
            .unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("HTTP/1.1 200 OK"), "{text}");
        assert!(text.contains("text/html"), "{text}");
        assert!(text.contains("<!doctype html>"), "{text}");
    }

    #[tokio::test]
    async fn any_path_gets_the_same_page() {
        // 承载窗口只会请求 `/`，但 WebView 还会自己去要 /favicon.ico。
        // 那个请求若得不到应答就会挂着，白占一条连接。
        let page = spawn().await.unwrap();
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", page.port()))
            .await
            .unwrap();
        s.write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        assert!(
            String::from_utf8_lossy(&out).starts_with("HTTP/1.1 200 OK"),
            "任何路径都该拿到同一张页面"
        );
    }

    #[tokio::test]
    async fn dropping_the_handle_releases_the_port() {
        let page = spawn().await.unwrap();
        let port = page.port();
        drop(page);
        // abort 生效是异步的，轮询等它释放而不是睡一个拍脑袋的固定时长。
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            if TcpListener::bind(("127.0.0.1", port)).await.is_ok() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "句柄 drop 一秒后端口 {port} 仍未释放"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}
