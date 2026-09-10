//! WebView 承载器（设计文档 §4.2 纪律③、§9.1）。
//!
//! 纪律③：**承载方式对出站层透明**。出站只声明「我要一个 transport」，由
//! 本模块决定它落在哪个 WebView。`CarrierPlan` 是
//! 纯数据决策（因此可穷举单测）；实际建窗（`spawn_carrier_windows`）也放
//! 在本模块——因为只有它知道 `CarrierPlan` 的内部结构，把"决定建什么"和
//! "怎么建"拆到两个模块没有实际收益，代价是 Task 4 的运行时重建要跨模块
//! 拼这两半。这一半（真的调 Tauri 建窗）不追求单测覆盖，靠集成/真机走查。
//!
//! 两种模式：
//!
//! - **`shared`（默认）**：一个 WebView 加载本机承载页（`crate::carrier_page`），
//!   全部出站挂在上面，各自用**绝对 URL** 发请求。承载页是本机的 http 壳，
//!   与任何出站都不同源，因此不存在「谁能吃相对路径」的区分。
//!   跨域名可行已实测确证，见
//!   `docs/superpowers/spikes/2026-08-25-cross-origin-carrier-spike.md`。
//! - **`isolated`**：每出站一个隐藏窗口，各自加载**同一张**本机承载页，
//!   与 `shared` 的差别收敛到只剩「建几个窗口」。换来故障隔离，代价是内存
//!   （实测首个约 128 MB、其后每个约 27 MB —— 多个 WKWebView 共用
//!   WebContent 进程池，**不是**旧说法里的 N×）。
//!
//! **故障隔离**：`shared` 的全部风险都在「出站 A 挂掉会不会连累出站 B」。
//! 答案由两条结构性保证给出，二者都有测试：
//!
//! 1. 会话循环**绝不** `core.mark_dead()`、**绝不** reload 页面（见
//!    `instance.rs`，测试 `stopping_instance_exits_the_loop_without_touching_core`
//!    与 `one_outbound_failing_does_not_kill_its_neighbour`）。core 的生死归管理器。
//! 2. 宿主出站下线**不影响**其他出站：页面已加载完毕，emitter 在页面上下文里
//!    继续跑，各出站的数据面基址是条带给出的各自独立的绝对 URL，互不依赖
//!    ——「宿主」在这一层只剩语义归属，不影响任何请求的落点。

use std::collections::BTreeMap;

/// 承载模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarrierMode {
    /// 全部出站共用一个 WebView。
    Shared,
    /// 每出站一个独立隐藏窗口。
    Isolated,
}

impl CarrierMode {
    /// 解析配置里的 `carrier` 字段。
    ///
    /// 无法识别的取值**报错而非默认**：静默按 shared 处理，等于用户以为自己
    /// 开了故障隔离、实际没开，而全程没有一条诊断。
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s.trim() {
            "shared" => Ok(Self::Shared),
            "isolated" => Ok(Self::Isolated),
            other => anyhow::bail!("未知的 carrier 取值 {other:?}（只接受 shared / isolated）"),
        }
    }
}

/// 单个出站在承载计划里的位置。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Slot {
    /// 承载它的窗口标签。
    window: String,
    /// 该窗口应加载的页面 URL。
    page_url: String,
}

/// 承载计划：谁住哪个窗口。
#[derive(Debug, Clone)]
pub struct CarrierPlan {
    mode: CarrierMode,
    /// 宿主出站名。`isolated` 模式下无意义，取第一个。
    host: String,
    /// 出站名 → 位置。BTreeMap 让 `windows()` 的顺序稳定可测。
    slots: BTreeMap<String, Slot>,
}

/// `shared` 模式下承载全部出站的那一个窗口。它同时也是 `main.rs` 里建的
/// 主窗口标签 —— 承载页面就是主窗口加载的伪装页。
pub const SHARED_WINDOW: &str = "main";

/// `isolated` 模式下每出站独占窗口的标签前缀。
///
/// `src-tauri/capabilities/transport.json` 的 `windows` 里必须同时有
/// `wsieve-transport-*` 这条 glob，否则 `isolated` 下新窗口里的 emitter
/// 一 invoke 就被 ACL 拒掉，表现为「出站永远握不上手」。
/// 已实测 `glob::Pattern("wsieve-transport-*")` 能匹配含中文的标签。
///
/// 反过来这条 glob 也匹配不到控制窗口的 `control` 标签 —— 传输侧与控制侧
/// 的权限面分家（capability 交集为空）正是靠这个前缀不重叠守住的。
pub const ISOLATED_WINDOW_PREFIX: &str = "wsieve-transport-";

