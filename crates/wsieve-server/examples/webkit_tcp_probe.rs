//! WebKit 连接池探针：测「浏览器给同一 origin 的 N 个 XHTTP 会话开几条 TCP」。
//!
//! 为什么单独测 WebKit 而不是测整个 Tauri app：多会话条带的收益全部来自
//! 「每会话一条独立 TCP ⇒ 独立拥塞窗口」。Tauri 客户端把全部会话交给同一个
//! `WebViewTransport`（同一 WKWebView、同一 origin，见 src-tauri/src/proxy.rs
//! 里复用的 `transport.clone()`），所以真正的未知数是 WebKit 网络栈的连接池
//! 行为，跟 websieve 自己的代码无关。把它单独隔离出来测，既不需要整个 app
//! 起得来（GUI app 在无窗口服务的进程里根本拿不到 WebView），也让结论不被
//! 握手/mux 的失败噪声污染。
//!
//! 做法：本进程既是被测服务端也是记账探针——裸 TCP 监听，每条连接一个 id，
//! 解析其上的请求行提取 `sid`，即得 TCP → 会话集合 的映射。页面用真实的
//! XHTTP 请求形状（每会话一条长挂 chunked GET `/api/events?sid=`＋周期
//! POST `/api/sync?n=&sid=`），因为决定 TCP 占用的正是那条长挂的下行流。
//!
//! 用法：
//!   WSIEVE_WK_PORT     监听端口（默认 29099）
//!   WSIEVE_WK_SESSIONS 模拟会话数（默认 4）
//!   WSIEVE_WK_SECS     采集时长秒（默认 12）
//! 然后 `open -a Safari http://127.0.0.1:29099/`（Safari 与 WKWebView 共用
//! 同一个 WebKit 网络栈）。到时自动打印 verdict 并退出。

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct Ledger {
    tcps: HashMap<u64, BTreeSet<String>>,
    protos: HashMap<u64, String>,
}

impl Ledger {
    fn note(&mut self, tcp: u64, sid: &str) -> bool {
        self.tcps.entry(tcp).or_default().insert(sid.to_string())
    }

    fn note_proto(&mut self, tcp: u64, proto: &str) {
        self.protos.insert(tcp, proto.to_string());
    }

    /// 承载过会话的 TCP 里协商到 h2 的条数。h2 是「多会话挤一条 TCP」的
    /// 头号成因，单独报出来。
    fn h2_count(&self) -> usize {
        self.tcps
            .iter()
            .filter(|(id, s)| !s.is_empty() && self.protos.get(id).is_some_and(|p| p == "h2"))
            .count()
    }

    /// 只统计承载过会话的 TCP：浏览器的预连接/favicon 请求不该进分母。
    fn verdict(&self) -> String {
        let bearing: Vec<&BTreeSet<String>> =
            self.tcps.values().filter(|s| !s.is_empty()).collect();
        let sids: BTreeSet<&String> = bearing.iter().flat_map(|s| s.iter()).collect();
        let max_per = bearing.iter().map(|s| s.len()).max().unwrap_or(0);
        let v = if sids.is_empty() {
            "NO-DATA"
        } else if max_per == 1 {
            "INDEPENDENT"
        } else if bearing.len() == 1 {
            "FULLY-MULTIPLEXED"
        } else {
            "PARTIALLY-MULTIPLEXED"
        };
        format!(
            "SUMMARY tcps={} sessions={} max_sessions_per_tcp={} h2_tcps={} verdict={}",
            bearing.len(),
            sids.len(),
            max_per,
            self.h2_count(),
            v
        )
    }
}

fn page(sessions: usize, alt_origin: &str) -> String {
    // 复刻 XHTTP 的请求形状：每会话一条长挂 GET（下行）＋周期 POST（上行）。
    //
    // alt_origin 非空时，奇数会话打到另一个 origin（同 IP 同端口同证书，
    // 仅主机名不同）——用来测 HTTP/2 connection coalescing（RFC 7540
    // §9.1.1）：浏览器若认定两个 origin 可复用同一条 h2 连接，则「多子域名
    // 绕开 h2 复用」这条路走不通。
    format!(
        r#"<!doctype html><meta charset=utf-8><title>webkit tcp probe</title>
<body style="font:14px/1.6 -apple-system,sans-serif;padding:2rem">
<h3>WebKit TCP 归属探针</h3><p>已开 <b>{sessions}</b> 个模拟 XHTTP 会话，
每个一条长挂 GET + 周期 POST。请回到终端看结论，本页无需操作。</p>
<pre id=log></pre>
<script>
const N = {sessions};
const ALT = "{alt_origin}";
const log = m => document.getElementById('log').textContent += m + '\n';
function sid(i) {{ return 'wkprobe' + i + 'x'.repeat(8); }}
function base(i) {{ return (ALT && i % 2 === 1) ? ALT : ''; }}
for (let i = 0; i < N; i++) {{
  const s = sid(i);
  const b = base(i);
  // 下行：长挂 chunked GET，读到流结束为止（占住一条连接，正是它决定 TCP 数）
  fetch(b + '/api/events?sid=' + s).then(async r => {{
    const rd = r.body.getReader();
    for (;;) {{ const {{done}} = await rd.read(); if (done) break; }}
    log('session ' + i + ' downlink closed');
  }}).catch(e => log('session ' + i + ' downlink error: ' + e));
  // 上行：周期 POST
  let n = 0;
  setInterval(() => {{
    fetch(b + '/api/sync?n=' + (n++) + '&sid=' + s, {{method:'POST', body:'x'.repeat(256)}})
      .catch(() => {{}});
  }}, 500);
}}
log('opened ' + N + ' sessions');
</script>"#
    )
}

