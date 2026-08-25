//! 跨域名承载 spike（设计文档 §14 / §15 待实测项 #3，阶段 2 计划 Part A Task 2）。
//!
//! **要回答的问题**：一个 WKWebView 里，加载自域名 A 的页面，能否**同时**与
//! 域名 A 和域名 B 各完成一次完整的 Noise 握手并跑数据？这决定 `carrier`
//! 的默认值是 `shared`（1× 内存）还是 `isolated`（N× 内存）。
//!
//! **为什么这条路可能成立**：sid 走 query 而非 cookie
//! （`wsieve-xhttp/src/client.rs:133`、`:205`、`:453`；服务端
//! `wsieve-server/src/lib.rs:228`、`:250`）。若 sid 在 cookie 里，WKWebView
//! 的 ITP 会拦掉跨站第三方 cookie，握手必然失败。
//!
//! **测试条件比计划书更严格**：计划书用 `wsieve-a.test` / `wsieve-b.test`
//! （需 sudo 改 hosts）。本实现改用公共泛解析回环域名 `localtest.me` 与
//! `lvh.me`——二者不仅是不同 origin，更是不同的 **eTLD+1**，即真正的
//! *cross-site*。ITP 的第三方 cookie 拦截正是按 eTLD+1 划界，故这是更强的
//! 测试条件，且无需 sudo、不留系统残留。
//!
//! **本进程扮演三个角色**：
//!   1. 两个真实 `wsieve-server`（各自监听一个端口，共用密钥便于对照）
//!   2. 一个 echo 目标服务器（验证数据面真的通）
//!   3. 一个 WebView 桥：真实 WKWebView 经 HTTP 回传 fetch 结果，
//!      Rust 侧的 `HttpTransport` 实现把它接进真实的 `XhttpConn::connect`
//!
//! 第 3 点是关键：**不是**用 reqwest 假装 WebView。fetch 由真实 WebKit
//! 网络栈发出，跨域名策略（CORS/ITP）由真实浏览器内核裁决，Rust 侧只是
//! 把结果接回协议层。这样得到的结论对 Tauri 的 WKWebView 才有效。
//!
//! 用法：`cargo run -p wsieve-server --example cross_origin_spike`
//! 需要 macOS 与 `swiftc`（脚本 `scripts/spike-cross-origin.sh` 会检查）。

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::routing::{any, get, post};
use axum::Router;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use wsieve_proto::crypto::gen_keypair;
use wsieve_proto::hello::MuxId;
use wsieve_server::{AppState, KeepaliveRange, ServerKeys, SEEN_CACHE_CAPACITY};
use wsieve_transport::{HttpTransport, PostReply};
use wsieve_xhttp::client::{random_group_id, UpstreamCfg, XhttpConn};

/// 两个 **cross-site** 域名（不同 eTLD+1），均公共泛解析到 127.0.0.1。
const HOST_A: &str = "localtest.me";
const HOST_B: &str = "lvh.me";

// ---------------------------------------------------------------------------
// WebView 桥：Rust ←→ 真实 WKWebView
// ---------------------------------------------------------------------------

/// 一次待处理的 fetch 请求（发给 WebView 执行）。
#[derive(Clone, serde::Serialize)]
struct FetchJob {
    id: u64,
    /// 绝对 URL（跨域名的关键：页面在 A，这个 URL 可能指向 B）
    url: String,
    kind: &'static str, // "post" | "stream"
    /// POST body，base64（与 emitter.js 的上行编码一致）
    body_b64: String,
}

type PostWaiters = Arc<Mutex<HashMap<u64, oneshot::Sender<anyhow::Result<PostReply>>>>>;
type StreamWaiters = Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<anyhow::Result<Bytes>>>>>;

