//! v2 服务端远程泵：mux accept → ConnHeader 路由（StripeListener）→
//! 每 conn 拨目标 TCP + 双向泵。旧「首帧 TargetAddr 单流」协议已整体删除。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use wsieve_mux::stripe_runtime::{ConnRegistry, SessionGroup, StripeCfg, StripeConn, StripeListener};
use wsieve_mux::{Mux, MuxStream};
use wsieve_proto::addr::{AddrPort, TargetAddr};
use wsieve_proto::hello::IpStrategy;

/// 拨号超时（TCP connect，含域名解析）。
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// 会话级 accept 循环：StripeListener 按 conn_id 路由 lane / 建新 conn。
/// `registry` 跨会话共享（多 TCP 条带：同一 conn 的 lane 可能来自任意会话）。
/// 会话死亡只让其上的 lane 断开；conn 存活与否取决于自身 lane 全断/CLOSE，
/// 与任一单会话无关。
///
/// `group` 是本会话所属的会话组（同一客户端 msg1.group_id 的全部会话）。
/// 新 conn 的下行 lane 在组内会话上轮转打开 —— 这是下行条带真正跨 TCP
/// 的地方：钉死在单会话上时下载全程只吃一个拥塞窗口。
/// 传入的 `mux` 由调用方持有 Arc（组表存 Weak），组成员的存活期即会话
/// 本身的存活期。
/// `ip_strategy` 来自本会话的 msg1，作用于域名目标的地址族选择。
pub async fn session_loop(
    mux: Arc<dyn Mux>,
    registry: Arc<ConnRegistry>,
    group: Arc<SessionGroup>,
    ip_strategy: IpStrategy,
) {
    let listener = StripeListener::with_group(mux, StripeCfg::with_env(), registry, group);
    listener
        .run(move |conn, addr| serve_conn(conn, addr, ip_strategy))
        .await;
}

/// 新 conn：拨目标 + 双向泵；目标 EOF → CLOSE(TargetEof)。
fn serve_conn(conn: Arc<StripeConn>, addr: AddrPort, ip_strategy: IpStrategy) {
    tokio::spawn(async move {
        let mut stream = conn.stream();
        let tcp = match dial(&addr, ip_strategy).await {
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

/// 按 `ip_strategy` 给候选地址排序/筛选。**只用于域名目标**。
///
/// 返回空 vec 表示「解析到了地址，但没有一个符合策略」——调用方必须把它
/// 当错误报出去，不能悄悄回退到被排除的那一族：用户配 `v4-only` 就是不想
/// 走 v6，静默回退等于配置没生效而他毫不知情（§6.4 同源纪律）。
fn apply_strategy(addrs: Vec<SocketAddr>, s: IpStrategy) -> Vec<SocketAddr> {
    match s {
        IpStrategy::Auto => addrs,
        IpStrategy::V4Only => addrs.into_iter().filter(|a| a.is_ipv4()).collect(),
        IpStrategy::V6Only => addrs.into_iter().filter(|a| a.is_ipv6()).collect(),
        // 排序而非筛选：v4 全不通时还能落到 v6 上。
        IpStrategy::PreferV4 => {
            let (v4, v6): (Vec<_>, Vec<_>) = addrs.into_iter().partition(|a| a.is_ipv4());
            v4.into_iter().chain(v6).collect()
        }
    }
}

/// 解析 TargetAddr → 拨第一个能通的 TCP。
///
/// IP 字面量目标不受 `ip_strategy` 影响：地址是客户端明确指定的，按策略
/// 把它换掉就是静默改道。策略只决定「域名解析出多个地址时选哪个」。
async fn dial(target: &AddrPort, ip_strategy: IpStrategy) -> std::io::Result<TcpStream> {
    let candidates: Vec<SocketAddr> = match &target.addr {
        TargetAddr::V4(o) => vec![SocketAddr::from((*o, target.port))],
        // 曾经这里恒返回 "::1" —— 客户端发来的任何 IPv6 目标都会被拨到服务端
        // **自己的环回地址**上：既丢了用户的流量，又把服务器本机端口暴露成
        // 可达目标。按实际地址拨。
        TargetAddr::V6(o) => vec![SocketAddr::from((*o, target.port))],
        TargetAddr::Domain(d) => {
            let resolved: Vec<SocketAddr> =
                tokio::net::lookup_host((d.as_str(), target.port)).await?.collect();
            let n_total = resolved.len();
            let picked = apply_strategy(resolved, ip_strategy);
            if picked.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrNotAvailable,
                    format!(
                        "{d} 解析到 {n_total} 个地址，但没有一个满足 {ip_strategy:?}"
                    ),
                ));
            }
            picked
        }
    };

    // 逐个试到第一个能通的。保留最后一个错误上报，别让调用方只看到
    // 一句笼统的「连不上」。
    let mut last_err = None;
    for addr in candidates {
        let fut = TcpStream::connect(addr);
        match tokio::time::timeout(DIAL_TIMEOUT, fut).await {
            Ok(Ok(s)) => return Ok(s),
            Ok(Err(e)) => last_err = Some(e),
            Err(_) => {
                last_err = Some(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "dial timeout",
                ))
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "no candidate address")
    }))
}

