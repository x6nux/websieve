//! 运行时状态快照（设计文档「运行时接入 config.yaml」§1）。
//!
//! 一次配置保存对应一份完整快照，整体构建、整体替换——不给
//! `RuleSet`/`OutboundManager`/`Router` 各开一把锁分别热替换，是因为
//! `Router`/`OutboundManager` 各自维护一份出站表，必须来自同一批
//! `Arc<OutboundInstance>` 才不会错位；分开加锁会有"两把锁各自被刷新，
//! 中间那一刻两者不一致"的窗口，合成一个快照结构体从根上排除这个可能。

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::outbound::instance::OutboundInstance;
use crate::outbound::OutboundManager;
use crate::router::Router;

#[allow(dead_code)] // Task 4 才会真正构造并托管进 Tauri 状态
pub struct RuntimeState {
    pub rule_set: Arc<wsieve_route::RuleSet>,
    pub outbound_manager: Arc<OutboundManager>,
    pub router: Arc<Router>,
    // GroupTable 是 Part 3（代理组接入）的产出，本计划不实现，先占位成
    // 一个空结构体，避免 Part 3 落地时要改这里的字段名/调用点。
    pub groups: Arc<GroupTable>,
}

/// 代理组表——本计划只放占位结构，真正的构建逻辑属于 Part 3。
#[allow(dead_code)] // 同上：Part 3 才会真正读写它
#[derive(Default)]
pub struct GroupTable;

/// 一次配置更新里，出站集合要如何从旧的过渡到新的。
///
/// 用 `Arc<OutboundInstance>` 而非 `OutboundCfg` 表示"复用"，是因为复用
/// 的重点就是**不重新构造实例**——上层拿到这个结构后，`reused` 里的每一项
/// 直接原样放进新 `OutboundManager`，`added` 里的每一项才需要真的
/// `OutboundInstance::new(cfg)`。
#[allow(dead_code)] // Task 4 才会真正调用 diff_outbounds 并消费这个结构
pub struct OutboundDiff {
    /// 名字与关键字段都未变的出站：原样复用的 `Arc`。
    pub reused: BTreeMap<String, Arc<OutboundInstance>>,
    /// 需要新建的出站配置（新增的 + 字段被改动、按"先停旧的再当新增"处理的）。
    pub added: Vec<crate::outbound::instance::OutboundCfg>,
    /// 需要停止并丢弃的旧出站名（删除的 + 字段被改动的旧版本）。
    pub removed: Vec<String>,
}

/// 纯函数：给定旧的出站实例表与新的 `Proxy` 列表，算出增量。
///
/// "未变"的判定标准是 `OutboundCfg` 的全部字段相等（`name`/`server_pub`/
/// `client_priv`/`mux_prefs`/`session_bases`）——`session_bases` 由承载
/// 计划算出、不来自 `Proxy` 本身，所以调用方要在算出新的 `session_bases`
/// 之后才能调这个函数；本函数只管"给定两份完整 `OutboundCfg`，谁跟谁一样"，
/// 不负责计算 `session_bases`。
///
/// **前提：`new_cfgs` 内 `name` 唯一**——调用方不必在这里再查一遍重名，
/// 因为 `wsieve_config::Config::validate()` 在写盘前已经拒绝了重名的
/// `proxies`（`ConfigError::DuplicateProxyName`），走到这个函数时的输入
/// 必然已经去重过。若未来某个调用方绕过了 `validate()` 直接喂重名列表
/// 进来，行为未定义（哪一条会被当成"这个名字对应的配置"取决于遍历顺序），
/// 这不是本函数要防的边界。
#[allow(dead_code)] // Task 4 才会把它接进 rebuild_and_swap，现在只有测试在调
pub fn diff_outbounds(
    old: &BTreeMap<String, Arc<OutboundInstance>>,
    new_cfgs: &[crate::outbound::instance::OutboundCfg],
) -> OutboundDiff {
    let mut reused = BTreeMap::new();
    let mut added = Vec::new();
    let mut seen_names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for c in new_cfgs {
        seen_names.insert(c.name.clone());
        match old.get(&c.name) {
            Some(inst) if inst.cfg() == c => {
                reused.insert(c.name.clone(), inst.clone());
            }
            _ => added.push(c.clone()),
        }
    }

    let removed = old
        .keys()
        .filter(|name| {
            // 不在新列表里 → 真删除；在新列表里但没进 reused → 字段变了，
            // 旧的这份也要走停机流程（新的那份已经进了 added）。
            !seen_names.contains(*name) || !reused.contains_key(*name)
        })
        .cloned()
        .collect();

    OutboundDiff { reused, added, removed }
}

