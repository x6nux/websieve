//! 配置模型。字段名与设计文档 §5.2 的 YAML schema 一一对应，
//! 命名沿用 Clash 的 kebab-case —— 用户可以直接照抄现成配置。
//!
//! 关于 `Serialize`：这些派生**不用于写回 YAML**（写回走 `edit.rs` 的行级
//! 定点改写，见 §5.6）。它们服务于把配置送给 UI 层的 JSON 序列化 ——
//! 那条路上没有注释可丢。
//!
//! 「不用于写回 YAML」现在是编译期事实而非约定：workspace 的 `serde-saphyr`
//! 关掉了 `serialize` feature，本 crate 派生的 `Serialize` 找不到 YAML
//! 序列化器可用，只能喂给 serde_json 之类。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_saphyr::Spanned;

/// 顶层配置。
///
/// `deny_unknown_fields` 是刻意的：没有它，把 `mixed-port` 手误写成
/// `mixed_port` 会**静默**退回默认值 25500 —— 文件上白纸黑字写着 9999，
/// 端口却没变，而全程没有任何一条诊断。手写 YAML 最常见的错误恰恰是
/// 键名拼错，若连它都不报，本 crate 花力气做行号诊断就失去了意义。
///
/// 注意它与 `default` 并不冲突：`default` 管的是「键**没出现**时取什么值」，
/// 这里管的是「出现了一个我不认识的键」。两者一个都不能少。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct Config {
    // ── 入口 ──
    pub mixed_port: u16,
    pub bind_address: String,
    pub allow_lan: bool,
    pub mode: String,
    /// `mode == "rule"` 时具体用哪份规则：`"custom"` 用下面 `rules` 里
    /// 用户手写的那份，`"china"` 用内置的中国大陆预设（见
    /// `wsieve_route::CHINA_PRESET_RULES`），**完全无视 `rules` 数组**。
    /// 与 `mode` 正交：`mode` 是路由引擎自己理解的三态语义
    /// （`wsieve_route::Mode`），这个字段只决定 `Mode::Rule` 时的规则来源，
    /// 引擎本身不需要知道它的存在。
    pub rule_preset: String,
    pub global_outbound: String,
    pub log_level: String,
    pub system_proxy: bool,

    // ── 出站 ──
    pub proxies: Vec<Proxy>,
    pub proxy_groups: Vec<ProxyGroup>,

    // ── 规则 ──
    /// 带行号：定点改写靠它定位。业务侧读值用 `.value`。
    pub rules: Vec<Spanned<String>>,

    // ── 其余 ──
    /// 静态解析表：域名 → IP 字面量。Clash 的 `hosts` 同名同义。
    ///
    /// 只做**精确匹配**，不支持 `*.example.com` 这类通配——通配要定义
    /// 「多个模式同时命中时谁赢」，而那套优先级规则一旦写下就得永远兼容。
    /// 没有需求之前不引入。
    ///
    /// 用 `BTreeMap` 而非 `HashMap`：送给 UI 的 JSON 与日志里的顺序要稳定，
    /// 否则每次序列化条目顺序都不一样，diff 全是噪声。
    #[serde(default)]
    pub hosts: BTreeMap<String, String>,
    pub dns: Dns,
    pub tun: Tun,
    pub geo_auto_update: bool,
    pub geo_update_interval: u32,
    pub geox_url: GeoxUrl,

    // ── websieve 专有 ──
    pub carrier: String,
    pub carrier_host: String,
    pub shard_base_port: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mixed_port: 25500,
            bind_address: "127.0.0.1".into(),
            allow_lan: false,
            mode: "rule".into(),
            rule_preset: "custom".into(),
            global_outbound: String::new(),
            log_level: "info".into(),
            system_proxy: false,
            proxies: Vec::new(),
            proxy_groups: Vec::new(),
            rules: Vec::new(),
            hosts: BTreeMap::new(),
            dns: Dns::default(),
            tun: Tun::default(),
            geo_auto_update: true,
            geo_update_interval: 24,
            geox_url: GeoxUrl::default(),
            carrier: "shared".into(),
            carrier_host: String::new(),
            shard_base_port: 18443,
        }
    }
}

