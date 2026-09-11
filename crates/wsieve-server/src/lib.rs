//! 服务端核心：axum 单一 fallback 处理器，auth-before-routing（spec §8）。
//!
//! 任何未通过认证的请求——无论路径方法——原样转交 [`disguise::handle`]。
//! 全局不产生 401/409 等任何专属错误响应。成功路径仅三种：
//! (a) 合法握手（n=0）→ 200 + TU(msg2)；
//! (b) 有效会话 POST（n≥1）→ 204 空 body；
//! (c) 有效会话未挂载的 GET /api/events → 200 TU 流。
//!
//! SessionStore 适配说明：`SessionStore::create(sid)` 只管理 seq 重排/去重/GC，
//! 不携带握手产物。Noise `TransportState`（上行解密/下行加密共用，snow 内部
//! 收发 nonce 独立计数）与下行通道句柄存放在 `AppState` 的旁路表中，以 sid
//! 关联，生命周期与 SessionStore 会话条目一致（会话 GC 后旁路条目变孤儿，
//! 保活任务下次 tick 自然退出）。

pub mod disguise;
/// HTTP/3 监听面。模块名刻意不叫 `h3`——那会与 `h3` crate 同名，读代码时
/// 分不清 `h3::server` 指的是本地模块还是外部 crate。
pub mod http3;
pub mod remote;
pub mod tls;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, StatusCode, Uri};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use bytes::Bytes;
use snow::TransportState;
use tokio::sync::{mpsc, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt as _;
use wsieve_proto::crypto::build_server;
use wsieve_proto::hello::{IpStrategy, decode_msg1, encode_msg2, ts_in_window, MuxId, Msg2, TS_WINDOW_MS};
use wsieve_xhttp::server::{SessionStore, Sid};

/// spec §6.3：已见 msg1 缓存默认容量 4096，条目寿命 = ts 窗口时长。
pub const SEEN_CACHE_CAPACITY: usize = 4096;

/// spec §6.5：保活 PADDING TU 间隔 20–80s 随机（测试可调小，属配置非 mock）。
#[derive(Debug, Clone, Copy)]
pub struct KeepaliveRange {
    pub min_ms: u64,
    pub max_ms: u64,
}

impl Default for KeepaliveRange {
    fn default() -> Self {
        Self {
            min_ms: 20_000,
            max_ms: 80_000,
        }
    }
}

/// 服务端静态密钥与客户端静态公钥白名单。
pub struct ServerKeys {
    pub priv_key: [u8; 32],
    pub whitelist: HashSet<[u8; 32]>,
}

/// 伪装配置：默认内嵌 nginx 页；配置 upstream 后未认证请求反代到上游（spec §8）。
#[derive(Clone, Default)]
pub struct DisguiseCfg {
    /// 反代上游基址（如 `https://example.com`）。None = 内嵌页模式。
    pub upstream: Option<String>,
    /// Alt-Svc 广播端口（§6.8 第 3 层铺路；None = 不广播）。
    pub alt_svc_port: Option<u16>,
}

/// 共享状态（公开构造，测试直连）。
pub struct AppState {
    pub store: SessionStore,
    pub keys: ServerKeys,
    /// sid → 下行 TU 字节通道发送端（保活/会话任务生产，GET /api/events 消费）。
    downlink_tx: Mutex<HashMap<Sid, mpsc::Sender<Bytes>>>,
    /// sid → 下行接收端（attach 时取出、转成响应体流）。
    downlink_rx: Mutex<HashMap<Sid, mpsc::Receiver<Bytes>>>,
    /// 已见 msg1 密文哈希 → 插入时刻。
    seen: Mutex<HashMap<[u8; 16], std::time::Instant>>,
    /// 缓存容量上限（fail-closed 阈值，测试可调小）。
    seen_capacity: usize,
    pub enabled_mux: Vec<MuxId>,
    pub keepalive: KeepaliveRange,
    pub disguise: DisguiseCfg,
    /// 条带 conn 全局表（跨 XHTTP 会话共享）：多 TCP 条带时同一 conn 的
    /// lane 可能来自任意会话。conn 存活不依赖任一单会话。
    pub stripe_registry: Arc<wsieve_mux::stripe_runtime::ConnRegistry>,
    /// 会话组表：msg1.group_id → 同一客户端的全部 XHTTP 会话。服务端据此
    /// 把下行 lane 铺到组内任意会话（= 任意 TCP）上，下载才能吃到多个
    /// 拥塞窗口。单会话客户端 = 只有一个成员的组，行为不变。
    pub session_groups: Arc<wsieve_mux::stripe_runtime::SessionGroups>,
}

impl AppState {
    pub fn new(
        keys: ServerKeys,
        enabled_mux: Vec<MuxId>,
        keepalive: KeepaliveRange,
        seen_capacity: usize,
    ) -> Arc<Self> {
        Self::with_disguise(keys, enabled_mux, keepalive, seen_capacity, DisguiseCfg::default())
    }

    pub fn with_disguise(
        keys: ServerKeys,
        enabled_mux: Vec<MuxId>,
        keepalive: KeepaliveRange,
        seen_capacity: usize,
        disguise: DisguiseCfg,
    ) -> Arc<Self> {
        let state = Arc::new(Self {
            store: SessionStore::new(), // 自带 GC 任务（attach 30s / 上行空闲 180s）
            keys,
            downlink_tx: Mutex::new(HashMap::new()),
            downlink_rx: Mutex::new(HashMap::new()),
            seen: Mutex::new(HashMap::new()),
            seen_capacity,
            enabled_mux,
            keepalive,
            disguise,
            stripe_registry: Arc::new(wsieve_mux::stripe_runtime::ConnRegistry::new()),
            session_groups: Arc::new(wsieve_mux::stripe_runtime::SessionGroups::new()),
        });
        spawn_seen_sweep(state.clone());
        state
    }

    /// 构建 Router（main 与测试共用）。
    ///
    /// Alt-Svc 加在**整个 Router 上**而不是 `disguise_resp` 里：承载页自
    /// 2026-09-10 起是本机 http 壳，WebView 的数据面请求直接走 `/api/sync`
    /// 与 `/api/events`，从不请求伪装页面。只给伪装响应加这个头，客户端就
    /// 永远看不到它——而 Apple 的网络栈不做推测性 QUIC 尝试，没看到
    /// Alt-Svc 就永远不会升级到 h3，等于 h3 白做。
    ///
    /// `alt_svc_port` 为 None 时**完全不挂这一层**：宣告一个连不上的 QUIC
    /// 端点，会让客户端此后每次连接都先试 QUIC 超时再回落，比不宣告更糟。
    pub fn router(self: Arc<Self>) -> Router {
        let alt_svc = self.disguise.alt_svc_port.and_then(|p| {
            axum::http::HeaderValue::from_str(&format!("h3=\":{p}\"; ma=86400")).ok()
        });
        let r = Router::new()
            .route("/", any(fallback))
            .route("/{*rest}", any(fallback))
            .with_state(self);
        match alt_svc {
            Some(v) => r.layer(axum::middleware::map_response(move |mut resp: Response| {
                let v = v.clone();
                async move {
                    resp.headers_mut().insert(header::ALT_SVC, v);
                    resp
                }
            })),
            None => r,
        }
    }
}

/// 周期清理已见缓存中过窗条目（spec §6.3：条目寿命 = ts 窗口时长；
/// 间隔 = 窗口 / 10，下限 1s）。
fn spawn_seen_sweep(state: Arc<AppState>) {
    tokio::spawn(async move {
        let period = Duration::from_millis((TS_WINDOW_MS / 10).max(1_000));
        loop {
            tokio::time::sleep(period).await;
            state
                .seen
                .lock()
                .await
                .retain(|_, at| at.elapsed() <= Duration::from_millis(TS_WINDOW_MS));
        }
    });
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// 最小 query 解析。
fn query_param(uri: &Uri, key: &str) -> Option<String> {
    uri.query()?.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

fn parse_sid(s: &str) -> Option<Sid> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    Some(Sid(URL_SAFE_NO_PAD.decode(s).ok()?.try_into().ok()?))
}

async fn disguise_resp(state: &Arc<AppState>, req: axum::http::Request<Body>) -> Response {
    // Alt-Svc 不在这里加——它已提升到 Router 层，覆盖含数据面在内的全部
    // 响应（见 `AppState::router`）。留在这里只会覆盖伪装这一条路径。
    match &state.disguise.upstream {
        Some(base) => {
            let method = req.method().as_str().to_string();
            let uri = req
                .uri()
                .path_and_query()
                .map(|pq| pq.as_str().to_string())
                .unwrap_or_else(|| "/".into());
            let headers = req
                .headers()
                .iter()
                .filter_map(|(k, v)| {
                    v.to_str().ok().map(|v| (k.as_str().to_string(), v.to_string()))
                })
                .collect();
            let body = axum::body::to_bytes(req.into_body(), 16 << 20)
                .await
                .unwrap_or_default();
            disguise::handle_upstream(method, uri, headers, body, base).await
        }
        None => {
            let method = req.method().as_str().to_string();
            let path = req.uri().path().to_string();
            disguise::handle_static(&method, &path).await
        }
    }
}

/// 唯一入口：任何方法任何路径。
async fn fallback(State(state): State<Arc<AppState>>, mut req: axum::http::Request<Body>) -> Response {
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let uri = req.uri().clone();
    // 只在协议路径的成功响应上使用（见 apply_cors 注释）。
    let cors = cors_origin(&req);

    if method == "POST" && path == "/api/sync" {
        let n = query_param(&uri, "n").and_then(|v| v.parse::<u64>().ok());
        let sid = query_param(&uri, "sid").and_then(|s| parse_sid(&s));
        let body = match axum::body::to_bytes(std::mem::replace(&mut req, axum::http::Request::builder().uri("/").body(Body::empty()).unwrap()).into_body(), 1 << 20).await {
            Ok(b) => b,
            Err(_) => return disguise_resp(&state, req).await,
        };
        match (n, sid) {
            (Some(0), Some(sid)) => {
                let mut r = handshake(&state, sid, &body, &method, &path).await;
                // 握手失败时 handshake 内部已转伪装；那种响应是 200 nginx 页，
                // 补 CORS 头会让它与协议响应可区分，故只在 200+TU 时补。
                if r.status() == StatusCode::OK && r.headers().get(header::CONTENT_TYPE).is_none() {
                    apply_cors(&mut r, &cors);
                }
                r
            }
            (Some(n @ 1..), Some(sid)) => {
                // 有效会话 → 恒 204 空 body；无效/死会话 → 伪装（无差别）。
                if state.store.push_post(&sid, n, body).await.is_ok() {
                    let mut r = Response::builder()
                        .status(StatusCode::NO_CONTENT)
                        .body(Body::empty())
                        .unwrap();
                    apply_cors(&mut r, &cors);
                    r
                } else {
                    disguise_resp(&state, bad_req()).await
                }
            }
            _ => disguise_resp(&state, bad_req()).await,
        }
    } else if method == "GET" && path == "/api/events" {
        let Some(sid) = query_param(&uri, "sid").and_then(|s| parse_sid(&s)) else {
            return disguise_resp(&state, req).await;
        };
        let mut r = attach(&state, sid, &method, &path).await;
        // attach 失败走伪装（无 text/event-stream 头），只给真流补。
        if r.headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|v| v == "text/event-stream")
        {
            apply_cors(&mut r, &cors);
        }
        r
    } else {
        disguise_resp(&state, req).await
    }
}

/// 跨源许可：回显请求的 `Origin`。
///
/// **这个函数看起来很宽松，但它不是防线。** 真正的防线在调用点：
/// `apply_cors` 只加在**认证成功**的响应上（§8：未认证请求一律走伪装处理器，
/// 那条路径根本不经过这里）。探测者发不出合法的 msg1，就永远看不到任何
/// CORS 痕迹 —— 无论他把 Origin 构造成什么样。
///
/// 放宽的原因（设计文档 §9.1）：单 WebView 承载多出站时，页面加载自宿主
/// 出站的域名，而请求发往其他出站的域名，二者本就不同域。服务端无从预知
/// 客户端把哪台机器当宿主，因此不能再做同域名判据。
///
/// 历史：此前限制为「Origin 与 Host 同域名」，那是为多端口条带
/// （同域名不同端口）设计的。该场景仍被覆盖 —— 它是本函数的一个特例。
fn cors_origin(req: &axum::http::Request<Body>) -> Option<String> {
    let origin = req.headers().get(header::ORIGIN)?.to_str().ok()?;
    // 仅做最低限度的形态校验：必须是个 scheme://host 形状的东西。
    // 目的不是安全（安全由调用点保证），而是避免把垃圾原样回显进响应头。
    let rest = origin.split("://").nth(1)?;
    let host = rest.split('/').next()?.split(':').next()?;
    (!host.is_empty()).then(|| origin.to_string())
}

/// 给协议响应补 CORS 头。仅用于认证成功的响应，伪装路径绝不调用。
fn apply_cors(resp: &mut Response, origin: &Option<String>) {
    let Some(o) = origin else { return };
    let Ok(v) = axum::http::HeaderValue::from_str(o) else { return };
    // emitter 用 credentials:'include'，故必须回显具体 Origin（不能用 `*`）
    // 并显式允许凭据。Vary: Origin 防止中间缓存把某个 Origin 的响应串给另一个。
    resp.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
    resp.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
        axum::http::HeaderValue::from_static("true"),
    );
    resp.headers_mut()
        .insert(header::VARY, axum::http::HeaderValue::from_static("Origin"));
}

