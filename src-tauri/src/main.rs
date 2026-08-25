//! websieve Tauri shell。
//!
//! 同源决策（spec §6.7）：承载 WebView 直接导航到服务端真实首页
//! （`WSIEVE_SERVER_URL`），所有代理 fetch 天然 same-origin。emitter 无法
//! 加进服务器的页面，唯一注入点是 `initialization_script`（页面脚本前
//! 执行，Tauri 2 `WebviewWindowBuilder::initialization_script`）——因此
//! 建窗在 Rust 代码里做（tauri.conf.json 的窗口条目仅是兜底配置）。
//!
//! IPC 二进制纪律（spec §3.2）：上行 POST body 与下行 chunk 均以顶层
//! Uint8Array invoke（raw 快路径），帧头 16 字节（BE）携带 requestId 等
//! 标量，绝不把二进制嵌进 JSON object。
//!
//! **启动序列**（阶段 2 Task 13）：
//!
//! ```text
//! 读配置 → 校验 → 建 RuleSet / GeoDb
//!   → CustodyGuard::acquire(hosts)        ← Part B
//!   → 端口分段 → 起转发器（预建 TCP）      ← Task 12
//!   → 按 CarrierPlan 建 WebView            ← Task 10
//!   → 并行拉起全部 enabled 出站            ← 优化④
//!   → 起混合端口入口                        ← Part C
//!   → （若配置开启）CustodyGuard::acquire(sysproxy)
//! ```
//!
//! `RunEvent::Exit` 里按**相反顺序** drop 全部 guard：先摘系统代理（不摘
//! 用户整机断网），再摘 hosts（不摘域名一直指向已停的转发器）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bridge;
mod custody;
mod emitter_src;
mod outbound;
mod router;
mod shard;
mod shard_setup;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tauri::Manager;

mod bootstrap;