/// 从请求行扒 `sid=<value>`（value 到 `&` 或空白为止）。
fn sid_of(req: &str) -> Option<String> {
    let i = req.find("sid=")? + 4;
    let rest = &req[i..];
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    Some(rest[..end].to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 支持逗号分隔的多端口：同一域名 + 不同端口 = 不同 origin，用来验证
    // 「hosts 劫持 + 本地多端口转发」能否让浏览器开出多条独立 TLS 连接。
    let ports: Vec<u16> = std::env::var("WSIEVE_WK_PORT")
        .unwrap_or_else(|_| "29099".into())
        .split(',')
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    let port = *ports.first().expect("至少一个端口");
    let sessions: usize = std::env::var("WSIEVE_WK_SESSIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let secs: u64 = std::env::var("WSIEVE_WK_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(12);

    // TLS 模式：`WSIEVE_WK_TLS=cert.pem,key.pem`。必须走 TLS 才测得到 h2——
    // 浏览器只在 TLS ALPN 里协商 HTTP/2，明文 h2c 一律不支持。生产走 CDN
    // 正是 https+h2，所以这个模式才对应真实部署。
    // 备用 origin（同 IP 同端口同证书，仅主机名不同）——测 h2 连接合并。
    let alt_origin = std::env::var("WSIEVE_WK_ALT_ORIGIN").unwrap_or_default();

    let tls = std::env::var("WSIEVE_WK_TLS").ok().map(|v| {
        let (c, k) = v.split_once(',').expect("WSIEVE_WK_TLS=cert.pem,key.pem");
        (c.to_string(), k.to_string())
    });

    let ledger = Arc::new(Mutex::new(Ledger::default()));
    let scheme = if tls.is_some() { "https" } else { "http" };
    println!("webkit probe on {scheme}://127.0.0.1:{port}/  sessions={sessions} 采集={secs}s");
    println!("现在执行: open -a Safari {scheme}://127.0.0.1:{port}/");

    let l = ledger.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(secs)).await;
        println!("{}", l.lock().unwrap().verdict());
        for (tcp, sids) in l.lock().unwrap().tcps.iter() {
            if !sids.is_empty() {
                println!("TCP#{tcp} sids={:?}", sids);
            }
        }
        std::process::exit(0);
    });

    let acceptor = match &tls {
        Some((cert, key)) => Some(tls_acceptor(cert, key)?),
        None => None,
    };

    // 全部端口共享一个 ledger 与一个 TCP id 计数器，归属才可比。
    let next = Arc::new(AtomicU64::new(0));
    let mut tasks = Vec::new();
    for p in ports {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", p)).await?;
        println!("listening on {scheme}://127.0.0.1:{p}/");
        let (acceptor, ledger, alt, next) = (
            acceptor.clone(),
            ledger.clone(),
            alt_origin.clone(),
            next.clone(),
        );
        tasks.push(tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else { return };
                let _ = sock.set_nodelay(true);
                let id = next.fetch_add(1, Ordering::Relaxed) + 1;
                match &acceptor {
                    None => {
                        tokio::spawn(serve(sock, id, ledger.clone(), sessions, alt.clone()));
                    }
                    Some(acc) => {
                        tokio::spawn(serve_tls(
                            acc.clone(), sock, id, ledger.clone(), sessions, alt.clone(),
                        ));
                    }
                }
            }
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
    Ok(())
}

/// ALPN 同时 advertise h2 与 http/1.1，由浏览器自己选——这才反映真实
/// CDN 边缘的行为（几乎所有 CDN 都优先 h2）。
fn tls_acceptor(cert_path: &str, key_path: &str) -> anyhow::Result<tokio_rustls::TlsAcceptor> {
    // 同 wsieve_server::tls：rustls 0.23 编译进多个 provider 时拒绝自动选择。
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let certs: Vec<tokio_rustls::rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut std::io::BufReader::new(std::fs::File::open(cert_path)?))
            .collect::<std::io::Result<_>>()?;
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(std::fs::File::open(
        key_path,
    )?))?
    .ok_or_else(|| anyhow::anyhow!("私钥文件不含 PEM 私钥"))?;
    let mut cfg = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(cfg)))
}

