//! 启动配置（环境变量，Task 18 E2E 的最小接口）+ emitter 注入。
//!
//! `initialization_script` 里无法直接 fetch 本地文件（页面导航到远端
//! origin 后相对路径失效），因此用 data: URL 携带完整 emitter 源——
//! 单一事实源是 ui/emitter.js，build.rs 把它嵌进 EMITTER_JS。

use crate::emitter_src::EMITTER_JS;
use wsieve_proto::hello::MuxId;

#[derive(Clone)]
pub struct AppConfig {
    pub server_url: String,
    pub server_pub: [u8; 32],
    pub client_priv: [u8; 32],
    pub mux_prefs: Vec<MuxId>,
    pub socks_listen: String,
}

fn hex32(s: &str) -> anyhow::Result<[u8; 32]> {
    let b = hex::decode(s.trim())?;
    b.try_into()
        .map_err(|v: Vec<u8>| anyhow::anyhow!("expected 32 bytes, got {}", v.len()))
}

pub fn load_cfg() -> anyhow::Result<AppConfig> {
    let server_url = std::env::var("WSIEVE_SERVER_URL")
        .unwrap_or_else(|_| "https://example.com/".to_string());
    let server_pub = hex32(&std::env::var("WSIEVE_SERVER_PUB")?)?;
    let client_priv = hex32(&std::env::var("WSIEVE_CLIENT_PRIV")?)?;
    let socks_listen =
        std::env::var("WSIEVE_SOCKS").unwrap_or_else(|_| "127.0.0.1:1080".to_string());
    let mux_prefs = match std::env::var("WSIEVE_MUX_PREFS") {
        Ok(s) => s
            .split(',')
            .map(|p| {
                let id: u8 = p.trim().parse().expect("mux id 0-4");
                MuxId::from_u8(id).expect("mux id 0-4")
            })
            .collect(),
        Err(_) => vec![
            MuxId::Yamux,
            MuxId::Smux,
            MuxId::Muxado,
            MuxId::Picomux,
            MuxId::H2mux,
        ],
    };
    Ok(AppConfig {
        server_url,
        server_pub,
        client_priv,
        mux_prefs,
        socks_listen,
    })
}

/// 注入 emitter 的 initialization script。base64(EMITTER_JS) 编成 data:
/// URL，避开在 JS 字符串里转义源码的坑。
pub fn loader_js() -> String {
    let b64 = crate::bridge::bs64_encode(EMITTER_JS.as_bytes());
    format!(
        r#"(function(){{var s=document.createElement('script');s.src='data:text/javascript;base64,{b64}';document.head?document.head.appendChild(s):document.currentScript.parentNode.appendChild(s);}})();"#
    )
}