/// IPC 命令可见的传输 core：emitter 的所有 invoke 都打到这里。
///
/// 多出站共享同一份（core 对应一个 WebView，不对应一个会话），
/// request_id 由它统一分配。
pub struct CurrentCore(pub Arc<bridge::TransportCore>);

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = match bootstrap::load_cfg() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e:#}");
            std::process::exit(2);
        }
    };

    // 混合端口入口的绑定地址。**这是一个安全决策，不是一个便利选项**：
    // 入口链路上目前没有任何认证（SOCKS5 只实现方法 0x00，HTTP 侧剔除
    // Proxy-Authorization 而不校验），绑到 0.0.0.0 就等于开一个经本机隧道
    // 出网的开放中继。默认关，开启时下面会打一条醒目告警。
    let inbound_bind = match bootstrap::inbound_bind_addr(&cfg.socks_listen, cfg.allow_lan) {
        Ok(a) => a,
        Err(e) => {
            // 猜一个地址等于把入口开在用户没预期的地方，宁可不启动。
            eprintln!("入口监听地址无效: {e:#}");
            std::process::exit(2);
        }
    };
    if cfg.allow_lan {
        tracing::warn!(
            "allow-lan 已开启：入口绑定 {inbound_bind}，同网段任何设备都能经本机隧道出网。\
             该路径**没有认证**（SOCKS5 仅方法 0x00；HTTP 不校验 Proxy-Authorization），\
             等同于一个开放中继。仅在完全可信的网段这样用。"
        );
    }

    // 本地条带编排：hosts 劫持 + 多端口转发（见 shard_setup）。必须在建
    // WebView 之前完成——承载 WebView 要加载的正是转发器的端口。任何一步
    // 失败都降级为单会话，不影响可用性。
    //
    // 端口分段（Task 13）：目前 env 配置只描述一个出站，因此它独占从
    // shard_base_port 起的一整段。多出站配置接入后（阶段 4 的配置文件），
    // 这里改成对 `outbound::plan_ports` 的调用即可 —— 分段逻辑与它的
    // 溢出/重叠校验已经就位并有测试。
    let extra_sessions = wsieve_mux::stripe_runtime::StripeCfg::with_env().extra_sessions;
    let outbound_name = std::env::var("WSIEVE_OUTBOUND_NAME")
        .unwrap_or_else(|_| "默认节点".to_string());
    let ports = match outbound::try_plan_ports(
        cfg.shard_base_port,
        &[(outbound_name.as_str(), extra_sessions)],
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("端口分段失败: {e:#}");
            std::process::exit(2);
        }
    };
    let base_port = ports[&outbound_name][0];

    let plan = tauri::async_runtime::block_on(shard_setup::plan(
        &cfg.server_url,
        base_port,
        extra_sessions,
        custody::hosts::system_path(),
    ));
    let page_url = plan.page_url.clone();
    let show_window = cfg.show_window;
    let session_bases = plan.session_bases.clone();

    // 规则集：env 配置只有一个出站，因此规则就是「全部走它」。
    // 这条 MATCH 是**显式**的 —— websieve 不设隐式默认（`RuleSet::build`
    // 会拒绝没有 MATCH 的规则表），隐式直连等于静默裸奔。
    let known: std::collections::HashSet<String> =
        std::iter::once(outbound_name.clone()).collect();
    let rules = match wsieve_route::RuleSet::build(
        &[format!("MATCH,{outbound_name}")],
        wsieve_route::Mode::Rule,
        "",
        &known,
    ) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("规则表无效: {e:#}");
            std::process::exit(2);
        }
    };
    // GEO 数据缺失不阻断启动（spec §12）：涉 GEO 的规则跳过并告警。
    // 目前的规则表里没有 GEO 规则，因此这两个路径一个字节都不会被读。
    let geo = Arc::new(wsieve_geo::GeoDb::new(
        geo_path("WSIEVE_GEOIP", "geoip.dat"),
        geo_path("WSIEVE_GEOSITE", "geosite.dat"),
    ));
    for w in rules.check_geo(&geo) {
        tracing::warn!("规则告警：{w}");
    }

    // guard 持有 hosts 清理职责；进程正常退出时由 RunEvent::Exit 显式 drop，
    // 崩溃路径由下次启动的 clear_managed 兜底。
    let shard_guard = std::sync::Mutex::new(plan.guard);
    // 系统代理托管（spec §8.2 / §10）。同 hosts 一样：持有即生效、drop 即恢复，
    // 崩溃残留由启动时的 clear_stale 兜底。
    let sysproxy_guard = std::sync::Mutex::new(setup_system_proxy(&cfg));

    // 承载计划（Task 10）：单出站时它就是宿主，用相对路径完全同源。
    let carrier = match outbound::carrier::CarrierPlan::build(
        match outbound::carrier::CarrierMode::parse(
            &std::env::var("WSIEVE_CARRIER").unwrap_or_else(|_| "shared".into()),
        ) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("carrier 配置无效: {e:#}");
                std::process::exit(2);
            }
        },
        "",
        &[(outbound_name.as_str(), page_url.as_str())],
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("承载计划无效: {e:#}");
            std::process::exit(2);
        }
    };
    tracing::info!(
        "承载模式 {:?}，宿主出站「{}」",
        carrier.mode(),
        carrier.host_name()
    );

    // 主会话的基址由**承载计划**决定，不是由条带决定：条带只管额外会话各自
    // 用哪个本地端口，而「这个出站相对承载页面是同源还是跨域名」只有承载
    // 计划知道。两层 Option 的外层是「不认识这个出站」——那必须当错误处理，
    // 悄悄给个默认基址就等于把流量发去了另一台服务器（§6.4）。
    let mut session_bases = session_bases;
    match carrier.base_for(&outbound_name) {
        Some(base) => session_bases[0] = base,
        None => {
            eprintln!("承载计划里没有出站「{outbound_name}」");
            std::process::exit(2);
        }
    }
    // 承载窗口标签：eval 要把 JS 送进**这一个**窗口。
    let carrier_window = match carrier.window_label(&outbound_name) {
        Some(w) => w,
        None => {
            eprintln!("承载计划里没有出站「{outbound_name}」的窗口");
            std::process::exit(2);
        }
    };

    let outbound_cfg = outbound::instance::OutboundCfg {
        name: outbound_name.clone(),
        server_pub: cfg.server_pub,
        client_priv: cfg.client_priv,
        mux_prefs: cfg.mux_prefs.clone(),
        session_bases,
    };

    tauri::Builder::default()
        .setup(move |app| {
            // 承载 WebView：加载服务端真实首页 + initialization_script 注入
            // emitter（同源关键，见模块注释）。
            for (label, url) in carrier.windows() {
                tauri::webview::WebviewWindowBuilder::new(
                    app,
                    label,
                    tauri::WebviewUrl::External(url.parse()?),
                )
                .title("websieve")
                .inner_size(480.0, 320.0)
                // 传输载体，不是用户界面（spec §6.7）。默认隐藏；
                // WSIEVE_SHOW_WINDOW=1 可打开排障。
                .visible(show_window)
                // spec §3.4：后台节流压制——macOS WKWebView 后台/隐藏时挂起
                // JS 定时器与 fetch（Task 18 E2E 实测心跳/流分块会停摆）。
                .background_throttling(
                    tauri_utils::config::BackgroundThrottlingPolicy::Disabled,
                )
                .initialization_script(bootstrap::loader_js())
                .build()?;
            }

            let handle = app.handle().clone();
            let bind = inbound_bind.clone();
            let ob = outbound_cfg.clone();
            let rules = rules.clone();
            let geo = geo.clone();
            let win = carrier_window.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = run_stack(handle, bind, win, ob, rules, geo).await {
                    tracing::error!("入口/出站栈退出: {e:#}");
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            wsieve_heartbeat,
            wsieve_raw_post,
            wsieve_raw_stream,
        ])
        .build(tauri::generate_context!())
        .expect("tauri build")
        .run(move |_app, event| {
            // 退出时**按获取的相反顺序**摘除全部外部系统状态托管（spec §10）：
            // 系统代理不关的话用户整机断网；hosts 不摘的话域名会一直指向
            // 已经不在跑的转发器。两者的崩溃路径都由下次启动的 clear_stale 兜底。
            if let tauri::RunEvent::Exit = event {
                sysproxy_guard.lock().unwrap().take();
                shard_guard.lock().unwrap().take();
            }
        });
}

