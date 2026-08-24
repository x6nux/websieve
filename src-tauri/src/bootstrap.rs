//! 启动配置（环境变量，Task 18 E2E 的最小接口）+ emitter 注入。
//!
//! `initialization_script` 里无法直接 fetch 本地文件（页面导航到远端
//! origin 后相对路径失效），因此用 data: URL 携带完整 emitter 源——
//! 单一事实源是 ui/emitter.js，build.rs 把它嵌进 EMITTER_JS。

use crate::emitter_src::EMITTER_JS;
use wsieve_proto::hello::MuxId;
use wsieve_xhttp::DEFAULT_MUX;

#[derive(Clone)]
pub struct AppConfig {
    pub server_url: String,
    pub server_pub: [u8; 32],
    pub client_priv: [u8; 32],
    pub mux_prefs: Vec<MuxId>,
    pub socks_listen: String,
    /// 本地条带转发器的起始端口（会话 i 用 base+i）。见 `crate::shard`。
    pub shard_base_port: u16,
    /// 是否显示传输 WebView 窗口。
    ///
    /// 该窗口加载的是服务端伪装页——它是**传输载体**而非用户界面（同源是
    /// 传输前提，见 spec §6.7），日常运行没有理由摆在用户面前。默认隐藏，
    /// 排障时置 `WSIEVE_SHOW_WINDOW=1` 打开看页面实际加载成什么样。
    pub show_window: bool,
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
            DEFAULT_MUX,
            MuxId::Yamux,
            MuxId::Muxado,
            MuxId::Picomux,
            MuxId::H2mux,
        ],
    };
    let shard_base_port = std::env::var("WSIEVE_SHARD_BASE_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        // 高端口：绑定 <1024 需要 root，而 hosts 已经要一次管理员权限了，
        // 不该再多要一个。对外仍然只走 :443，本地端口不出网。
        .unwrap_or(18443);
    // 传输 WebView 默认隐藏（见字段注释）。注意 macOS 的 WKWebView 在窗口
    // 不可见时会挂起 JS 定时器与 fetch，靠建窗时的
    // background_throttling(Disabled) 压制（macOS 14+ 生效）。
    let show_window = std::env::var("WSIEVE_SHOW_WINDOW")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false);
    Ok(AppConfig {
        server_url,
        server_pub,
        client_priv,
        mux_prefs,
        socks_listen,
        shard_base_port,
        show_window,
    })
}

/// 注入 emitter 的 initialization script。
///
/// 双层 base64：内层是 emitter 源码，外层再编一次，使 `window.atob()` 的
/// 输出恰好是合法的 emitter 源码文本，经 `<script>.textContent` 直接执行。
/// 不用 `src=data:` 子资源——WKWebView 会拦截 data: URL 的脚本加载
/// （Task 18 E2E 实测：onload/onerror 均不触发）。
pub fn loader_js() -> String {
    let b64 = crate::bridge::bs64_encode(EMITTER_JS.as_bytes());
    let b64_b64 = crate::bridge::bs64_encode(b64.as_bytes());
    let js = format!(
        r#"(function(){{var t=document.createElement('script');t.textContent=window.atob(window.atob('{b64_b64}'));(document.head||document.documentElement).appendChild(t);}})();"#
    );
    js
}
