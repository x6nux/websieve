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
mod commands;
mod control;
mod custody;
mod emitter_src;
mod events;
mod outbound;
mod router;
mod runtime_state;
mod shard;
mod shard_setup;
mod stats;
mod tun;
mod tray;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tauri::Manager;

mod bootstrap;

/// IPC 命令可见的传输 core：emitter 的所有 invoke 都打到这里。
///
/// 多出站共享同一份（core 对应一个 WebView，不对应一个会话），
/// request_id 由它统一分配。
///
/// **可替换，且只 `manage()` 一次。** 承载代死掉换新一代时要把这里指向新
/// core——而 Tauri 的 `manage()` 对已注册类型是**空操作**
/// （`StateManager::set` 里 `if !already_set` 才 insert），靠反复 `manage`
/// 更新不报错、只是静默无效。这里曾经正是如此：第二代起页面新 emitter 的
/// 心跳全打到第一代那个已死的 core 上，承载页面死过一次之后应用再也不恢复。
/// 完整失效链与现象见 `a_generation_swap_routes_heartbeats_to_the_new_core`。
pub struct CurrentCore(std::sync::RwLock<Arc<bridge::TransportCore>>);

impl CurrentCore {
    pub fn new(core: Arc<bridge::TransportCore>) -> Self {
        Self(std::sync::RwLock::new(core))
    }