/// GEO 数据文件路径：环境变量优先，否则取当前目录下的同名文件。
fn geo_path(env: &str, name: &str) -> std::path::PathBuf {
    std::env::var(env)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(name))
}

/// 出站管理器 + 路由分派 + 混合端口入口的总装与看护。
///
/// 一代「承载」= 一个 `TransportCore`。core 死（页面崩了、心跳停摆）时
/// 重载页面并整代重来 —— 这是两级生命周期里的**上**一级，波及全部出站，
/// 因此只能在这里统一决策。单个出站的会话死亡由它自己的循环处理，
/// 不碰 core、不 reload（§9.4 优化①）。
///
/// 入口只起一次并跨代复用：它持有的是 `Router`，而 `Router` 查的是出站
/// 实例的 `dialer()`，实例本身跨代存活。换代期间 `dialer()` 为 `None`，
/// 于是连接被**拒绝**而不是被静默改道（§6.4）。
async fn run_stack(
    app: tauri::AppHandle,
    bind: String,
    carrier_window: String,
    ob_cfg: outbound::instance::OutboundCfg,
    rules: Arc<wsieve_route::RuleSet>,
    geo: Arc<wsieve_geo::GeoDb>,
) -> anyhow::Result<()> {
    let inst = outbound::instance::OutboundInstance::new(ob_cfg);
    let mut table = BTreeMap::new();
    // 键取自实例自己的 `name()`，不取外面另传的一份：两者一旦不一致，
    // 规则里写的名字就查不到实例，表现为「配置明明写了却说出站不存在」。
    table.insert(inst.name().to_string(), inst);

    // 路由分派器与出站管理器**共用同一批实例**：管理器跑它们的会话循环、
    // 往里装 dialer，分派器读 dialer 决定这条连接能不能走。各持一份副本
    // 的话，分派器会永远看到一个空的 dialer 格 —— 表现为「明明连上了却
    // 一直被拒」。
    //
    // 阶段 2 尚未接入 DNS 解析器，`without_resolver` 会对着规则表里的
    // IP 类规则条数如实告警 —— 静默的覆盖面缺失比报错更危险。
    let router = Arc::new(router::Router::without_resolver(rules, geo, table.clone()));

    // 混合端口入口（Part C）：同一端口上嗅探 SOCKS5 与 HTTP。
    // bind 决策见 main() 里的注释与告警。
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| anyhow::anyhow!("入口监听 {bind} 失败: {e}"))?;
    tracing::info!("混合端口入口就绪：{bind}（SOCKS5 与 HTTP 同口）");
    {
        let router = router.clone();
        let dispatch: wsieve_inbound::Dispatch = Arc::new(move |target| {
            let router = router.clone();
            Box::pin(async move { router.dispatch(target).await })
        });
        tokio::spawn(async move {
            if let Err(e) = wsieve_inbound::serve(listener, dispatch).await {
                // accept 循环退出 = 入口彻底失守，绝不静默。
                tracing::error!("混合端口入口退出：{e}");
            }
        });
    }

    // 承载代循环：core 死一次就换一代。
    loop {
        let core = Arc::new(bridge::TransportCore::new_beat_pending());
        app.manage(CurrentCore(core.clone()));

        let env = outbound::instance::SessionEnv {
            eval: eval_fn(app.clone(), carrier_window.clone()),
            on_status: status_fn(app.clone()),
        };
        let manager =
            outbound::OutboundManager::new(table.clone(), core.clone(), env);

        // 等 emitter 就绪再拉起出站：没有 emitter 就没有 fetch，握手必然
        // 失败并白白吃掉一轮退避。
        if !wait_first_heartbeat(&core, Duration::from_secs(60)).await {
            tracing::warn!("等待承载页面心跳超时——仍然尝试建会话");
        }

        // **优化④**：并行拉起全部启用的出站，不排队。
        manager.start_all().await;

        // 看着 core：心跳停摆即判死，本代结束。
        watch_core(&core).await;

        tracing::warn!(
            "承载页面已失效，重载并重建全部出站（当时各出站状态：{:?}）",
            manager.statuses()
        );
        manager.stop_all().await;
        // core 死才 reload —— 这是两级生命周期里唯一该 reload 的那一级。
        for (label, _) in app.webview_windows() {
            if let Some(w) = app.get_webview_window(&label) {
                let _ = w.eval("window.location.reload()");
            }
        }
        // 给页面一点时间重新加载并把 emitter 注回去。
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// 盯住 core：心跳超时就标死。返回即代表本代承载结束。
async fn watch_core(core: &Arc<bridge::TransportCore>) {
    loop {
        tokio::time::sleep(bridge::HEARTBEAT_STALE).await;
        if core.is_dead() {
            return;
        }
        if core.heartbeat_stale() {
            core.mark_dead("心跳停摆").await;
            return;
        }
    }
}

/// 首个心跳：`TransportCore::new_beat_pending` 把 last_heartbeat 初始化为
/// 「远古」，因此 `stale() == false` ⇔ 已收到过真实心跳。
async fn wait_first_heartbeat(core: &Arc<bridge::TransportCore>, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if !core.heartbeat_stale() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    !core.heartbeat_stale()
}

/// 把 JS 送进**指定的**承载 WebView。
///
/// 窗口标签来自 `CarrierPlan::window_label`，不是写死的 "main"：
/// `isolated` 模式下每出站一个窗口，写死就会把全部出站的 JS 都塞进第一个
/// 窗口 —— 那等于所有出站共用一个 origin，故障隔离与多 TCP 双双落空。
fn eval_fn(app: tauri::AppHandle, window: String) -> outbound::instance::EvalFn {
    Arc::new(move |js: String| {
        match app.get_webview_window(&window) {
            Some(w) => {
                if let Err(e) = w.eval(&js) {
                    tracing::warn!("向承载窗口 {window} 注入 JS 失败: {e}");
                }
            }
            // 窗口不在 = 承载没了，这条 JS 无处可去。绝不静默：
            // 表现出来是「握手一直超时」，不说清原因根本无从定位。
            None => tracing::warn!("承载窗口 {window} 不存在，JS 无法送达"),
        }
    })
}

/// 状态变化推给 UI（spec §9.3）。日志与事件都发：日志给排障，事件给界面。
fn status_fn(app: tauri::AppHandle) -> outbound::instance::StatusFn {
    Arc::new(move |name: &str, s: &outbound::instance::Status| {
        tracing::info!("出站 {name}: {s:?}");
        let _ = tauri::Emitter::emit(
            &app,
            "wsieve-outbound-status",
            (name.to_string(), format!("{s:?}")),
        );
    })
}

/// 建立系统代理托管。
///
/// **无论 `system-proxy` 开没开，都要先清一次残留** —— 用户上次崩溃后
/// 把开关关掉，残留就再也没人清了。这与 `shard_setup` 里 hosts 清残留
/// 放在所有早退分支之前是同一条纪律（spec §10）。
///
/// 任何一步失败都只告警不阻断启动：代理本身还能用，用户手工设一次即可，
/// 而拒绝启动等于整个程序不可用。
fn setup_system_proxy(
    cfg: &bootstrap::AppConfig,
) -> Option<custody::CustodyGuard<custody::sysproxy::SysProxyCustody>> {
    use custody::sysproxy::SysProxyCustody;
    use custody::{CustodyGuard, ManagedSystemState};

    let services = match SysProxyCustody::enumerate_services() {
        Ok(s) => s,
        Err(e) => {
            // 非 macOS 平台走的就是这条（见 sysproxy 的 ponytail 标注）。
            if cfg.system_proxy {
                tracing::warn!("系统代理未启用：枚举网络服务失败（{e:#}）——请手工设置");
            } else {
                tracing::debug!("跳过系统代理托管：{e:#}");
            }
            return None;
        }
    };

    // SOCKS 监听口就是要写进系统设置的地址。
    let (host, port) = match parse_listen(&cfg.socks_listen) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("系统代理未启用：监听地址 {} 解析失败（{e}）", cfg.socks_listen);
            return None;
        }
    };

    let custody = match SysProxyCustody::new(host, port, services) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("系统代理未启用：{e:#}");
            return None;
        }
    };

    if !cfg.system_proxy {
        // 本次不开，但残留照清。
        if let Err(e) = custody.clear_stale() {
            tracing::warn!("清理系统代理残留失败（{e:#}）——若上次异常退出，设置可能仍在");
        }
        return None;
    }

    match CustodyGuard::acquire(custody) {
        Ok(g) => {
            tracing::info!("系统代理已指向 {}", cfg.socks_listen);
            Some(g)
        }
        Err(e) => {
            tracing::warn!("系统代理未启用（{e:#}）——请手工设置");
            None
        }
    }
}