/// 失败路径的最小请求（body 已消费的场合）。
fn bad_req() -> axum::http::Request<Body> {
    axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri("/api/sync")
        .body(Body::empty())
        .unwrap()
}

/// n=0 握手。任一失败 → 伪装处理器（与垃圾请求同路径、响应无差别）。
async fn handshake(
    state: &Arc<AppState>,
    sid: Sid,
    body: &[u8],
    method: &str,
    path: &str,
) -> Response {
    let fail = || async {
        disguise_resp(
            state,
            axum::http::Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    };

    // 首个 TU：u16 BE 长度前缀 + Noise msg1 密文。
    if body.len() < 2 {
        return fail().await;
    }
    let len = u16::from_be_bytes([body[0], body[1]]) as usize;
    if body.len() < 2 + len || len < 16 {
        return fail().await;
    }
    let msg1_cipher = &body[2..2 + len];

    // replay 键：完整 msg1 密文的 blake3 截断 16 字节。
    // snow 不暴露 ephemeral 公钥；同一 msg1 重放 → 同一密文 → 同一键，
    // 不同握手的 ephemeral 不同 → IK 密文必不同 → 键不同。等价于 ephemeral 指纹。
    let replay_key: [u8; 16] = blake3::hash(msg1_cipher).as_bytes()[..16]
        .try_into()
        .unwrap();

    // 解密（垃圾/错误密文在此失败）。
    let mut hs = match build_server(&state.keys.priv_key) {
        Ok(h) => h,
        Err(_) => return fail().await,
    };
    let mut plain_buf = vec![0u8; 65535];
    let plain_len = match hs.read_message(msg1_cipher, &mut plain_buf) {
        Ok(n) => n,
        Err(_) => return fail().await,
    };
    // 白名单（认证优先于载荷内容解析）。
    match hs.get_remote_static() {
        Some(rs) if state.keys.whitelist.contains(rs) => {}
        _ => return fail().await,
    }
    // version + ts 窗口。
    let msg1 = match decode_msg1(&plain_buf[..plain_len]) {
        Ok(m) => m,
        Err(_) => return fail().await,
    };
    if !ts_in_window(msg1.ts_ms, now_ms()) {
        return fail().await;
    }

    // 已见缓存：命中 → 重放 → 伪装。
    // 容量已满 → fail-closed（同样转伪装）：若按驱逐策略淘汰旧条目，攻击者
    // 可在窗口内用垃圾握手顶掉受害条目、随后重放该 msg1（nonce 重用 + 会话
    // 状态注入）。拒绝新握手无可用性损失——诚实客户端重试时缓存已被周期
    // 任务清理或 ts 窗口已过期。
    {
        let mut seen = state.seen.lock().await;
        if seen.contains_key(&replay_key) || seen.len() >= state.seen_capacity {
            return fail().await;
        }
        seen.insert(replay_key, std::time::Instant::now());
    }

    // mux 协商（§7.4）+ msg2。
    let (chosen, fb) = pick_mux(&msg1.mux_prefs, &state.enabled_mux);
    let msg2_plain = encode_msg2(&Msg2 {
        chosen_mux_id: chosen,
        fallback: fb,
    });
    let mut msg2_buf = vec![0u8; 65535];
    let msg2_len = hs.write_message(&msg2_plain, &mut msg2_buf).unwrap();
    let msg2_cipher = &msg2_buf[..msg2_len];

    let transport = match hs.into_transport_mode() {
        Ok(t) => t,
        Err(_) => return fail().await,
    };

    // 建会话（SessionStore 管 seq/GC；Noise 与下行通道入旁路表）。
    state.store.create(sid).await;
    let (dl_tx, dl_rx) = mpsc::channel::<Bytes>(256);
    state.downlink_tx.lock().await.insert(sid, dl_tx.clone());
    state.downlink_rx.lock().await.insert(sid, dl_rx);
    spawn_session(state.clone(), sid, chosen, transport, msg1.group_id, msg1.ip_strategy);

    // 200 + TU(msg2)。
    let mut resp_body = Vec::with_capacity(2 + msg2_cipher.len());
    resp_body.extend_from_slice(&(msg2_cipher.len() as u16).to_be_bytes());
    resp_body.extend_from_slice(msg2_cipher);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::SERVER, "nginx")
        .body(Body::from(resp_body))
        .unwrap()
}

