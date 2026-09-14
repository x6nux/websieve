//! SOCKS5 CONNECT 入站（spec §5）：终止 SOCKS5 协议，把 (目标地址, 双向字节流)
//! 交给 handler 决定出口。真实接线时 handler = mux.open() + 首帧 TargetAddr。
//!
//! 仅实现 RFC 1928 子集：无认证（0x00）、CONNECT（0x01）。BIND/UDP 不支持；
//! 失败统一回复 0x01（general SOCKS server failure）。

use futures::future::BoxFuture;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use wsieve_proto::addr::{decode_addr, AddrPort};

/// 成功回复：VER REP RSV ATYP BND.ADDR=0.0.0.0 BND.PORT=0
const REPLY_OK: [u8; 10] = [0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];

fn reply_err(rep: u8) -> [u8; 10] {
    let mut r = REPLY_OK;
    r[1] = rep;
    r
}

/// 收 SOCKS5 连接。handler 为每条 CONNECT 决定出口流；返回 Err 则回复 0x01。
pub async fn serve(
    listener: TcpListener,
    handler: impl Fn(AddrPort) -> BoxFuture<'static, std::io::Result<tokio::io::DuplexStream>>
        + Clone
        + Send
        + 'static,
) -> anyhow::Result<()> {
    loop {
        let (tcp, _peer) = listener.accept().await?;
        let handler = handler.clone();
        tokio::spawn(async move {
            // 协议/IO 错误按 RFC 语义已尽量在流内回复；此处静默收尾。
            let _ = serve_conn(tcp, handler).await;
        });
    }
}

/// 在**单条**已建立的连接上终止 SOCKS5 协议。
///
/// 与 [`serve`] 的区别只是不含 accept 循环。混合端口入口（wsieve-inbound）
/// 需要它：那边先嗅探首字节判定协议族，再把连接交给对应的处理器，
/// accept 循环由入口自己持有。
pub async fn serve_conn(
    tcp: TcpStream,
    handler: impl Fn(AddrPort) -> BoxFuture<'static, std::io::Result<tokio::io::DuplexStream>>,
) -> anyhow::Result<()> {
    handle_conn(tcp, handler).await
}

/// 把一条已建立的连接上的 SOCKS5 协商跑完，返回 CONNECT 的目标。
///
/// `Ok(None)` = **这条连接不用再管了**：对端在协商中途断开、首字节不是 0x05、
/// 或者请求用了我们不支持的命令/地址类型（后两种已按 RFC 回过对应的错误码）。
/// 调用方直接丢掉连接即可，不必也不该再回什么。
///
/// `Ok(Some(target))` = 协商成功，**成功应答还没发**——发不发、发之前还要不要
/// 做别的检查（比如目标是否在白名单里），是调用方的策略。
///
/// 拆出这个函数是为了让第二个 SOCKS5 服务端能共用同一份解析：
/// `src-tauri` 的 `webview_proxy` 也终止 SOCKS5（给 WebView 改写目标端口），
/// 它曾经自己写了一遍 greeting + CONNECT 的解析。同一个 RFC 子集解两遍，
/// 修一边的 bug 到不了另一边。它需要的只是「别替我决定怎么回复」，所以这里
/// 只做解析，策略留给调用方。
pub async fn negotiate(tcp: &mut TcpStream) -> anyhow::Result<Option<AddrPort>> {
    // 1. greeting: VER NMETHODS METHODS...
    let mut hdr = [0u8; 2];
    if !read_exact_opt(tcp, &mut hdr).await? {
        return Ok(None); // 对端在问候前断开
    }
    // 即使 VER 不对也先把 METHODS 读干净，避免残留数据导致 RST
    let mut methods = vec![0u8; hdr[1] as usize];
    if !read_exact_opt(tcp, &mut methods).await? {
        return Ok(None);
    }
    if hdr[0] != 0x05 || hdr[1] == 0 {
        return Ok(None); // 非 SOCKS5 / 无方法：直接关闭
    }
    if !methods.contains(&0x00) {
        // 明确回绝（0xFF = 无可接受方法）而不是默默按无认证往下走。
        //
        // 早先这里不看 METHODS，直接回 `[05 00]` 宣布选中无认证——对一个只
        // 支持用户名口令的客户端，那是在回一个它没提供的方式，接下来双方对
        // 帧的理解就错位了，表现为连接莫名卡住。0xFF 让它立刻报错。
        tcp.write_all(&[0x05, 0xFF]).await?;
        return Ok(None);
    }
    tcp.write_all(&[0x05, 0x00]).await?; // 选中 NO AUTHENTICATION

    // 2. request: VER CMD RSV ATYP addr port
    let mut req = [0u8; 4];
    if !read_exact_opt(tcp, &mut req).await? {
        return Ok(None);
    }
    if req[0] != 0x05 {
        return Ok(None);
    }
    if req[1] != 0x01 {
        tcp.write_all(&reply_err(0x07)).await?; // command not supported
        return Ok(None);
    }
    // 地址部分与 mux 首帧的 TargetAddr 编码一致（atyp + addr + be port）
    let mut tail = Vec::new();
    tail.push(req[3]);
    let addr_len = match req[3] {
        0x01 => 4 + 2,
        0x03 => {
            let mut l = [0u8; 1];
            if !read_exact_opt(tcp, &mut l).await? {
                return Ok(None);
            }
            tail.push(l[0]);
            l[0] as usize + 2
        }
        0x04 => 16 + 2,
        other => {
            let _ = other;
            tcp.write_all(&reply_err(0x08)).await?; // address type not supported
            return Ok(None);
        }
    };
    let mut rest = vec![0u8; addr_len];
    if !read_exact_opt(tcp, &mut rest).await? {
        return Ok(None);
    }
    tail.extend_from_slice(&rest);
    let (target, _) = decode_addr(&tail).map_err(|e| anyhow::anyhow!("bad request addr: {e}"))?;
    Ok(Some(target))
}

async fn handle_conn(
    mut tcp: TcpStream,
    handler: impl Fn(AddrPort) -> BoxFuture<'static, std::io::Result<tokio::io::DuplexStream>>,
) -> anyhow::Result<()> {
    let Some(target) = negotiate(&mut tcp).await? else {
        return Ok(());
    };

    // 3. 出口接线
    match handler(target).await {
        Ok(mut stream) => {
            tcp.write_all(&REPLY_OK).await?;
            let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await?;
        }
        Err(_) => {
            tcp.write_all(&reply_err(0x01)).await?;
        }
    }
    Ok(())
}

/// read_exact 的可关闭变体：对端 EOF 返回 Ok(false)，其余错误向上传。
async fn read_exact_opt(tcp: &mut TcpStream, buf: &mut [u8]) -> anyhow::Result<bool> {
    match tcp.read_exact(buf).await {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e.into()),
    }
}
