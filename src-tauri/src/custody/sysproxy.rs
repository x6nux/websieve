//! 系统代理设置的托管（设计文档 §8.2 / §10）。
//!
//! 崩溃残留与 hosts 是同一类问题，而且后果更重：进程崩了而系统代理
//! 还指着一个已经不在跑的端口，**用户整机断网**，且他多半不知道
//! 是这个程序干的。所以 clear_stale 不是可选项。
//!
//! §10 要求「托管条目需可识别」。hosts 用 `# wsieve-managed` 行尾标记；
//! 系统代理没地方写标记，这里用**等价的判据**：某个网络服务的代理
//! 处于「已启用 + 服务器与端口恰好是我们配置的 host:port」时才认作
//! 我们留下的，否则一律不碰。于是用户自己设的公司代理不会被误关。
//!
//! ponytail: 该判据认不出「上次用的端口与本次配置不同」的残留 ——
//! 用户在两次运行之间改了 mixed-port 就会漏掉。上限即此；升级路径是
//! 把「上次实际写入的 host:port + 服务名」落盘到 app 数据目录，
//! 启动时按那份记录清理，本次配置只作兜底。
//!
//! ponytail: 目前只实现 macOS（networksetup）。上限：Windows / Linux
//! 上 system-proxy 开关不生效，UI 需灰掉并提示手工设置。
//! 升级路径：Windows 写 HKCU\...\Internet Settings 并广播
//! WM_SETTINGCHANGE；Linux 走 gsettings（仅 GNOME 系）。

use crate::custody::ManagedSystemState;

/// macOS 上一个网络服务有三套互相独立的代理开关，缺一套就会有流量绕过。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyKind {
    /// HTTP
    Web,
    /// HTTPS
    SecureWeb,
    /// SOCKS
    Socks,
}

impl ProxyKind {
    const ALL: [ProxyKind; 3] = [ProxyKind::Web, ProxyKind::SecureWeb, ProxyKind::Socks];

    /// 读当前设置的子命令。
    fn getter(self) -> &'static str {
        match self {
            ProxyKind::Web => "-getwebproxy",
            ProxyKind::SecureWeb => "-getsecurewebproxy",
            ProxyKind::Socks => "-getsocksfirewallproxy",
        }
    }

    /// 设置服务器与端口（并自动置为启用）的子命令。
    fn setter(self) -> &'static str {
        match self {
            ProxyKind::Web => "-setwebproxy",
            ProxyKind::SecureWeb => "-setsecurewebproxy",
            ProxyKind::Socks => "-setsocksfirewallproxy",
        }
    }

    /// 只切换启用状态的子命令。
    fn state_setter(self) -> &'static str {
        match self {
            ProxyKind::Web => "-setwebproxystate",
            ProxyKind::SecureWeb => "-setsecurewebproxystate",
            ProxyKind::Socks => "-setsocksfirewallproxystate",
        }
    }
}

/// `networksetup -get*proxy` 读回来的一套设置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyState {
    pub enabled: bool,
    pub server: String,
    pub port: u16,
}

/// 解析 `networksetup -getwebproxy "<服务>"` 的输出。
///
/// 典型输出：
/// ```text
/// Enabled: No
/// Server: 127.0.0.1
/// Port: 10808
/// Authenticated Proxy Enabled: 0
/// ```
/// 注意「Authenticated Proxy Enabled」也以 `Enabled` 结尾，用前缀匹配
/// 而不是包含匹配，否则会把它的 `0` 当成主开关。
fn parse_proxy_state(out: &str) -> anyhow::Result<ProxyState> {
    let mut enabled = None;
    let mut server = String::new();
    let mut port = 0u16;
    for line in out.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("Enabled:") {
            enabled = Some(v.trim().eq_ignore_ascii_case("yes"));
        } else if let Some(v) = line.strip_prefix("Server:") {
            server = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("Port:") {
            // 未配置过时可能是 0 或空，都当作 0 处理；非数字才是真异常。
            let v = v.trim();
            port = if v.is_empty() {
                0
            } else {
                v.parse().map_err(|e| {
                    anyhow::anyhow!("networksetup 输出的端口 {v:?} 解析失败：{e}")
                })?
            };
        }
    }
    // 读不到 Enabled 说明输出格式不是我们认识的那种 —— 此时若按「没启用」
    // 处理，就会把真实残留漏过去，那正是 clear_stale 要防的事。必须报错。
    let enabled = enabled
        .ok_or_else(|| anyhow::anyhow!("networksetup 输出里没有 Enabled 字段：{out:?}"))?;
    Ok(ProxyState {
        enabled,
        server,
        port,
    })
}

