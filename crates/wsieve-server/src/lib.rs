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
use wsieve_proto::hello::{decode_msg1, encode_msg2, ts_in_window, MuxId, Msg2, TS_WINDOW_MS};
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
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/", any(fallback))
            .route("/{*rest}", any(fallback))
            .with_state(self)
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
    let alt_svc = state.disguise.alt_svc_port;
    let mut resp = match &state.disguise.upstream {
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
    };
    if let Some(p) = alt_svc {
        if let Ok(v) = axum::http::HeaderValue::from_str(&format!("h3=\":{p}\"; ma=86400")) {
            resp.headers_mut().insert(header::ALT_SVC, v);
        }
    }
    resp
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
    spawn_session(state.clone(), sid, chosen, transport, msg1.group_id);

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
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt as _;
                let mut src = dl_pump_side;
                let mut buf = vec![0u8; 65536];
                loop {
                    match src.read(&mut buf).await {
                        Ok(0) | Err(_) => break, // 会话任务退出（mux 关闭）
                        Ok(n) => {
                            if tx.send(Bytes::copy_from_slice(&buf[..n])).await.is_err() {
                                return; // GET 断开
                            }
                        }
                    }
                }
            });
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
            remote::session_loop(mux.clone(), state.stripe_registry.clone(), group).await;
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

/// spec §7.4：按客户端偏好顺序取第一个服务端也支持的；无交集 → yamux + fallback=true。
/// 永不失败——任何情况下连接都要建起来。
pub fn pick_mux(client_prefs: &[MuxId], server_enabled: &[MuxId]) -> (MuxId, bool) {
    for &c in client_prefs {
        if server_enabled.contains(&c) {
            return (c, false);
        }
    }
    (MuxId::Yamux, true) // 基线回退
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 交集命中时取客户端偏好顺序的第一个，而非服务端顺序。
    #[test]
    fn intersection_takes_first_client_pref() {
        let (chosen, fb) = pick_mux(
            &[MuxId::H2mux, MuxId::Smux],
            &[MuxId::Smux, MuxId::Yamux, MuxId::H2mux],
        );
        assert_eq!(chosen, MuxId::H2mux);
        assert!(!fb);
    }

    /// 无交集 → yamux + fallback。
    #[test]
    fn no_intersection_falls_back_to_yamux() {
        let (chosen, fb) = pick_mux(&[MuxId::Picomux], &[MuxId::Smux, MuxId::H2mux]);
        assert_eq!(chosen, MuxId::Yamux);
        assert!(fb);
    }

    /// 客户端偏好为空 → 直接回退。
    #[test]
    fn empty_client_prefs_fall_back() {
        let (chosen, fb) = pick_mux(&[], &[MuxId::Smux]);
        assert_eq!(chosen, MuxId::Yamux);
        assert!(fb);
    }

    /// 重复/乱序输入行为正常：命中即返回，服务端列表含重复无影响。
    #[test]
    fn dedup_and_odd_inputs_sane() {
        let (chosen, fb) = pick_mux(
            &[MuxId::Smux, MuxId::Smux, MuxId::Yamux],
            &[MuxId::Yamux, MuxId::Yamux, MuxId::Smux],
        );
        assert_eq!(chosen, MuxId::Smux);
        assert!(!fb);

        // 服务端全空也算无交集
        let (chosen, fb) = pick_mux(&[MuxId::Yamux], &[]);
        assert_eq!(chosen, MuxId::Yamux);
        assert!(fb); // 注意：即使客户端要的就是 yamux，服务端未启用也算 fallback
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
