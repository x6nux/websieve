//! 混合入口：同一端口上同时接受 SOCKS5 与 HTTP 代理请求。
//!
//! 设计文档 §8.1。三种入口（混合端口 / 系统代理 / TUN）统一产出
//! `(AddrPort, 双向流)`，下游对入口类型无感 —— TUN 因此是纯增量，
//! 加一个入口不动其余任何一层。

pub mod http;
pub mod sniff;

use std::future::Future;
use std::pin::Pin;

use tokio::net::TcpListener;
use wsieve_proto::addr::AddrPort;

/// 入口把每条连接交给它，由调用方决定去哪。
///
/// 返回的 DuplexStream 是「已经连上目标」的双向管道；入口负责把它
/// 与客户端连接对接。判决为拒绝时返回 Err，入口据此给客户端一个
/// 合乎协议的失败响应（SOCKS5 回复码 / HTTP 502）。
pub type Dispatch = std::sync::Arc<
    dyn Fn(AddrPort) -> Pin<Box<dyn Future<Output = std::io::Result<tokio::io::DuplexStream>> + Send>>
        + Send
        + Sync,
>;

/// 在给定监听器上服务混合入口，直到 accept 出错。
///
/// **单条连接的任何失败都不能拖垮 accept 循环**：这是一个对内网开放的
/// 端口，随便一个扫描器就能送来畸形输入。每条连接各自 spawn，错误就地
/// 记日志收尾 —— 但不是静默丢弃，日志里能看到是谁、因为什么结束的。
pub async fn serve(listener: TcpListener, dispatch: Dispatch) -> std::io::Result<()> {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                // 每连接的 fd 耗尽是暂时的，等一下还能恢复；把它当致命
                // 错误退出循环，等于让一次瞬时压力永久关掉入口。
                if is_transient_accept_error(&e) {
                    tracing::warn!("accept 暂时失败，稍后重试：{e}");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
                return Err(e);
            }
        };
        let dispatch = dispatch.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, dispatch).await {
                // 单条连接失败不能拖垮监听循环
                tracing::debug!("来自 {peer} 的连接处理结束：{e}");
            }
        });
    }
}

/// 这类 accept 错误是资源瞬时紧张或单个对端的问题，不是监听器本身坏了。
///
/// 最常见的是 `ConnectionAborted`：客户端在 SYN 与 accept 之间就 RST 了。
/// 把它当致命错误退出循环，等于让任意一个对端随手关掉整个入口。
/// EMFILE/ENFILE（fd 用尽）同理 —— 等一下就能恢复，退出则永久失守。
fn is_transient_accept_error(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    if matches!(
        e.kind(),
        ConnectionAborted | ConnectionReset | Interrupted | WouldBlock | OutOfMemory
    ) {
        return true;
    }
    // Rust 尚未给 EMFILE/ENFILE 稳定的 ErrorKind，只能看 errno。
    // 两者在 Linux 与各 BSD/macOS 上取值一致。
    const EMFILE: i32 = 24; // 本进程 fd 用尽
    const ENFILE: i32 = 23; // 系统级 fd 用尽
    matches!(e.raw_os_error(), Some(EMFILE) | Some(ENFILE))
}

