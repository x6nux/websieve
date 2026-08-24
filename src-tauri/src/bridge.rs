//! WebViewTransport：HttpTransport 的 WebView 实现（spec §4/§5.1/§9.1）。
//!
//! Rust → JS 的两条路（spec §3.2 IPC 纪律：二进制走顶层 `InvokeBody::Raw`
//! 快路径，绝不嵌在 JSON object 里）：
//!
//! - `post(path, body)`：Rust 侧 `webview.eval()` 调 `window.__wsieve.post(n,
//!   path, null, <base64>)`，JS 完成后 `invoke('wsieve_post_result', ...)`
//!   回填 pending 表。base64 用于上行是可接受的折衷：上行每个 POST 一次、
//!   ≤1 MB，而下行（数据主体）走 JS→Rust 的 `InvokeBody::Raw`，每 64 KB
//!   一次零拷贝级传输。
//! - `get_stream(path)`：Rust `eval` `window.__wsieve.openStream(n, path)`，
//!   JS 逐块 `invoke('wsieve_stream_chunk', chunk)`（Uint8Array 顶层参数 →
//!   content-type application/octet-stream → `InvokeBody::Raw`，spec §3.2），
//!   流结束/出错再 invoke `wsieve_stream_end` / `wsieve_stream_err`。
//!
//! Tauri 2 的 `WebviewWindow::eval` 是官方的 Rust→JS 通道（等价
//! initialization_script 之外的运行时注入），`InvokeBody::Raw` 是官方的
//! JS→Rust 二进制快路径（tauri/src/ipc/protocol.rs：非 JSON content-type
//! 直接 `Vec<u8>`），两者组合避免 `window.__TAURI__.core.invoke` 在 Rust
//! 侧的等价物缺失问题。
//!
//! 死亡检测：任一 invoke 报错（webview 崩溃/导航离开）或心跳超时
//! （>15s 无 `wsieve_heartbeat`）→ 标记死亡 → 所有 pending waiter 立即
//! 失败 → XhttpConn 会话死 → proxy.rs 重载页面重建（spec §9.1）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
use wsieve_transport::{HttpTransport, PostReply};

/// 心跳超时（spec：5s 心跳——间隔在 emitter.js 里——>15s 判死）。
pub const HEARTBEAT_STALE: Duration = Duration::from_secs(15);

/// pending POST：request_id -> 完成回调。
type PendingPosts = Arc<AsyncMutex<HashMap<u64, oneshot::Sender<anyhow::Result<PostReply>>>>>;
/// pending 流：request_id -> chunk 通道发送端。
type PendingStreams = Arc<AsyncMutex<HashMap<u64, mpsc::Sender<anyhow::Result<Bytes>>>>>;

pub struct WebViewTransport {
    inner: Arc<TransportInner>,
}

/// 与 Tauri 类型解耦的核心状态——可独立单元测试。
pub struct TransportCore {
    pub next_request: AtomicU64,
    pending_posts: PendingPosts,
    pending_streams: PendingStreams,
    dead: AtomicBool,
    last_heartbeat: Mutex<Instant>,
}

impl TransportCore {
    /// last_heartbeat 初始化为 now（stale 判定走默认语义）。
    pub fn new() -> Self {
        Self::with_initial_beat(Instant::now())
    }

    /// 「等首个心跳」语义：last_heartbeat 初始化为远古，stale()==false
    /// ⇔ 已收到过至少一次真实心跳。
    pub fn new_beat_pending() -> Self {
        Self::with_initial_beat(Instant::now() - HEARTBEAT_STALE - Duration::from_secs(1))
    }

    fn with_initial_beat(at: Instant) -> Self {
        Self {
            next_request: AtomicU64::new(1),
            pending_posts: Arc::new(AsyncMutex::new(HashMap::new())),
            pending_streams: Arc::new(AsyncMutex::new(HashMap::new())),
            dead: AtomicBool::new(false),
            last_heartbeat: Mutex::new(at),
        }
    }

    pub fn alloc_request_id(&self) -> u64 {
        self.next_request.fetch_add(1, Ordering::Relaxed)
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }

    /// 登记 pending POST；返回接收端。供 eval 分支 await。
    pub async fn register_post(&self, id: u64) -> oneshot::Receiver<anyhow::Result<PostReply>> {
        let (tx, rx) = oneshot::channel();
        self.pending_posts.lock().await.insert(id, tx);
        rx
    }

    /// 登记 pending 流；返回 chunk 接收端（含终止消息）。
    pub async fn register_stream(
        &self,
        id: u64,
    ) -> mpsc::Receiver<anyhow::Result<Bytes>> {
        let (tx, rx) = mpsc::channel::<anyhow::Result<Bytes>>(32);
        self.pending_streams.lock().await.insert(id, tx);
        // 正常结束：complete_stream drop 发送端 → 通道关闭 → BoxStream EOF；
        // 出错：complete_stream 先送一条 Err 再 drop。
        rx
    }

    /// JS 回填 post 结果。未知 id（已被超时清理）→ 静默丢弃。
    pub async fn complete_post(&self, id: u64, result: anyhow::Result<PostReply>) {
        if let Some(tx) = self.pending_posts.lock().await.remove(&id) {
            let _ = tx.send(result);
        }
    }

    /// JS 推送一个下行 chunk。
    pub async fn push_chunk(&self, id: u64, chunk: Bytes) {
        let mut map = self.pending_streams.lock().await;
        if let Some(tx) = map.get(&id) {
            if tx.send(Ok(chunk)).await.is_err() {
                map.remove(&id);
            }
        }
    }

    /// 流结束（Ok(None) 语义）或出错。
    pub async fn complete_stream(&self, id: u64, err: Option<anyhow::Error>) {
        let tx = self.pending_streams.lock().await.remove(&id);
        if let Some(tx) = tx {
            if let Some(e) = err {
                let _ = tx.send(Err(e)).await;
            }
            // drop tx 关闭通道 → 下游 BoxStream 结束
        }
    }

    /// 心跳到达。
    pub fn heartbeat(&self) {
        *self.last_heartbeat.lock().unwrap() = Instant::now();
    }

    /// 心跳是否超时。
    pub fn heartbeat_stale(&self) -> bool {
        self.last_heartbeat.lock().unwrap().elapsed() > HEARTBEAT_STALE
    }

    /// 标记死亡并唤醒所有 pending waiter（spec §9.1）。
    pub async fn mark_dead(&self, reason: &str) {
        if self.dead.swap(true, Ordering::Relaxed) {
            return; // 已死，幂等
        }
        tracing::warn!("WebViewTransport dead: {reason}");
        let posts = self.pending_posts.lock().await.drain().collect::<Vec<_>>();
        for (_, tx) in posts {
            let _ = tx.send(Err(anyhow::anyhow!("transport dead: {reason}")));
        }
        let streams = self.pending_streams.lock().await.drain().collect::<Vec<_>>();
        for (_, tx) in streams {
            let _ = tx.send(Err(anyhow::anyhow!("transport dead: {reason}"))).await;
        }
    }
}

impl Default for TransportCore {
    fn default() -> Self {
        Self::new()
    }
}

struct TransportInner {
    core: std::sync::RwLock<Arc<TransportCore>>,
    /// eval 目标：把 JS 命令送进 webview（tauri `Webview::eval`）。
    eval: Box<dyn Fn(String) + Send + Sync>,
    /// 请求基址（如 `https://x.com:18444`，无尾斜杠）。空 = 用相对路径，
    /// 即页面自身 origin（单会话时的原语义）。
    ///
    /// 多会话条带靠它把各会话分到不同 origin（同域名不同端口）——h2 只在
    /// 同 origin 内复用连接，分开 origin 才能拿到各自的 TCP 与拥塞窗口。
    /// 代价是这些 fetch 变成跨源：会触发 preflight、带 CORS 头、
    /// `Sec-Fetch-Site` 由 same-origin 变 same-site。这些全部在 TLS 内部，
    /// 中间人只看到若干条到 :443 的连接且 SNI 相同，故不损伤对外伪装；
    /// 且这些头仍由浏览器自然生成，不是我们伪造的。
    base: String,
}