/// GET /api/events：挂载 + TU 字节流。
async fn attach(state: &Arc<AppState>, sid: Sid, method: &str, path: &str) -> Response {
    let fail = || async {
        disguise_resp(
            state,
            axum::http::Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    };
    // 挂载（已挂载/无会话 → Err → 伪装）。句柄持有至响应流结束。
    let handle = match state.store.attach_downlink(&sid).await {
        Ok(h) => h,
        Err(_) => return fail().await,
    };
    let Some(dl_rx) = state.downlink_rx.lock().await.remove(&sid) else {
        return fail().await;
    };

    // 句柄随流存活：map 闭包 move 捕获 handle，Body 消费完毕（流结束/客户端
    // 断开）时闭包 drop → DownlinkHandle::Drop → 异步 GC 会话。
    let stream = ReceiverStream::new(dl_rx).map(move |b| {
        let _keep = &handle;
        Ok::<Bytes, std::io::Error>(b)
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::SERVER, "nginx")
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// 下行合并的效果计数：发出的 HTTP chunk 数、这些 chunk 由多少次 read 拼成、
/// 总字节。`WSIEVE_DL_STATS=<n>` 时每 n 个 chunk 往 stderr 打一行。
///
/// 存在的理由是「合并率不能靠猜」：reads/chunks 这个比值直接就是平均每个
/// chunk 并进了几次 read，1.0 就是压根没合并上。
static DL_CHUNKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static DL_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static DL_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn dl_stats_every() -> usize {
    static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("WSIEVE_DL_STATS")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    })
}

/// 下行泵单次 read 的上限，等于 dl duplex 的缓冲大小。
const DL_READ_CHUNK: usize = 65536;
/// 合并后单个 HTTP chunk 的字节上限。取值等于 [`DL_READ_CHUNK`]，于是批量
/// 下载（一次 read 就填满）走不进合并分支，半点额外调度都不加。
const DL_MERGE_MAX: usize = DL_READ_CHUNK;

/// 合并最多让出几轮调度。`WSIEVE_DL_MERGE_ROUNDS`，0 = 关闭合并。
///
/// 用「让出轮次」而不是「等多少微秒」当旋钮，是因为定时窗口对**孤立**小包
/// 是纯亏：那种场景下续读必然空手而归，等的每一微秒都原样计进 P50。让出一
/// 轮的代价只有一次任务重排（微秒级），而且空手就立刻收手——爆发期自动多攒
/// 几轮，安静期一轮都不多花，不需要任何时钟。
const DL_MERGE_ROUNDS_DEFAULT: usize = 4;

/// 硬等待窗口，`WSIEVE_DL_MERGE_US`，默认 0（不等）。
///
/// 让出轮次拿不到的那部分 TU，只能靠真的等。这条路拿延迟换 CPU：实测 400µs
/// 能把两端 CPU 各砍三成，代价是并发 P50 涨 1ms。默认关，留给 CPU 吃紧的
/// 部署自己开。
fn dl_merge_cfg() -> (usize, u64) {
    static V: std::sync::OnceLock<(usize, u64)> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        let rounds = std::env::var("WSIEVE_DL_MERGE_ROUNDS")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(DL_MERGE_ROUNDS_DEFAULT);
        let us = std::env::var("WSIEVE_DL_MERGE_US")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        (rounds, us)
    })
}