/// 从 `Config` 构建初始 `RuntimeState` 需要的各项材料（运行时接入
/// config.yaml 设计文档 §2）。
///
/// 拆成"纯计算"（本函数）与"有副作用"（`main.rs` 里真的建
/// `OutboundInstance`、建窗口、跑本地条带）两半——前者不依赖 Tauri，
/// 可以脱离真实文件系统/网络单测；后者留给调用方。
///
/// **`session_bases` 只是占位**：每个出站的会话基址长度按
/// `extra_sessions + 1` 算好（与既有的单出站逻辑一致），但值全部是
/// `None`。真正的会话基址（本地条带分到的端口、承载计划决定的相对/
/// 绝对路径）依赖异步 IO（DNS 解析、hosts 改写、起转发器），这些
/// 不属于"纯"的范围，必须留给 `main.rs` 在有了 `AppHandle` 之后再算，
/// 算完要整体替换掉这里的占位值，不能指望这里的值直接可用。
pub struct StartupPlan {
    pub outbound_cfgs: Vec<crate::outbound::instance::OutboundCfg>,
    /// 出站为空时是 `None`——`CarrierPlan::build` 在空列表上会报错，
    /// 因此本函数在出站为空时压根不调用它，而不是指望它优雅地处理空输入。
    pub carrier: Option<crate::outbound::carrier::CarrierPlan>,
    pub rule_lines: Vec<String>,
    pub mode: wsieve_route::Mode,
    pub global_outbound: String,
}

/// 规则表为空时的隐式兜底：拒绝而非放行。
///
/// `wsieve_route::RuleSet::build` 无条件要求恰好收尾一条 `MATCH` 规则
/// （`BuildError::MissingMatch`），这与 `mode` 取值无关——即使
/// `mode` 是 `direct`/`global`（此时规则表根本不参与判决），`build`
/// 仍然会在语法层拒绝一份没有 `MATCH` 的规则表。而
/// `commands/config.rs::DEFAULT_CONFIG_YAML` 刻意生成 `rules: []`
/// （首次启动的默认配置），用户也可能把全部规则删光——两者都会让
/// `RuleSet::build` 在启动时直接报错退出，与"零出站也要能正常起来"
/// 这条要求相悖。
///
/// 用 `MATCH,REJECT` 补一条隐式兜底：这与 `DEFAULT_CONFIG_YAML` 自己
/// 的注释承诺（"代理会拒绝连接而不是偷偷直连"）完全一致——不是
/// §6.4 禁止的那种"悄悄放行"，而是把同一份产品承诺翻译成
/// `RuleSet::build` 认识的语法。只在规则表整体为空时补，规则表非空但
/// 缺 `MATCH` 收尾属于用户的配置错误，应当照常从 `RuleSet::build` 报出来，
/// 不在这里静默吞掉。
fn rule_lines_with_reject_fallback(config: &wsieve_config::Config) -> Vec<String> {
    let lines = config.rule_lines();
    if lines.is_empty() {
        vec!["MATCH,REJECT".to_string()]
    } else {
        lines
    }
}

