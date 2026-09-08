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
    /// `WSIEVE_MUX_PREFS`——Task 3 起不再是出站 mux 偏好的来源（那来自
    /// `Config.proxies[].mux-prefs`，每个出站各自的一份），因此目前没有
    /// 任何调用方读取这个字段。按团队要求（见运行时接入 config.yaml 计划
    /// Task 3）保留这个 env 变量本身不删——只是它暂时没有接线到任何出站
    /// 构造路径上，留作后续排障/覆盖手段的预留位。
    #[allow(dead_code)]
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
    /// 是否启用 TUN 入口（设计文档 §8.3）。
    ///
    /// **默认关。** 它需要管理员权限（创建 utun + 写路由表），而且改的是
    /// 整机的默认路由 —— 比系统代理更重。开启时若权限不足，会打一条可操作
    /// 的错误并**不启用**，绝不静默降级：静默降级会让用户以为 TUN 开着而
    /// 实际全部流量走的是混合端口（与 §6.4「禁止回退直连」同源）。
    pub tun: bool,
}

/// fake-ip DNS 的上游解析器地址。
///
/// **必须是 IP 字面量**：TUN 生效后域名解析会走我们自己，用域名配上游就是
/// 一个先有鸡还是先有蛋的死结。
///
/// 默认 `1.1.1.1:53`，与 `wsieve-dns` 的 bootstrap 默认一致。
///
/// ponytail: 上游是显式配置的明文 UDP，不走 DoH/DoT。**上限**：与上游之间
/// 的查询是明文的，本机到 `1.1.1.1` 这一段可被观测与篡改（`screen_upstream`
/// 只挡得住段内污染这一类）。**升级路径**：阶段 3 的 `wsieve-dns` 已经有
/// 完整的 DoH/DoT 上游实现，把 `DnsServer::new` 的第二个参数从 `SocketAddr`
/// 换成那边的 `Upstream` 即可 —— 本模块只是留了个注入点。
pub fn dns_upstream() -> anyhow::Result<std::net::SocketAddr> {
    let raw = std::env::var("WSIEVE_DNS_UPSTREAM").unwrap_or_else(|_| "1.1.1.1:53".to_string());
    parse_dns_upstream(&raw)
}

/// `dns_upstream` 的纯函数内核（从 env 里摘出来才好测）。
///
/// 先试**裸 IP**：裸 IPv6（`2606:4700:4700::1111`）自己就带一堆冒号，
/// 靠「有没有冒号」去猜带不带端口必然猜错 —— 那会把一个完全合法的上游
/// 判成非法，而用户看到的只是「DNS 起不来」。裸 IP 一律补 53。
/// 不是裸 IP 才按 `地址:端口` 解析，因此 `[::1]:5353` 这类写法照常可用。
pub fn parse_dns_upstream(raw: &str) -> anyhow::Result<std::net::SocketAddr> {
    if let Ok(ip) = raw.parse::<std::net::IpAddr>() {
        return Ok(std::net::SocketAddr::new(ip, 53));
    }
    raw.parse().map_err(|e| {
        anyhow::anyhow!(
            "DNS 上游 {raw} 不是合法的 IP[:端口]（{e}）。\
             必须写 IP 而非域名 —— TUN 生效后域名解析走我们自己，\
             用域名配上游是个死结"
        )
    })
}

/// 32 字节十六进制解码：出站的 `server-pub`/`client-priv` 与（历史上）
/// env 引导路径共用同一条纪律——解析失败报错，绝不悄悄给一个全零数组
/// （那等于用一把错的钥匙悄悄握手，失败现象离病因很远）。
///
/// `pub(crate)`：Task 3 起，`runtime_state::build_startup_plan` 用它把
/// `Config.proxies[].server_pub/client_priv` 解码成 `[u8; 32]`——两处
/// 解码逻辑必须是同一个函数，而不是各写一份可能悄悄跑偏的拷贝。
pub(crate) fn hex32(s: &str) -> anyhow::Result<[u8; 32]> {
    let b = hex::decode(s.trim())?;
    b.try_into()
        .map_err(|v: Vec<u8>| anyhow::anyhow!("expected 32 bytes, got {}", v.len()))
}

/// 一个布尔型 env 旋钮：没设置返回 `None`（交给配置文件定），设置了就
/// 按「非空且不是 `0`」判真。
///
/// 返回 `Option` 而不是带默认值的 `bool` 是关键：分不清「没设置」与
/// 「设成了 false」的话，env 就会用它的默认值把配置文件里的 `true` 盖掉，
/// 表现为「界面上开关是开的，运行时却是关的」。
fn env_bool(key: &str) -> Option<bool> {
    std::env::var(key).ok().map(|v| v != "0" && !v.is_empty())
}