/// 非阻塞地把 duplex 里此刻**已有**的字节续读进 `agg`。返回续读成功的次数。
///
/// 用 `poll_fn` 手动 poll 一次而不是 `read().await`：后者没数据就会挂起，
/// 那正是要避开的——这里要的是"有就拿，没有立刻走"。
async fn drain_ready(
    src: &mut tokio::io::DuplexStream,
    agg: &mut bytes::BytesMut,
    scratch: &mut [u8],
) -> usize {
    use std::pin::Pin;
    use std::task::Poll;
    use tokio::io::{AsyncRead, ReadBuf};
    std::future::poll_fn(move |cx| {
        let mut got = 0usize;
        while agg.len() < DL_MERGE_MAX {
            let cap = scratch.len().min(DL_MERGE_MAX - agg.len());
            let mut rb = ReadBuf::new(&mut scratch[..cap]);
            match Pin::new(&mut *src).poll_read(cx, &mut rb) {
                Poll::Ready(Ok(())) => {
                    let n = rb.filled().len();
                    if n == 0 {
                        break; // EOF：留给下一轮阻塞 read 去发现并收尾
                    }
                    agg.extend_from_slice(rb.filled());
                    got += 1;
                }
                // 出错同样交给下一轮阻塞 read 处理，这里只管别把已攒的丢了
                Poll::Ready(Err(_)) | Poll::Pending => break,
            }
        }
        Poll::Ready(got)
    })
    .await
}

