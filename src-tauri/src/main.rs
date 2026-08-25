//! websieve Tauri shell（Task 17）。
//!
//! 同源决策（spec §6.7）：主 WebView 直接导航到服务端真实首页
//! （`WSIEVE_SERVER_URL`），所有代理 fetch 天然 same-origin。emitter 无法
//! 加进服务器的页面，唯一注入点是 `initialization_script`（页面脚本前
//! 执行，Tauri 2 `WebviewWindowBuilder::initialization_script`）——因此
//! 建窗在 Rust 代码里做（tauri.conf.json 的窗口条目仅是兜底配置）。
//!
//! IPC 二进制纪律（spec §3.2）：上行 POST body 与下行 chunk 均以顶层
//! Uint8Array invoke（raw 快路径），帧头 16 字节（BE）携带 requestId 等
//! 标量，绝不把二进制嵌进 JSON object。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bridge;
mod custody;
mod emitter_src;
mod outbound;
mod proxy;
mod shard;
mod shard_setup;

use std::sync::Arc;

mod bootstrap;

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
    // 本地条带编排：hosts 劫持 + 多端口转发（见 shard_setup）。必须在建
    // WebView 之前完成——主 WebView 要加载的正是转发器的端口。任何一步失败
    // 都降级为单会话，不影响可用性。
    let extra_sessions = wsieve_mux::stripe_runtime::StripeCfg::with_env().extra_sessions;
    let plan = tauri::async_runtime::block_on(shard_setup::plan(
        &cfg.server_url,
        cfg.shard_base_port,
        extra_sessions,
        custody::hosts::system_path(),
    ));
    let server_url = plan.page_url.clone();
    let show_window = cfg.show_window;
    let session_bases = plan.session_bases.clone();
    // guard 持有 hosts 清理职责；进程正常退出时由 RunEvent::Exit 显式 drop，
    // 崩溃路径由下次启动的 clear_managed 兜底。
    let shard_guard = std::sync::Mutex::new(plan.guard);
    // 系统代理托管（spec §8.2 / §10）。同 hosts 一样：持有即生效、drop 即恢复，
    // 崩溃残留由启动时的 clear_stale 兜底。
    let sysproxy_guard = std::sync::Mutex::new(setup_system_proxy(&cfg));

    tauri::Builder::default()
        .setup(move |app| {
            // 主 WebView：加载服务端真实首页 + initialization_script 注入
            // emitter（同源关键，见模块注释）。
            let window = tauri::webview::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::External(server_url.parse()?),
            )
            .title("websieve")
            .inner_size(480.0, 320.0)
            // 传输载体，不是用户界面（spec §6.7）。默认隐藏；
            // WSIEVE_SHOW_WINDOW=1 可打开排障。
            .visible(show_window)
            // spec §3.4：后台节流压制——macOS WKWebView 后台/隐藏时挂起
            // JS 定时器与 fetch（Task 18 E2E 实测心跳/流分块会停摆）。
            .background_throttling(tauri_utils::config::BackgroundThrottlingPolicy::Disabled)
            // spec §3.4：后台节流压制（macOS 14+ 生效）
            .initialization_script(bootstrap::loader_js())
            .build()?;
            let _ = window;

            let cfg = cfg.clone();
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = proxy::run(
                    handle,
                    proxy::ProxyCfg {
                        server_pub: cfg.server_pub,
                        client_priv: cfg.client_priv,
                        mux_prefs: cfg.mux_prefs,
                        socks_listen: cfg.socks_listen,
                        session_bases,
                    },
                )
                .await
                {
                    tracing::error!("proxy loop exited: {e:#}");
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
            // 退出时摘除全部外部系统状态托管（spec §10）：
            // hosts 不摘的话域名会一直指向已经不在跑的转发器；系统代理不关的话
            // 用户整机断网。两者的崩溃路径都由下次启动的 clear_stale 兜底。
            if let tauri::RunEvent::Exit = event {
                sysproxy_guard.lock().unwrap().take();
                shard_guard.lock().unwrap().take();
            }
        });
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
async fn wsieve_heartbeat(state: tauri::State<'_, proxy::CurrentCore>) -> Result<(), String> {
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
async fn wsieve_raw_post(state: tauri::State<'_, proxy::CurrentCore>, request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let body = match request.body() {
        tauri::ipc::InvokeBody::Raw(b) => b.clone(),
        _ => return Err("raw body required".into()),
    };
    handle_frame(&state.0, &body).await
}

#[tauri::command]
async fn wsieve_raw_stream(state: tauri::State<'_, proxy::CurrentCore>, request: tauri::ipc::Request<'_>) -> Result<(), String> {
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
