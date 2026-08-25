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
    /// 是否把本机系统代理指向我们的 SOCKS 监听口（spec §5.2 `system-proxy`）。
    ///
    /// 默认关：它改的是全局系统设置，开着崩一次就让用户整机断网。
    /// 崩溃残留由启动时的 `clear_stale` 兜底（spec §8.2 / §10）。
    pub system_proxy: bool,
    /// 是否把混合端口入口开放给局域网（配置里的 `allow-lan`，spec §5.2）。
    ///
    /// **默认关，而且这个默认值是安全边界本身。**
    ///
    /// 开启后监听地址从 `127.0.0.1` 变成 `0.0.0.0`，同一网段内任何设备都能
    /// 把流量塞进来、经本机的隧道出网。而**这条路径上目前没有任何认证**：
    /// SOCKS5 侧只实现了方法 `0x00`（无认证），HTTP 侧则把
    /// `Proxy-Authorization` 直接剔除（那是防泄漏给源站，不是在校验它）。
    /// 两者合起来，`allow-lan: true` 就是一个**开放中继**——蹭网的人用你的
    /// 出口，出了事记在你头上，而你在本机看不到任何异常。
    ///
    /// 设计文档 §5.2 列了这个开关，但**没有**规定配套的认证方案。在补上
    /// 认证之前，开启它只应发生在完全可信的网段，且必须由用户显式选择。
    /// 因此这里不提供任何「自动开」的路径，开启时也会打一条醒目的告警。
    pub allow_lan: bool,
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
    // 系统代理默认关：改的是全局设置，崩一次就让用户整机断网（见字段注释）。
    let system_proxy = std::env::var("WSIEVE_SYSTEM_PROXY")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false);
    // 局域网开放默认关：这条路径上没有认证，开了就是开放中继（见字段注释）。
    let allow_lan = std::env::var("WSIEVE_ALLOW_LAN")
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
        system_proxy,
        allow_lan,
    })
}

/// 混合端口入口该绑在哪个地址。
///
/// `allow-lan: false`（默认）→ 只绑环回，本机之外连不上。
/// `allow-lan: true` → 绑 `0.0.0.0`，整个网段都能用。
///
/// **这不是一个纯粹的便利开关**。入口链路上目前没有任何认证：SOCKS5 只实现
/// 方法 `0x00`，HTTP 侧剔除 `Proxy-Authorization` 而不校验它。绑到 `0.0.0.0`
/// 之后，同网段的任何设备都能拿本机当出口 —— 流量记在本机名下，而本机毫无
/// 察觉。设计文档 §5.2 列了 `allow-lan` 却没规定配套认证，因此这里能做的
/// 只有两件：默认关，以及开启时把代价说清楚（由调用方打告警）。
pub fn inbound_bind_addr(listen: &str, allow_lan: bool) -> anyhow::Result<String> {
    let port = listen
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("监听地址缺少端口: {listen}"))?
        .1;
    let port: u16 = port
        .parse()
        .map_err(|_| anyhow::anyhow!("监听端口不是合法数字: {port}"))?;
    if port == 0 {
        // 端口 0 让内核随机挑一个，用户永远不知道该往哪连。
        anyhow::bail!("监听端口不能是 0: {listen}");
    }
    Ok(if allow_lan {
        format!("0.0.0.0:{port}")
    } else {
        format!("127.0.0.1:{port}")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_posture_is_loopback_only() {
        // 默认必须只绑环回。这不是风格问题：入口链路上没有任何认证
        // （SOCKS5 仅方法 0x00，HTTP 不校验 Proxy-Authorization），
        // 一旦默认绑到 0.0.0.0，装上就是个开放中继。
        assert_eq!(
            inbound_bind_addr("127.0.0.1:7890", false).unwrap(),
            "127.0.0.1:7890"
        );
        // 即使配置里写的是 0.0.0.0，allow-lan 关着就仍然只绑环回 ——
        // 开放与否的唯一开关是 allow-lan，不能从监听地址里绕过去。
        assert_eq!(
            inbound_bind_addr("0.0.0.0:7890", false).unwrap(),
            "127.0.0.1:7890"
        );
    }

    #[test]
    fn allow_lan_opens_the_port_to_the_whole_segment() {
        // 开了就是真的开：这条测试的存在本身是提醒 —— 改动这里就是在改
        // 攻击面。
        assert_eq!(
            inbound_bind_addr("127.0.0.1:7890", true).unwrap(),
            "0.0.0.0:7890"
        );
    }

    #[test]
    fn a_malformed_listen_addr_is_an_error_not_a_guess() {
        // 猜一个端口等于把入口开在用户没预期的地方，而用户以为自己配的是
        // 另一个 —— 他会以为端口没开，实际开着。
        assert!(inbound_bind_addr("127.0.0.1", false).is_err(), "缺端口");
        assert!(inbound_bind_addr("127.0.0.1:abc", false).is_err());
        assert!(inbound_bind_addr("127.0.0.1:70000", false).is_err(), "越界");
        // 端口 0 让内核随机挑一个，用户永远不知道该往哪连
        assert!(inbound_bind_addr("127.0.0.1:0", false).is_err());
    }

    #[test]
    fn the_port_survives_the_bind_decision_unchanged() {
        // 端口被悄悄改掉的话，用户配的 7890 变成别的，浏览器连不上而
        // 日志里看着一切正常。
        for p in [1u16, 1080, 7890, 65535] {
            for lan in [false, true] {
                let got = inbound_bind_addr(&format!("127.0.0.1:{p}"), lan).unwrap();
                assert!(got.ends_with(&format!(":{p}")), "端口被改了：{got}");
            }
        }
    }
}
