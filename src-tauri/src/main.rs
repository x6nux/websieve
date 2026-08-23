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
mod emitter_src;
mod proxy;

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
    let server_url = cfg.server_url.clone();

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
            .visible(true)
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
        .run(tauri::generate_context!())
        .expect("tauri run");
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