/// 这套设置是不是我们留下的。
///
/// 这是 §10 「托管条目需可识别」在系统代理上的落法：没有地方写标记，
/// 就以「已启用且恰好指着我们的 host:port」为判据。判据以外的一律不碰 ——
/// 关掉用户自己设的公司代理，比留下我们的残留更难排查。
fn is_ours(state: &ProxyState, host: &str, port: u16) -> bool {
    state.enabled && state.server == host && state.port == port
}

/// 生成 apply 阶段要执行的命令，每条是 `networksetup` 的完整 argv（不含程序名）。
///
/// 拆出来是为了可测 —— 平台命令没法在单测里真跑。
///
/// 用 argv 而不是拼 shell 命令行：网络服务名来自 `-listallnetworkservices`，
/// 用户可以把它改成任意字符串（含空格、引号、`$`、反引号）。走 `sh -c`
/// 就得自己做转义，转义漏一个就是命令注入；argv 直接绕开整个问题。
fn macos_apply_commands(services: &[String], host: &str, port: u16) -> Vec<Vec<String>> {
    let mut cmds = Vec::with_capacity(services.len() * ProxyKind::ALL.len());
    for s in services {
        for kind in ProxyKind::ALL {
            cmds.push(vec![
                kind.setter().to_string(),
                s.clone(),
                host.to_string(),
                port.to_string(),
            ]);
        }
    }
    cmds
}

/// 生成把指定 (服务, 类型) 关掉的命令。
///
/// 注意这里收的是**逐条筛过的**目标，而不是「这些服务的全部代理」——
/// revert / clear_stale 只关认作我们的那些（见 `is_ours`），
/// 用户自己设的公司代理不在其列。
fn macos_off_commands(targets: &[(String, ProxyKind)]) -> Vec<Vec<String>> {
    targets
        .iter()
        .map(|(s, kind)| {
            vec![
                kind.state_setter().to_string(),
                s.clone(),
                "off".to_string(),
            ]
        })
        .collect()
}