impl CarrierPlan {
    /// `shared`：一个 WebView 加载承载页，全部出站挂在上面。
    ///
    /// `host` 传空串表示「未指定」，取第一个启用的出站。**宿主这个概念在
    /// 基址职责搬走之后只剩语义归属与错误信息**——它不再影响任何一个出站的
    /// 请求发往何处（那些全部由条带给出的绝对 URL 决定）。
    pub fn shared(host: &str, outbounds: &[&str], page_url: &str) -> anyhow::Result<Self> {
        let entries = validate(outbounds)?;
        let host = resolve_host(host, &entries)?;
        let mut slots = BTreeMap::new();
        for name in &entries {
            slots.insert(
                name.clone(),
                Slot {
                    window: SHARED_WINDOW.to_string(),
                    page_url: page_url.to_string(),
                },
            );
        }
        Ok(Self {
            mode: CarrierMode::Shared,
            host,
            slots,
        })
    }

    /// `isolated`：每出站一个隐藏窗口。
    ///
    /// 它们加载的是**同一个**本机承载 server 的同一张 HTML —— 与 `shared`
    /// 的差别收敛到只剩「建几个窗口」。
    pub fn isolated(outbounds: &[&str], page_url: &str) -> anyhow::Result<Self> {
        let entries = validate(outbounds)?;
        let host = entries[0].clone();
        let mut slots = BTreeMap::new();
        for name in &entries {
            slots.insert(
                name.clone(),
                Slot {
                    window: format!("{ISOLATED_WINDOW_PREFIX}{name}"),
                    page_url: page_url.to_string(),
                },
            );
        }
        Ok(Self {
            mode: CarrierMode::Isolated,
            host,
            slots,
        })
    }

    /// 按模式构造。
    pub fn build(
        mode: CarrierMode,
        host: &str,
        outbounds: &[&str],
        page_url: &str,
    ) -> anyhow::Result<Self> {
        match mode {
            CarrierMode::Shared => Self::shared(host, outbounds, page_url),
            CarrierMode::Isolated => Self::isolated(outbounds, page_url),
        }
    }

    pub fn mode(&self) -> CarrierMode {
        self.mode
    }

    /// 宿主出站名。
    pub fn host_name(&self) -> &str {
        &self.host
    }

    /// 该出站落在哪个窗口。未知出站返回 `None`。
    ///
    /// **`None` 必须当错误处理**：认不出这个出站，说明承载计划与出站表不
    /// 同步，悄悄放过去就等于让一个没有窗口承载的出站去发请求——与 §6.4
    /// 「绝不给一个默认值把流量发去别处」同源。这条防线原本立在 `base_for`
    /// 上，数据面基址的职责搬去条带之后挪到了这里。
    pub fn window_label(&self, name: &str) -> Option<String> {
        self.slots.get(name).map(|s| s.window.clone())
    }

    /// 该出站的承载窗口应加载哪个页面。未知出站返回 `None`。
    ///
    /// 逐项 `allow(dead_code)`：`windows()` 已经覆盖了建窗这条主路径，
    /// 本方法服务于「某个出站到底挂在哪张页面上」这类诊断与阶段 4 的
    /// 出站详情视图。逐项标而非整模块开 —— 整模块 allow 会连真正的
    /// 死代码一起盖住。
    #[allow(dead_code)]
    pub fn page_url_for(&self, name: &str) -> Option<String> {
        self.slots.get(name).map(|s| s.page_url.clone())
    }

    /// 需要建的窗口：`(标签, 页面 URL)`，已去重。
    /// `shared` 恒为 1 个，`isolated` 为出站数个。
    pub fn windows(&self) -> Vec<(String, String)> {
        let mut seen = BTreeMap::new();
        for s in self.slots.values() {
            seen.entry(s.window.clone())
                .or_insert_with(|| s.page_url.clone());
        }
        seen.into_iter().collect()
    }

    /// 计划内的出站名（字典序）。见 `page_url_for` 上关于 allow 的说明。
    #[allow(dead_code)]
    pub fn outbounds(&self) -> Vec<String> {
        self.slots.keys().cloned().collect()
    }
}