/// 桥的共享状态：待派发队列 + 等待回填的两张表。
struct Bridge {
    next_id: AtomicU64,
    /// 待 WebView 拉取的任务队列
    pending: Mutex<Vec<FetchJob>>,
    post_waiters: PostWaiters,
    stream_waiters: StreamWaiters,
    /// 记录 WebView 侧报告的错误，供失败时给出真实原因（绝不静默吞掉）
    errors: Mutex<Vec<String>>,
    /// 驱动页是否已至少轮询过一次 —— WebView 就绪的真实信号。
    polled: std::sync::atomic::AtomicBool,
}

impl Bridge {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(Vec::new()),
            post_waiters: Arc::new(Mutex::new(HashMap::new())),
            stream_waiters: Arc::new(Mutex::new(HashMap::new())),
            errors: Mutex::new(Vec::new()),
            polled: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn note_error(&self, msg: String) {
        eprintln!("[bridge] WebView 侧错误: {msg}");
        self.errors.lock().unwrap().push(msg);
    }
}

/// 把「经真实 WKWebView 发 fetch」实现成 `HttpTransport`，从而能直接喂给
/// 真实的 `XhttpConn::connect`——协议层完全不知道自己在跟浏览器说话。
struct WebViewBridgeTransport {
    bridge: Arc<Bridge>,
    /// 请求基址（绝对 URL）。这就是「页面在 A、请求发往 B」的实现方式。
    base: String,
}

#[async_trait::async_trait]
impl HttpTransport for WebViewBridgeTransport {
    async fn post(&self, path: &str, body: Bytes) -> anyhow::Result<PostReply> {
        let id = self.bridge.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.bridge.post_waiters.lock().unwrap().insert(id, tx);
        self.bridge.pending.lock().unwrap().push(FetchJob {
            id,
            url: format!("{}{}", self.base, path),
            kind: "post",
            body_b64: b64_encode(&body),
        });
        match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => anyhow::bail!("post {id} 的回填通道被丢弃"),
            Err(_) => anyhow::bail!("post {id} 超时（30s）——WebView 未回填结果"),
        }
    }

    async fn get_stream(
        &self,
        path: &str,
    ) -> anyhow::Result<futures::stream::BoxStream<'static, anyhow::Result<Bytes>>> {
        let id = self.bridge.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        self.bridge.stream_waiters.lock().unwrap().insert(id, tx);
        self.bridge.pending.lock().unwrap().push(FetchJob {
            id,
            url: format!("{}{}", self.base, path),
            kind: "stream",
            body_b64: String::new(),
        });
        use futures::StreamExt;
        Ok(tokio_stream::wrappers::UnboundedReceiverStream::new(rx).boxed())
    }
}

fn b64_encode(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}

fn b64_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.decode(s)?)
}

// ---------------------------------------------------------------------------
// 桥的 HTTP 面（WebView 侧 JS 与之交互）
// ---------------------------------------------------------------------------

/// WebView 拉取待执行的 fetch 任务。
async fn bridge_poll(State(b): State<Arc<Bridge>>) -> Response {
    b.polled.store(true, Ordering::Relaxed);
    let jobs: Vec<FetchJob> = std::mem::take(&mut *b.pending.lock().unwrap());
    let body = serde_json::to_string(&jobs).unwrap_or_else(|_| "[]".into());
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(body))
        .unwrap()
}

#[derive(serde::Deserialize)]
struct PostResult {
    id: u64,
    ok: bool,
    status: u16,
    body_b64: String,
    error: Option<String>,
}

/// WebView 回填 POST 结果。
async fn bridge_post_result(
    State(b): State<Arc<Bridge>>,
    axum::Json(r): axum::Json<PostResult>,
) -> StatusCode {
    let waiter = b.post_waiters.lock().unwrap().remove(&r.id);
    let Some(tx) = waiter else {
        return StatusCode::NOT_FOUND;
    };
    let msg = if r.ok {
        match b64_decode(&r.body_b64) {
            Ok(body) => Ok(PostReply {
                status: r.status,
                body: Bytes::from(body),
            }),
            Err(e) => Err(anyhow::anyhow!("回填 body 解码失败: {e}")),
        }
    } else {
        let why = r.error.unwrap_or_else(|| "未提供原因".into());
        b.note_error(format!("POST id={} 失败: {why}", r.id));
        Err(anyhow::anyhow!("WebView fetch 失败: {why}"))
    };
    let _ = tx.send(msg);
    StatusCode::OK
}

