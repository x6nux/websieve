//! 客户端总装（spec §4 分层：socks5 → mux → noise → xhttp → fetch）。
//!
//! 会话生命周期（spec §9.1）：transport 死亡/会话死亡 → 拆 mux → 重载
//! WebView 页面（emitter 随 initialization_script 重新注入）→ 重新握手 →
//! 重建。连续失败指数退避 100ms → 30s 封顶，成功即重置。
//!
//! 关键结构：每代会话一个新的 `WebViewTransport`（自带 pending 表与死亡
//! 标记），其 `core` 通过 `tauri::Manager::manage` 注册为该代的命令状态，
//! IPC 命令操作「当前代」。会话死亡信号 = `Notify`（由 transport core 的
//! 死亡监视任务或握手失败触发）。

use std::sync::Arc;
use std::time::Duration;

use tauri::Manager;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use wsieve_mux::stripe_runtime::{StripeCfg, StripeDialer};
use wsieve_mux::{mux_factory, Mux};
use wsieve_proto::hello::MuxId;
use wsieve_xhttp::client::{random_group_id, UpstreamCfg, XhttpConn};

use crate::bridge::{TransportCore, WebViewTransport};

#[derive(Clone)]
pub struct ProxyCfg {
    pub server_pub: [u8; 32],
    pub client_priv: [u8; 32],
    pub mux_prefs: Vec<MuxId>,
    pub socks_listen: String,
    /// 每个会话的请求基址；`None` = 相对路径（同源）。长度即会话数。
    /// 由 `shard_setup::plan` 产出：条带启用时会话 0 同源、其余各自一个
    /// 本地端口 origin；降级时只有一个 `None`。
    pub session_bases: Vec<Option<String>>,
}

/// IPC 命令可见的「当前代」状态：emitter 的所有 invoke 都打到这里。
pub struct CurrentCore(pub Arc<TransportCore>);

fn emit_status(app: &tauri::AppHandle, msg: &str) {
    tracing::info!("status: {msg}");
    let _ = tauri::Emitter::emit(app, "wsieve-status", msg.to_string());
}

fn eval_fn(app: tauri::AppHandle) -> Box<dyn Fn(String) + Send + Sync> {
    Box::new(move |js: String| {
        if let Some(w) = app.get_webview_window("main") {
            if let Err(e) = w.eval(&js) {
                tracing::warn!("eval failed: {e}");
            }
        }
    })
}

/// SOCKS5 handler（v2 条带化）：StripeDialer 开 conn（OPEN + TargetAddr
/// 首 lane），后台 accept 任务自动归并服务端新开的 DOWN lane。
fn socks_handler(
    dialer: Arc<tokio::sync::RwLock<Option<Arc<StripeDialer>>>>,
) -> impl Fn(wsieve_proto::addr::AddrPort) -> futures::future::BoxFuture<'static, std::io::Result<tokio::io::DuplexStream>>
       + Clone + Send + 'static {
    move |target| {
        let dialer = dialer.clone();
        Box::pin(async move {
            let guard = dialer.read().await;
            let Some(dialer) = guard.clone() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "no session",
                ));
            };
            drop(guard);
            let stream = dialer
                .connect(&target)
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            Ok(bridge_duplex(stream).await)
        })
    }
}

/// 把 StripeStreamHandle 包成 socks5::serve 要的 DuplexStream：开一条本地
/// duplex，桥接任务做双向 copy。
async fn bridge_duplex(
    stream: wsieve_mux::stripe_runtime::StripeStreamHandle,
) -> tokio::io::DuplexStream {
    let (local, mut remote_end) = tokio::io::duplex(64 * 1024);
    let mut stream = stream;
    tokio::spawn(async move {
        // remote_end <-> stripe conn；local 返回给 socks5 层
        let _ = tokio::io::copy_bidirectional(&mut remote_end, &mut stream).await;
    });
    local
}