/// 按 `carrier.windows()` 给出的每一个 `(标签, URL)` 建一个隐藏（或按
/// `show_window` 显示）的承载 WebView。
///
/// 可以在 `.setup()` 里调（进程启动时），也可以在启动之后调（运行时
/// 新增出站需要一个之前没有过的窗口时）——两处用的是同一份逻辑，参数
/// 完全一致，不允许出现"启动时建的窗口"和"运行时建的窗口"配置不一致
/// 这种分叉。
///
/// **幂等**：若某个标签对应的窗口已经存在，跳过，不重复建（不返回错误——
/// 调用方在"增量出站更新"场景下，`carrier.windows()` 里混着新旧标签是
/// 正常情况，不该因为窗口已存在就整体失败）。
///
/// **check-then-build 不是原子的**：`get_webview_window` 判存在与
/// `.build()` 之间没有锁保护。今天唯一的调用点（`.setup()`）是单线程
/// 启动期，不构成竞态；Part 1 计划里的运行时重建（配置保存触发）会是
/// 第二个调用点——**调用方必须保证同一时刻只有一次这个函数在跑**（比如
/// 靠一把序列化重建流程的锁），否则两次并发调用可能都判定"标签不存在"，
/// 都尝试建同一个窗口，后一个 `.build()` 会因为标签重复而报错。
pub fn spawn_carrier_windows(
    app: &tauri::AppHandle,
    plan: &CarrierPlan,
    show_window: bool,
) -> anyhow::Result<()> {
    use tauri::Manager;
    for (label, url) in plan.windows() {
        if app.get_webview_window(&label).is_some() {
            continue;
        }
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
        .background_throttling(tauri_utils::config::BackgroundThrottlingPolicy::Disabled)
        .initialization_script(crate::bootstrap::loader_js())
        .build()?;
    }
    Ok(())
}

/// 校验出站表：非空、无重名、名字非空。
///
/// **URL 校验已随基址职责一起搬去条带**（`shard_setup` 的 `split_url` /
/// `origin_of`）。承载计划不再从出站 URL 推导任何东西，它只认名字——在这里
/// 再校验一次，只会让同一个坏 URL 在两处各报一次，而修的人只想得到其中一处。
fn validate(outbounds: &[&str]) -> anyhow::Result<Vec<String>> {
    if outbounds.is_empty() {
        anyhow::bail!("承载计划至少需要一个启用的出站");
    }
    let mut seen = BTreeMap::<&str, usize>::new();
    let mut out = Vec::with_capacity(outbounds.len());
    for (i, name) in outbounds.iter().enumerate() {
        if name.trim().is_empty() {
            anyhow::bail!("第 {} 个出站的名字为空", i + 1);
        }
        if let Some(prev) = seen.insert(name, i) {
            anyhow::bail!(
                "出站名重复：{name:?} 同时出现在第 {} 和第 {} 个",
                prev + 1,
                i + 1
            );
        }
        out.push(name.to_string());
    }
    Ok(out)
}