#[derive(serde::Deserialize)]
struct ChunkResult {
    id: u64,
    /// "chunk" | "end" | "error"
    kind: String,
    body_b64: String,
    error: Option<String>,
}

/// WebView 回填下行 chunk / 流结束 / 流错误。
async fn bridge_chunk(
    State(b): State<Arc<Bridge>>,
    axum::Json(r): axum::Json<ChunkResult>,
) -> StatusCode {
    match r.kind.as_str() {
        "chunk" => {
            let tx = b.stream_waiters.lock().unwrap().get(&r.id).cloned();
            let Some(tx) = tx else {
                return StatusCode::NOT_FOUND;
            };
            match b64_decode(&r.body_b64) {
                Ok(bytes) => {
                    let _ = tx.send(Ok(Bytes::from(bytes)));
                }
                Err(e) => {
                    let _ = tx.send(Err(anyhow::anyhow!("chunk 解码失败: {e}")));
                }
            }
        }
        "end" => {
            b.stream_waiters.lock().unwrap().remove(&r.id);
        }
        "error" => {
            let why = r.error.unwrap_or_else(|| "未提供原因".into());
            b.note_error(format!("下行流 id={} 失败: {why}", r.id));
            if let Some(tx) = b.stream_waiters.lock().unwrap().remove(&r.id) {
                let _ = tx.send(Err(anyhow::anyhow!("WebView 下行流失败: {why}")));
            }
        }
        other => return {
            b.note_error(format!("未知 chunk kind: {other}"));
            StatusCode::BAD_REQUEST
        },
    }
    StatusCode::OK
}

/// WebView 侧未捕获错误的上报口（绝不静默吞）。
async fn bridge_log(State(b): State<Arc<Bridge>>, body: String) -> StatusCode {
    println!("[webview] {body}");
    if body.contains("ERROR") || body.contains("error") {
        b.errors.lock().unwrap().push(body);
    }
    StatusCode::OK
}

/// 驱动页：跑在**域名 A** 上，但对 A 和 B 都发 fetch。
const DRIVER_HTML: &str = r#"<!doctype html>
<meta charset="utf-8"><title>wsieve cross-origin spike</title>
<body><pre id="log">driver booting...</pre><script>
// 本页加载自域名 A。它对 A 与 B 两个 origin 都发 fetch —— 对 B 的那些
// 就是被测的跨域名（cross-site）请求。
var BRIDGE = location.origin;   // 桥回传口（与本页同源）
var streams = new Map();
function log(m){ document.getElementById('log').textContent += "\n"+m;
  fetch(BRIDGE+'/__bridge/log',{method:'POST',body:m}).catch(function(){}); }
function b2b64(u8){ var s=''; for(var i=0;i<u8.length;i++) s+=String.fromCharCode(u8[i]); return btoa(s); }
function b642b(s){ var bin=atob(s); var u=new Uint8Array(bin.length);
  for(var i=0;i<bin.length;i++) u[i]=bin.charCodeAt(i); return u; }

async function doPost(job){
  try{
    // credentials:'include' 与 Content-Type:text/plain 与 emitter.js 一致
    var resp = await fetch(job.url,{method:'POST',body:b642b(job.body_b64),
      credentials:'include',headers:{'Content-Type':'text/plain'}});
    var buf = new Uint8Array(await resp.arrayBuffer());
    await fetch(BRIDGE+'/__bridge/post_result',{method:'POST',
      headers:{'Content-Type':'application/json'},
      body:JSON.stringify({id:job.id,ok:true,status:resp.status,body_b64:b2b64(buf)})});
  }catch(e){
    log('POST FAIL '+job.url+' :: '+e);
    await fetch(BRIDGE+'/__bridge/post_result',{method:'POST',
      headers:{'Content-Type':'application/json'},
      body:JSON.stringify({id:job.id,ok:false,status:0,body_b64:'',error:String(e)})}).catch(function(){});
  }
}