/// 把「此刻前后几十微秒内的多个下行 TU」收进同一个 HTTP chunk。返回续读次数。
///
/// 不合并的话，并发小包时每个响应的 TU 各自成一个 chunk：客户端要多一次
/// WebKit 分片交付、多走一轮 emitter 循环、多一次 IPC。客户端 emitter 已有
/// 一个"让出一轮问问还有没有下一片"的探测，但让出一轮之后 WebKit 往往还
/// 没来得及交付——在服务端合并才是治本的位置。
///
/// TU 自带 2 字节长度前缀，客户端 `TuDecoder` 本来就按前缀拆，一个 body 里
/// 塞多少个 TU 都认（上行早就这么干了），所以协议侧零改动。
async fn merge_downlink(
    src: &mut tokio::io::DuplexStream,
    agg: &mut bytes::BytesMut,
    scratch: &mut [u8],
) -> usize {
    let (rounds, window_us) = dl_merge_cfg();
    // 批量下载：首次 read 已填满，合并无从谈起，直接走。
    if rounds == 0 || agg.len() >= DL_MERGE_MAX {
        return 0;
    }
    // 让出一轮，给同 runtime 上已就绪的 mux/noise 任务一个把各自 TU 写进
    // duplex 的机会——它们和本任务往往只差几十微秒。还有货就再让一轮：
    // 「还在出货就继续攒」本身就是自适应，空手一轮立刻收手。
    let mut extra = 0;
    for _ in 0..rounds {
        tokio::task::yield_now().await;
        let got = drain_ready(src, agg, scratch).await;
        extra += got;
        if got == 0 || agg.len() >= DL_MERGE_MAX {
            break;
        }
    }
    // 让出轮次够不着的那批，只能真的等。默认关（window_us = 0）。
    if extra > 0 && window_us > 0 && agg.len() < DL_MERGE_MAX {
        tokio::time::sleep(Duration::from_micros(window_us)).await;
        extra += drain_ready(src, agg, scratch).await;
    }
    extra
}