impl TransportInner {
    fn core(&self) -> Arc<TransportCore> {
        self.core.read().unwrap().clone()
    }

    /// 把协议层给的相对 path 变成实际请求 URL。
    fn url(&self, path: &str) -> String {
        if self.base.is_empty() {
            path.to_string()
        } else {
            format!("{}{}", self.base, path)
        }
    }
}

#[async_trait::async_trait]
impl HttpTransport for WebViewTransport {
    async fn post(&self, path: &str, body: Bytes) -> anyhow::Result<PostReply> {
        let core = self.inner.core();
        if core.is_dead() {
            anyhow::bail!("transport dead");
        }
        let id = core.alloc_request_id();
        let rx = core.register_post(id).await;
        let b64 = bs64_encode(&body);
        let url = self.inner.url(path);
        // JS 侧 post() 完成后 invoke wsieve_post_result 回填。
        let js = format!(
            "window.__wsieve && window.__wsieve.post({id}, {url:?}, {b64:?});"
        );
        (self.inner.eval)(js);
        match rx.await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!("post waiter dropped (transport dead)")),
        }
    }

    async fn get_stream(
        &self,
        path: &str,
    ) -> anyhow::Result<futures::stream::BoxStream<'static, anyhow::Result<Bytes>>> {
        let core = self.inner.core();
        if core.is_dead() {
            anyhow::bail!("transport dead");
        }
        let id = core.alloc_request_id();
        let rx = core.register_stream(id).await;
        // 路径转义：{path:?} 的 Rust Debug 转义对非 ASCII 会产 `\u{...}`，
        // 非合法 JS。实际 path 均为本项目 ASCII 常量，这里换成显式 JS 字面量
        // 转义（引号/反斜杠），消除隐患。
        let js_path: String = self.inner.url(path).chars().map(|c| match c {
            '\\' => "\\\\".to_string(),
            '"' => "\\\"".to_string(),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => format!("\\u{:04x}", c as u32),
            c => c.to_string(),
        }).collect();
        let js = format!(
            "window.__wsieve && window.__wsieve.openStream({id}, \"{js_path}\");"
        );
        (self.inner.eval)(js);
        // 流被 drop（XhttpConn 会话死亡）时通知 JS 取消下载（spec §6.5）：
        // reader.cancel() 先、controller.abort() 后（emitter 侧钉死顺序）。
        let inner = Arc::clone(&self.inner);
        let cancel = move || {
            (inner.eval)(format!("window.__wsieve && window.__wsieve.cancelStream({id});"));
        };
        let inner_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        use futures::StreamExt as _;
        Ok(inner_stream
            .take_while(|item: &anyhow::Result<Bytes>| futures::future::ready(item.is_ok()))
            .chain(futures::stream::once(async move {
                cancel();
                Err(anyhow::anyhow!("stream ended"))
            }))
            .boxed())
    }
}

impl WebViewTransport {
    /// `eval` 闭包负责把 JS 命令送进 webview（tauri `Webview::eval`）。
    pub fn new(eval: Box<dyn Fn(String) + Send + Sync>) -> Arc<Self> {
        Self::with_base(eval, String::new())
    }

    /// 带基址的构造：多会话条带下每个会话一个 origin（同域名不同端口）。
    /// 各会话共享同一个 `TransportCore`（request_id 由它统一分配，全局唯一），
    /// 因此 IPC 侧无需区分来源。
    pub fn with_base(eval: Box<dyn Fn(String) + Send + Sync>, base: String) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(TransportInner {
                core: std::sync::RwLock::new(Arc::new(TransportCore::new())),
                eval,
                base: base.trim_end_matches('/').to_string(),
            }),
        })
    }

    /// 注入外部 core（proxy 每代把 core 注册给 IPC 命令，transport 用同一份）。
    pub fn set_core(&self, core: Arc<TransportCore>) {
        *self.inner.core.write().unwrap() = core;
    }
}