/// 解析 `-listallnetworkservices` 的输出。
///
/// 首行是说明文字；带 `*` 前缀的是已禁用的服务，跳过。
fn parse_service_list(out: &str) -> Vec<String> {
    out.lines()
        .skip(1)
        .filter(|l| !l.starts_with('*') && !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect()
}

#[cfg(target_os = "macos")]
fn networksetup(args: &[String]) -> anyhow::Result<String> {
    let out = std::process::Command::new("networksetup")
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("执行 networksetup {args:?} 失败：{e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "networksetup {args:?} 退出码 {:?}：{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn networksetup(_args: &[String]) -> anyhow::Result<String> {
    anyhow::bail!("当前平台尚未实现系统代理设置（见模块注释的 ponytail 标注）")
}

/// 逐条执行；**不中途 return**。
///
/// 关代理的场景里，第一条失败就放弃会把后面本可以关掉的服务丢下不管，
/// 用户那几个服务就一直指着死端口。所以全部试完，再把错误一并抛出 ——
/// 尽力恢复，但绝不静默。
fn run_all(cmds: &[Vec<String>]) -> anyhow::Result<()> {
    let mut errs = Vec::new();
    for c in cmds {
        if let Err(e) = networksetup(c) {
            errs.push(format!("{e:#}"));
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{} 条命令失败：{}", errs.len(), errs.join("；"))
    }
}

pub struct SysProxyCustody {
    host: String,
    port: u16,
    services: Vec<String>,
}

impl SysProxyCustody {
    /// `services` 为空即报错 —— 枚举不到网络服务时静默成功，
    /// 会让用户以为代理已生效（照 shard.rs 的房规：报错不静默跳过）。
    pub fn new(host: String, port: u16, services: Vec<String>) -> anyhow::Result<Self> {
        if services.is_empty() {
            anyhow::bail!("枚举不到任何网络服务，无法设置系统代理");
        }
        Ok(Self {
            host,
            port,
            services,
        })
    }

    /// 枚举当前机器的网络服务名。
    pub fn enumerate_services() -> anyhow::Result<Vec<String>> {
        Ok(parse_service_list(&networksetup(&[
            "-listallnetworkservices".to_string()
        ])?))
    }

    /// 读某个服务某一类代理的当前设置。
    fn read_state(&self, service: &str, kind: ProxyKind) -> anyhow::Result<ProxyState> {
        let out = networksetup(&[kind.getter().to_string(), service.to_string()])?;
        parse_proxy_state(&out)
            .map_err(|e| anyhow::anyhow!("读取 {service} 的 {:?} 代理：{e:#}", kind))
    }

    /// 挑出「确实是我们留下的」那些 (服务, 类型)。
    ///
    /// 读不到状态时**当作是我们的**：宁可多关一次（用户重设一遍代理），
    /// 也不能漏掉一个指向死端口的残留（用户整机断网且无从查起）。
    /// 读失败本身照样向上报，不静默。
    fn ours(&self) -> (Vec<(String, ProxyKind)>, Vec<String>) {
        let mut targets = Vec::new();
        let mut errs = Vec::new();
        for s in &self.services {
            for kind in ProxyKind::ALL {
                match self.read_state(s, kind) {
                    Ok(st) => {
                        if is_ours(&st, &self.host, self.port) {
                            targets.push((s.clone(), kind));
                        }
                    }
                    Err(e) => {
                        errs.push(format!("{e:#}"));
                        targets.push((s.clone(), kind));
                    }
                }
            }
        }
        (targets, errs)
    }

    /// 关掉所有认作我们的代理设置。`revert` 与 `clear_stale` 共用。
    fn turn_off_ours(&self) -> anyhow::Result<()> {
        let (targets, read_errs) = self.ours();
        let run_err = run_all(&macos_off_commands(&targets)).err();
        match (read_errs.is_empty(), run_err) {
            (true, None) => Ok(()),
            (_, run_err) => {
                let mut parts = read_errs;
                if let Some(e) = run_err {
                    parts.push(format!("{e:#}"));
                }
                anyhow::bail!("系统代理恢复过程中出错：{}", parts.join("；"))
            }
        }
    }
}

impl ManagedSystemState for SysProxyCustody {
    fn name(&self) -> &'static str {
        "系统代理设置"
    }

    fn apply(&self) -> anyhow::Result<()> {
        run_all(&macos_apply_commands(&self.services, &self.host, self.port))
    }

    fn revert(&self) -> anyhow::Result<()> {
        self.turn_off_ours()
    }

    fn clear_stale(&self) -> anyhow::Result<()> {
        // 与 revert 同一动作。上次崩溃留下的代理设置在这里被关掉 ——
        // 判据是「已启用且指着我们的 host:port」，所以用户自己设的代理
        // 不受影响（见模块注释）。
        //
        // 注意：启动时即便本次不打算开系统代理，也必须调用一次 ——
        // 否则用户上次崩溃后把开关关掉，残留就永远留在系统里。
        // 这和 shard_setup 里 hosts 清残留放在所有早退分支之前是同一条纪律。
        self.turn_off_ours()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(cmds: &[Vec<String>]) -> Vec<String> {
        cmds.iter().map(|c| c.join(" ")).collect()
    }

    #[test]
    fn macos_apply_sets_both_socks_and_http_for_each_service() {
        let cmds = macos_apply_commands(&["Wi-Fi".into(), "Ethernet".into()], "127.0.0.1", 7890);
        // 每个网络服务都要设 socks + http + https，共 3 条
        assert_eq!(cmds.len(), 6, "两个服务 × 3 条命令");
        let j = joined(&cmds);
        assert!(j
            .iter()
            .any(|c| c.contains("-setsocksfirewallproxy") && c.contains("Wi-Fi")));
        assert!(j
            .iter()
            .any(|c| c.contains("-setwebproxy") && c.contains("Ethernet")));
        assert!(j
            .iter()
            .any(|c| c.contains("-setsecurewebproxy") && c.contains("Wi-Fi")));
        assert!(j.iter().all(|c| c.contains("7890")));
    }

    #[test]
    fn turning_a_service_off_covers_all_three_switches() {
        // 三套开关缺一不可 —— 少关一套就有流量继续走死端口。
        let targets: Vec<(String, ProxyKind)> = ProxyKind::ALL
            .into_iter()
            .map(|k| ("Wi-Fi".to_string(), k))
            .collect();
        let cmds = macos_off_commands(&targets);
        assert_eq!(cmds.len(), 3);
        let j = joined(&cmds);
        assert!(j.iter().all(|c| c.ends_with("off")));
        for sub in [
            "-setwebproxystate",
            "-setsecurewebproxystate",
            "-setsocksfirewallproxystate",
        ] {
            assert!(j.iter().any(|c| c.contains(sub)), "缺少 {sub}：{j:?}");
        }
    }

    #[test]
    fn service_names_with_spaces_stay_a_single_argument() {
        // "Wi-Fi" 没空格，但 "Thunderbolt Bridge" 有。走 argv 就不需要引号，
        // 只要保证它没被拆成两个参数。
        let cmds = macos_apply_commands(&["Thunderbolt Bridge".into()], "127.0.0.1", 7890);
        assert!(
            cmds.iter().all(|c| c[1] == "Thunderbolt Bridge"),
            "含空格的服务名必须整体作为一个参数：{cmds:?}"
        );
        assert!(cmds.iter().all(|c| c.len() == 4));
    }

    #[test]
    fn shell_metacharacters_in_service_name_are_not_interpreted() {
        // 用户可以把网络服务改名成任何东西。走 argv 时元字符只是普通字节；
        // 一旦有人改回拼 shell 命令行，这条会挂。
        let evil = r#"Wi-Fi"; rm -rf /tmp/x; echo ""#.to_string();
        let cmds = macos_apply_commands(std::slice::from_ref(&evil), "127.0.0.1", 7890);
        assert!(cmds.iter().all(|c| c[1] == evil));
        assert!(
            cmds.iter().all(|c| c.len() == 4),
            "参数个数固定为 4，元字符不得引入额外参数"
        );
    }

    #[test]
    fn empty_service_list_is_an_error_not_a_silent_noop() {
        // 枚举不到任何网络服务时，静默成功会让用户以为代理已生效
        assert!(SysProxyCustody::new("127.0.0.1".into(), 7890, vec![]).is_err());
    }

    #[test]
    fn proxy_state_is_parsed_from_real_networksetup_output() {
        // 实机 `networksetup -getwebproxy "Wi-Fi"` 的原样输出
        let out = "Enabled: No\nServer: 127.0.0.1\nPort: 10808\nAuthenticated Proxy Enabled: 0\n";
        let st = parse_proxy_state(out).unwrap();
        assert_eq!(
            st,
            ProxyState {
                enabled: false,
                server: "127.0.0.1".into(),
                port: 10808
            }
        );
        let on = "Enabled: Yes\nServer: 127.0.0.1\nPort: 65432\nAuthenticated Proxy Enabled: 0\n";
        assert!(parse_proxy_state(on).unwrap().enabled);
    }

    #[test]
    fn authenticated_proxy_line_does_not_hijack_the_enabled_flag() {
        // "Authenticated Proxy Enabled: 0" 也以 Enabled 结尾。若用包含匹配，
        // 它会把主开关的 Yes 覆盖成 false，残留就再也清不掉了。
        let out = "Enabled: Yes\nServer: 1.2.3.4\nPort: 8080\nAuthenticated Proxy Enabled: 0\n";
        assert!(
            parse_proxy_state(out).unwrap().enabled,
            "主开关必须来自行首的 Enabled:"
        );
    }

    #[test]
    fn unparseable_output_is_an_error_not_a_default() {
        // 认不出格式却返回 enabled:false，会让 clear_stale 把真残留漏过去。
        assert!(parse_proxy_state("").is_err());
        assert!(parse_proxy_state("Server: 1.2.3.4\nPort: 80\n").is_err());
        assert!(parse_proxy_state("Enabled: Yes\nServer: x\nPort: 不是数字\n").is_err());
    }

    #[test]
    fn only_entries_pointing_at_us_are_recognised_as_ours() {
        let ours = ProxyState {
            enabled: true,
            server: "127.0.0.1".into(),
            port: 7890,
        };
        assert!(is_ours(&ours, "127.0.0.1", 7890));

        // 用户自己设的公司代理：不能碰
        let corporate = ProxyState {
            enabled: true,
            server: "proxy.corp.example".into(),
            port: 8080,
        };
        assert!(!is_ours(&corporate, "127.0.0.1", 7890));

        // 另一个本机代理程序占着别的端口：也不能碰
        let other_app = ProxyState {
            enabled: true,
            server: "127.0.0.1".into(),
            port: 10808,
        };
        assert!(!is_ours(&other_app, "127.0.0.1", 7890));

        // 端口对上但没启用：不是残留，关它是白关一次
        let disabled = ProxyState {
            enabled: false,
            server: "127.0.0.1".into(),
            port: 7890,
        };
        assert!(!is_ours(&disabled, "127.0.0.1", 7890));
    }

    #[test]
    fn service_list_skips_header_and_disabled_entries() {
        // 实机 `networksetup -listallnetworkservices` 的原样输出（带一条禁用项）
        let out = "An asterisk (*) denotes that a network service is disabled.\n\
                   Ethernet\nThunderbolt Bridge\n*Old VPN\nWi-Fi\n";
        assert_eq!(
            parse_service_list(out),
            vec![
                "Ethernet".to_string(),
                "Thunderbolt Bridge".into(),
                "Wi-Fi".into()
            ],
            "首行说明与 * 前缀的禁用服务都要跳掉"
        );
    }

    /// 崩溃恢复的命令层证明：上次运行把 Wi-Fi 设成了我们的 127.0.0.1:7890
    /// 后被 SIGKILL；本次启动读到这个状态，必须生成关掉它的命令，
    /// 同时**不碰** Ethernet 上用户自己的公司代理。
    #[test]
    fn stale_entry_from_a_crash_is_targeted_and_users_proxy_is_left_alone() {
        let stale = ProxyState {
            enabled: true,
            server: "127.0.0.1".into(),
            port: 7890,
        };
        let users = ProxyState {
            enabled: true,
            server: "proxy.corp.example".into(),
            port: 8080,
        };
        let mut targets = Vec::new();
        for kind in ProxyKind::ALL {
            if is_ours(&stale, "127.0.0.1", 7890) {
                targets.push(("Wi-Fi".to_string(), kind));
            }
            if is_ours(&users, "127.0.0.1", 7890) {
                targets.push(("Ethernet".to_string(), kind));
            }
        }
        let cmds = joined(&macos_off_commands(&targets));
        assert_eq!(cmds.len(), 3, "只该关 Wi-Fi 的三套：{cmds:?}");
        assert!(cmds.iter().all(|c| c.contains("Wi-Fi")));
        assert!(
            !cmds.iter().any(|c| c.contains("Ethernet")),
            "用户自己的代理不能被关：{cmds:?}"
        );
        assert!(cmds.iter().all(|c| c.ends_with("off")));
    }
}

/// 真机验收：真跑一次 apply + revert，确认命令确实生效并恢复原状。
///
/// **默认不跑**（`#[ignore]`）—— 它改的是这台开发机的系统网络设置，
/// `cargo test` 不该有这种副作用。手动跑：
/// ```text
/// cargo test -p wsieve-app sysproxy::live -- --ignored --test-threads=1
/// ```
/// 不需要 sudo：`networksetup` 的 set 系子命令对当前登录用户可用（实测）。
///
/// 安全网是 `Restore` 这个 RAII 守卫：测试中途 panic 也会在 unwind 时
/// 把三套设置逐字还原（服务器、端口、启用位）。把恢复写在测试末尾一行
/// 的话，一次断言失败就会在开发机上留下一个指向死端口的系统代理。
#[cfg(test)]
mod live {
    use super::*;

    /// 记录进入测试前的原状，drop 时逐字还原。
    struct Restore {
        service: String,
        before: Vec<(ProxyKind, ProxyState)>,
    }

    impl Restore {
        fn snapshot(custody: &SysProxyCustody, service: &str) -> anyhow::Result<Self> {
            let mut before = Vec::new();
            for kind in ProxyKind::ALL {
                before.push((kind, custody.read_state(service, kind)?));
            }
            Ok(Self {
                service: service.to_string(),
                before,
            })
        }
    }

    impl Drop for Restore {
        fn drop(&mut self) {
            for (kind, st) in &self.before {
                // 先把服务器/端口写回（该命令会顺带置为启用），
                // 再按原状复位启用位。服务器为空说明从未配置过，只复位开关。
                if !st.server.is_empty() {
                    let _ = networksetup(&[
                        kind.setter().to_string(),
                        self.service.clone(),
                        st.server.clone(),
                        st.port.to_string(),
                    ]);
                }
                let _ = networksetup(&[
                    kind.state_setter().to_string(),
                    self.service.clone(),
                    if st.enabled { "on" } else { "off" }.to_string(),
                ]);
            }
        }
    }

    #[test]
    #[ignore = "改动本机系统网络设置，手动跑：--ignored --test-threads=1"]
    fn apply_then_revert_really_takes_effect_and_restores() {
        let services = SysProxyCustody::enumerate_services().unwrap();
        assert!(!services.is_empty(), "枚举不到网络服务");
        // 只动一个服务，把影响面压到最小。
        let service = services.last().unwrap().clone();

        // 用一个几乎不可能与真实代理撞上的端口，免得误判归属。
        const PORT: u16 = 59873;
        let custody =
            SysProxyCustody::new("127.0.0.1".into(), PORT, vec![service.clone()]).unwrap();

        let _restore = Restore::snapshot(&custody, &service).unwrap();

        custody.apply().unwrap();
        for kind in ProxyKind::ALL {
            let st = custody.read_state(&service, kind).unwrap();
            assert!(st.enabled, "{service} 的 {kind:?} 没被启用：{st:?}");
            assert_eq!(st.server, "127.0.0.1");
            assert_eq!(st.port, PORT, "{kind:?} 端口没写进去");
        }

        // clear_stale 走的正是 revert 那条路 —— 崩溃后下次启动清残留即此。
        custody.clear_stale().unwrap();
        for kind in ProxyKind::ALL {
            let st = custody.read_state(&service, kind).unwrap();
            assert!(!st.enabled, "{service} 的 {kind:?} 没被关掉：{st:?}");
        }

        // 幂等：再清一次不该报错。
        custody.clear_stale().unwrap();
    }
}