async fn handle(stream: tokio::net::TcpStream, dispatch: Dispatch) -> anyhow::Result<()> {
    match sniff::sniff(&stream).await? {
        sniff::Protocol::Socks5 => {
            // 复用既有的 wsieve-socks5：它的 handler 签名与 Dispatch 同形。
            // 嗅探用的是 peek，首字节还在缓冲区里，socks5 能读到完整握手。
            wsieve_socks5::serve_conn(stream, move |t| dispatch(t)).await?;
        }
        sniff::Protocol::Http => http::serve_conn(stream, dispatch).await?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// 一个把目标地址记下来、并回显负载的 dispatch，便于断言解析结果。
    fn echo_dispatch(seen: std::sync::Arc<tokio::sync::Mutex<Vec<String>>>) -> Dispatch {
        std::sync::Arc::new(move |target: AddrPort| {
            let seen = seen.clone();
            Box::pin(async move {
                seen.lock().await.push(target.display());
                let (a, mut b) = tokio::io::duplex(4096);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 1024];
                    while let Ok(n) = b.read(&mut buf).await {
                        if n == 0 || b.write_all(&buf[..n]).await.is_err() {
                            return;
                        }
                    }
                });
                Ok(a)
            })
        })
    }

    /// 起一个混合入口，返回其地址。
    async fn spawn_serve(d: Dispatch) -> std::net::SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(serve(l, d));
        addr
    }

    #[tokio::test]
    async fn http_connect_reaches_dispatch_with_right_target() {
        let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let addr = spawn_serve(echo_dispatch(seen.clone())).await;

        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut buf = [0u8; 12];
        c.read_exact(&mut buf).await.unwrap();
        assert!(buf.starts_with(b"HTTP/1.1 200"), "应回 200 建立隧道");
        assert_eq!(seen.lock().await.as_slice(), &["example.com:443".to_string()]);
    }

    #[tokio::test]
    async fn socks5_reaches_dispatch_with_right_target() {
        let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let addr = spawn_serve(echo_dispatch(seen.clone())).await;

        let mut c = TcpStream::connect(addr).await.unwrap();
        // 无认证握手
        c.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut b = [0u8; 2];
        c.read_exact(&mut b).await.unwrap();
        assert_eq!(b, [0x05, 0x00]);
        // CONNECT example.com:443
        let mut req = vec![0x05, 0x01, 0x00, 0x03, 11];
        req.extend_from_slice(b"example.com");
        req.extend_from_slice(&443u16.to_be_bytes());
        c.write_all(&req).await.unwrap();
        let mut resp = [0u8; 10];
        c.read_exact(&mut resp).await.unwrap();
        assert_eq!(resp[1], 0x00, "应回成功");
        assert_eq!(seen.lock().await.as_slice(), &["example.com:443".to_string()]);
    }

    #[tokio::test]
    async fn rejected_target_yields_protocol_correct_failure() {
        // dispatch 返回 Err（规则判决 REJECT）时，两种协议都要给出
        // 合乎自己规范的失败响应，而不是直接断开
        let d: Dispatch =
            std::sync::Arc::new(|_| Box::pin(async { Err(std::io::Error::other("rejected")) }));
        let addr = spawn_serve(d.clone()).await;

        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(b"CONNECT a.com:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut buf = [0u8; 12];
        c.read_exact(&mut buf).await.unwrap();
        assert!(buf.starts_with(b"HTTP/1.1 502"), "拒绝应回 502");

        // SOCKS5 一侧：回复码 0x01（general failure），同样不是静默断开
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut m = [0u8; 2];
        c.read_exact(&mut m).await.unwrap();
        let mut req = vec![0x05, 0x01, 0x00, 0x03, 5];
        req.extend_from_slice(b"a.com");
        req.extend_from_slice(&443u16.to_be_bytes());
        c.write_all(&req).await.unwrap();
        let mut resp = [0u8; 10];
        c.read_exact(&mut resp).await.unwrap();
        assert_eq!(resp[1], 0x01, "SOCKS5 拒绝应回 0x01");
    }

    #[tokio::test]
    async fn plain_http_is_rewritten_to_origin_form() {
        // 绝对 URI 进来，发给上游的必须是 origin-form，且逐跳头被剔除
        let (tx, rx) = tokio::sync::oneshot::channel();
        let tx = std::sync::Arc::new(tokio::sync::Mutex::new(Some(tx)));
        let d: Dispatch = std::sync::Arc::new(move |_| {
            let tx = tx.clone();
            Box::pin(async move {
                let (a, mut b) = tokio::io::duplex(4096);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = b.read(&mut buf).await.unwrap();
                    buf.truncate(n);
                    if let Some(tx) = tx.lock().await.take() {
                        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
                    }
                    // 回一个最小响应，免得客户端侧空等
                    let _ = b.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await;
                });
                Ok(a)
            })
        });
        let addr = spawn_serve(d).await;

        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(
            b"GET http://example.com/a?b=1 HTTP/1.1\r\n\
              Host: example.com\r\n\
              Proxy-Connection: keep-alive\r\n\
              Proxy-Authorization: Basic Zm9v\r\n\
              User-Agent: probe\r\n\r\n",
        )
        .await
        .unwrap();

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("上游应收到请求")
            .unwrap();
        assert!(
            got.starts_with("GET /a?b=1 HTTP/1.1\r\n"),
            "请求行应改写为 origin-form，实际：{got:?}"
        );
        assert!(got.contains("Host: example.com\r\n"), "Host 要保留");
        assert!(got.contains("User-Agent: probe\r\n"), "普通头要保留");
        assert!(
            !got.to_ascii_lowercase().contains("proxy-connection"),
            "逐跳头 Proxy-Connection 必须剔除：{got:?}"
        );
        assert!(
            !got.to_ascii_lowercase().contains("proxy-authorization"),
            "Proxy-Authorization 不能泄漏给源站：{got:?}"
        );
        assert!(got.contains("Connection: close\r\n"), "应注入 Connection: close");
    }

    #[tokio::test]
    async fn bytes_after_head_are_not_lost() {
        // 客户端把 CONNECT 与紧随其后的载荷放在同一个分段里发出（抢跑）。
        // 那段载荷在读头时已经离开 socket 缓冲区，必须由我们补给上游，
        // 否则请求就会莫名其妙地卡住。
        let (tx, rx) = tokio::sync::oneshot::channel();
        let tx = std::sync::Arc::new(tokio::sync::Mutex::new(Some(tx)));
        let d: Dispatch = std::sync::Arc::new(move |_| {
            let tx = tx.clone();
            Box::pin(async move {
                let (a, mut b) = tokio::io::duplex(4096);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = b.read(&mut buf).await.unwrap();
                    buf.truncate(n);
                    if let Some(tx) = tx.lock().await.take() {
                        let _ = tx.send(buf);
                    }
                });
                Ok(a)
            })
        });
        let addr = spawn_serve(d).await;

        let mut c = TcpStream::connect(addr).await.unwrap();
        // 头与载荷一次写出，模拟同分段到达
        c.write_all(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n\x16\x03\x01EARLY")
            .await
            .unwrap();

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("上游应收到抢跑的载荷")
            .unwrap();
        assert_eq!(
            got, b"\x16\x03\x01EARLY",
            "头之后的字节必须原样送达上游，一个都不能丢"
        );
    }

    #[tokio::test]
    async fn malformed_client_does_not_kill_the_listener() {
        // 一条畸形连接（既非 SOCKS5 也非 HTTP）之后，监听器必须照常服务。
        // 这是整组测试里最要紧的一条：accept 循环是单点。
        let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let addr = spawn_serve(echo_dispatch(seen.clone())).await;

        // 1. 垃圾首字节
        let mut bad = TcpStream::connect(addr).await.unwrap();
        bad.write_all(&[0xFF, 0xFE, 0xFD]).await.unwrap();
        let mut sink = [0u8; 8];
        let _ = bad.read(&mut sink).await; // 期望被关掉
        drop(bad);

        // 2. 连上就跑，一个字节都不发
        drop(TcpStream::connect(addr).await.unwrap());

        // 3. SOCKS4（明确不支持）
        let mut s4 = TcpStream::connect(addr).await.unwrap();
        s4.write_all(&[0x04, 0x01, 0x00, 0x50]).await.unwrap();
        let _ = s4.read(&mut sink).await;
        drop(s4);

        // 4. 是 HTTP 但请求行畸形
        let mut b4 = TcpStream::connect(addr).await.unwrap();
        b4.write_all(b"GET /origin-form-only HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut resp = [0u8; 12];
        b4.read_exact(&mut resp).await.unwrap();
        assert!(resp.starts_with(b"HTTP/1.1 400"), "畸形请求应回 400");
        drop(b4);

        // 监听器仍在服务：一条正常的 CONNECT 必须照常成功
        let mut ok = TcpStream::connect(addr).await.unwrap();
        ok.write_all(b"CONNECT still.alive:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut buf = [0u8; 12];
        ok.read_exact(&mut buf).await.unwrap();
        assert!(
            buf.starts_with(b"HTTP/1.1 200"),
            "畸形连接之后监听器必须照常工作"
        );
        assert_eq!(seen.lock().await.as_slice(), &["still.alive:443".to_string()]);
    }

    #[test]
    fn transient_accept_errors_do_not_end_the_loop() {
        use std::io::{Error, ErrorKind};
        // 对端在 SYN 与 accept 之间就跑了 —— 最常见的一种，绝不能致命
        assert!(is_transient_accept_error(&Error::from(
            ErrorKind::ConnectionAborted
        )));
        assert!(is_transient_accept_error(&Error::from(
            ErrorKind::Interrupted
        )));
        // fd 用尽是暂时的：退出循环等于让一次瞬时压力永久关掉入口
        assert!(is_transient_accept_error(&Error::from_raw_os_error(24)));
        assert!(is_transient_accept_error(&Error::from_raw_os_error(23)));
        // 监听器本身坏了则必须向上报，不能装作没事继续空转
        assert!(!is_transient_accept_error(&Error::from(
            ErrorKind::InvalidInput
        )));
        assert!(!is_transient_accept_error(&Error::other("bad listener")));
    }

    #[tokio::test]
    async fn silent_client_does_not_leak_the_connection() {        // 连上不说话的客户端要被嗅探超时清掉，而不是永久占住一个 task。
        // 用一个很短的自定义上限验证机制本身，默认值是 10s。
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let s = tokio::spawn(async move {
            let (stream, _) = l.accept().await.unwrap();
            sniff::sniff_within(&stream, std::time::Duration::from_millis(120)).await
        });

        let quiet = TcpStream::connect(addr).await.unwrap();
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), s)
            .await
            .expect("嗅探必须自己结束，不能一直挂着")
            .unwrap();
        assert!(matches!(r, Err(sniff::SniffError::Timeout(_))), "实际：{r:?}");
        drop(quiet);
    }
}