/// 下行泵：dl 管道的密文 TU 字节 → GET 响应通道，一次 send 就是一个 HTTP
/// chunk。通道满 = 背压 = mux 写停滞，这是有意的。
///
/// 字节流语义必须原样保持：TU 靠 2 字节长度前缀自定界，合并只改变「多少
/// 字节挤进同一个 chunk」，绝不能丢字节、乱序或重复。
async fn downlink_pump(mut src: tokio::io::DuplexStream, tx: mpsc::Sender<Bytes>) {
    use tokio::io::AsyncReadExt as _;
    let mut scratch = vec![0u8; DL_READ_CHUNK];
    let mut agg = bytes::BytesMut::with_capacity(DL_MERGE_MAX);
    loop {
        let n = match src.read(&mut scratch).await {
            Ok(0) | Err(_) => break, // 会话任务退出（mux 关闭）
            Ok(n) => n,
        };
        agg.extend_from_slice(&scratch[..n]);
        let merged = merge_downlink(&mut src, &mut agg, &mut scratch).await;
        let every = dl_stats_every();
        if every > 0 {
            use std::sync::atomic::Ordering::Relaxed;
            DL_READS.fetch_add(1 + merged, Relaxed);
            DL_BYTES.fetch_add(agg.len(), Relaxed);
            let c = DL_CHUNKS.fetch_add(1, Relaxed) + 1;
            if c % every == 0 {
                eprintln!(
                    "dl_stats chunks={c} reads={} bytes={} reads_per_chunk={:.2}",
                    DL_READS.load(Relaxed),
                    DL_BYTES.load(Relaxed),
                    DL_READS.load(Relaxed) as f64 / c as f64,
                );
            }
        }
        if tx.send(agg.split().freeze()).await.is_err() {
            return; // GET 断开
        }
    }
}

/// 会话任务（Task 15 全栈接线）：SessionStore 上行（重排后的 TU 字节）↔
/// NoiseStream ↔ mux ↔ remote 拨号泵。
///
/// 结构：两条独立管道——上行泵把 SessionStore::read 的字节写进 up 管道
/// （NoiseStream 读半解密给 mux）；mux 下行输出经 NoiseStream 加密写进
/// dl 管道，下行泵读出送 GET 响应通道。保活 PADDING TU 经 PadHandle
/// 进入 dl 管道，与数据 TU 的顺序由 state 锁串行化，nonce 序不乱。
///
/// 启动即跑（不等 GET attach）：下行通道有界缓冲（256 TU），客户端正常
/// 情况毫秒级 attach；30s attach 窗口内不 attach 则会话被 GC，本任务随
/// SessionStore::read 返回 SessionGone 退出——背压停滞与死亡二选一，可接受。
fn spawn_session(
    state: Arc<AppState>,
    sid: Sid,
    chosen: MuxId,
    transport: TransportState,
    group_id: u128,
    ip_strategy: IpStrategy,
) {
    tokio::spawn(async move {
        let (up_pump_side, up_noise_side) = tokio::io::duplex(65536);
        let (dl_noise_side, dl_pump_side) = tokio::io::duplex(65536);
        let noise = wsieve_proto::noise_stream::NoiseStream::new(
            transport,
            PairIo {
                read: up_noise_side,
                write: dl_noise_side,
            },
        );
        let pad = noise.pad_handle();
        spawn_keepalive(state.clone(), sid, pad);

        // 上行泵：SessionStore::read（阻塞到数据/会话死亡）→ up 管道。
        {
            let state = state.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt as _;
                let mut up = up_pump_side;
                let mut buf = vec![0u8; 65536];
                loop {
                    match state.store.read(&sid, &mut buf).await {
                        Ok(n) if n > 0 => {
                            if up.write_all(&buf[..n]).await.is_err() {
                                break; // 会话任务已退出
                            }
                        }
                        Ok(_) => continue,
                        Err(_) => {
                            break;
                        }
                    }
                }
                let _ = up.shutdown().await;
            });
        }

        // 下行泵：dl 管道密文 TU → GET 响应通道。通道满 = 背压 = mux 写停滞。
        {
            let tx = dl_tx_clone(&state, &sid).await;
            tokio::spawn(downlink_pump(dl_pump_side, tx));
        }

        // mux + remote：NoiseStream 即 mux 的底层流。
        let io: wsieve_mux::MuxStream = Box::new(noise);
        if let Ok(mux) = wsieve_mux::mux_server_factory(chosen, io).await {
            let mux: Arc<dyn wsieve_mux::Mux> = Arc::from(mux);
            // 会话组登记：同一 group_id 的会话构成一组，新 conn 的下行 lane
            // 在组内轮转开 → 下载跨多条 TCP（多拥塞窗口）。组表存 Weak，
            // 本处的 Arc 是唯一强引用，会话任务退出即释放。
            let group = state.session_groups.join(group_id, &mux);
            // 注意：会话死亡不清理 stripe_registry——conn 由自身 lane
            // EOF/CLOSE 机制终结，跨会话存活的 conn 不得被会话拆除带走。
            remote::session_loop(mux.clone(), state.stripe_registry.clone(), group, ip_strategy)
                .await;
            // 会话拆除：退出会话组（组空则删组条目）。
            state.session_groups.leave(group_id, &mux);
        }
        // 会话终结：清理旁路表 + 杀会话（上行泵随 SessionGone 退出）。
        state.downlink_tx.lock().await.remove(&sid);
        state.downlink_rx.lock().await.remove(&sid);
        state.store.kill(&sid).await;
    });
}

