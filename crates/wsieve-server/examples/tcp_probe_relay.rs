//! TCP 归属探针：透传中继 + 记账「哪个 XHTTP 会话跑在哪条 TCP 上」。
//!
//! 为什么需要它：多会话条带（aria2 效应）的全部收益都建立在「每个会话一条
//! 独立 TCP ⇒ 独立拥塞窗口」上。但客户端是否真开了新 TCP，取决于它的 HTTP
//! 栈——reqwest 每个 Client 一个连接池；WKWebView 的 NSURLSession 会跨请求
//! 池化，若协商到 HTTP/2 更是把所有会话复用到**一条** TCP 上。那样会话组就
//! 只是摆设，收益归零。客户端侧看不出这件事，只有服务端前面的 socket 能看出。
//!
//! 原理：XHTTP 的会话 id 明文出现在请求行里（`/api/sync?n=..&sid=..` /
//! `/api/events?sid=..`，见 wsieve-xhttp/src/client.rs）。本中继逐条 TCP
//! 嗅探上行字节流提取 sid，即可得到 TCP → 会话集合 的映射。同时识别 HTTP/2
//! 连接前导，因为 h2 复用正是「多会话挤一条 TCP」的头号成因。
//!
//! 用法：
//!   WSIEVE_PROBE_UPSTREAM 127.0.0.1:28443  真实服务端
//!   WSIEVE_PROBE_LISTEN   127.0.0.1:28080  中继监听
//!   WSIEVE_PROBE_INTERVAL 2                汇总打印间隔秒（0=只在发现时打印）
//!
//! 输出（stdout，逐行；被 kill 也不会丢已发现的映射）：
//!   TCP#1 proto=http/1.1 sid=Ab3d..
//!   SUMMARY tcps=4 sessions=4 max_sessions_per_tcp=1 verdict=INDEPENDENT

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// h2 连接前导（RFC 7540 §3.5）。出现即说明这条 TCP 是 HTTP/2，
/// 后续所有会话都会复用它。
const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

#[derive(Default)]
struct Ledger {
    /// tcp_id → (协议, 该 TCP 上出现过的 sid 集合)
    tcps: HashMap<u64, (String, BTreeSet<String>)>,
}

impl Ledger {
    /// 记录一条 TCP 上出现的 sid；首次出现才返回 true（供打印增量行）。
    fn note_sid(&mut self, tcp: u64, sid: &str) -> bool {
        let e = self.tcps.entry(tcp).or_insert_with(|| ("http/1.1".into(), BTreeSet::new()));
        e.1.insert(sid.to_string())
    }

    fn note_proto(&mut self, tcp: u64, proto: &str) {
        let e = self.tcps.entry(tcp).or_insert_with(|| ("http/1.1".into(), BTreeSet::new()));
        e.0 = proto.to_string();
    }

    /// 只统计真正承载过会话的 TCP：客户端 HTTP 栈的预热/探测连接不该算进分母。
    fn summary(&self) -> String {
        let bearing: Vec<&(String, BTreeSet<String>)> =
            self.tcps.values().filter(|(_, s)| !s.is_empty()).collect();
        let all_sids: BTreeSet<&String> = bearing.iter().flat_map(|(_, s)| s.iter()).collect();
        let max_per_tcp = bearing.iter().map(|(_, s)| s.len()).max().unwrap_or(0);
        let h2 = bearing.iter().filter(|(p, _)| p == "h2").count();
        let verdict = if all_sids.is_empty() {
            "NO-DATA"
        } else if max_per_tcp == 1 {
            "INDEPENDENT" // 每条 TCP 恰好一个会话 —— 条带能拿到独立拥塞窗口
        } else if bearing.len() == 1 {
            "FULLY-MULTIPLEXED" // 全部会话挤一条 TCP —— 会话组完全是摆设
        } else {
            "PARTIALLY-MULTIPLEXED"
        };
        format!(
            "SUMMARY tcps={} sessions={} max_sessions_per_tcp={} h2_tcps={} verdict={}",
            bearing.len(),
            all_sids.len(),
            max_per_tcp,
            h2,
            verdict
        )
    }
}