    /// 当前这一代的 core。**每个 IPC 帧都会调它**，所以持锁区间只覆盖一次
    /// `Arc` 克隆（引用计数 +1），锁里绝不做别的事。
    ///
    /// 锁中毒时取回内层值继续用，而不是跟着 panic：这里护的只是一个 `Arc`
    /// 指针，写者持锁期间只做一次赋值，不存在「改了一半」的中间态。让一次
    /// 无关的 panic 把整条传输通路打死，比中毒本身危险得多（与
    /// `runtime_state::RuntimeHandle` 同一条理由）。
    pub fn current(&self) -> Arc<bridge::TransportCore> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 换代：指向新一代的 core。
    pub fn set(&self, core: Arc<bridge::TransportCore>) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = core;
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // ── 读 config.yaml（设计文档「运行时接入 config.yaml」§2）────────────
    //
    // 出站、规则、模式、承载方式、入口端口、TUN/系统代理开关全部来自这份
    // 文件——`WSIEVE_SERVER_PUB` / `WSIEVE_CLIENT_PRIV` / `WSIEVE_OUTBOUND_NAME`
    // 这条「一个硬编码出站」的 env 引导路径到此为止；其余 env 变量降级成
    // **排障旋钮**，显式设置时才压过配置文件（见 bootstrap::load_cfg）。
    //
    // 路径要与 `commands::config::config_path` 算出的**完全一致**，否则
    // 控制窗口编辑的和运行时读到的是两份不同的文件，而界面上看不出来。
    // 这一刻还没有 `AppHandle`，用不了 `app.path().app_config_dir()`，因此
    // 复刻它的实现：`dirs::config_dir().join(identifier)`。identifier 取自
    // `generate_context!()` 产出的那一份，不另抄一个字面量。
    let tauri_ctx = tauri::generate_context!();
    let config_file = match config_file_path(&tauri_ctx.config().identifier) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("取配置文件路径失败: {e:#}");
            std::process::exit(2);
        }
    };
    // 首次启动时落一份默认配置（零出站、零规则），与 `config_get` 走同一个
    // 函数——两处各写一份默认值迟早会分叉。
    if let Err(e) = commands::config::ensure_config_exists(&config_file) {
        eprintln!("创建默认配置 {} 失败: {e:?}", config_file.display());
        std::process::exit(2);
    }
    let config = match load_config(&config_file) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("配置无效（{}）: {e:#}", config_file.display());
            std::process::exit(2);
        }
    };
    tracing::info!(
        "已加载 {}：{} 个出站，{} 条规则，模式 {}",
        config_file.display(),
        config.proxies.len(),
        config.rules.len(),
        config.mode
    );

    let cfg = match bootstrap::load_cfg(&config) {
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
    // 端口分段：每个出站独占 `extra_sessions + 1` 个端口，从 shard_base_port
    // 起依次排开；溢出与重名由 `try_plan_ports` 报错（两个出站抢同一个端口
    // 会让 A 的流量偶尔跑到 B 的服务器上）。
    let sessions: Vec<(&str, usize)> = config
        .proxies
        .iter()
        .map(|p| (p.name.as_str(), p.extra_sessions))
        .collect();
    let ports = match outbound::try_plan_ports(cfg.shard_base_port, &sessions) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("端口分段失败: {e:#}");
            std::process::exit(2);
        }
    };

    // ── TUN 的前两步（§8.3.2 的第 0 与第 2 步）────────────────────────────
    //
    // 顺序纪律钉死的六步在这里与 `shard_setup::plan_many` 交错：
    //   0) 查 fake-ip 段归属      ← 这里（只读、无需 root，掉头最干净）
    //   1) 解析真实 IP            ← plan_many 内部
    //   2) 写 bypass 路由         ← plan_many 内部，经下面这个钩子
    //   3) 起转发器               ← plan_many 内部
    //   4) 写 hosts               ← plan_many 内部
    //   5) 最后拉起 TUN           ← 建 WebView 之后，run_stack 里
    //
    // 第 2 步必须夹在 1 与 3 之间，因此只能做成钩子递进去 —— 那正是
    // `UpstreamHook` 存在的全部理由。
    let server_urls: Vec<String> = config.proxies.iter().map(|p| p.url.clone()).collect();
    let tun_setup = tun_prepare(&cfg, &server_urls);

    // **每个出站各自一个钩子**：它们的服务器 IP 各不相同，而少写任何一条
    // bypass 路由，那个出站的转发器就会被 TUN 捕获、判「走代理」、绕回本机。
    // 钩子内部按出站名做引用计数（`BypassSet::insert`），共享同一台服务器的
    // 两个出站不会互相覆盖。
    let targets: Vec<shard_setup::ShardTarget> = config
        .proxies
        .iter()
        .map(|p| shard_setup::ShardTarget {
            server_url: p.url.clone(),
            base_port: ports[&p.name][0],
            extra_sessions: p.extra_sessions,
            on_upstream: tun_setup.as_ref().map(|t| {
                Arc::new(tun::bypass_hook(
                    t.bypass.clone(),
                    t.routes.clone(),
                    p.name.clone(),
                )) as shard_setup::UpstreamHook
            }),
        })
        .collect();

    let shard = tauri::async_runtime::block_on(shard_setup::plan_many(
        targets,
        custody::hosts::system_path(),
    ));
    // 没有 bypass 就绝不拉 TUN（§8.3.1）。这不是保守，是确定性：
    // 转发器的第一条出网连接会被 TUN 捕获、判「走代理」、绕回本机。
    //
    // 多出站下这条门禁是**全体通过才放行**：环路只需要一条腿就能成立，而它
    // 是静默的。零出站同样不拉——此时规则表兜底成 `MATCH,REJECT`，接管默认
    // 路由只会把用户整机流量黑洞掉。
    let tun_ok = !shard.entries.is_empty() && shard.entries.iter().all(|e| e.tun_may_start());
    let mut tun_setup = match (tun_setup, tun_ok) {
        (Some(t), true) => Some(t),
        (Some(_), false) => {
            let why = shard
                .entries
                .iter()
                .find_map(|e| e.bypass_error.clone())
                .unwrap_or_else(|| {
                    if shard.entries.is_empty() {
                        "配置里还没有任何出站".to_string()
                    } else {
                        "有出站的服务器地址未解析成功，没有 bypass 就拉起 TUN 必成环路"
                            .to_string()
                    }
                });
            tracing::error!("TUN 未启用：{why}。混合端口入口不受影响，代理照常可用");
            None
        }
        (None, _) => None,
    };
    // 退出时要撤的 TUN 路由。与 hosts / 系统代理并列的第三项托管（§10）。
    // 单独留一个句柄而不是靠 `TunSetup` —— 后者被 move 进 setup 闭包里了，
    // 而撤销发生在 `RunEvent::Exit`，两处的生命周期不重叠。
    let tun_routes = std::sync::Mutex::new(tun_setup.as_ref().map(|t| t.routes.clone()));
    let show_window = cfg.show_window;

    // ── 从配置构建启动计划：出站配置 + 承载计划 + 规则表材料 ────────────
    //
    // 条带的结果是它的**输入**（承载页面 URL 与各会话基址都取决于 hosts
    // 劫持成没成功），因此必须排在 `plan_many` 之后。
    let plan = match runtime_state::build_startup_plan(&config, &shard.entries) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("启动计划构建失败: {e:#}");
            std::process::exit(2);
        }
    };

    // 规则集。**规则表有问题时不退出，退回 `MATCH,REJECT`**：规则引用了一个
    // 不存在的出站（UI 的保存路径只校验规则语法，不校验引用），或者用户把
    // 收尾的 MATCH 删了——这些都是能从控制窗口改回来的错误，而直接
    // `exit(2)` 会让用户连打开控制窗口的机会都没有，配置就此锁死。
    // 兜底成拒绝而不是直连：与 §6.4 一致，宁可连不上也不静默裸奔。
    let known = config.outbound_names();
    let rules = match wsieve_route::RuleSet::build(
        &plan.rule_lines,
        plan.mode,
        &plan.global_outbound,
        &known,
    ) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            tracing::error!(
                "规则表无效（{e}）——本次启动改用隐式 MATCH,REJECT 兜底：\
                 全部连接都会被拒绝，直到你在控制窗口的「规则」页把它改好。\
                 绝不静默直连（§6.4）"
            );
            let fallback = vec!["MATCH,REJECT".to_string()];
            match wsieve_route::RuleSet::build(
                &fallback,
                wsieve_route::Mode::Rule,
                "",
                &Default::default(),
            ) {
                Ok(r) => Arc::new(r),
                Err(e) => {
                    // 连一条 MATCH,REJECT 都建不起来，说明规则引擎本身坏了。
                    eprintln!("兜底规则表也无法构建: {e:#}");
                    std::process::exit(2);
                }
            }
        }
    };
    // GEO 数据缺失不阻断启动（spec §12）：涉 GEO 的规则跳过并告警。
    let geo = Arc::new(wsieve_geo::GeoDb::new(
        geo_path("WSIEVE_GEOIP", "geoip.dat"),
        geo_path("WSIEVE_GEOSITE", "geosite.dat"),
    ));
    for w in rules.check_geo(&geo) {
        tracing::warn!("规则告警：{w}");
    }

    // guard 持有 hosts 清理职责；进程正常退出时由 RunEvent::Exit 显式 drop，
    // 崩溃路径由下次启动的 clear_managed 兜底。
    let shard_guard = std::sync::Mutex::new(shard.guard);
    // 系统代理托管（spec §8.2 / §10）。同 hosts 一样：持有即生效、drop 即恢复，
    // 崩溃残留由启动时的 clear_stale 兜底。
    let sysproxy_guard = std::sync::Mutex::new(setup_system_proxy(&cfg));

    // 承载窗口标签表：`eval` 要按**出站名**把 JS 送进它自己的那个窗口。
    // `isolated` 下每出站一个窗口，写死一个标签就等于把全部出站的 JS 都塞进
    // 第一个窗口——所有出站共用一个 origin，故障隔离与多 TCP 双双落空。
    let mut carrier_windows: BTreeMap<String, String> = BTreeMap::new();
    match &plan.carrier {
        Some(c) => {
            tracing::info!("承载模式 {:?}，宿主出站「{}」", c.mode(), c.host_name());
            for ob in &plan.outbound_cfgs {
                match c.window_label(&ob.name) {
                    Some(w) => {
                        carrier_windows.insert(ob.name.clone(), w);
                    }
                    None => {
                        // 承载计划与出站列表来自同一份 proxies，对不上说明
                        // 两者的构建逻辑分叉了——猜一个窗口等于把这个出站的
                        // 流量发去别人的 origin。
                        eprintln!("承载计划里没有出站「{}」的窗口", ob.name);
                        std::process::exit(2);
                    }
                }
            }
        }
        // 零出站是全新用户的正常状态：不建任何承载窗口，控制窗口与混合端口
        // 入口照常起，用户在界面上添加第一个服务器之前不需要任何前置条件。
        None => tracing::info!("配置里还没有出站——不建承载窗口，控制窗口与入口照常"),
    }

    let carrier = plan.carrier;
    let outbound_cfgs = plan.outbound_cfgs;
    // 产生首份运行时快照的那一段 `proxies:`——配置保存后要拿它比对「出站段
    // 到底动没动」（`runtime_state::outbound_changes_pending`）。
    let proxies = Arc::new(config.proxies.clone());

    tauri::Builder::default()
        .setup(move |app| {
            // 承载 WebView：加载服务端真实首页 + initialization_script 注入
            // emitter（同源关键，见模块注释）。零出站时没有承载计划，整段
            // 跳过——不是「建一个空窗口」，是一个都不建。
            if let Some(c) = &carrier {
                outbound::carrier::spawn_carrier_windows(&app.handle().clone(), c, show_window)?;
            }

            // 托盘先于窗口建：窗口建失败时用户至少还有托盘可用（这两条
            // 生命周期是独立的，见 tray.rs 模块注释）。同样不阻断代理。
            if let Err(e) = tray::build(&app.handle().clone()) {
                tracing::error!("托盘创建失败：{e:#}");
            }

            // 控制窗口（设计文档 §11.3）。建不起来不阻断代理 —— 用户至少
            // 还能靠托盘和日志排障，而代理本身与界面无关。
            if let Err(e) = control::open(&app.handle().clone()) {
                tracing::error!("控制窗口创建失败：{e:#}");
            }

            // 事件聚合节流（设计文档 §11.2）。必须在建完控制窗口之后起：
            // emit_control 会先查窗口在不在，不在就短路。
            let agg = events::Aggregator::new();

            // 恢复上次的命中计数（§11.2），要在 spawn 之前 —— rule_hit_loop
            // 的增量是「本轮快照 − 上轮快照」，先 spawn 再 restore 会让恢复
            // 的历史值被当成一整轮的新增，UI 上凭空冒出一个巨大的尖峰。
            // 取不到配置目录时跳过：观察数据丢了不影响功能，但要说出来。
            match app.path().app_config_dir() {
                Ok(dir) => {
                    let saved = stats::load(&stats::path(&dir));
                    agg.hits.restore(saved.rule_hits);
                }
                Err(e) => tracing::warn!("取配置目录失败（{e}），命中计数不恢复"),
            }

            agg.spawn(app.handle().clone());
            app.manage(agg);

            let handle = app.handle().clone();
            let bind = inbound_bind.clone();
            let obs = outbound_cfgs.clone();
            let proxies = proxies.clone();
            let rules = rules.clone();
            let geo = geo.clone();
            let wins = carrier_windows.clone();
            let tun = tun_setup.take();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = run_stack(handle, bind, wins, obs, proxies, rules, geo, tun).await {
                    tracing::error!("入口/出站栈退出: {e:#}");
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            wsieve_heartbeat,
            wsieve_raw_post,
            wsieve_raw_stream,
            // ── 以下只授权给 control 窗口（capabilities/control.json）──
            // 往 transport.json 里加它们中的任何一个，都等于把配置读写权
            // 交给那台随时可能被攻破的服务器。守它的是 tests/capability_isolation.rs。
            commands::config::config_get,
            commands::config::config_get_raw,
            commands::config::config_save,
            commands::config::config_save_raw,
            commands::config::config_insert_proxy,
            commands::config::config_delete_proxy,
            commands::control::connect,
            commands::control::disconnect,
            commands::control::set_mode,
            commands::control::outbound_enable,
            commands::control::control_hide,
            commands::control::app_quit,
            commands::probe::rule_test,
            commands::probe::outbound_latency_probe,
            commands::probe::geo_update,
            commands::probe::geo_status,
            commands::probe::traffic_snapshot,
        ])
        .build(tauri_ctx)
        .expect("tauri build")
        .run(move |app, event| {
            // 退出时**按获取的相反顺序**摘除全部外部系统状态托管（spec §10）：
            // TUN 路由不撤的话半个 IPv4 空间指向一个即将消失的 utun；
            // 系统代理不关的话用户整机断网；hosts 不摘的话域名会一直指向
            // 已经不在跑的转发器。三者的崩溃路径都由下次启动的 clear_stale 兜底。
            if let tauri::RunEvent::Exit = event {
                // TUN 最先撤：它接管的是**默认路由**，影响面最大。撤晚了的话，
                // 在系统代理与 hosts 摘除的那一小段时间里，流量仍在往一个
                // 正在拆的 TUN 里走。
                if let Some(r) = tun_routes.lock().unwrap().take() {
                    use crate::custody::ManagedSystemState;
                    if let Err(e) = ManagedSystemState::revert(&*r) {
                        // drop 路径上报不了给用户，日志是唯一去处。
                        tracing::error!(
                            "撤销 TUN 路由失败（{e:#}）：可能残留 0/1 与 128.0/1。\
                             default 未被改写，基本上网不受影响；\
                             下次启动会自动清理，也可手工执行 netstat -rn 检查"
                        );
                    } else {
                        tracing::info!("已撤销 TUN 路由");
                    }
                }
                sysproxy_guard.lock().unwrap().take();
                shard_guard.lock().unwrap().take();

                // 落盘命中计数（§11.2）。放在最后：它纯粹是观察数据，
                // 而上面两条摘的是会影响用户整机网络的东西，先做要紧的。
                // 失败要报出来 —— 静默丢数据是房规明令禁止的。
                if let Some(agg) = app.try_state::<events::Aggregator>() {
                    match app.path().app_config_dir() {
                        Ok(dir) => {
                            let f = stats::StatsFile {
                                version: stats::VERSION,
                                rule_hits: agg.hits.snapshot(),
                            };
                            if let Err(e) = stats::save(&stats::path(&dir), &f) {
                                tracing::error!("写 stats.json 失败：{e}");
                            }
                        }
                        Err(e) => tracing::warn!("取配置目录失败（{e}），命中计数未保存"),
                    }
                }
            }
        });
}