pub fn build_startup_plan(config: &wsieve_config::Config) -> anyhow::Result<StartupPlan> {
    let mut outbound_cfgs = Vec::with_capacity(config.proxies.len());
    for p in &config.proxies {
        let server_pub = crate::bootstrap::hex32(&p.server_pub).map_err(|e| {
            anyhow::anyhow!("出站「{}」的 server-pub 不是合法的十六进制: {e}", p.name)
        })?;
        let client_priv = crate::bootstrap::hex32(&p.client_priv).map_err(|e| {
            anyhow::anyhow!("出站「{}」的 client-priv 不是合法的十六进制: {e}", p.name)
        })?;
        let mux_prefs = p
            .mux_prefs
            .iter()
            .map(|&id| {
                wsieve_proto::hello::MuxId::from_u8(id).ok_or_else(|| {
                    anyhow::anyhow!(
                        "出站「{}」的 mux-prefs 含非法值 {id}（合法取值见 MuxId 的十六进制标识）",
                        p.name
                    )
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        outbound_cfgs.push(crate::outbound::instance::OutboundCfg {
            name: p.name.clone(),
            server_pub,
            client_priv,
            mux_prefs,
            // 占位，main.rs 建好本地条带之后整体替换——见结构体文档。
            session_bases: vec![None; p.extra_sessions + 1],
        });
    }

    let carrier = if outbound_cfgs.is_empty() {
        None
    } else {
        let mode = crate::outbound::carrier::CarrierMode::parse(&config.carrier)?;
        let entries: Vec<(&str, &str)> = config
            .proxies
            .iter()
            .map(|p| (p.name.as_str(), p.url.as_str()))
            .collect();
        Some(crate::outbound::carrier::CarrierPlan::build(
            mode,
            &config.carrier_host,
            &entries,
        )?)
    };

    let mode: wsieve_route::Mode = config
        .mode
        .parse()
        .map_err(|e: wsieve_route::RuleError| anyhow::anyhow!("mode 无效: {e}"))?;

    Ok(StartupPlan {
        outbound_cfgs,
        carrier,
        rule_lines: rule_lines_with_reject_fallback(config),
        mode,
        global_outbound: config.global_outbound.clone(),
    })
}

#[cfg(test)]
mod startup_plan_tests {
    use super::*;

    fn proxy(name: &str, server_pub: &str, client_priv: &str) -> wsieve_config::Proxy {
        wsieve_config::Proxy {
            name: name.to_string(),
            kind: "websieve".to_string(),
            url: format!("https://{name}.example/"),
            server_pub: server_pub.to_string(),
            client_priv: client_priv.to_string(),
            extra_sessions: 0,
            // 有效的 MuxId 取值（0x01..=0x05），不是 wsieve-config 的
            // `default_mux_prefs()`——见下面
            // `a_proxy_using_the_crate_default_mux_prefs_currently_fails_to_convert`
            // 里对那个默认值的单独记录。
            mux_prefs: vec![1, 2, 3, 4, 5],
        }
    }

    fn valid_pub() -> String {
        "11".repeat(32)
    }

    fn valid_priv() -> String {
        "22".repeat(32)
    }

    fn config_with(proxies: Vec<wsieve_config::Proxy>) -> wsieve_config::Config {
        wsieve_config::Config {
            proxies,
            ..Default::default()
        }
    }

    #[test]
    fn empty_proxies_yield_no_outbounds_and_no_carrier() {
        let cfg = config_with(vec![]);
        let plan = build_startup_plan(&cfg).unwrap();
        assert!(plan.outbound_cfgs.is_empty());
        assert!(plan.carrier.is_none(), "空出站不该调 CarrierPlan::build");
    }

    #[test]
    fn a_single_proxy_is_hex_decoded_into_the_outbound_cfg() {
        let cfg = config_with(vec![proxy("A", &valid_pub(), &valid_priv())]);
        let plan = build_startup_plan(&cfg).unwrap();
        assert_eq!(plan.outbound_cfgs.len(), 1);
        let ob = &plan.outbound_cfgs[0];
        assert_eq!(ob.name, "A");
        assert_eq!(ob.server_pub, [0x11u8; 32]);
        assert_eq!(ob.client_priv, [0x22u8; 32]);
        assert_eq!(ob.mux_prefs.len(), 5);
        // extra_sessions 默认 0 ⇒ 只有主会话
        assert_eq!(ob.session_bases, vec![None]);
    }

    #[test]
    fn session_bases_length_follows_extra_sessions() {
        let mut p = proxy("A", &valid_pub(), &valid_priv());
        p.extra_sessions = 3;
        let cfg = config_with(vec![p]);
        let plan = build_startup_plan(&cfg).unwrap();
        assert_eq!(plan.outbound_cfgs[0].session_bases.len(), 4);
    }

    #[test]
    fn invalid_hex_in_server_pub_is_an_error_not_a_panic_or_zero_fill() {
        let cfg = config_with(vec![proxy("A", "不是十六进制", &valid_priv())]);
        let err = build_startup_plan(&cfg).unwrap_err().to_string();
        assert!(err.contains('A'), "错误要点名是哪个出站：{err}");
    }

    #[test]
    fn invalid_hex_in_client_priv_is_an_error_not_a_panic_or_zero_fill() {
        let cfg = config_with(vec![proxy("A", &valid_pub(), "zz")]);
        assert!(build_startup_plan(&cfg).is_err());
    }

    #[test]
    fn wrong_length_hex_is_an_error() {
        // 32 字节要求 64 个十六进制字符，短一位也不能悄悄放行。
        let cfg = config_with(vec![proxy("A", "11", &valid_priv())]);
        assert!(build_startup_plan(&cfg).is_err());
    }

    #[test]
    fn a_proxy_using_the_crate_default_mux_prefs_currently_fails_to_convert() {
        // 已知的、本 Task 之外的既有不一致：`wsieve_config::model` 里
        // `default_mux_prefs()` 返回 `[0, 1, 2, 3, 4]`，而
        // `MuxId::from_u8` 的合法输入是 `0x01..=0x05`（即 1-5）——`0`
        // 不在其中。也就是说，用户在 UI 上添加一个不手动填 mux-prefs
        // 的出站，会在这里报错。这条测试如实记录当前行为（报错，不是
        // panic，也不是悄悄吞掉），并把这个不一致钉在测试里：
        // wsieve-config 那边一旦修好默认值，这条测试会变红，提醒同步
        // 更新——细节见本次任务（Task 3）的完成报告。
        let mut p = proxy("A", &valid_pub(), &valid_priv());
        p.mux_prefs = vec![0, 1, 2, 3, 4];
        let cfg = config_with(vec![p]);
        let err = build_startup_plan(&cfg).unwrap_err().to_string();
        assert!(err.contains("mux-prefs"), "{err}");
    }

    #[test]
    fn two_proxies_with_shared_carrier_share_one_window_label() {
        let cfg = config_with(vec![
            proxy("A", &valid_pub(), &valid_priv()),
            proxy("B", &valid_pub(), &valid_priv()),
        ]);
        let plan = build_startup_plan(&cfg).unwrap();
        let carrier = plan.carrier.expect("两个出站应当有承载计划");
        assert_eq!(
            carrier.window_label("A"),
            carrier.window_label("B"),
            "shared 模式下应共用同一个窗口标签"
        );
        assert_eq!(carrier.window_label("A").as_deref(), Some("main"));
    }

    #[test]
    fn isolated_carrier_gives_each_outbound_its_own_window_label() {
        let mut cfg = config_with(vec![
            proxy("A", &valid_pub(), &valid_priv()),
            proxy("B", &valid_pub(), &valid_priv()),
        ]);
        cfg.carrier = "isolated".to_string();
        let plan = build_startup_plan(&cfg).unwrap();
        let carrier = plan.carrier.unwrap();
        assert_ne!(carrier.window_label("A"), carrier.window_label("B"));
    }

    #[test]
    fn mode_global_outbound_and_rule_lines_are_read_from_config() {
        // 走真实的 YAML 解析拿一份带行号的 `Config`——这正是
        // `build_startup_plan` 在生产路径上会拿到的输入形态，也顺带避免
        // 手工构造 `Spanned`（那需要额外引入 `serde-saphyr` 这个当前
        // src-tauri 并未直接依赖的 crate）。
        let text = format!(
            "mode: global\n\
             global-outbound: A\n\
             proxies:\n\
             \x20\x20- name: \"A\"\n\
             \x20\x20\x20\x20type: websieve\n\
             \x20\x20\x20\x20url: https://a.example/\n\
             \x20\x20\x20\x20server-pub: \"{}\"\n\
             \x20\x20\x20\x20client-priv: \"{}\"\n\
             \x20\x20\x20\x20mux-prefs: [1, 2, 3, 4, 5]\n\
             rules:\n\
             \x20\x20- DOMAIN,a.com,A\n\
             \x20\x20- MATCH,A\n",
            valid_pub(),
            valid_priv()
        );
        let cfg = wsieve_config::load_str(&text).unwrap();
        let plan = build_startup_plan(&cfg).unwrap();
        assert_eq!(plan.mode, wsieve_route::Mode::Global);
        assert_eq!(plan.global_outbound, "A");
        assert_eq!(
            plan.rule_lines,
            vec!["DOMAIN,a.com,A".to_string(), "MATCH,A".to_string()]
        );
    }

    #[test]
    fn empty_rules_get_an_implicit_reject_fallback() {
        let cfg = config_with(vec![]);
        let plan = build_startup_plan(&cfg).unwrap();
        assert_eq!(plan.rule_lines, vec!["MATCH,REJECT".to_string()]);
    }

    #[test]
    fn the_implicit_reject_fallback_actually_satisfies_rule_set_build() {
        // 这是本函数存在的核心动机：零规则的默认配置不能在
        // RuleSet::build 这一步把整个启动炸掉。
        let cfg = config_with(vec![]);
        let plan = build_startup_plan(&cfg).unwrap();
        let known = std::collections::HashSet::new();
        wsieve_route::RuleSet::build(&plan.rule_lines, plan.mode, &plan.global_outbound, &known)
            .expect("空规则应当能通过隐式 REJECT 兜底通过 RuleSet::build");
    }

    #[test]
    fn an_invalid_mode_string_is_reported_not_panicked() {
        let mut cfg = config_with(vec![]);
        cfg.mode = "bogus".to_string();
        assert!(build_startup_plan(&cfg).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbound::instance::OutboundCfg;
    use wsieve_proto::hello::MuxId;

    fn cfg(name: &str) -> OutboundCfg {
        OutboundCfg {
            name: name.into(),
            server_pub: [1u8; 32],
            client_priv: [2u8; 32],
            mux_prefs: vec![MuxId::Yamux],
            session_bases: vec![None],
        }
    }

    fn instance_of(c: OutboundCfg) -> (String, Arc<OutboundInstance>) {
        (c.name.clone(), OutboundInstance::new(c))
    }

    #[test]
    fn unchanged_outbound_is_reused_not_rebuilt() {
        let (name, inst) = instance_of(cfg("A"));
        let old = BTreeMap::from([(name.clone(), inst.clone())]);
        let diff = diff_outbounds(&old, &[cfg("A")]);
        assert!(Arc::ptr_eq(diff.reused.get("A").unwrap(), &inst),
            "字段完全没变时必须原样复用同一个 Arc，不能悄悄换成一个新实例");
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn brand_new_outbound_is_added() {
        let old = BTreeMap::new();
        let diff = diff_outbounds(&old, &[cfg("A")]);
        assert!(diff.reused.is_empty());
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].name, "A");
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn removed_outbound_is_listed_for_teardown() {
        let (name, inst) = instance_of(cfg("A"));
        let old = BTreeMap::from([(name, inst)]);
        let diff = diff_outbounds(&old, &[]);
        assert!(diff.reused.is_empty());
        assert!(diff.added.is_empty());
        assert_eq!(diff.removed, vec!["A".to_string()]);
    }

    #[test]
    fn changed_field_is_treated_as_remove_then_add() {
        let (name, inst) = instance_of(cfg("A"));
        let old = BTreeMap::from([(name, inst)]);
        let mut changed = cfg("A");
        changed.server_pub = [9u8; 32]; // 换了公钥——身份变了
        let diff = diff_outbounds(&old, &[changed]);
        assert!(diff.reused.is_empty(), "字段变了不该被当成复用");
        assert_eq!(diff.removed, vec!["A".to_string()]);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].server_pub, [9u8; 32]);
    }

    #[test]
    fn mixed_scenario_reuse_add_remove_together() {
        let (a_name, a_inst) = instance_of(cfg("A"));
        let (b_name, b_inst) = instance_of(cfg("B"));
        let old = BTreeMap::from([(a_name, a_inst.clone()), (b_name, b_inst)]);
        // A 不变，B 删掉，C 新增
        let diff = diff_outbounds(&old, &[cfg("A"), cfg("C")]);
        assert!(Arc::ptr_eq(diff.reused.get("A").unwrap(), &a_inst));
        assert_eq!(diff.reused.len(), 1);
        assert_eq!(diff.removed, vec!["B".to_string()]);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].name, "C");
    }
}