async fn serve_tls(
    acceptor: tokio_rustls::TlsAcceptor,
    sock: tokio::net::TcpStream,
    tcp_id: u64,
    ledger: Arc<Mutex<Ledger>>,
    sessions: usize,
    alt_origin: String,
) {
    let Ok(stream) = acceptor.accept(sock).await else { return };
    let alpn = stream
        .get_ref()
        .1
        .alpn_protocol()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .unwrap_or_else(|| "http/1.1".into());
    ledger.lock().unwrap().note_proto(tcp_id, &alpn);
    println!("TCP#{tcp_id} alpn={alpn}");

    let l = ledger.clone();
    let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let l = l.clone();
        let alt = alt_origin.clone();
        async move {
            let path_q = req
                .uri()
                .path_and_query()
                .map(|p| p.to_string())
                .unwrap_or_default();
            if let Some(s) = sid_of(&path_q) {
                if l.lock().unwrap().note(tcp_id, &s) {
                    println!("TCP#{tcp_id} <- {s}");
                }
            }
            Ok::<_, std::convert::Infallible>(respond(&path_q, sessions, &alt))
        }
    });
    // auto builder：按 ALPN 结果自动走 h1 或 h2 —— 与真实 CDN 边缘一致。
    let _ = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
        .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
        .await;
}

fn respond(path_q: &str, sessions: usize, alt_origin: &str) -> hyper::Response<axum::body::Body> {
    if path_q.starts_with("/api/events") {
        // 长挂流：永不结束，占住这条流/连接 —— 复刻 XHTTP 下行。
        let s = futures::stream::unfold((), |_| async {
            tokio::time::sleep(Duration::from_millis(400)).await;
            Some((
                Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"ping")),
                (),
            ))
        });
        hyper::Response::builder()
            .header("content-type", "application/octet-stream")
            .header("cache-control", "no-store")
            .header("access-control-allow-origin", "*")
            .body(axum::body::Body::from_stream(s))
            .unwrap()
    } else if path_q == "/" {
        hyper::Response::builder()
            .header("content-type", "text/html; charset=utf-8")
            .body(axum::body::Body::from(page(sessions, alt_origin)))
            .unwrap()
    } else {
        hyper::Response::builder()
            .header("access-control-allow-origin", "*")
            .body(axum::body::Body::from("ok"))
            .unwrap()
    }
}

async fn serve(
    mut sock: tokio::net::TcpStream,
    tcp_id: u64,
    ledger: Arc<Mutex<Ledger>>,
    sessions: usize,
    alt_origin: String,
) {
    let mut buf = vec![0u8; 8192];
    loop {
        // keep-alive：一条 TCP 上可能来多个请求，逐个处理（这正是要观测的复用）。
        let n = match sock.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let req = String::from_utf8_lossy(&buf[..n]).into_owned();
        let line = req.lines().next().unwrap_or("").to_string();
        if let Some(s) = sid_of(&line) {
            if ledger.lock().unwrap().note(tcp_id, &s) {
                println!("TCP#{tcp_id} <- {s}");
            }
        }

        if line.starts_with("GET / ") {
            let body = page(sessions, &alt_origin);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                body.len()
            );
            if sock.write_all(head.as_bytes()).await.is_err()
                || sock.write_all(body.as_bytes()).await.is_err()
            {
                return;
            }
        } else if line.contains("/api/events") {
            // 长挂 chunked 流：不结束，占住这条 TCP —— 复刻 XHTTP 下行。
            let head = "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\nCache-Control: no-store\r\n\r\n";
            if sock.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            loop {
                tokio::time::sleep(Duration::from_millis(400)).await;
                if sock.write_all(b"4\r\nping\r\n").await.is_err() {
                    return;
                }
            }
        } else {
            let head = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nAccess-Control-Allow-Origin: *\r\nConnection: keep-alive\r\n\r\nok";
            if sock.write_all(head.as_bytes()).await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sid_parsed_from_request_lines() {
        assert_eq!(
            sid_of("POST /api/sync?n=3&sid=wkprobe0xxx HTTP/1.1").as_deref(),
            Some("wkprobe0xxx")
        );
        assert_eq!(
            sid_of("GET /api/events?sid=abc HTTP/1.1").as_deref(),
            Some("abc")
        );
        assert!(sid_of("GET / HTTP/1.1").is_none());
    }

    #[test]
    fn verdict_reflects_pooling() {
        let mut l = Ledger::default();
        l.note(1, "a");
        l.note(2, "b");
        assert!(l.verdict().contains("INDEPENDENT"));

        let mut l = Ledger::default();
        for s in ["a", "b", "c", "d"] {
            l.note(1, s);
        }
        assert!(l.verdict().contains("FULLY-MULTIPLEXED"));
        assert!(l.verdict().contains("sessions=4"));
    }
}