/// 宿主选择：空串取第一个启用的出站；指定了就必须存在。
fn resolve_host(host: &str, entries: &[String]) -> anyhow::Result<String> {
    let host = host.trim();
    if host.is_empty() {
        return Ok(entries[0].clone());
    }
    if entries.iter().any(|n| n == host) {
        Ok(host.to_string())
    } else {
        // 报错而非退回第一个：用户点名要拿某个节点当宿主是个明确意图，
        // 悄悄换一个等于把这个判断作废。
        anyhow::bail!(
            "carrier-host 指向不存在的出站：{host}（可选：{}）",
            entries.join("、")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试专用的承载页地址——本模块不关心承载页 server 本身怎么起来
    /// （那是 `crate::carrier_page` 的职责），只关心这个字符串被原样传递。
    const PAGE: &str = "http://127.0.0.1:53119/";

    #[test]
    fn host_defaults_to_first_enabled_when_unspecified() {
        let c = CarrierPlan::shared("", &["A", "B"], PAGE).unwrap();
        assert_eq!(c.host_name(), "A");
    }

    #[test]
    fn unknown_host_name_is_an_error() {
        let e = CarrierPlan::shared("幽灵", &["A"], PAGE)
            .unwrap_err()
            .to_string();
        assert!(e.contains("幽灵"), "{e}");
    }

    #[test]
    fn isolated_carrier_gives_every_outbound_its_own_window() {
        let c = CarrierPlan::isolated(&["A", "B"], PAGE).unwrap();
        assert_eq!(c.window_label("A"), Some("wsieve-transport-A".to_string()));
        assert_eq!(c.window_label("B"), Some("wsieve-transport-B".to_string()));
        // 承载页全局唯一——两个窗口的窗口标签不同，但加载的是同一张页面。
        assert_eq!(c.page_url_for("A").as_deref(), Some(PAGE));
        assert_eq!(c.page_url_for("B").as_deref(), Some(PAGE));
    }

    #[test]
    fn empty_outbound_list_is_an_error() {
        assert!(CarrierPlan::shared("", &[], PAGE).is_err());
        assert!(CarrierPlan::isolated(&[], PAGE).is_err());
    }

    #[test]
    fn shared_builds_exactly_one_window_isolated_builds_n() {
        let obs = ["A", "B", "C"];
        let s = CarrierPlan::shared("B", &obs, PAGE).unwrap();
        assert_eq!(
            s.windows(),
            vec![("main".to_string(), PAGE.to_string())],
            "shared 只建一个窗口，加载的是承载页"
        );
        let i = CarrierPlan::isolated(&obs, PAGE).unwrap();
        assert_eq!(i.windows().len(), 3);
    }

    #[test]
    fn unknown_outbound_yields_none_never_a_default_window() {
        // §6.4：认不出的出站必须让调用方拿到 None 去拒绝。这条防线原本
        // 立在 base_for 上，基址职责搬走之后由 window_label 承担。
        let c = CarrierPlan::shared("A", &["A"], PAGE).unwrap();
        assert_eq!(c.window_label("不存在的节点"), None);
        assert_eq!(c.page_url_for("不存在的节点"), None);
    }

    #[test]
    fn duplicate_outbound_names_are_an_error() {
        // 重名在 isolated 下会撞窗口标签（Tauri 建第二个同标签窗口直接失败），
        // 在 shared 下会让 window_label 变成「看谁后写入」。必须早报。
        let e = CarrierPlan::shared("", &["A", "A"], PAGE)
            .unwrap_err()
            .to_string();
        assert!(e.contains("重复"), "{e}");
        assert!(CarrierPlan::isolated(&["A", "A"], PAGE).is_err());
    }

    #[test]
    fn empty_outbound_name_is_an_error() {
        assert!(CarrierPlan::shared("", &[""], PAGE).is_err());
        assert!(CarrierPlan::isolated(&["  "], PAGE).is_err());
    }

    #[test]
    fn every_window_loads_the_one_carrier_page() {
        // 承载页全局唯一。两种模式的差别收敛到只剩「建几个窗口」。
        const PAGE: &str = "http://127.0.0.1:53119/";
        let s = CarrierPlan::shared("B", &["A", "B", "C"], PAGE).unwrap();
        assert_eq!(s.windows(), vec![("main".to_string(), PAGE.to_string())]);

        let i = CarrierPlan::isolated(&["A", "B", "C"], PAGE).unwrap();
        assert_eq!(i.windows().len(), 3);
        for (_, url) in i.windows() {
            assert_eq!(url, PAGE, "isolated 的每个窗口也加载同一张承载页");
        }
    }

    #[test]
    fn carrier_plan_no_longer_cares_about_server_urls() {
        // 出站 URL 不再进承载计划——它推导数据面基址的职责已经交给条带。
        // 这条钉死「别把 URL 校验又加回来」：那会让一个坏 URL 在两个地方
        // 各报一次，而修的时候只会想到其中一个。
        let c = CarrierPlan::shared("", &["只有名字"], "http://127.0.0.1:1/").unwrap();
        assert_eq!(c.window_label("只有名字").as_deref(), Some("main"));
    }

    #[test]
    fn carrier_mode_rejects_unknown_values_instead_of_defaulting() {
        assert_eq!(CarrierMode::parse("shared").unwrap(), CarrierMode::Shared);
        assert_eq!(CarrierMode::parse(" isolated ").unwrap(), CarrierMode::Isolated);
        let e = CarrierMode::parse("Shared").unwrap_err().to_string();
        assert!(e.contains("Shared"), "{e}");
        // 静默按 shared 处理 = 用户以为开了故障隔离、实际没开
        assert!(CarrierMode::parse("").is_err());
        assert!(CarrierMode::parse("isolate").is_err());
    }

    #[test]
    fn build_dispatches_on_mode() {
        let obs = ["A", "B"];
        let s = CarrierPlan::build(CarrierMode::Shared, "B", &obs, PAGE).unwrap();
        assert_eq!(s.host_name(), "B");
        assert_eq!(s.window_label("A"), Some(SHARED_WINDOW.to_string()));
        let i = CarrierPlan::build(CarrierMode::Isolated, "B", &obs, PAGE).unwrap();
        assert_eq!(i.window_label("A"), Some("wsieve-transport-A".to_string()));
        assert_eq!(i.outbounds(), vec!["A".to_string(), "B".to_string()]);
    }

    #[test]
    fn isolated_window_labels_are_actually_granted_by_the_capability_file() {
        // capabilities/transport.json 用 glob 授权窗口。isolated 建的窗口若不在
        // 授权表里，窗口里的 emitter 一 invoke 就被 ACL 拒掉，表现为「这个出站
        // 永远握不上手」—— 而且只在真跑起来时才暴露。
        //
        // 这里读**真实的**能力文件而不是硬编码一份，否则哪天有人把那行删了，
        // 测试照样绿。
        let caps = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/capabilities/transport.json"
        ))
        .expect("读不到 capabilities/transport.json");
        let caps: serde_json::Value = serde_json::from_str(&caps).unwrap();
        let patterns: Vec<glob::Pattern> = caps["windows"]
            .as_array()
            .expect("capabilities 必须有 windows 数组")
            .iter()
            .map(|v| glob::Pattern::new(v.as_str().unwrap()).unwrap())
            .collect();

        // 含中文的出站名也必须被覆盖到。
        let c = CarrierPlan::isolated(&["日本节点", "B"], PAGE).unwrap();
        for (label, _) in c.windows() {
            assert!(
                patterns.iter().any(|p| p.matches(&label)),
                "isolated 的窗口 {label} 没有被 capabilities/transport.json 授权"
            );
        }
        // shared 用的主窗口同样要在表里。
        let s = CarrierPlan::shared("", &["A"], PAGE).unwrap();
        for (label, _) in s.windows() {
            assert!(
                patterns.iter().any(|p| p.matches(&label)),
                "shared 的窗口 {label} 没有被授权"
            );
        }
    }

    /// 从 eval 出去的 JS 里取回 request id。emitter 侧就是拿这个 id 回填的。
    fn eval_request_id(js: &str) -> Option<u64> {
        let (_, after) = js
            .split_once(".post(")
            .or_else(|| js.split_once(".openStream("))?;
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse().ok()
    }

    #[tokio::test]
    async fn one_outbound_failing_does_not_kill_its_neighbour_on_a_shared_carrier() {
        // shared 承载的**全部风险**都在这一条上：出站 A 的服务端挂了，
        // 与它同住一个 WebView 的出站 B 还能不能继续发请求。
        //
        // 失败注入走的是真实路径：A 的 eval 把每个请求以「fetch 失败」回填，
        // 正是 emitter 在服务器不可达时做的事（main.rs 的 kind=2 帧 →
        // complete_post(id, Err)）。没有 mock，A 走的就是生产代码。
        use crate::bridge::{TransportCore, WebViewTransport};
        use crate::outbound::instance::{OutboundCfg, OutboundInstance, SessionEnv, Status};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        use wsieve_proto::hello::MuxId;
        use wsieve_transport::{HttpTransport, PostReply};

        // 一个 core 对应一个 WebView，两个出站共用 —— 这正是 shared 的定义。
        // session_bases 不再从 CarrierPlan 取——那是条带的职责，此处直接
        // 写死（这正是改动后条带会给出的形态），构造 CarrierPlan 本身对
        // 本测试已经没有意义。
        let core = Arc::new(TransportCore::new());

        // ── 出站 A（宿主）：服务端是死的 ──
        let a_evals: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let a_fails = Arc::new(AtomicUsize::new(0));
        let a_core = core.clone();
        let seen = a_evals.clone();
        let fails = a_fails.clone();
        let a_env = SessionEnv {
            eval: Arc::new(move |_name: &str, js: String| {
                seen.lock().unwrap().push(js.clone());
                if let Some(id) = eval_request_id(&js) {
                    let c = a_core.clone();
                    tokio::spawn(async move {
                        c.complete_post(id, Err(anyhow::anyhow!("fetch failed"))).await;
                    });
                }
            }),
            on_status: Arc::new(move |_, s: &Status| {
                if matches!(s, Status::Failed { .. }) {
                    fails.fetch_add(1, Ordering::Relaxed);
                }
            }),
        };

        let a = OutboundInstance::new(OutboundCfg {
            name: "宿主".into(),
            server_pub: [7u8; 32],
            client_priv: [9u8; 32],
            mux_prefs: vec![MuxId::Wsmux],
            session_bases: vec![Some("https://host.example:18443".to_string())],
            ip_strategy: wsieve_proto::hello::IpStrategy::Auto,
        });
        let a2 = a.clone();
        let core2 = core.clone();
        let a_loop = tokio::spawn(async move { a2.run(core2, a_env).await });

        // 等 A 第一次失败。
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while a_fails.load(Ordering::Relaxed) < 1 {
            assert!(tokio::time::Instant::now() < deadline, "A 应当失败");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // ── 断言一：A 的失败没有把共享的 core 标死 ──
        // 标死会让 B 的每一个 pending 请求当场失败（bridge 的 mark_dead），
        // 也就是「A 挂掉顺手掐死 B」。这是 shared 承载唯一真正的风险。
        // 反证已验：在 A 的失败分支里加回原 proxy.rs 的无条件
        // `core.mark_dead()`，本断言立即失败。
        assert!(!core.is_dead(), "出站 A 的故障绝不能标死共享 core");

        // 再等 A 反复重试若干轮（退避 100ms→200ms→400ms），
        // 证明它是在原地重新握手，而不是退出了循环。
        while a_fails.load(Ordering::Relaxed) < 3 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "A 应当反复失败重试，而不是退出循环"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!core.is_dead(), "重试多轮之后 core 依然必须是活的");
        // ── 断言二：A 没有 reload 页面 ──
        // reload 会把 B 的 emitter 一起冲掉（§9.4 优化①）。
        let a_js = a_evals.lock().unwrap().clone();
        assert!(!a_js.is_empty(), "A 应当真的发过请求");
        for js in &a_js {
            assert!(
                !js.contains("location.reload"),
                "会话死亡不该 reload 页面：{js}"
            );
        }
        // ── 断言三：A 自己拿不到 dialer（§6.4 拒绝而非回退）──
        assert!(a.dialer().is_none());

        // ── 断言四：B 在同一个 core 上仍然发得出请求、收得到应答 ──
        // 这是「A 挂了 B 还活着」的数据面证据，不是结构推断。
        let b_base = "https://peer.example".to_string();
        let b_core = core.clone();
        let b_seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let bs = b_seen.clone();
        let b_transport = WebViewTransport::with_base(
            Box::new(move |js: String| {
                bs.lock().unwrap().push(js.clone());
                if let Some(id) = eval_request_id(&js) {
                    let c = b_core.clone();
                    tokio::spawn(async move {
                        c.complete_post(
                            id,
                            Ok(PostReply {
                                status: 200,
                                body: bytes::Bytes::from_static(b"ok"),
                            }),
                        )
                        .await;
                    });
                }
            }),
            b_base.clone(),
        );
        b_transport.set_core(core.clone());
        let reply = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            b_transport.post("/api/sync?n=0", bytes::Bytes::from_static(b"x")),
        )
        .await
        .expect("B 的请求不该挂起")
        .expect("A 挂掉之后 B 必须还能发请求");
        assert_eq!(reply.status, 200);
        assert_eq!(&reply.body[..], b"ok");
        // B 的请求确实打在自己的 origin 上，没有被 A 的域名污染。
        let b_js = b_seen.lock().unwrap().clone();
        assert!(
            b_js.iter().any(|js| js.contains("https://peer.example/api/sync")),
            "B 应当用自己的绝对 URL 发请求：{b_js:?}"
        );
        assert!(
            !b_js.iter().any(|js| js.contains("host.example")),
            "B 的请求里不该出现宿主域名：{b_js:?}"
        );

        // 收尾：叫停 A。
        a.request_stop();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), a_loop).await;
    }
}