/// 读侧来自 up 管道、写侧进 dl 管道的组合 IO（NoiseStream 的底层）。
struct PairIo {
    read: tokio::io::DuplexStream,
    write: tokio::io::DuplexStream,
}

impl tokio::io::AsyncRead for PairIo {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.read).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for PairIo {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.write).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.write).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.write).poll_shutdown(cx)
    }
}

/// 取会话下行通道发送端（spawn 时条目必然存在：handshake 刚插入）。
async fn dl_tx_clone(state: &Arc<AppState>, sid: &Sid) -> mpsc::Sender<Bytes> {
    state
        .downlink_tx
        .lock()
        .await
        .get(sid)
        .cloned()
        .unwrap_or_else(|| mpsc::channel(1).0)
}

/// 会话保活任务（spec §6.5）：每区间随机延迟，经 PadHandle 向同一 Noise
/// nonce 序列写一个真实加密的 PADDING TU（Task 15：与 mux 数据流同管道）。
fn spawn_keepalive(
    state: Arc<AppState>,
    sid: Sid,
    pad: wsieve_proto::noise_stream::PadHandle,
) {
    tokio::spawn(async move {
        use rand::Rng;
        loop {
            let delay_ms =
                rand::rng().random_range(state.keepalive.min_ms..=state.keepalive.max_ms);
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            if pad.write_padding().await.is_err() {
                // 会话任务已退出（管道断）：清理 + 杀会话
                state.downlink_tx.lock().await.remove(&sid);
                state.downlink_rx.lock().await.remove(&sid);
                state.store.kill(&sid).await;
                return;
            }
        }
    });
}

/// spec §7.4：按客户端偏好顺序取第一个服务端也支持的；无交集 → 基线 + fallback=true。
/// 永不失败——任何情况下连接都要建起来。
pub fn pick_mux(client_prefs: &[MuxId], server_enabled: &[MuxId]) -> (MuxId, bool) {
    for &c in client_prefs {
        if server_enabled.contains(&c) {
            return (c, false);
        }
    }
    (MuxId::Wsmux, true) // 基线回退
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 交集命中时取客户端偏好顺序的第一个，而非服务端顺序。
    #[test]
    fn intersection_takes_first_client_pref() {
        let (chosen, fb) = pick_mux(
            &[MuxId::Wsmux, MuxId::Wsmux],
            &[MuxId::Wsmux, MuxId::Wsmux, MuxId::Wsmux],
        );
        assert_eq!(chosen, MuxId::Wsmux);
        assert!(!fb);
    }

    /// 无交集 → 回退到基线。
    ///
    /// 只剩一种 mux 之后，"无交集"没法再拿两个非空列表构造了；服务端启用列表
    /// 为空是仅存的一种，语义与原来一致。
    #[test]
    fn no_intersection_falls_back_to_the_baseline() {
        let (chosen, fb) = pick_mux(&[MuxId::Wsmux], &[]);
        assert_eq!(chosen, MuxId::Wsmux);
        assert!(fb);
    }

    /// 客户端偏好为空 → 直接回退。
    #[test]
    fn empty_client_prefs_fall_back() {
        let (chosen, fb) = pick_mux(&[], &[MuxId::Wsmux]);
        assert_eq!(chosen, MuxId::Wsmux);
        assert!(fb);
    }

    /// 重复/乱序输入行为正常：命中即返回，服务端列表含重复无影响。
    #[test]
    fn dedup_and_odd_inputs_sane() {
        let (chosen, fb) = pick_mux(
            &[MuxId::Wsmux, MuxId::Wsmux, MuxId::Wsmux],
            &[MuxId::Wsmux, MuxId::Wsmux, MuxId::Wsmux],
        );
        assert_eq!(chosen, MuxId::Wsmux);
        assert!(!fb);

        // 服务端全空也算无交集
        let (chosen, fb) = pick_mux(&[MuxId::Wsmux], &[]);
        assert_eq!(chosen, MuxId::Wsmux);
        assert!(fb); // 注意：即使客户端要的就是基线那种，服务端未启用也算 fallback
    }
}

#[cfg(test)]
mod downlink_merge_tests {
    use super::*;
    use tokio::io::AsyncWriteExt as _;

    /// 把 `n` 个带序号的小块写进 duplex，收集泵吐出来的所有 chunk。
    /// 返回 (拼接结果, chunk 数)。
    async fn pump_roundtrip(chunks: &[Vec<u8>]) -> (Vec<u8>, usize) {
        let (mut writer, reader) = tokio::io::duplex(DL_READ_CHUNK);
        let (tx, mut rx) = mpsc::channel::<Bytes>(1024);
        let pump = tokio::spawn(downlink_pump(reader, tx));

        let owned: Vec<Vec<u8>> = chunks.to_vec();
        let feeder = tokio::spawn(async move {
            for c in &owned {
                writer.write_all(c).await.unwrap();
                // 让出一轮，模拟「多个 TU 前后脚落地」——泵有机会合并，也有
                // 机会不合并，两种都必须给出同样的字节流。
                tokio::task::yield_now().await;
            }
            writer.shutdown().await.unwrap();
        });

        let mut out = Vec::new();
        let mut count = 0usize;
        while let Some(b) = rx.recv().await {
            assert!(
                b.len() <= DL_MERGE_MAX,
                "chunk {} 超出合并上限 {}",
                b.len(),
                DL_MERGE_MAX
            );
            assert!(!b.is_empty(), "不该发出空 chunk");
            out.extend_from_slice(&b);
            count += 1;
        }
        feeder.await.unwrap();
        pump.await.unwrap();
        (out, count)
    }