/// GEO 数据文件路径：环境变量优先，否则取当前目录下的同名文件。
fn geo_path(env: &str, name: &str) -> std::path::PathBuf {
    std::env::var(env)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(name))
}

/// `config.yaml` 的绝对路径。
///
/// 这是 `tauri::path::PathResolver::app_config_dir()` 的复刻
/// （`dirs::config_dir().join(identifier)`）——`main()` 里读配置的那一刻
/// 还没有 `AppHandle`。两处**必须**算出同一条路径：不一致就意味着控制窗口
/// 写的和运行时读的是两份文件，用户改了配置却毫无效果，而界面上一切正常。
/// 因此 identifier 由调用方从 `generate_context!()` 里取，不在这里另抄一份
/// 字面量。
fn config_file_path(identifier: &str) -> anyhow::Result<std::path::PathBuf> {
    let dir = dirs::config_dir().ok_or_else(|| {
        anyhow::anyhow!("取不到本机的用户配置目录，无法定位 config.yaml")
    })?;
    Ok(dir.join(identifier).join("config.yaml"))
}

/// 读 + 解析 + 语义校验。
///
/// `validate()` 不能省：它拦的是枚举字段取值、出站重名、未知出站类型这类
/// 「语法对、含义不对」的问题。少了这一步，坏配置会一路漏到各个下游模块，
/// 报出来的错离病因很远。
fn load_config(p: &std::path::Path) -> anyhow::Result<wsieve_config::Config> {
    let config = wsieve_config::load_file(p)?;
    config.validate()?;
    Ok(config)
}

