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
use wsieve_proto::tu::{encode_frame, Frame};
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

/// 共享状态（公开构造，测试直连）。
pub struct AppState {
    pub store: SessionStore,
    pub keys: ServerKeys,
    /// sid → 会话 Noise 状态（上行解密 / 下行加密共用）。
    noise: Mutex<HashMap<Sid, TransportState>>,
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
}

impl AppState {
    pub fn new(
        keys: ServerKeys,
        enabled_mux: Vec<MuxId>,
        keepalive: KeepaliveRange,
        seen_capacity: usize,
    ) -> Arc<Self> {
        let state = Arc::new(Self {
            store: SessionStore::new(), // 自带 GC 任务（attach 30s / 上行空闲 180s）
            keys,
            noise: Mutex::new(HashMap::new()),
            downlink_tx: Mutex::new(HashMap::new()),
            downlink_rx: Mutex::new(HashMap::new()),
            seen: Mutex::new(HashMap::new()),
            seen_capacity,
            enabled_mux,
            keepalive,
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

async fn disguise_resp(method: &str, path: &str) -> Response {
    disguise::handle(method, path).await
}

/// 唯一入口：任何方法任何路径。
async fn fallback(State(state): State<Arc<AppState>>, req: axum::http::Request<Body>) -> Response {
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let uri = req.uri().clone();

    if method == "POST" && path == "/api/sync" {
        let n = query_param(&uri, "n").and_then(|v| v.parse::<u64>().ok());
        let sid = query_param(&uri, "sid").and_then(|s| parse_sid(&s));
        let body = match axum::body::to_bytes(req.into_body(), 1 << 20).await {
            Ok(b) => b,
            Err(_) => return disguise_resp(&method, &path).await,
        };
        match (n, sid) {
            (Some(0), Some(sid)) => handshake(&state, sid, &body, &method, &path).await,
            (Some(n @ 1..), Some(sid)) => {
                // 有效会话 → 恒 204 空 body；无效/死会话 → 伪装（无差别）。
                if state.store.push_post(&sid, n, body).await.is_ok() {
                    Response::builder()
                        .status(StatusCode::NO_CONTENT)
                        .body(Body::empty())
                        .unwrap()
                } else {
                    disguise_resp(&method, &path).await
                }
            }
            _ => disguise_resp(&method, &path).await,
        }
    } else if method == "GET" && path == "/api/events" {
        let Some(sid) = query_param(&uri, "sid").and_then(|s| parse_sid(&s)) else {
            return disguise_resp(&method, &path).await;
        };
        attach(&state, sid, &method, &path).await
    } else {
        disguise_resp(&method, &path).await
    }
}

/// n=0 握手。任一失败 → 伪装处理器（与垃圾请求同路径、响应无差别）。
async fn handshake(
    state: &Arc<AppState>,
    sid: Sid,
    body: &[u8],
    method: &str,
    path: &str,
) -> Response {
    let fail = || async { disguise_resp(method, path).await };

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
    let (dl_tx, dl_rx) = mpsc::channel::<Bytes>(64);
    state.noise.lock().await.insert(sid, transport);
    state.downlink_tx.lock().await.insert(sid, dl_tx.clone());
    state.downlink_rx.lock().await.insert(sid, dl_rx);
    spawn_keepalive(state.clone(), sid);

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
    let fail = || async { disguise_resp(method, path).await };
    // 挂载（已挂载/无会话 → Err → 伪装）。句柄持有至响应流结束。
    let _downlink = match state.store.attach_downlink(&sid).await {
        Ok(h) => h,
        Err(_) => return fail().await,
    };
    let Some(dl_rx) = state.downlink_rx.lock().await.remove(&sid) else {
        return fail().await;
    };

    let stream = ReceiverStream::new(dl_rx);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::SERVER, "nginx")
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(
            stream.map(Result::<Bytes, std::io::Error>::Ok),
        ))
        .unwrap()
}

/// 会话保活任务：每区间随机延迟，向下行通道写一个真实加密的 PADDING TU
/// （spec §6.5）。Task 13 内这是下行流的唯一生产者；Task 15 的 mux/remote
/// 数据流将与本任务并存于同一通道。
fn spawn_keepalive(state: Arc<AppState>, sid: Sid) {
    tokio::spawn(async move {
        use rand::Rng;
        loop {
            let delay_ms =
                rand::rng().random_range(state.keepalive.min_ms..=state.keepalive.max_ms);
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;

            let plain = {
                let mut noise_guard = state.noise.lock().await;
                let Some(noise) = noise_guard.get_mut(&sid) else {
                    return; // 会话已不存在
                };
                let mut rng = rand::rng();
                let plain = match encode_frame(&Frame::Padding, &mut rng) {
                    Ok(p) => p,
                    Err(_) => return,
                };
                let mut cipher_buf = vec![0u8; 65535];
                let Ok(cipher_len) = noise.write_message(&plain, &mut cipher_buf) else {
                    return;
                };
                let mut tu = Vec::with_capacity(2 + cipher_len);
                tu.extend_from_slice(&(cipher_len as u16).to_be_bytes());
                tu.extend_from_slice(&cipher_buf[..cipher_len]);
                tu
            };
            let dl = state.downlink_tx.lock().await;
            let Some(tx) = dl.get(&sid) else {
                return;
            };
            if tx.send(Bytes::from(plain)).await.is_err() {
                return; // GET 已断开
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