async function doStream(job){
  try{
    var resp = await fetch(job.url,{credentials:'include'});
    if(!resp.ok){ throw new Error('status '+resp.status); }
    var reader = resp.body.getReader();
    streams.set(job.id, reader);
    while(true){
      var r = await reader.read();
      if(r.done) break;
      await fetch(BRIDGE+'/__bridge/chunk',{method:'POST',
        headers:{'Content-Type':'application/json'},
        body:JSON.stringify({id:job.id,kind:'chunk',body_b64:b2b64(r.value)})});
    }
    await fetch(BRIDGE+'/__bridge/chunk',{method:'POST',
      headers:{'Content-Type':'application/json'},
      body:JSON.stringify({id:job.id,kind:'end',body_b64:''})});
  }catch(e){
    log('STREAM FAIL '+job.url+' :: '+e);
    await fetch(BRIDGE+'/__bridge/chunk',{method:'POST',
      headers:{'Content-Type':'application/json'},
      body:JSON.stringify({id:job.id,kind:'error',body_b64:'',error:String(e)})}).catch(function(){});
  }
}

async function pump(){
  for(;;){
    try{
      var jobs = await (await fetch(BRIDGE+'/__bridge/poll',{cache:'no-store'})).json();
      for(var i=0;i<jobs.length;i++){
        var j = jobs[i];
        if(j.kind==='post') doPost(j); else doStream(j);
      }
    }catch(e){ log('poll error '+e); }
    await new Promise(function(r){ setTimeout(r,20); });
  }
}
log('driver ready at '+location.origin);
pump();
</script></body>"#;

async fn driver_page() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(DRIVER_HTML))
        .unwrap()
}

// ---------------------------------------------------------------------------
// echo 目标：验证数据面真的通
// ---------------------------------------------------------------------------

async fn spawn_echo() -> anyhow::Result<std::net::SocketAddr> {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = l.local_addr()?;
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if s.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    Ok(addr)
}

// ---------------------------------------------------------------------------
// 主流程
// ---------------------------------------------------------------------------

/// 起一个真实 wsieve-server；`extra` 为挂在同一 router 上的附加路由
/// （桥与驱动页只挂在 A 上）。
async fn spawn_server(
    port: u16,
    keys: ServerKeys,
    extra: Option<Router>,
) -> anyhow::Result<Arc<AppState>> {
    let state = AppState::new(
        keys,
        vec![MuxId::Yamux, MuxId::Smux],
        KeepaliveRange::default(),
        SEEN_CACHE_CAPACITY,
    );
    let mut router = state.clone().router();
    if let Some(e) = extra {
        // 桥的路由必须先匹配：wsieve 的 router 是 `/{*rest}` 全捕获。
        router = e.fallback_service(router);
    }
    let l = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tokio::spawn(async move {
        if let Err(e) = axum::serve(l, router).await {
            eprintln!("server on :{port} 退出: {e}");
        }
    });
    Ok(state)
}

/// 经指定 base 建一条真实会话（真实 Noise 握手，fetch 走真实 WebView）。
async fn connect_session(
    bridge: &Arc<Bridge>,
    base: &str,
    server_pub: [u8; 32],
    client_priv: [u8; 32],
    group_id: u128,
) -> anyhow::Result<(XhttpConn, wsieve_xhttp::client::Negotiated)> {
    let t = Arc::new(WebViewBridgeTransport {
        bridge: bridge.clone(),
        base: base.to_string(),
    });
    XhttpConn::connect(
        t,
        &UpstreamCfg {
            server_pub,
            client_priv,
            mux_prefs: vec![MuxId::Yamux],
            group_id,
        },
    )
    .await
}