/// TUN 拉起前就该准备好的东西：bypass 名单、路由托管、fake-ip 池。
///
/// 之所以要在建 WebView 之前构造，是因为 `bypass_hook` 必须递进
/// `shard_setup::plan` —— 那是 §8.3.2 第 2 步唯一正确的位置。
struct TunSetup {
    bypass: wsieve_tun::bypass::BypassSet,
    routes: Arc<wsieve_tun::managed::TunRoutes>,
    pool: Arc<wsieve_tun::fakeip::FakeIpPool>,
}

/// §8.3.2 的第 0 步：查 fake-ip 段归属，并备好路由托管与 fake-ip 池。
///
/// 返回 `None` = 本次不开 TUN。**每一条 `None` 路径都必须留下一条可操作的
/// 日志**（M8）：静默不开会让用户以为 TUN 在跑而实际全部流量走的是混合端口，
/// 与 §6.4「禁止回退直连」同源。
///
/// **无论开不开 TUN，路由残留都先清一次**（§10）：用户上次崩溃后把开关关掉，
/// 残留的 `0/1` 就再也没人清了 —— 那条路由指向一个已经消失的 utun，会把半个
/// IPv4 空间黑洞掉，而用户在任何界面上都看不出这跟本程序有关。这与
/// `setup_system_proxy` 里「本次不开也照清」是同一条纪律。
#[cfg(target_os = "macos")]
fn tun_prepare(cfg: &bootstrap::AppConfig, server_urls: &[String]) -> Option<TunSetup> {
    use wsieve_tun::managed::{self, FakeIpRangeOwner, MacRouteBackend, TunRoutes};

    // 物理网关取不到就不能继续：全部 bypass 路由都要指向它，猜错的话每条
    // 出站都连不上，而路由表看上去一切正常。
    let phys_gw = match managed::default_gateway() {
        Ok(g) => g,
        Err(e) => {
            if cfg.tun {
                tracing::error!("TUN 未启用：取不到物理网关（{e:#}）——没有它就写不出 bypass 路由");
            } else {
                tracing::debug!("跳过 TUN 路由残留清理：{e:#}");
            }
            return None;
        }
    };
    let backend = Arc::new(MacRouteBackend::new(wsieve_tun::device::TUN_ADDR));
    let routes = TunRoutes::new(backend, wsieve_tun::device::TUN_ADDR, &phys_gw);

    if !cfg.tun {
        // 本次不开，但残留照清。清不掉多半是没 root —— 说清楚怎么办。
        if let Err(e) = routes.clear_stale() {
            tracing::warn!(
                "清理 TUN 路由残留失败（{e:#}）——若上次异常退出，\
                 残留的 0/1 路由可能仍在黑洞流量。可用 sudo 启动一次让它自清，\
                 或手工执行：sudo route -n delete -net 0.0.0.0/1"
            );
        }
        return None;
    }

    // §8.3.2 第 0 步：段归属。只读、无需 root，是唯一「发现问题可以干净掉头」
    // 的时刻 —— 此刻还没动过系统任何状态。
    match managed::fakeip_range_owner(tun::RANGE_PROBE) {
        Ok(FakeIpRangeOwner::Unclaimed) => {}
        Ok(FakeIpRangeOwner::Claimed {
            destination,
            interface,
        }) => {
            // 两个 fake-ip 池共用一个段会互相认领对方分配的假 IP，
            // 表现为随机的域名错连且无从排查。拒绝，并说清关谁。
            tracing::error!(
                "TUN 未启用：fake-ip 段 198.18.0.0/15 已被{}接管（命中路由 {destination}），\
                 多半是机器上另有一个 TUN 代理在跑。两个 fake-ip 池共用同一个段会互相认领\
                 对方分配的假 IP，表现为随机的域名错连且无从排查。\
                 请关闭另一个代理的 TUN 模式后重试",
                match &interface {
                    Some(i) => format!("接口 {i}"),
                    None => "一个未知接口".to_string(),
                }
            );
            return None;
        }
        Err(e) => {
            // 「问不出来」绝不能当成「没人占」—— 那等于在一台状况不明的
            // 机器上照常拉起 TUN。
            tracing::error!("TUN 未启用：查询 fake-ip 段归属失败（{e:#}）");
            return None;
        }
    }

    // 残留必须在 apply 之前清 —— 且是在**已经决定要开**之后清，
    // 否则刚写好的条目会被紧接着的清理抹掉（`CustodyGuard::acquire` 的顺序）。
    if let Err(e) = routes.clear_stale() {
        tracing::error!(
            "TUN 未启用：清理路由残留失败（{e:#}）。\
             写路由需要管理员权限，请用 sudo 启动，或在配置里关掉 tun.enable"
        );
        return None;
    }

    // 出站服务器域名自动并入 fake-ip filter（§7.2 纪律①）：它一旦拿到假 IP，
    // 转发器就连向虚空，且全程零报错。用户不该需要记住这件事。
    //
    // **每一个**出站的域名都要进：漏掉任何一个，那个出站就单独连向虚空，
    // 而其余出站一切正常——这种「只有一个节点用不了」的现象最难归因。
    let server_domains: Vec<String> = server_urls.iter().flat_map(|u| server_host(u)).collect();
    let pool = tun::build_pool(Vec::new(), &server_domains);

    Some(TunSetup {
        bypass: wsieve_tun::bypass::BypassSet::new(),
        routes: Arc::new(routes),
        pool,
    })
}

