//! 服务端核心循环（Task 15）：accept() mux 流 → 读首帧 TargetAddr →
//! TcpStream::connect → copy_bidirectional(stream, tcp)。每流一个 tokio task。
//! 拨号失败 → 直接关流（客户端 SOCKS5 层会收到 EOF → 回 RST）。

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wsieve_mux::{Mux, MuxStream};
use wsieve_proto::addr::{decode_addr, AddrPort, TargetAddr};

/// 拨号超时（TCP connect，含域名解析）。
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
/// 首帧 TargetAddr 的最大长度（域名 ≤255 + 头尾，留足余量）。
const ADDR_FRAME_MAX: usize = 512;

/// 会话级 accept 循环：每条 mux 子流一个 task。
pub async fn session_loop(mux: Box<dyn Mux>) {
    loop {
        let stream = match mux.accept().await {
            Ok(s) => s,
            Err(_) => return, // 会话终结
        };
        tokio::spawn(async move {
            if let Err(e) = serve_stream(stream).await {
                let _ = e;
            }
        });
    }
}

/// 单流：首帧 TargetAddr → 拨号 → 双向拷贝。任何失败 → 关流（drop）。
async fn serve_stream(mut stream: MuxStream) -> std::io::Result<()> {
    // 读首帧（完整读出一个 Frame::Data 载荷——TargetAddr 可能分多次 read 到齐）
    let mut buf = Vec::with_capacity(ADDR_FRAME_MAX);
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            // 对端没给地址就关了：静默结束
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > ADDR_FRAME_MAX {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "address frame too large",
            ));
        }
        if let Ok((_, consumed)) = decode_addr(&buf) {
            let _ = consumed;
            break;
        }
        // 不完整：继续读
    }

    let (target, _) = decode_addr(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    // 首帧地址之后同帧携带的早期数据（若有）先发给目标
    let mut tcp = dial(&target).await?;
    if let Some(rest) = first_frame_rest(&buf) {
        if !rest.is_empty() {
            tcp.write_all(rest).await?;
        }
    }
    copy_both(&mut stream, &mut tcp).await
}

/// 解析 TargetAddr → SocketAddr 串，拨第一个能通的。
async fn dial(target: &AddrPort) -> std::io::Result<tokio::net::TcpStream> {
    let host = match &target.addr {
        TargetAddr::V4(o) => format!("{}.{}.{}.{}", o[0], o[1], o[2], o[3]),
        TargetAddr::V6(_) => {
            // 本地回环场景罕见 IPv6 直填；交给 getaddrvia format
            target.display().trim_matches(|c| c == '[' || c == ']').split(']').next().unwrap_or("::1").to_string()
        }
        TargetAddr::Domain(d) => d.clone(),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']').to_string();
    let fut = tokio::net::TcpStream::connect((host.as_str(), target.port));
    match tokio::time::timeout(DIAL_TIMEOUT, fut).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "dial timeout",
        )),
    }
}

/// 双向拷贝直至任一侧结束。
async fn copy_both(
    stream: &mut MuxStream,
    tcp: &mut tokio::net::TcpStream,
) -> std::io::Result<()> {
    tokio::io::copy_bidirectional(stream, tcp).await.map(|_| ())
}

/// 首帧编码辅助（客户端侧约定，服务端测试也用它构造拨号帧）。
pub fn target_frame(t: &AddrPort) -> Vec<u8> {
    wsieve_proto::addr::encode_addr(t)
}

/// 读完首帧后剩余的载荷（Address 之后同帧携带的早期数据）。
pub fn first_frame_rest(buf: &[u8]) -> Option<&[u8]> {
    decode_addr(buf).ok().map(|(_, consumed)| &buf[consumed..])
}