/// `PartialEq`：运行时要回答「这次保存到底动没动出站段」。逐字段比较而不是
/// 只比几个「看起来重要的」字段——`url` 改了同样要重连，而它在
/// `OutboundCfg` 里根本没有对应项，只比那边的字段会把它整个漏掉，表现为
/// 「改了地址保存成功，流量却还发去旧服务器，且没有任何提示」。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Proxy {
    pub name: String,
    /// 只接受 "websieve"。其他类型（ss / vmess / trojan…）在校验期
    /// 明确报错而非静默忽略 —— 用户粘贴整份 Clash 配置时要知道为什么不生效。
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    pub server_pub: String,
    pub client_priv: String,
    #[serde(default = "default_extra_sessions")]
    pub extra_sessions: usize,
    #[serde(default = "default_mux_prefs")]
    pub mux_prefs: Vec<u8>,
    /// 服务端解析**域名**目标时用哪个地址族：
    /// `auto` / `v4-only` / `v6-only` / `prefer-v4`。
    ///
    /// 逐出站而非全局：一台服务器的 IPv6 出口被目标站点墙掉时，只该改这一台
    /// 的行为。这里存字符串、由 `build_startup_plan` 转成
    /// `wsieve_proto::hello::IpStrategy`——与 `mux_prefs` 存 `Vec<u8>` 同理，
    /// 配置层不依赖协议层的类型。合法写法在 `Config::validate` 里 `check_enum`
    /// 复核一遍，两处列表必须与 `IpStrategy::parse` 的分支逐字对齐。
    #[serde(default = "default_ip_strategy")]
    pub ip_strategy: String,
}

fn default_extra_sessions() -> usize {
    3
}

/// 默认 `auto` = 交给服务端的系统解析器按 RFC 6724 选，即加这个字段之前的行为。
fn default_ip_strategy() -> String {
    "auto".into()
}

/// 这里的数字是 **`MuxId` 的线上标识**（`wsieve_proto::hello::MuxId`），
/// 不是「第几个」。三方 mux 全部换成自研 wsmux 之后，合法取值只剩
/// `Wsmux = 0x01` 一个，因此这份默认值也只能是 `[1]`。
///
/// 这个列表里**绝不能出现非法值**：`ProxyForm` 添加服务器时从不写
/// `mux-prefs`，于是每个从界面新建的出站都落到这个默认值上，下次启动
/// `build_startup_plan` 转换失败、进程 `exit(2)`——用户只看到「加完服务器
/// 后应用打不开了」。这个洞踩过两次：先是写成 `[0, 1, 2, 3, 4]`（0-based
/// 序号，而 `0` 不是合法 `MuxId`），后是 mux 收敛成一种时漏改了这里，留下
/// `[2, 1, 3, 4, 5]` 里的 2/3/4/5 四个已经不存在的 id。
/// 守卫是 src-tauri 的 `the_crate_default_mux_prefs_are_all_valid_mux_ids`
/// ——它是唯一同时看得到两个 crate 的地方。
///
/// 顺序 = 偏好顺序，与 `bootstrap::load_cfg` 里 `WSIEVE_MUX_PREFS` 缺省时
/// 那份 `[DEFAULT_MUX]` 逐项对齐（`DEFAULT_MUX` 就是 `Wsmux`，见
/// `wsieve_xhttp`）——两条默认路径给出不同的首选 mux，会让「同一份服务端，
/// 从配置起和从 env 起协商出的复用器不一样」。
fn default_mux_prefs() -> Vec<u8> {
    vec![1]
}

/// 代理组（设计文档「代理组与首页视图」§1）。
///
/// 只用 `name` 做唯一标识，不单独设 `id`——`selected` 字段写在组自己的
/// YAML 块里，不是外部按键索引的状态，改名不会打断任何引用（见设计文档
/// §1「只用 name」一节的完整论证）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ProxyGroup {
    pub name: String,
    pub kind: String,
    pub proxies: Vec<String>,
    /// 仅 `select` 类型使用；其余类型省略时为空字符串。
    #[serde(default)]
    pub selected: String,
    /// 仅 `load-balance` 类型使用；其余类型省略时为空字符串。
    #[serde(default)]
    pub strategy: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct Dns {
    pub enable: bool,
    pub listen: String,
    pub enhanced_mode: String,
    pub fake_ip_range: String,
    pub fake_ip_filter: Vec<String>,
    pub nameserver: Vec<String>,
    pub proxy_server_nameserver: Vec<String>,
    pub timeout_ms: u64,
    pub cache: DnsCache,
}

impl Default for Dns {
    fn default() -> Self {
        Self {
            enable: true,
            listen: String::new(),
            enhanced_mode: "fake-ip".into(),
            fake_ip_range: "198.18.0.0/15".into(),
            fake_ip_filter: Vec::new(),
            nameserver: vec!["https://1.1.1.1/dns-query".into()],
            proxy_server_nameserver: vec!["system".into()],
            timeout_ms: 2000,
            cache: DnsCache::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct DnsCache {
    pub max: usize,
    pub negative_ttl_s: u64,
}

impl Default for DnsCache {
    fn default() -> Self {
        Self { max: 4096, negative_ttl_s: 30 }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct Tun {
    pub enable: bool,
    pub stack: String,
    pub auto_route: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct GeoxUrl {
    pub geoip: String,
    pub geosite: String,
}