/// 从 `host:port` 取出两段。IPv6 字面量形如 `[::1]:1080`。
fn parse_listen(s: &str) -> anyhow::Result<(String, u16)> {
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let (h, tail) = rest
            .split_once(']')
            .ok_or_else(|| anyhow::anyhow!("IPv6 字面量不完整"))?;
        (
            h.to_string(),
            tail.strip_prefix(':')
                .ok_or_else(|| anyhow::anyhow!("缺少端口"))?,
        )
    } else {
        let (h, p) = s
            .rsplit_once(':')
            .ok_or_else(|| anyhow::anyhow!("缺少端口"))?;
        (h.to_string(), p)
    };
    if host.is_empty() {
        anyhow::bail!("缺少主机");
    }
    Ok((host, port.parse()?))
}

#[tauri::command]
async fn wsieve_heartbeat(state: tauri::State<'_, CurrentCore>) -> Result<(), String> {
    state.0.heartbeat();
    Ok(())
}

/// 上行 POST 结果与下行 chunk 的统一二进制入口。
/// 帧格式（16B 头，全大端，与 emitter.js 的 frame() 逐字段对齐）：
///   [0..4]  magic "WSIE"
///   [4]     kind = 1 post_ok / 2 post_err / 3 chunk / 4 stream_end / 5 stream_err
///   [5..9]  u32 request_id
///   [9..11] u16 status（仅 kind=1）
///   [11..16] 保留（填充至 16B）
/// 之后为原始字节（kind=1 的 body / kind=3 的 chunk，其余无 payload）。
#[tauri::command]
async fn wsieve_raw_post(state: tauri::State<'_, CurrentCore>, request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let body = match request.body() {
        tauri::ipc::InvokeBody::Raw(b) => b.clone(),
        _ => return Err("raw body required".into()),
    };
    handle_frame(&state.0, &body).await
}