/// 纯 Rust base64（标准字母表 + padding），避免只为上行引入依赖。
/// 上行 POST body ≤ 1MB 且每 POST 一次，编码成本可忽略。
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn bs64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_joins_base_and_path() {
        let t = WebViewTransport::with_base(Box::new(|_| {}), "https://x.com:18444/".into());
        // 尾斜杠必须归一，否则拼出 //api/sync 变成另一个路径
        assert_eq!(t.inner.url("/api/sync?n=0"), "https://x.com:18444/api/sync?n=0");
        let bare = WebViewTransport::new(Box::new(|_| {}));
        assert_eq!(bare.inner.url("/api/sync?n=0"), "/api/sync?n=0");
    }

    #[tokio::test]
    async fn post_evals_absolute_url_when_base_set() {
        let seen = Arc::new(Mutex::new(String::new()));
        let s2 = seen.clone();
        let t = WebViewTransport::with_base(
            Box::new(move |js| *s2.lock().unwrap() = js),
            "https://x.com:18445".into(),
        );
        // post 会一直等 JS 回填，这里只关心 eval 出去的 JS，超时即可
        let _ = tokio::time::timeout(
            Duration::from_millis(50),
            t.post("/api/sync?n=1", Bytes::from_static(b"x")),
        )
        .await;
        assert!(
            seen.lock().unwrap().contains("https://x.com:18445/api/sync?n=1"),
            "eval 出的 JS 应含绝对 URL: {}",
            seen.lock().unwrap()
        );
    }

    #[test]
    fn b64_known_vectors() {
        assert_eq!(bs64_encode(b""), "");
        assert_eq!(bs64_encode(b"f"), "Zg==");
        assert_eq!(bs64_encode(b"fo"), "Zm8=");
        assert_eq!(bs64_encode(b"foo"), "Zm9v");
        assert_eq!(bs64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[tokio::test]
    async fn pending_post_add_complete() {
        let core = TransportCore::new();
        let rx = core.register_post(7).await;
        core.complete_post(7, Ok(PostReply { status: 204, body: Bytes::new() }))
            .await;
        let r = rx.await.unwrap().unwrap();
        assert_eq!(r.status, 204);
    }

    #[tokio::test]
    async fn unknown_post_completion_is_silent() {
        let core = TransportCore::new();
        core.complete_post(999, Ok(PostReply { status: 200, body: Bytes::new() }))
            .await; // 不 panic
    }

    #[tokio::test]
    async fn stream_assembly_order_and_end() {
        let core = TransportCore::new();
        let mut rx = core.register_stream(1).await;
        core.push_chunk(1, Bytes::from_static(b"abc")).await;
        core.push_chunk(1, Bytes::from_static(b"def")).await;
        core.complete_stream(1, None).await;
        let mut got = Vec::new();
        while let Some(item) = rx.recv().await {
            got.extend_from_slice(&item.unwrap());
        }
        assert_eq!(got, b"abcdef");
    }

    #[tokio::test]
    async fn stream_error_propagates() {
        let core = TransportCore::new();
        let mut rx = core.register_stream(2).await;
        core.complete_stream(2, Some(anyhow::anyhow!("net err"))).await;
        assert!(rx.recv().await.unwrap().is_err());
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn death_marks_pending_and_is_idempotent() {
        let core = TransportCore::new();
        let rx = core.register_post(3).await;
        let mut srx = core.register_stream(4).await;
        core.mark_dead("test").await;
        assert!(rx.await.unwrap().is_err());
        assert!(srx.recv().await.unwrap().is_err());
        assert!(core.is_dead());
        core.mark_dead("again").await; // 幂等，不 panic
    }

    #[tokio::test]
    async fn heartbeat_staleness() {
        let core = TransportCore::new();
        assert!(!core.heartbeat_stale());
        *core.last_heartbeat.lock().unwrap() = Instant::now() - HEARTBEAT_STALE - Duration::from_secs(1);
        assert!(core.heartbeat_stale());
        core.heartbeat();
        assert!(!core.heartbeat_stale());
    }
}