/// 经 mux 向 echo 目标跑一次真实往返，确证数据面通。
async fn echo_roundtrip(
    conn: XhttpConn,
    neg: wsieve_xhttp::client::Negotiated,
    echo: std::net::SocketAddr,
    payload: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let io: wsieve_mux::MuxStream = Box::new(conn);
    let mux: Arc<dyn wsieve_mux::Mux> = Arc::from(wsieve_mux::mux_factory(neg.mux_id, io).await?);
    let dialer = wsieve_mux::stripe_runtime::StripeDialer::new(
        mux,
        wsieve_mux::stripe_runtime::StripeCfg::with_env(),
    );
    let target = wsieve_proto::addr::AddrPort {
        addr: wsieve_proto::addr::TargetAddr::V4(match echo.ip() {
            std::net::IpAddr::V4(v4) => v4.octets(),
            std::net::IpAddr::V6(_) => anyhow::bail!("echo 目标应为 IPv4"),
        }),
        port: echo.port(),
    };
    let mut s = dialer.connect(&target).await?;
    s.write_all(payload).await?;
    s.flush().await?;
    let mut got = vec![0u8; payload.len()];
    tokio::time::timeout(Duration::from_secs(20), s.read_exact(&mut got)).await??;
    Ok(got)
}

/// 进程常驻内存（macOS），用于给出「shared 到底省多少」的实测数字。
fn rss_kb() -> Option<u64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let port_a: u16 = std::env::var("WSIEVE_SPIKE_PORT_A")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(18081);
    let port_b: u16 = std::env::var("WSIEVE_SPIKE_PORT_B")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(18082);

    // 两台服务端共用同一套密钥：本 spike 测的是**承载**（跨域名 fetch 能否
    // 完成握手），不是密钥分发。共用可排除「密钥配错」这一混淆变量。
    let (server_priv, server_pub) = gen_keypair();
    let (client_priv, client_pub) = gen_keypair();
    let mk_keys = || {
        let mut wl = HashSet::new();
        wl.insert(client_pub);
        ServerKeys {
            priv_key: server_priv,
            whitelist: wl,
        }
    };

    let bridge = Bridge::new();
    // 桥与驱动页只挂在 A 上——B 是纯粹的「另一个出站」，不含任何 spike 设施。
    let extra = Router::new()
        .route("/__spike/driver", get(driver_page))
        .route("/__bridge/poll", get(bridge_poll))
        .route("/__bridge/post_result", post(bridge_post_result))
        .route("/__bridge/chunk", post(bridge_chunk))
        .route("/__bridge/log", post(bridge_log))
        .route("/__bridge/{*rest}", any(StatusCode::NOT_FOUND))
        .with_state(bridge.clone());

    spawn_server(port_a, mk_keys(), Some(extra)).await?;
    spawn_server(port_b, mk_keys(), None).await?;
    let echo = spawn_echo().await?;

    let base_a = format!("http://{HOST_A}:{port_a}");
    let base_b = format!("http://{HOST_B}:{port_b}");
    println!("出站 A = {base_a}");
    println!("出站 B = {base_b}（与 A 不同 eTLD+1，即 cross-site）");
    println!("echo 目标 = {echo}");
    println!("驱动页 = {base_a}/__spike/driver");

    let rss_before = rss_kb();

    // 等 WebView 把驱动页跑起来（脚本已在外部启动它）。就绪信号取「桥被
    // 轮询过至少一次」——驱动页一上线就开始 poll，这比睡固定时长可靠。
    println!("\n== 等待 WebView 驱动页上线 ==");
    let mut ready = false;
    for _ in 0..150 {
        if bridge.polled.load(Ordering::Relaxed) {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if ready {
        println!("  驱动页已开始轮询，WebView 就绪");
    } else {
        // 不直接退出：让后续握手去撞真实的超时，错误信息比这里的猜测更有用。
        println!("  ⚠️ 30s 内未见驱动页轮询——继续，握手失败时以其真实原因为准");
    }

    let group_id = random_group_id();

    println!("\n== [1/3] 同源握手（页面在 A → 请求发往 A）==");
    let (conn_a, neg_a) =
        match connect_session(&bridge, &base_a, server_pub, client_priv, group_id).await {
            Ok(v) => {
                println!("  ✅ A 握手成功，mux = {:?}", v.1.mux_id);
                v
            }
            Err(e) => {
                println!("  ❌ A 握手失败: {e:#}");
                println!("\n判定：基线就不通，spike 无法继续（这不是跨域名问题）。");
                dump_errors(&bridge);
                std::process::exit(2);
            }
        };

    println!("\n== [2/3] 跨域名握手（页面在 A → 请求发往 B）==");
    let cross = connect_session(&bridge, &base_b, server_pub, client_priv, group_id).await;
    let (conn_b, neg_b) = match cross {
        Ok(v) => {
            println!("  ✅ B 握手成功，mux = {:?}", v.1.mux_id);
            v
        }
        Err(e) => {
            println!("  ❌ B 跨域名握手失败: {e:#}");
            dump_errors(&bridge);
            println!("\n=== 判定：carrier: shared 不成立 ===");
            println!("按计划书 Task 2 Step 4：把默认改为 isolated，回填 spec，不要硬推。");
            std::process::exit(3);
        }
    };

    println!("\n== [3/3] 两个出站各跑一次真实数据往返 ==");
    let pa = b"spike-payload-via-outbound-A";
    let got_a = echo_roundtrip(conn_a, neg_a, echo, pa).await;
    match &got_a {
        Ok(g) if g == pa => println!("  ✅ A 数据面通（{} 字节往返一致）", g.len()),
        Ok(g) => {
            println!("  ❌ A 数据面内容不符: {:?}", String::from_utf8_lossy(g));
            std::process::exit(4);
        }
        Err(e) => {
            println!("  ❌ A 数据面失败: {e:#}");
            std::process::exit(4);
        }
    }

    let pb = b"spike-payload-via-outbound-B-cross-site";
    let got_b = echo_roundtrip(conn_b, neg_b, echo, pb).await;
    match &got_b {
        Ok(g) if g == pb => println!("  ✅ B 数据面通（{} 字节往返一致，且是跨域名会话）", g.len()),
        Ok(g) => {
            println!("  ❌ B 数据面内容不符: {:?}", String::from_utf8_lossy(g));
            std::process::exit(4);
        }
        Err(e) => {
            println!("  ❌ B 数据面失败: {e:#}");
            dump_errors(&bridge);
            std::process::exit(4);
        }
    }

    let rss_after = rss_kb();
    println!("\n== 内存 ==");
    match (rss_before, rss_after) {
        (Some(a), Some(b)) => println!(
            "  spike 进程 RSS: {a} KB → {b} KB（含两个服务端与 echo，仅供参考）",
            a = a,
            b = b
        ),
        _ => println!("  RSS 读取失败（非致命）"),
    }
    println!("  WebView 进程数由脚本侧统计（见 spike-cross-origin.sh 的内存小节）");

    dump_errors(&bridge);
    println!("\n=== 判定：carrier: shared 成立 ===");
    println!("单个 WKWebView 同时承载了两个 cross-site 出站的握手与数据面。");
    Ok(())
}

fn dump_errors(b: &Arc<Bridge>) {
    let errs = b.errors.lock().unwrap();
    if errs.is_empty() {
        return;
    }
    println!("\n-- WebView 侧记录到的错误（{} 条）--", errs.len());
    for e in errs.iter() {
        println!("   {e}");
    }
}