    /// 合并只改分块边界，不改字节流——一个字节都不能丢、不能乱、不能重。
    ///
    /// 这是整套合并逻辑唯一不可妥协的性质：TU 靠 2 字节长度前缀自定界，
    /// 边界错一个字节，客户端 `TuDecoder` 就会把密文当成长度去解析，
    /// 整条会话当场跑飞。
    #[tokio::test]
    async fn merging_preserves_byte_stream_exactly() {
        let chunks: Vec<Vec<u8>> = (0u16..200)
            .map(|i| {
                let mut v = i.to_be_bytes().to_vec();
                v.extend(std::iter::repeat(i as u8).take(60));
                v
            })
            .collect();
        let expect: Vec<u8> = chunks.iter().flatten().copied().collect();

        let (got, count) = pump_roundtrip(&chunks).await;
        assert_eq!(got, expect, "字节流被合并改动了");
        assert!(count >= 1 && count <= chunks.len());
    }

    /// 写入端关闭后泵必须退出，通道随之关闭——否则 GET 响应流永远不结束。
    #[tokio::test]
    async fn pump_exits_on_writer_eof() {
        let (got, _) = pump_roundtrip(&[b"hello".to_vec()]).await;
        assert_eq!(got, b"hello");
    }

    /// 合并确实在减少 chunk 数——否则整套逻辑就是白跑一趟调度。
    ///
    /// 不走 [`downlink_pump`] 而直接驱动 [`merge_downlink`]：泵里那次
    /// `tx.send().await` 会改变 current_thread 上的任务排队顺序，让写入方
    /// 每次都恰好抢在合并窗口之前把上一块交付完，于是一块都并不上。这里要
    /// 钉的是合并循环本身，把那个干扰项摘掉。
    #[tokio::test]
    async fn merge_loop_actually_coalesces() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        const WRITES: usize = 10;
        let (mut w, mut r) = tokio::io::duplex(DL_READ_CHUNK);
        let feeder = tokio::spawn(async move {
            for i in 0..WRITES as u8 {
                w.write_all(&[i; 32]).await.unwrap();
                tokio::task::yield_now().await;
            }
            w.shutdown().await.unwrap();
        });

        let mut scratch = vec![0u8; DL_READ_CHUNK];
        let mut agg = bytes::BytesMut::new();
        let (mut out, mut chunks) = (Vec::new(), 0usize);
        loop {
            let n = match r.read(&mut scratch).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            agg.extend_from_slice(&scratch[..n]);
            merge_downlink(&mut r, &mut agg, &mut scratch).await;
            out.extend_from_slice(&agg);
            agg.clear();
            chunks += 1;
        }
        feeder.await.unwrap();

        let expect: Vec<u8> = (0..WRITES as u8).flat_map(|i| [i; 32]).collect();
        assert_eq!(out, expect, "合并把字节流改了");
        assert!(chunks < WRITES, "{WRITES} 次写入并出 {chunks} 个 chunk，没合并上");
    }

    /// 单块就超过合并上限时照常整块转发，不被截断。
    #[tokio::test]
    async fn oversized_single_write_is_not_truncated() {
        let big = vec![0xABu8; DL_MERGE_MAX + 4096];
        let (got, _) = pump_roundtrip(&[big.clone()]).await;
        assert_eq!(got.len(), big.len());
        assert_eq!(got, big);
    }
}

#[cfg(test)]
mod cors_tests {
    use axum::body::Body;
    use axum::http::{header, Request};

    fn req(origin: &str, host: &str) -> Request<Body> {
        Request::builder()
            .header(header::ORIGIN, origin)
            .header(header::HOST, host)
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn same_domain_different_port_still_allowed() {
        // 多端口条带的既有场景，不能回归
        let r = req("https://a.com:18444", "a.com");
        assert_eq!(super::cors_origin(&r).as_deref(), Some("https://a.com:18444"));
    }

    #[test]
    fn cross_domain_is_now_allowed() {
        // 单 WebView 承载多出站：页面在 A，请求发往 B
        let r = req("https://host-a.com", "server-b.net");
        assert_eq!(super::cors_origin(&r).as_deref(), Some("https://host-a.com"));
    }

    #[test]
    fn missing_origin_yields_none() {
        // 同源请求不带 Origin —— 不该凭空造一个 CORS 头出来
        let r = Request::builder()
            .header(header::HOST, "a.com")
            .body(Body::empty())
            .unwrap();
        assert!(super::cors_origin(&r).is_none());
    }

    #[test]
    fn malformed_origin_yields_none() {
        let r = req("not-a-url", "a.com");
        assert!(super::cors_origin(&r).is_none());
    }
}

