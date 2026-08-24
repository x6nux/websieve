//! v2 服务端远程泵：mux accept → ConnHeader 路由（StripeListener）→
//! 每 conn 拨目标 TCP + 双向泵。旧「首帧 TargetAddr 单流」协议已整体删除。

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use wsieve_mux::stripe_runtime::{StripeCfg, StripeConn, StripeListener};
use wsieve_mux::{Mux, MuxStream};
use wsieve_proto::addr::{AddrPort, TargetAddr};

/// 拨号超时（TCP connect，含域名解析）。
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// 会话级 accept 循环：StripeListener 按 conn_id 路由 lane / 建新 conn。
pub async fn session_loop(mux: Box<dyn Mux>) {
    let listener = StripeListener::new(Arc::from(mux), StripeCfg::with_env());
    listener.run(serve_conn).await;
}

/// 新 conn：拨目标 + 双向泵；目标 EOF → CLOSE(TargetEof)。
fn serve_conn(conn: Arc<StripeConn>, addr: AddrPort) {
    tokio::spawn(async move {
        let mut stream = conn.stream();
        let tcp = match dial(&addr).await {
            Ok(t) => t,
            Err(e) => {
                // 拨号失败：错误原因下发给客户端后终结 conn
                eprintln!("[remote] dial {} failed: {e}", addr.display());
                let _ = stream.shutdown().await;
                conn.close_send(wsieve_proto::stripe::CloseReason::TargetError)
                    .await;
                return;
            }
        };
        let (mut tcp_r, mut tcp_w) = tokio::io::split(tcp);

        // 上行：conn → 目标。客户端 EOF（read 返回 0）→ 半关目标写侧。
        let up = {
            let mut up_s = stream.clone();
            async move {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match up_s.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if tcp_w.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        };
        // 下行：目标 → conn。目标 EOF → shutdown 触发 CLOSE(TargetEof)。
        let down = async {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match tcp_r.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stream.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        };
        // 下行结束（目标 EOF/错误）即触发 CLOSE；上行泵继续排空客户端残余
        // 数据直至其 EOF，不阻塞 CLOSE 发送。
        let up_handle = tokio::spawn(up);
        down.await;
        stream.shutdown().await.ok();
        conn.close_send(wsieve_proto::stripe::CloseReason::TargetEof)
            .await;
        up_handle.abort();
    });
}

/// 解析 TargetAddr → 拨第一个能通的 TCP。
async fn dial(target: &AddrPort) -> std::io::Result<TcpStream> {
    let host = match &target.addr {
        TargetAddr::V4(o) => format!("{}.{}.{}.{}", o[0], o[1], o[2], o[3]),
        TargetAddr::V6(_) => "::1".to_string(),
        TargetAddr::Domain(d) => d.clone(),
    };
    let fut = TcpStream::connect((host.as_str(), target.port));
    match tokio::time::timeout(DIAL_TIMEOUT, fut).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "dial timeout",
        )),
    }
}

/// 测试辅助：构造客户端 OPEN 前缀（ConnHeader + TargetAddr）。
pub fn open_prefix(conn_id: u64, target: &AddrPort) -> Vec<u8> {
    use wsieve_proto::stripe::{encode_header, Cmd, ConnHeader, Dir};
    let mut prefix = encode_header(&ConnHeader {
        conn_id,
        cmd: Cmd::Open,
        dir: Dir::Bidi,
        lane_id: 0,
    })
    .to_vec();
    prefix.extend_from_slice(&wsieve_proto::addr::encode_addr(target));
    prefix
}

/// 测试辅助：读首帧头 + addr。返回 (conn_id, addr, 同批剩余字节)。
pub async fn read_open(stream: &mut MuxStream) -> std::io::Result<(u64, AddrPort, Vec<u8>)> {
    let mut b = [0u8; 16];
    let mut got = 0;
    while got < 16 {
        let n = stream.read(&mut b[got..]).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "eof before header",
            ));
        }
        got += n;
    }
    let h = wsieve_proto::stripe::decode_header(&b)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    let mut chunk = [0u8; 256];
    loop {
        if let Ok((addr, consumed)) = wsieve_proto::addr::decode_addr(&buf) {
            let rest = buf.split_off(consumed);
            return Ok((h.conn_id, addr, rest));
        }
        if buf.len() > 512 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "address frame too large",
            ));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "eof before address",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}