/// 其余平台：TUN 不可用，明确说出来而不是静默什么都不做。
///
/// 静默的后果与 M8 同源 —— 用户以为 TUN 开着，实际全部流量走的是别的路。
#[cfg(not(target_os = "macos"))]
fn tun_prepare(cfg: &bootstrap::AppConfig, _server_urls: &[String]) -> Option<TunSetup> {
    if cfg.tun {
        tracing::error!(
            "TUN 未启用：本平台暂不支持（阶段 6 只完整实现 macOS）。\
             Linux 需要 CAP_NET_ADMIN 与 netlink 路由实现，\
             Windows 需要 wintun.dll 与 IPHLPAPI 路由实现，二者均待补。\
             混合端口入口不受影响"
        );
    }
    None
}

/// 从服务端 URL 取出主机名，供 fake-ip filter 用。
///
/// 取不到时返回空 vec 而不是 panic：URL 非法在 `shard_setup::plan` 那边
/// 已经会降级并告警，这里再报一次只是噪音。但**空 filter 要能被察觉** ——
/// 由调用方的日志承担。
fn server_host(url: &str) -> Vec<String> {
    let Some((_, rest)) = url.split_once("://") else {
        return Vec::new();
    };
    let authority = rest.split('/').next().unwrap_or("");
    let host = match authority.strip_prefix('[') {
        // IPv6 字面量不需要进 filter：fake-ip 只发 IPv4，撞不上。
        Some(_) => return Vec::new(),
        None => authority.split(':').next().unwrap_or(""),
    };
    if host.is_empty() || host.parse::<std::net::IpAddr>().is_ok() {
        // IP 字面量同理：它压根不经过 DNS。
        return Vec::new();
    }
    vec![host.to_string()]
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
/// `carrier_windows` 是「出站名 → 承载窗口标签」。零出站时它是空的，
/// 整条栈照常起来：`OutboundManager`/`Router` 的出站表为空，`start_all()`
/// 是空操作，入口照常监听并按规则表（此时兜底为 `MATCH,REJECT`）拒绝连接。
async fn run_stack(
    app: tauri::AppHandle,
    bind: String,
    carrier_windows: BTreeMap<String, String>,
    ob_cfgs: Vec<outbound::instance::OutboundCfg>,
    proxies: Arc<Vec<wsieve_config::Proxy>>,
    rules: Arc<wsieve_route::RuleSet>,
    geo: Arc<wsieve_geo::GeoDb>,
    tun_setup: Option<TunSetup>,
) -> anyhow::Result<()> {
    let mut table = BTreeMap::new();
    for c in ob_cfgs {
        let inst = outbound::instance::OutboundInstance::new(c);
        // 键取自实例自己的 `name()`，不取外面另传的一份：两者一旦不一致，
        // 规则里写的名字就查不到实例，表现为「配置明明写了却说出站不存在」。
        table.insert(inst.name().to_string(), inst);
    }

    // 路由分派器与出站管理器**共用同一批实例**：管理器跑它们的会话循环、
    // 往里装 dialer，分派器读 dialer 决定这条连接能不能走。各持一份副本
    // 的话，分派器会永远看到一个空的 dialer 格 —— 表现为「明明连上了却
    // 一直被拒」。
    //
    // 阶段 2 尚未接入 DNS 解析器，`without_resolver` 会对着规则表里的
    // IP 类规则条数如实告警 —— 静默的覆盖面缺失比报错更危险。
    let router = Arc::new(router::Router::without_resolver(
        rules.clone(),
        geo,
        table.clone(),
    ));

    // 第一代承载核心先建出来：运行时快照要在 `dispatch` 之前就位（下面
    // 每条连接都经它读当前 `Router`），而快照里的 `OutboundManager` 需要
    // 一个 core。之后每一代在循环末尾换新的。
    let mut core = Arc::new(bridge::TransportCore::new_beat_pending());
    let mut manager = new_manager(&app, &carrier_windows, &table, &core);

    // **只托管这一次**：Tauri 的 `manage()` 在类型已注册时是空操作，后续
    // 更新一律走 `RuntimeHandle` 那把写锁（见 `runtime_state::install`）。
    runtime_state::install(
        &app,
        runtime_state::RuntimeState {
            rule_set: rules,
            outbound_manager: manager.clone(),
            router,
            groups: Arc::new(runtime_state::GroupTable),
            proxies,
        },
    );

    // 混合端口入口（Part C）：同一端口上嗅探 SOCKS5 与 HTTP。
    // bind 决策见 main() 里的注释与告警。
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| anyhow::anyhow!("入口监听 {bind} 失败: {e}"))?;
    tracing::info!("混合端口入口就绪：{bind}（SOCKS5 与 HTTP 同口）");
    // **一个 dispatch，两个入口。** 这是「TUN 只是又一个入口」的实体：
    // 下面把同一个 `dispatch` 分别递给混合端口的 `serve` 与 TUN 的 `run`。
    // 若哪天这里需要第二个 dispatch，说明有人在 TUN 那边另建了一条通路 ——
    // 那正是 §4.2 纪律②禁止的事。
    //
    // **每条连接都现读当前快照里的 `Router`，不捕获一份。** 捕获的话，
    // 配置保存后换上的那个新 `Router` 永远不会被真实流量用到——用户改了
    // 规则、日志说已生效、UI 也显示新规则，而分流仍按旧表走，且没有任何
    // 迹象。设计文档 §1 正是把「路由决策」列为这把锁的读者之一。
    // 读一次只是拿读锁克隆一个 `Arc`（引用计数 +1），不在热路径上引入
    // 可观察的开销。
    let dispatch: wsieve_inbound::Dispatch = {
        let app = app.clone();
        Arc::new(move |target| {
            // 取不到快照就**拒绝**这条连接，绝不兜底直连（§6.4）——
            // 正常路径上它必然已托管（上面几行），走到这里说明有人改坏了
            // 启动顺序，此时静默放行比连不上危险得多。
            let state = app
                .try_state::<runtime_state::RuntimeHandle>()
                .map(|h| h.current());
            Box::pin(async move {
                match state {
                    Some(s) => s.router.dispatch(target).await,
                    None => Err(std::io::Error::other(
                        "运行时快照尚未就绪，拒绝这条连接（绝不回退直连）",
                    )),
                }
            })
        })
    };
    {
        let dispatch = dispatch.clone();
        tokio::spawn(async move {
            if let Err(e) = wsieve_inbound::serve(listener, dispatch).await {
                // accept 循环退出 = 入口彻底失守，绝不静默。
                tracing::error!("混合端口入口退出：{e}");
            }
        });
    }

    // §8.3.2 的第 5 步，也是最后一步：拉起 TUN。
    //
    // 走到这里意味着前四步都已完成 —— 段归属查过、真实 IP 解析过、bypass
    // 路由写过、转发器与 hosts 就位（`tun_may_start()` 在 main 里把关）。
    // DNS 劫持从这一刻起生效，此后任何解析都可能是 fake-ip。
    if let Some(t) = tun_setup {
        bring_up_tun(t, dispatch.clone()).await;
    }

    // **只托管这一次**，之后换代走 `CurrentCore::set`（见该类型的注释：
    // 反复 `manage()` 是空操作，曾经让应用在承载页面死过一次之后再不恢复）。
    app.manage(CurrentCore::new(core.clone()));

    // 承载代循环：core 死一次就换一代。
    loop {
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

        // 下一代：换 core、换管理器。**必须排在 reload 之前** —— 页面一重载
        // 就会立刻开始每 5 秒一拍的心跳，此时 `CurrentCore` 若还指着上一代
        // 那个已死的 core，最先几拍就白白丢掉，而新 core 那边
        // `wait_first_heartbeat` 正在空等。
        core = Arc::new(bridge::TransportCore::new_beat_pending());
        match app.try_state::<CurrentCore>() {
            Some(c) => c.set(core.clone()),
            // 托管发生在循环之前，走到这里说明启动顺序被改坏了。绝不静默：
            // 后果正是这个 bug 本身——IPC 永远打在上一代的 core 上。
            None => tracing::error!("CurrentCore 尚未托管，换代后的传输核心无处安放"),
        }
        // **只把管理器这一个字段写回运行时快照** —— 这一代跑着的期间用户
        // 可能保存过配置（`rebuild_and_swap` 已经换掉了 rule_set/router），
        // 拿本代开头捕获的整份快照去覆盖，会把刚保存的规则悄悄回滚，
        // 而 UI 上显示的仍是新规则。
        manager = new_manager(&app, &carrier_windows, &table, &core);
        runtime_state::swap_outbound_manager(&app, manager.clone());

        // core 死才 reload —— 这是两级生命周期里唯一该 reload 的那一级。
        // 但只 reload 承载/传输窗口：control 窗口装着用户正在编辑的
        // Svelte 状态（当前视图、打开的对话框、未提交的表单），它的页面
        // 没死，reload 它只会把这些状态原地清空——用户什么都没做，
        // 出站重连一次首页就白丢一次。
        let all_labels: Vec<String> = app.webview_windows().keys().cloned().collect();
        for label in windows_to_reload(&all_labels, control::LABEL) {
            if let Some(w) = app.get_webview_window(&label) {
                let _ = w.eval("window.location.reload()");
            }
        }
        // 给页面一点时间重新加载并把 emitter 注回去。
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// 建一代出站管理器。
///
/// 实例表与 `Router` 共用**同一批** `Arc`（见 `run_stack` 里的说明），
/// core 是这一代承载的；换代时除了 core 之外的一切都原样沿用，因此这里
/// 收的是引用、只做克隆，不重新构造任何实例。
fn new_manager(
    app: &tauri::AppHandle,
    carrier_windows: &BTreeMap<String, String>,
    table: &BTreeMap<String, Arc<outbound::instance::OutboundInstance>>,
    core: &Arc<bridge::TransportCore>,
) -> Arc<outbound::OutboundManager> {
    let env = outbound::instance::SessionEnv {
        eval: eval_fn(app.clone(), carrier_windows.clone()),
        on_status: status_fn(app.clone()),
    };
    outbound::OutboundManager::new(table.clone(), core.clone(), env)
}

/// §8.3.2 的第 5 步：创建 utun、写默认路由、起 fake-ip DNS、跑入站循环。
///
/// **本函数不阻塞调用方**：TUN 入站循环与 DNS 服务器都 spawn 出去，
/// `run_stack` 继续跑它的承载代循环。
///
/// 失败一律**明确报错并不启用**，绝不静默降级（M8）—— 静默降级会让用户
/// 以为 TUN 开着而实际全部流量走的是混合端口，与 §6.4 同源。
async fn bring_up_tun(t: TunSetup, dispatch: wsieve_inbound::Dispatch) {
    use crate::custody::ManagedSystemState;

    // 1) 写两条 /1 默认路由。**先于**创建设备：设备起来了而路由没写，
    //    TUN 什么都收不到，用户看到的是「开了 TUN 但毫无变化」。
    //    路由写失败的最常见原因是没 root —— `MacRouteBackend` 已经把它
    //    翻译成一句可操作的话。
    if let Err(e) = ManagedSystemState::apply(&*t.routes) {
        tracing::error!(
            "TUN 未启用：写默认路由失败（{e:#}）。\
             创建 utun 与写路由都需要管理员权限，请用 sudo 启动，\
             或在配置里关掉 tun.enable。混合端口入口不受影响"
        );
        // 写了一半的要撤干净，否则残留的 0/1 指向一个根本没建的设备。
        if let Err(e2) = ManagedSystemState::revert(&*t.routes) {
            tracing::error!("回滚 TUN 路由同样失败（{e2:#}）——请手工检查 netstat -rn");
        }
        return;
    }

    // 2) 创建 utun 并对接 netstack。
    let stack = match wsieve_tun::device::spawn_netstack().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                "TUN 未启用：创建 utun 设备失败（{e}）。\
                 需要管理员权限，请用 sudo 启动，或在配置里关掉 tun.enable"
            );
            // 设备没建成，路由必须撤 —— 留着就是指向虚空的黑洞。
            if let Err(e2) = ManagedSystemState::revert(&*t.routes) {
                tracing::error!("撤销 TUN 路由失败（{e2:#}）——请手工执行 netstat -rn 检查 0/1 与 128.0/1");
            }
            return;
        }
    };
    tracing::info!(
        "TUN 设备就绪：{}（{}/{}），默认路由已接管",
        stack.if_name,
        wsieve_tun::device::TUN_ADDR,
        wsieve_tun::device::TUN_PREFIX
    );

    // 3) fake-ip DNS。**与入站共用同一个池** —— 两个池的话 DNS 发出去的
    //    地址在入站那边永远反查不到，每条连接都被拒，表现是「TUN 一开
    //    什么都打不开」而两边日志都显示自己正常。
    let fake = Arc::new(wsieve_tun::fakedns::FakeDns::new(t.pool.clone()));
    match bootstrap::dns_upstream() {
        Ok(upstream) => {
            let server = Arc::new(wsieve_tun::dns_server::DnsServer::new(fake, upstream));
            // 绑 TUN 网关的 53：段内流量都进本设备，客户端把 DNS 指到
            // 网关上就能被我们接住，不需要动系统 DNS 设置。
            let listen = std::net::SocketAddr::new(
                wsieve_tun::device::TUN_ADDR.parse().expect("TUN_ADDR 是常量字面量"),
                53,
            );
            match server.serve(listen).await {
                Ok(handle) => {
                    tracing::info!("fake-ip DNS 就绪：{}（上游 {upstream}）", handle.local_addr());
                    // 句柄要活到进程结束 —— drop 掉会 abort 掉监听任务，
                    // 53 端口从此没人应答，整机 DNS 静默失败。
                    std::mem::forget(handle);
                }
                Err(e) => tracing::error!(
                    "fake-ip DNS 启动失败（{e}）：域名规则（GEOSITE / DOMAIN-SUFFIX）\
                     将无法命中，TUN 只能按 IP 分流"
                ),
            }
        }
        Err(e) => tracing::error!(
            "fake-ip DNS 未启动（{e:#}）：域名规则将无法命中，TUN 只能按 IP 分流"
        ),
    }

    // 4) 入站循环。dispatch 就是混合端口用的那一个。
    let inbound = Arc::new(wsieve_tun::inbound::TunInbound::new(t.pool, t.bypass));
    tokio::spawn(tun::run(stack, inbound, dispatch));
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