#[cfg(test)]
mod strategy_tests {
    use super::*;

    fn addrs() -> Vec<SocketAddr> {
        vec![
            "[2001:db8::1]:443".parse().unwrap(),
            "203.0.113.7:443".parse().unwrap(),
            "[2001:db8::2]:443".parse().unwrap(),
            "203.0.113.8:443".parse().unwrap(),
        ]
    }

    #[test]
    fn auto_keeps_the_resolver_order_untouched() {
        // Auto 就是「改动前的行为」：一个都不动，交给系统的 RFC 6724 排序。
        assert_eq!(apply_strategy(addrs(), IpStrategy::Auto), addrs());
    }

    #[test]
    fn v4_only_drops_every_v6_and_vice_versa() {
        let v4 = apply_strategy(addrs(), IpStrategy::V4Only);
        assert_eq!(v4.len(), 2);
        assert!(v4.iter().all(|a| a.is_ipv4()));

        let v6 = apply_strategy(addrs(), IpStrategy::V6Only);
        assert_eq!(v6.len(), 2);
        assert!(v6.iter().all(|a| a.is_ipv6()));
    }

    /// **筛空了要如实空着**，不能悄悄把被排除的那一族放回来——
    /// 用户配 v4-only 就是不想走 v6，静默回退等于配置没生效而他毫不知情。
    /// 调用方（`dial`）据此报错。
    #[test]
    fn filtering_everything_out_yields_empty_not_a_fallback() {
        let only_v6: Vec<SocketAddr> = vec!["[2001:db8::1]:443".parse().unwrap()];
        assert!(apply_strategy(only_v6.clone(), IpStrategy::V4Only).is_empty());
        let only_v4: Vec<SocketAddr> = vec!["203.0.113.7:443".parse().unwrap()];
        assert!(apply_strategy(only_v4, IpStrategy::V6Only).is_empty());
    }

    /// PreferV4 是**排序**不是筛选：v4 排前面，但 v6 仍留作兜底。
    #[test]
    fn prefer_v4_reorders_but_keeps_v6_as_fallback() {
        let got = apply_strategy(addrs(), IpStrategy::PreferV4);
        assert_eq!(got.len(), 4, "一个都不该被丢掉");
        assert!(got[0].is_ipv4() && got[1].is_ipv4(), "v4 要排在前面");
        assert!(got[2].is_ipv6() && got[3].is_ipv6());
    }

    #[test]
    fn prefer_v4_on_a_v6_only_host_still_returns_the_v6() {
        let only_v6: Vec<SocketAddr> = vec!["[2001:db8::1]:443".parse().unwrap()];
        assert_eq!(apply_strategy(only_v6.clone(), IpStrategy::PreferV4), only_v6);
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