/// 从 `config.yaml` 装配运行参数。
///
/// **`config.yaml` 是事实来源，env 变量只是排障旋钮**（显式设置时压过它）。
/// 反过来的话，用户在控制窗口里改的端口/开关永远不会生效——而这正是这轮
/// 「运行时接入 config.yaml」要消灭的那类断层：`mixed-port` / `allow-lan` /
/// `system-proxy` / `shard-base-port` / `tun.enable` 五个字段此前**没有
/// 任何调用方**，界面上改完连重启都不生效，且没有任何提示。
///
/// `show_window` 没有对应的配置字段，仍然只认 env——它是排障用的窗口开关，
/// 不是产品级配置，设计文档 §5.2 也没有列它。
pub fn load_cfg(config: &wsieve_config::Config) -> anyhow::Result<AppConfig> {
    // 监听地址的**主机部分**只是记录：真正绑哪个网卡由 `inbound_bind_addr`
    // 按 allow-lan 决定（那是安全边界，不允许从地址里绕过去，见该函数注释）。
    let socks_listen = std::env::var("WSIEVE_SOCKS")
        .unwrap_or_else(|_| format!("{}:{}", config.bind_address, config.mixed_port));
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
    // 高端口：绑定 <1024 需要 root，而 hosts 已经要一次管理员权限了，
    // 不该再多要一个。对外仍然只走 :443，本地端口不出网。
    let shard_base_port = std::env::var("WSIEVE_SHARD_BASE_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(config.shard_base_port);
    // 传输 WebView 默认隐藏（见字段注释）。注意 macOS 的 WKWebView 在窗口
    // 不可见时会挂起 JS 定时器与 fetch，靠建窗时的
    // background_throttling(Disabled) 压制（macOS 14+ 生效）。
    // 只认 env：它没有对应的配置字段（见 load_cfg 的文档）。
    let show_window = env_bool("WSIEVE_SHOW_WINDOW").unwrap_or(false);
    // 下面三个的「默认关」现在由 `Config` 的 Default 承担（system-proxy /
    // allow-lan / tun.enable 都默认 false），各自为什么默认关见字段注释。
    let system_proxy = env_bool("WSIEVE_SYSTEM_PROXY").unwrap_or(config.system_proxy);
    let allow_lan = env_bool("WSIEVE_ALLOW_LAN").unwrap_or(config.allow_lan);
    let tun = env_bool("WSIEVE_TUN").unwrap_or(config.tun.enable);
    Ok(AppConfig {
        mux_prefs,
        socks_listen,
        shard_base_port,
        show_window,
        system_proxy,
        allow_lan,
        tun,
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

    /// **`config.yaml` 里的运行参数必须真的到达运行时。**
    ///
    /// 这五个字段（`mixed-port` / `allow-lan` / `system-proxy` /
    /// `shard-base-port` / `tun.enable`）此前**没有任何调用方**——`load_cfg`
    /// 只读 env，配置文件里写什么都不看。用户在控制窗口把端口从 25500 改成
    /// 别的，保存成功、界面显示新值、重启也没用，而且没有一条诊断。这条
    /// 测试就是防它复发。
    ///
    /// 环境里已经设了对应旋钮时跳过：那说明测试进程本身被显式覆盖了，
    /// 此时断言配置值反而是错的（旋钮就该压过配置文件）。
    #[test]
    fn the_config_file_drives_the_runtime_parameters() {
        for k in [
            "WSIEVE_SOCKS",
            "WSIEVE_ALLOW_LAN",
            "WSIEVE_SYSTEM_PROXY",
            "WSIEVE_TUN",
            "WSIEVE_SHARD_BASE_PORT",
        ] {
            if std::env::var(k).is_ok() {
                return;
            }
        }
        let mut c = wsieve_config::Config {
            mixed_port: 9999,
            allow_lan: true,
            system_proxy: true,
            shard_base_port: 30000,
            ..Default::default()
        };
        c.tun.enable = true;

        let cfg = load_cfg(&c).unwrap();
        assert_eq!(cfg.socks_listen, "127.0.0.1:9999", "mixed-port 没到达入口");
        assert!(cfg.allow_lan, "allow-lan 没到达入口");
        assert!(cfg.system_proxy, "system-proxy 没到达托管");
        assert!(cfg.tun, "tun.enable 没到达 TUN 准备");
        assert_eq!(cfg.shard_base_port, 30000, "shard-base-port 没到达条带");
    }

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