/// 承载代循环 reload 时该刷新哪些窗口 —— 除 `control` 外的全部。
///
/// control 窗口的页面从未失效（失效的是出站的传输核心），reload 它只会
/// 白白清空用户正在编辑的 Svelte 状态，见调用处注释。
fn windows_to_reload(all_labels: &[String], control_label: &str) -> Vec<String> {
    all_labels
        .iter()
        .filter(|l| l.as_str() != control_label)
        .cloned()
        .collect()
}

/// 把 JS 送进**这条 JS 所属出站的**承载 WebView。
///
/// 窗口标签按出站名查 `CarrierPlan::window_label` 的结果，不是写死的 "main"：
/// `isolated` 模式下每出站一个窗口，写死就会把全部出站的 JS 都塞进第一个
/// 窗口 —— 那等于所有出站共用一个 origin，故障隔离与多 TCP 双双落空。
///
/// 查不到出站时**不挑一个窗口顶上**：那会把这个出站的请求从别人的 origin
/// 发出去，流量到了另一台服务器而全程零报错（§6.4）。只告警并丢弃。
fn eval_fn(
    app: tauri::AppHandle,
    windows: BTreeMap<String, String>,
) -> outbound::instance::EvalFn {
    Arc::new(move |outbound: &str, js: String| {
        let Some(window) = windows.get(outbound) else {
            tracing::warn!("出站「{outbound}」没有对应的承载窗口，JS 无法送达");
            return;
        };
        match app.get_webview_window(window) {
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
    state.current().heartbeat();
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
    handle_frame(&state.current(), &body).await
}

#[tauri::command]
async fn wsieve_raw_stream(state: tauri::State<'_, CurrentCore>, request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let body = match request.body() {
        tauri::ipc::InvokeBody::Raw(b) => b.clone(),
        _ => return Err("raw body required".into()),
    };
    handle_frame(&state.current(), &body).await
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

    /// **换代之后，IPC 命令必须打到新一代的 core 上。**
    ///
    /// 这条测试守的是一个真实发生过的故障：`CurrentCore` 原本是个不可变
    /// newtype，靠承载代循环里反复 `app.manage()` 来「更新」——而 Tauri 的
    /// `manage()` 在类型已注册时是**空操作**（`StateManager::set` 里
    /// `if !already_set` 才 insert）。于是第二代起，页面新 emitter 的心跳
    /// 全打到第一代那个已死的 core 上：新 core 永远等不到心跳，
    /// `wait_first_heartbeat` 空等 60 秒，会话建起来后 POST 挂在新 core 的
    /// pending 表上，而页面回帧走 `complete_post` 打到旧 core、被静默丢弃
    /// （那里没有 else 分支），随后 `watch_core` 判新 core 心跳停摆又杀一代
    /// ——**承载页面死过一次之后应用再也不恢复**，且每轮日志都只说
    /// 「承载页面已失效」，看着像网络抖动。
    ///
    /// 失效后的现象是每轮 60s 空等 + 15s 判死 = 约 75 秒换一代，日志里只有
    /// 一句「承载页面已失效」，看着像网络抖动。**注意别把这个周期直接当成
    /// 本 bug 的判据**：承载页面压根没加载成功时（占位服务器、URL 不通）
    /// 会有一模一样的 75 秒周期，且从第一代起就有。本 bug 的判据是
    /// 「页面明明加载成功过、第一代工作正常，死过一次之后就再不恢复」。
    #[test]
    fn a_generation_swap_routes_heartbeats_to_the_new_core() {
        let gen1 = Arc::new(bridge::TransportCore::new_beat_pending());
        let gen2 = Arc::new(bridge::TransportCore::new_beat_pending());
        let current = CurrentCore::new(gen1.clone());

        // 承载页面死了，换代。
        current.set(gen2.clone());
        // 页面重载后新 emitter 的第一拍心跳——IPC 命令读的就是这个。
        current.current().heartbeat();

        assert!(
            !gen2.heartbeat_stale(),
            "换代后的心跳必须落到新一代的 core 上，否则它永远等不到心跳而被判死"
        );
        assert!(
            gen1.heartbeat_stale(),
            "上一代那个已死的 core 不该再收到心跳"
        );
    }

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

    /// 服务器域名必须能从 URL 里取出来 —— 它是 fake-ip filter 的唯一来源。
    ///
    /// 取不出来的后果不是「少一条 filter」，而是**服务器域名会拿到 fake-ip**，
    /// 转发器随即连向虚空，且全程零报错（§7.2 纪律①）。
    #[test]
    fn the_server_host_is_extracted_for_the_fake_ip_filter() {
        assert_eq!(server_host("https://srv.example.com/"), vec!["srv.example.com"]);
        assert_eq!(server_host("https://srv.example.com:8443/p"), vec!["srv.example.com"]);
        assert_eq!(server_host("http://srv.example.com"), vec!["srv.example.com"]);
    }

    /// 承载代循环 reload 时绝不能带上 control —— 它装着用户正在编辑的
    /// Svelte 状态，其它窗口（承载/传输）该照常刷新。
    #[test]
    fn windows_to_reload_excludes_control_but_keeps_others() {
        let all = vec![
            "control".to_string(),
            "wsieve-transport-A".to_string(),
            "wsieve-transport-B".to_string(),
        ];
        let reloaded = windows_to_reload(&all, "control");
        assert!(!reloaded.iter().any(|l| l == "control"), "control 不该被 reload");
        assert!(reloaded.iter().any(|l| l == "wsieve-transport-A"));
        assert!(reloaded.iter().any(|l| l == "wsieve-transport-B"));
        assert_eq!(reloaded.len(), 2);
    }

    /// IP 字面量与畸形 URL 不进 filter，但也不能 panic。
    ///
    /// IP 压根不经过 DNS，往 filter 里放它没有意义；而 URL 非法时
    /// `shard_setup::plan` 那边已经会降级并告警，这里再报一次只是噪音。
    #[test]
    fn ip_literals_and_malformed_urls_yield_no_filter_entry() {
        assert!(server_host("https://203.0.113.7/").is_empty(), "IPv4 字面量不走 DNS");
        assert!(server_host("https://[2001:db8::1]:443/").is_empty(), "IPv6 字面量同理");
        assert!(server_host("srv.example.com").is_empty(), "缺 scheme");
        assert!(server_host("https://").is_empty(), "缺主机");
        assert!(server_host("").is_empty());
    }

    /// DNS 上游必须是 IP —— 用域名配上游是个先有鸡还是先有蛋的死结。
    ///
    /// 裸 IPv6 是这里唯一的坑：它自己就带一堆冒号，靠「有没有冒号」猜带不带
    /// 端口必然猜错，把一个完全合法的上游判成非法。用户看到的只是
    /// 「DNS 起不来」，而域名规则会因此全部失效。
    #[test]
    fn the_dns_upstream_takes_bare_ips_and_rejects_domains() {
        use bootstrap::parse_dns_upstream as p;
        assert_eq!(p("1.1.1.1").unwrap().to_string(), "1.1.1.1:53", "裸 IPv4 补 53");
        assert_eq!(p("1.1.1.1:5353").unwrap().port(), 5353, "显式端口要保住");
        assert_eq!(
            p("2606:4700:4700::1111").unwrap().port(),
            53,
            "裸 IPv6 不能被当成「带端口」误解析"
        );
        assert_eq!(p("[2606:4700:4700::1111]:5353").unwrap().port(), 5353);

        // 域名一律拒绝，且错误要说清为什么。
        let e = p("dns.example.com").unwrap_err().to_string();
        assert!(e.contains("必须写 IP"), "错误要点明原因：{e}");
        assert!(p("dns.example.com:53").is_err());
        assert!(p("").is_err());
    }
}