#[tauri::command]
async fn wsieve_raw_stream(state: tauri::State<'_, CurrentCore>, request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let body = match request.body() {
        tauri::ipc::InvokeBody::Raw(b) => b.clone(),
        _ => return Err("raw body required".into()),
    };
    handle_frame(&state.0, &body).await
}

async fn handle_frame(core: &Arc<bridge::TransportCore>, body: &[u8]) -> Result<(), String> {
    const HEADER: usize = 16;
    if body.len() < HEADER {
        return Err("short frame".into());
    }
    let magic = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    if magic != 0x5753_4945 {
        return Err("bad magic".into());
    }
    let kind = body[4];
    let request_id = u32::from_be_bytes([body[5], body[6], body[7], body[8]]) as u64;
    let status = u16::from_be_bytes([body[9], body[10]]);
    let payload = &body[HEADER..];
    match kind {
        1 => {
            core.complete_post(
                request_id,
                Ok(wsieve_transport::PostReply {
                    status,
                    body: bytes::Bytes::copy_from_slice(payload),
                }),
            )
            .await;
        }
        2 => {
            core.complete_post(request_id, Err(anyhow::anyhow!("post fetch failed")))
                .await;
        }
        3 => {
            core.push_chunk(request_id, bytes::Bytes::copy_from_slice(payload))
                .await;
        }
        4 => core.complete_stream(request_id, None).await,
        5 => {
            core.complete_stream(request_id, Some(anyhow::anyhow!("stream fetch failed")))
                .await;
        }
        other => return Err(format!("bad kind {other}")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_addr_splits_host_and_port() {
        assert_eq!(
            parse_listen("127.0.0.1:1080").unwrap(),
            ("127.0.0.1".to_string(), 1080)
        );
        // IPv6 字面量：rsplit_once(':') 会切在地址中间，必须走方括号分支
        assert_eq!(
            parse_listen("[::1]:7890").unwrap(),
            ("::1".to_string(), 7890)
        );
        assert_eq!(
            parse_listen("[::]:7890").unwrap(),
            ("::".to_string(), 7890)
        );
    }

    #[test]
    fn malformed_listen_addr_is_an_error_not_a_guess() {
        // 猜错了就把系统代理指向一个不存在的地址，用户整机断网。
        assert!(parse_listen("127.0.0.1").is_err(), "缺端口");
        assert!(parse_listen(":1080").is_err(), "缺主机");
        assert!(parse_listen("127.0.0.1:not-a-port").is_err());
        assert!(parse_listen("[::1:7890").is_err(), "方括号不闭合");
        assert!(parse_listen("[::1]7890").is_err(), "方括号后缺冒号");
    }
}