/// 主循环：建会话 → 供 SOCKS5 → 死了重来（带退避）。
pub async fn run(app: tauri::AppHandle, cfg: ProxyCfg) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&cfg.socks_listen).await?;
    tracing::info!("SOCKS5 listening on {}", cfg.socks_listen);

    let dialer_slot: Arc<tokio::sync::RwLock<Option<Arc<StripeDialer>>>> =
        Arc::new(tokio::sync::RwLock::new(None));
    let session_dead = Arc::new(Notify::new());

    // SOCKS5 服务任务
    let serve_slot = dialer_slot.clone();
    tokio::spawn(async move {
        let _ = wsieve_socks5::serve(listener, socks_handler(serve_slot)).await;
    });

    let mut backoff = Duration::from_millis(100);
    loop {
        // 1. 等 WebView / emitter 就绪：本代 core 注册后等首个心跳
        let transport = WebViewTransport::new(eval_fn(app.clone()));
        let core = Arc::new(TransportCore::new_beat_pending());
        transport.set_core(core.clone());
        app.manage(CurrentCore(core.clone()));

        emit_status(&app, "waiting for webview heartbeat");
        if !wait_first_heartbeat(&core, Duration::from_secs(60)).await {
            emit_status(&app, "webview heartbeat timeout");
        }

        // 2. 死亡监视：心跳超时 → 标死 → 会话重来（spec §9.1）
        {
            let core = core.clone();
            let session_dead = session_dead.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(crate::bridge::HEARTBEAT_STALE).await;
                    if core.heartbeat_stale() {
                        core.mark_dead("heartbeat stale").await;
                        session_dead.notify_one();
                        return;
                    }
                    if core.is_dead() {
                        session_dead.notify_one();
                        return;
                    }
                }
            });
        }

        // 3. 握手 + mux
        emit_status(&app, "handshaking");
        // 会话数由 shard 编排决定（条带降级时就是 1），不再直接读
        // StripeCfg——否则会出现「想要 4 个会话但只有 1 个端口」的错配。
        let extra_sessions = cfg.session_bases.len().saturating_sub(1);
        // 会话组 id：本代会话（主 + 全部额外）共用一个值，服务端据此把它们
        // 归为一组并跨会话铺下行 lane。每代重新生成——上一代会话已拆除，
        // 复用旧 id 只会让服务端组表里混进死会话。
        let group_id = random_group_id();
        let attempt: anyhow::Result<Arc<StripeDialer>> = async {
            let (conn, neg) = XhttpConn::connect(
                transport.clone(),
                &UpstreamCfg {
                    server_pub: cfg.server_pub,
                    client_priv: cfg.client_priv,
                    mux_prefs: cfg.mux_prefs.clone(),
                    group_id,
                },
            )
            .await?;
            if neg.fallback {
                emit_status(&app, "WARN: mux fallback (server did not honor prefs)");
            }
            let io: wsieve_mux::MuxStream = Box::new(conn);
            let mux: Arc<dyn Mux> = Arc::from(mux_factory(neg.mux_id, io).await?);
            let dialer = StripeDialer::new(mux, StripeCfg::with_env());
            // 多 TCP 条带（aria2 效应）：急切建额外会话，lane 跨会话轮转。
            // 任一会话死 → 其上 lane 断；只要还有会话活着 conn 继续。
            for i in 0..extra_sessions {
                // 每个额外会话一个独立 origin（同域名不同端口）——共用一个
                // transport 就等于共用一个 origin，h2 会把它们复用回同一条
                // TCP，多会话就白做了。各 transport 共享同一个 core。
                let base = cfg.session_bases[i + 1].clone().unwrap_or_default();
                let t2 = WebViewTransport::with_base(eval_fn(app.clone()), base);
                t2.set_core(core.clone());
                let (conn2, neg2) = XhttpConn::connect(
                    t2,
                    &UpstreamCfg {
                        server_pub: cfg.server_pub,
                        client_priv: cfg.client_priv,
                        mux_prefs: vec![neg.mux_id],
                        group_id,
                    },
                )
                .await?;
                let io2: wsieve_mux::MuxStream = Box::new(conn2);
                let mux2: Arc<dyn Mux> = Arc::from(mux_factory(neg2.mux_id, io2).await?);
                dialer.attach_session(mux2);
            }
            if extra_sessions > 0 {
                tracing::info!(
                    "multi-session striping: {} sessions",
                    dialer.session_count()
                );
            }
            Ok(dialer)
        }
        .await;

        match attempt {
            Ok(dialer) => {
                backoff = Duration::from_millis(100);
                emit_status(&app, "connected");
                *dialer_slot.write().await = Some(dialer);
                // 会话死亡信号：XhttpConn 内部断开时其流全部失败，但主循环
                // 通过心跳/transport 死亡感知。等通知。
                session_dead.notified().await;
                emit_status(&app, "session dead, reloading");
                *dialer_slot.write().await = None;
            }
            Err(e) => {
                tracing::warn!("session attempt failed: {e:#}");
            }
        }

        // 4. 拆干净 + 重载页面（emitter 随 initialization_script 重注入）
        core.mark_dead("session teardown").await;
        if let Some(w) = app.get_webview_window("main") {
            let _ = w.eval("window.location.reload()");
        }
        emit_status(&app, &format!("retrying in {:?}", backoff));
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// 首个心跳：`TransportCore::new_with_beat_pending` 把 last_heartbeat 初始化
/// 为「远古」，因此 stale()==false ⇔ 已收到过真实心跳。
async fn wait_first_heartbeat(core: &Arc<TransportCore>, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if !core.heartbeat_stale() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    !core.heartbeat_stale()
}