/// 从上行字节里扒 `sid=<value>`。value 是 URL-safe base64（含 `-_`），
/// 以 `&`、空白或引号收尾。
fn scan_sids(buf: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let pat = b"sid=";
    let mut i = 0;
    while i + pat.len() < buf.len() {
        if &buf[i..i + pat.len()] == pat {
            let start = i + pat.len();
            let mut end = start;
            while end < buf.len() {
                let c = buf[end];
                if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'=' {
                    end += 1;
                } else {
                    break;
                }
            }
            // 收尾必须是明确的分隔符：若正好停在缓冲末尾，说明 sid 可能被
            // 切断，丢弃（下一轮靠 carry 重新拼出完整的）。
            if end < buf.len() && end > start {
                out.push(String::from_utf8_lossy(&buf[start..end]).into_owned());
            }
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let up: std::net::SocketAddr = std::env::var("WSIEVE_PROBE_UPSTREAM")
        .expect("WSIEVE_PROBE_UPSTREAM")
        .parse()?;
    let listen = std::env::var("WSIEVE_PROBE_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:28080".to_string());
    let interval: u64 = std::env::var("WSIEVE_PROBE_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);

    let ledger = Arc::new(Mutex::new(Ledger::default()));
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    println!("tcp probe {listen} -> {up}");

    if interval > 0 {
        let l = ledger.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
                println!("{}", l.lock().unwrap().summary());
            }
        });
    }

    let next_id = AtomicU64::new(0);
    loop {
        let (client, _) = listener.accept().await?;
        let server = match tokio::net::TcpStream::connect(up).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("probe: upstream connect failed: {e}");
                continue;
            }
        };
        let _ = client.set_nodelay(true);
        let _ = server.set_nodelay(true);
        let id = next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (cr, cw) = client.into_split();
        let (sr, sw) = server.into_split();
        // 上行：嗅探 + 透传。下行：纯透传。
        tokio::spawn(sniff_up(cr, sw, id, ledger.clone()));
        tokio::spawn(async move {
            let (mut sr, mut cw) = (sr, cw);
            let _ = tokio::io::copy(&mut sr, &mut cw).await;
            let _ = cw.shutdown().await;
        });
    }
}

async fn sniff_up(
    mut src: tokio::net::tcp::OwnedReadHalf,
    mut dst: tokio::net::tcp::OwnedWriteHalf,
    tcp_id: u64,
    ledger: Arc<Mutex<Ledger>>,
) {
    let mut buf = vec![0u8; 64 * 1024];
    // 跨读边界的残尾：sid 可能被 TCP 分段切成两半，保留尾部若干字节与下一
    // 块拼接后再扫。64 字节足够覆盖 "sid=" + 一个 base64 会话 id。
    let mut carry: Vec<u8> = Vec::new();
    let mut first = true;
    loop {
        let n = match src.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if first {
            first = false;
            if buf[..n].starts_with(H2_PREFACE) {
                ledger.lock().unwrap().note_proto(tcp_id, "h2");
                println!("TCP#{tcp_id} proto=h2 (所有会话将复用这一条)");
            }
        }
        // 先转发再分析：嗅探绝不能拖慢数据路径，否则测的是探针不是被测对象。
        if dst.write_all(&buf[..n]).await.is_err() {
            break;
        }
        let mut scan = std::mem::take(&mut carry);
        scan.extend_from_slice(&buf[..n]);
        for sid in scan_sids(&scan) {
            let fresh = ledger.lock().unwrap().note_sid(tcp_id, &sid);
            if fresh {
                let short: String = sid.chars().take(12).collect();
                println!("TCP#{tcp_id} sid={short}");
            }
        }
        let keep = scan.len().min(64);
        carry = scan[scan.len() - keep..].to_vec();
    }
    let _ = dst.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_extracts_sids_from_real_paths() {
        let req = b"POST /api/sync?n=0&sid=AbC-_123 HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(scan_sids(req), vec!["AbC-_123"]);
        let get = b"GET /api/events?sid=ZZ9 HTTP/1.1\r\n";
        assert_eq!(scan_sids(get), vec!["ZZ9"]);
    }

    #[test]
    fn truncated_sid_is_not_reported() {
        // 缓冲正好在 sid 中间断开 —— 不能报半个 id，否则会虚增会话数。
        assert!(scan_sids(b"POST /api/sync?n=1&sid=AbC").is_empty());
    }

    #[test]
    fn verdict_distinguishes_multiplexing() {
        let mut l = Ledger::default();
        l.note_sid(1, "a");
        l.note_sid(2, "b");
        assert!(l.summary().contains("verdict=INDEPENDENT"));

        let mut l = Ledger::default();
        l.note_sid(1, "a");
        l.note_sid(1, "b");
        assert!(l.summary().contains("verdict=FULLY-MULTIPLEXED"));
        assert!(l.summary().contains("max_sessions_per_tcp=2"));

        // 空 TCP（客户端预热连接）不进分母，否则 verdict 被稀释成假阳性。
        let mut l = Ledger::default();
        l.note_sid(1, "a");
        l.note_sid(1, "b");
        l.note_proto(2, "http/1.1");
        assert!(l.summary().contains("tcps=1"));
    }
}
