//! 局域网测速服务器（基准测量专用，不参与产品构建）。
//!
//! URL 形态照抄 Cloudflare 的测速端点，好让既有的测试命令原样复用：
//!
//! ```text
//! GET /__down?bytes=N   → N 字节响应体
//! POST /__up            → 读完请求体并丢弃，回 204
//! GET /ip               → 回一行对端地址（连通性自检用）
//! ```
//!
//! **只用 std**：这台机器上没有 cargo 时可以直接 `rustc -O speedsrv.rs`。
//! 线程模型也是刻意的——测速服务器自己绝不能成为被测系统的瓶颈，而
//! 「一连接一线程 + 大块写」在 1Gbps 这个量级上是最难写慢的写法。
//!
//! 响应体是一块**复用的固定缓冲**，不是每次现生成：现生成的话
//! 分配与填充会先于网络成为瓶颈，测出来的就是本机 memset 的速度。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

/// 单次写出的块大小。太小则系统调用次数主导，太大则浪费内存。
const BLOCK: usize = 256 * 1024;

fn main() {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "0.0.0.0:18080".to_string());
    let listener = TcpListener::bind(&addr).expect("bind 失败");
    // 全局共享的零块：所有连接共用，避免每连接一份。
    let block: Arc<Vec<u8>> = Arc::new(vec![0x5Au8; BLOCK]);
    eprintln!("speedsrv 监听 {addr}");
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let block = block.clone();
        std::thread::spawn(move || {
            let _ = stream.set_nodelay(true);
            if let Err(e) = serve(stream, &block) {
                // 客户端提前断开是测速里的常态，不值得刷屏
                let _ = e;
            }
        });
    }
}

fn serve(mut stream: TcpStream, block: &[u8]) -> std::io::Result<()> {
    let peer = stream.peer_addr()?;
    let mut reader = BufReader::new(stream.try_clone()?);

    loop {
        // ---- 请求行 ----
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(()); // 对端关闭
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("/").to_string();

        // ---- 头部：读到空行为止，顺便取 Content-Length ----
        let mut content_len: usize = 0;
        let mut keep_alive = true;
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h)? == 0 {
                return Ok(());
            }
            let t = h.trim_end();
            if t.is_empty() {
                break;
            }
            let lower = t.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                content_len = v.trim().parse().unwrap_or(0);
            }
            if lower.starts_with("connection:") && lower.contains("close") {
                keep_alive = false;
            }
        }

        match (method.as_str(), split_path(&path)) {
            ("GET", ("/__down", q)) => {
                let n = query_usize(q, "bytes").unwrap_or(0);
                write_down(&mut stream, n, block)?;
            }
            ("POST", ("/__up", _)) | ("PUT", ("/__up", _)) => {
                drain(&mut reader, content_len)?;
                stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")?;
            }
            ("GET", ("/ip", _)) => {
                let body = format!("{}\n", peer.ip());
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )?;
            }
            _ => {
                stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")?;
            }
        }
        stream.flush()?;
        if !keep_alive {
            return Ok(());
        }
    }
}

/// 写 `n` 字节响应体。**先写完整头再连续写体**，中途不做任何分配。
fn write_down(stream: &mut TcpStream, n: usize, block: &[u8]) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
         Cache-Control: no-store\r\nContent-Length: {n}\r\n\r\n"
    )?;
    let mut left = n;
    while left > 0 {
        let take = left.min(block.len());
        stream.write_all(&block[..take])?;
        left -= take;
    }
    Ok(())
}

/// 读掉并丢弃 `n` 字节请求体。上行测速要的是「收得多快」，不是内容。
fn drain(reader: &mut BufReader<TcpStream>, n: usize) -> std::io::Result<()> {
    let mut buf = vec![0u8; 256 * 1024];
    let mut left = n;
    while left > 0 {
        let take = left.min(buf.len());
        let got = reader.read(&mut buf[..take])?;
        if got == 0 {
            break;
        }
        left -= got;
    }
    Ok(())
}

fn split_path(p: &str) -> (&str, &str) {
    match p.split_once('?') {
        Some((a, b)) => (a, b),
        None => (p, ""),
    }
}

fn query_usize(q: &str, key: &str) -> Option<usize> {
    q.split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.parse().ok())
}
