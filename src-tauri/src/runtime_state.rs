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

pub struct RuntimeState {
    pub rule_set: Arc<wsieve_route::RuleSet>,
    pub outbound_manager: Arc<OutboundManager>,
    pub router: Arc<Router>,
    // GroupTable 是 Part 3（代理组接入）的产出，本计划不实现，先占位成
    // 一个空结构体，避免 Part 3 落地时要改这里的字段名/调用点。
    pub groups: Arc<GroupTable>,
    /// **产生这份快照的那一段 `proxies:`**。
    ///
    /// 留着原始配置而不是只留 `OutboundCfg`，是为了能如实回答「这次保存
    /// 动没动出站段」：`url` / `extra-sessions` 改了同样要重连，而 `url`
    /// 在 `OutboundCfg` 里根本没有对应项——只比 `OutboundCfg` 会把它整个
    /// 漏掉。Part 2 做出站热增删时，diff 的另一边也正是这份。
    pub proxies: Arc<Vec<wsieve_config::Proxy>>,
}

/// 代理组表——本计划只放占位结构，真正的构建逻辑属于 Part 3。
#[derive(Default)]
pub struct GroupTable;

/// 托管进 Tauri 状态的那一份快照，可原子替换。
///
/// **`app.manage()` 只调用一次**（`install`），之后的更新一律是对这把锁的
/// 写入。Tauri 的 `manage()` 在类型已注册时是**空操作**——靠反复 `manage`
/// 来「更新」不会报错，只是静默无效，读到的永远是第一次注册的那份。
/// `CurrentCore` 正踩在这个坑里（`run_stack` 的承载代循环每一代都
/// `manage` 一次，IPC 命令读到的却始终是第一代那个早已死掉的 core）。
/// 本结构用真正可变的 `RwLock` 从根上避开，见设计文档 §1。
pub struct RuntimeHandle(std::sync::RwLock<Arc<RuntimeState>>);

impl RuntimeHandle {
    /// 取当前快照。
    ///
    /// **路由热路径每条连接都会调它**，所以持锁区间只覆盖一次 `Arc` 克隆
    /// （引用计数 +1，纳秒级），锁里绝不做别的事。
    ///
    /// 锁中毒时取回内层值继续用，而不是跟着 panic：这里护的数据只是一个
    /// `Arc` 指针，写者持锁期间只做一次赋值，不存在「改了一半」的中间态。
    /// 让一次无关的 panic 把此后每一条连接都打死，比中毒本身危险得多。
    pub fn current(&self) -> Arc<RuntimeState> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set(&self, next: Arc<RuntimeState>) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = next;
    }

    /// 换代时替换 `outbound_manager`，**其余字段沿用当前快照**。
    ///
    /// 不能拿换代开始时捕获的那份快照整体覆盖：两次换代之间用户可能保存过
    /// 配置（`rebuild_and_swap` 已经换掉了 `rule_set`/`router`），整体覆盖
    /// 会把刚保存的规则悄悄回滚成旧的——而控制窗口上显示的仍是新规则，
    /// 用户完全看不出流量正在按一份已经被自己改掉的规则表走。
    fn set_outbound_manager(&self, manager: Arc<OutboundManager>) {
        let cur = self.current();
        self.set(Arc::new(RuntimeState {
            rule_set: cur.rule_set.clone(),
            outbound_manager: manager,
            router: cur.router.clone(),
            groups: cur.groups.clone(),
            proxies: cur.proxies.clone(),
        }));
    }
}

/// 进程启动时托管一次（`run_stack` 里，第一份快照就绪之后、`dispatch`
/// 建立之前——`dispatch` 每条连接都要经它读当前 `Router`）。
pub fn install(app: &tauri::AppHandle, state: RuntimeState) {
    use tauri::Manager;
    app.manage(RuntimeHandle(std::sync::RwLock::new(Arc::new(state))));
}

/// `RuntimeHandle::set_outbound_manager` 的取状态包装（换代逻辑本身在那边，
/// 好脱离 Tauri 单测）。
pub fn swap_outbound_manager(app: &tauri::AppHandle, manager: Arc<OutboundManager>) {
    use tauri::Manager;
    match app.try_state::<RuntimeHandle>() {
        Some(h) => h.set_outbound_manager(manager),
        // 托管发生在第一代之前，走到这里说明调用顺序被改坏了。
        None => tracing::error!("运行时快照尚未托管，换代后的出站管理器无处安放"),
    }
}

/// 纯函数：给定当前快照与刚保存的配置，算出下一份快照。
///
/// **本 Part 只热替换规则与模式**，出站表原样沿用当前快照里的那一批
/// `Arc<OutboundInstance>`。这不是「没做完」，是 `shard_setup` 现在的形状
/// 决定的：`ShardGuard` 持有的是「全部转发器 + 一份整体替换的 hosts 托管」，
/// 要给新出站起转发器就得重跑 `plan_many`，而那必须先 drop 旧 guard——于是
/// **没有变化的出站的转发器也会一起被 abort**，已经连上的连接全断。这与
/// 设计文档 §3「没变的出站不重启、不断连接」这条已确认的产品决策直接冲突。
/// 真正支持出站热增删，要先把转发器与 hosts 托管改成按出站增量持有，那是
/// Part 2 的范围。在那之前，出站的增删改由 `outbound_changes_pending`
/// 如实报给用户（需要重启），而不是假装已经生效。
///
/// 失败时调用方**保留旧快照**——半份新状态比一份旧状态危险得多。
pub fn next_state(
    current: &RuntimeState,
    config: &wsieve_config::Config,
) -> anyhow::Result<RuntimeState> {
    let mode: wsieve_route::Mode = config
        .mode
        .parse()
        .map_err(|e: wsieve_route::RuleError| anyhow::anyhow!("mode 无效: {e}"))?;

    // `known` 取**配置里**的出站名，不是当前正在跑的那一批。用户新加了一个
    // 出站、同时写了引用它的规则时，规则表应当照常建起来：那个出站要等重启
    // 才真正跑，此前落到它头上的判决会在 `Router::via_outbound` 处被拒绝，
    // 与「出站在运行时被删了」是同一条既有错误路径（§2.1）。若改用正在跑的
    // 那一批，整张规则表会因为「引用了不存在的出站」被拒——一条规则连累全部。
    let known = config.outbound_names();
    let rule_set = Arc::new(wsieve_route::RuleSet::build(
        &rule_lines_with_reject_fallback(config),
        mode,
        &config.global_outbound,
        &known,
    )?);

    Ok(RuntimeState {
        router: Arc::new(current.router.with_rules(rule_set.clone())),
        rule_set,
        outbound_manager: current.outbound_manager.clone(),
        groups: current.groups.clone(),
        proxies: Arc::new(config.proxies.clone()),
    })
}

/// 这次保存里，出站段有哪些改动是本轮热替换**吃不下的**（见 `next_state`）。
///
/// 返回人话描述，空表示出站段没动。调用方原样写进日志：用户改了出站却什么
/// 都没发生、还不告诉他为什么，比明说「暂不支持，请重启」糟糕得多。
pub fn outbound_changes_pending(
    current: &RuntimeState,
    config: &wsieve_config::Config,
) -> Vec<String> {
    let old = &*current.proxies;
    let new = &config.proxies;
    let mut out = Vec::new();
    for p in new {
        match old.iter().find(|o| o.name == p.name) {
            None => out.push(format!("新增了出站「{}」", p.name)),
            // 逐字段比较：`url` 改了也要重连，而它在 OutboundCfg 里没有
            // 对应项——只比那边的字段会把这种改动整个漏掉。
            Some(o) if o != p => out.push(format!("改动了出站「{}」", p.name)),
            Some(_) => {}
        }
    }
    for o in old {
        if !new.iter().any(|p| p.name == o.name) {
            out.push(format!("删除了出站「{}」", o.name));
        }
    }
    out
}

/// 从磁盘重新读配置、重建快照、原子替换托管状态里的那一份。
///
/// **失败时保留旧快照，不替换**（设计文档「错误处理」一节）：写盘前的
/// `validate()` 已经挡住绝大多数非法配置，走到这里的失败基本只剩「写盘与
/// 重读之间文件被外部改坏」这类边缘情况。此时让运行时继续用旧的那一份、
/// 只记一条错误日志，比强行换上一个不完整的半成品安全。
///
/// 调用方是 `config_save` 系列命令，**它们不该因为这里失败而报错给用户**：
/// 盘已经写成功了（文件是对的），重建运行时状态失败是另一个层面的问题。
/// 把「保存成功但运行时暂时没跟上」升级成「保存失败」，是把一个可恢复状况
/// 误报成一个更严重的状况。
pub fn rebuild_and_swap(app: &tauri::AppHandle) -> anyhow::Result<()> {
    use tauri::Manager;
    let Some(h) = app.try_state::<RuntimeHandle>() else {
        // 出站栈还没起来（`run_stack` 在 `.setup()` 之后才 spawn）。盘已经
        // 写了，等它起来时会照常读到新配置，不需要额外补偿。
        anyhow::bail!("运行时快照尚未托管——出站栈还没起来，本次保存只落了盘");
    };
    let path = crate::commands::config::config_path(app)
        .map_err(|e| anyhow::anyhow!("取配置路径失败: {e:?}"))?;
    // 与启动路径同一个函数：读 + 解析 + 语义校验。两处各写一份迟早分叉。
    let config = crate::load_config(&path)?;

    let current = h.current();
    // 先算出新快照再取差异：`next_state` 失败时（比如规则表引用了不存在的
    // 出站）什么都不该换，此时报告出站差异只会误导——用户会以为「除了出站
    // 之外都生效了」，而实际上规则也没换。
    let next = next_state(&current, &config)?;
    let pending = outbound_changes_pending(&current, &config);
    h.set(Arc::new(next));

    tracing::info!(
        "配置已重新加载：{} 条规则、模式 {} 立即生效",
        config.rules.len(),
        config.mode
    );
    if !pending.is_empty() {
        tracing::warn!(
            "以下改动本次**未生效**，需要重启应用：{}。\
             原因：本地条带的转发器与 hosts 托管目前是整体持有的，为新出站\
             重跑一遍会把已经连上的其余出站一并断开——宁可明说要重启，也不\
             悄悄断掉用户正在用的连接。出站的热增删见 Part 2",
            pending.join("、")
        );
    }
    Ok(())
}

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
/// `client_priv`/`mux_prefs`/`session_bases`）——`session_bases` 由条带
/// 算出、不来自 `Proxy` 本身，所以调用方要在算出新的 `session_bases`
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
/// **本地条带的结果是输入而不是输出**：会话基址取决于 hosts 劫持有没有
/// 成功（成功则是本地转发端口，失败则是原始服务端的 origin），而那是异步
/// IO，不属于"纯"的范围。因此调用方先跑 `shard_setup::plan_many`，把结果
/// 原样递进来——`ShardPlanEntry` 是纯数据，本函数照旧可以脱离 Tauri/网络
/// 单测。**承载页面 URL 与本地条带无关**：它是本机 http 壳的固定地址
/// （`carrier_page::spawn` 分配的端口），由调用方作为 `carrier_page_url`
/// 单独注入。
#[derive(Debug)]
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

/// 判决路径的解析器链 + 供转发改写用的 hosts 表。
///
/// 链的形状固定是 `HostsResolver(DnsResolver | NoResolver)`：hosts 永远叠在
/// 最外层，命中就不查 DNS——用户写这张表就是不想让这个域名走解析器。
///
/// **不返回 `Result`**：DNS 配错了不该让整个应用起不来。上游非法、列表为空
/// 之类一律降级成 `NoResolver` 并告警，此时 IP 类规则对域名目标不命中
/// （§6.2 的既有语义），代理照常可用。反过来让启动失败的话，用户改坏一行
/// `nameserver` 就再也打不开应用，连改回去的界面都进不去。
///
/// hosts 表即使为空也照样返回：`Router::with_hosts` 拿到空表是无操作，
/// 而调用方不必写分支。
pub fn build_resolver(
    config: &wsieve_config::Config,
    resolving_rule_count: usize,
) -> (
    std::sync::Arc<dyn wsieve_dns::RoutingResolver>,
    std::sync::Arc<wsieve_dns::Hosts>,
) {
    let (hosts, rejected) = wsieve_dns::Hosts::build(
        config
            .hosts
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str())),
    );
    for r in &rejected {
        // Config::validate 已经拦过一道，走到这里说明两处对「什么算合法」
        // 的看法不一致——那是 bug，不能静默跳过。
        tracing::warn!("hosts 条目被丢弃：{r}");
    }
    if !hosts.is_empty() {
        tracing::info!("hosts 静态解析表已装载：{} 条", hosts.len());
    }

    let base: std::sync::Arc<dyn wsieve_dns::RoutingResolver> = if !config.dns.enable {
        // 用户显式关的，不必告警成「故障」，但覆盖面缺失仍要说清楚。
        if resolving_rule_count > 0 {
            tracing::warn!(
                "dns.enable 为 false：{resolving_rule_count} 条 IP 类规则（GEOIP / IP-CIDR）\
                 对**域名**目标不会命中，这类流量会落到后续规则或 MATCH 兜底。"
            );
        }
        std::sync::Arc::new(crate::router::NoResolver)
    } else {
        match wsieve_dns::DnsResolver::new(
            &config.dns.nameserver,
            std::time::Duration::from_millis(config.dns.timeout_ms),
            config.dns.cache.max as u64,
            std::time::Duration::from_secs(config.dns.cache.negative_ttl_s),
        ) {
            Ok(r) => {
                tracing::info!(
                    "内部 DNS 解析器就绪：{} 个上游，超时 {}ms",
                    config.dns.nameserver.len(),
                    config.dns.timeout_ms
                );
                std::sync::Arc::new(r)
            }
            Err(e) => {
                tracing::warn!(
                    "DNS 解析器构建失败（{e}），降级为不解析：\
                     {resolving_rule_count} 条 IP 类规则对域名目标不会命中。代理本身不受影响。"
                );
                std::sync::Arc::new(crate::router::NoResolver)
            }
        }
    };

    let hosts = std::sync::Arc::new(hosts);
    let resolver: std::sync::Arc<dyn wsieve_dns::RoutingResolver> = if hosts.is_empty() {
        // 空表就不套壳：多一层间接调用换不来任何行为。
        base
    } else {
        std::sync::Arc::new(wsieve_dns::HostsResolver::new(hosts.clone(), base))
    };
    (resolver, hosts)
}

/// `shard` 必须与 `config.proxies` **按下标一一对应**（`plan_many` 的契约
/// 就是这样，见 `ShardManyPlan::entries`）。长度对不上时报错而非按短的那个
/// 截断：截断意味着有出站会拿到别人的会话基址，流量发去另一台服务器
/// （§6.4），而现象离病因极远。
pub fn build_startup_plan(
    config: &wsieve_config::Config,
    shard: &[crate::shard_setup::ShardPlanEntry],
    carrier_page_url: &str,
) -> anyhow::Result<StartupPlan> {
    if shard.len() != config.proxies.len() {
        anyhow::bail!(
            "本地条带结果有 {} 项，出站有 {} 项——两者必须按下标一一对应",
            shard.len(),
            config.proxies.len()
        );
    }

    // 出站 URL 合法性校验：这道校验以前长在 `outbound::carrier::validate`
    // 里（那时承载页的 URL 就是出站 URL 的 origin，非法 URL 会在那一步现形）。
    // 承载页挪到本机 http 壳之后，承载计划完全不摸出站 URL 了，这道校验就
    // 无处可挂——只能搬到这里，直接校验 `config.proxies[].url` 本身。
    // 不是可省的活：`ShardPlanEntry::degraded` 在 `origin_of` 解析失败时会
    // 静默退回原始字符串（见它的注释），没有这一步，坏 URL 会一路滑到
    // `OutboundCfg` 里才在请求时炸，而不是在启动时报清楚是哪个出站配错了。
    for p in &config.proxies {
        crate::shard_setup::origin_of(&p.url)
            .map_err(|e| anyhow::anyhow!("出站「{}」的 url 不是合法的 URL: {e}", p.name))?;
    }

    // 承载计划：单张本机 http 壳页面，与出站 URL 完全无关——`carrier_page_url`
    // 由调用方（main.rs）注入，是本地 server 分配的端口，这里只管
    // 「谁落在哪个窗口」。
    let carrier = if config.proxies.is_empty() {
        None
    } else {
        let mode = crate::outbound::carrier::CarrierMode::parse(&config.carrier)?;
        let names: Vec<&str> = config.proxies.iter().map(|p| p.name.as_str()).collect();
        Some(crate::outbound::carrier::CarrierPlan::build(
            mode,
            &config.carrier_host,
            &names,
            carrier_page_url,
        )?)
    };

    let mut outbound_cfgs = Vec::with_capacity(config.proxies.len());
    for (p, entry) in config.proxies.iter().zip(shard) {
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

        // 条带给出的会话基址：**每一项都是绝对 URL，含会话 0**。承载页已经
        // 不是任何出站的 origin（它是本机的 http 壳），因此没有任何一条会话
        // 还能吃相对路径。
        let session_bases = entry.session_bases.clone();
        if session_bases.is_empty() {
            anyhow::bail!("出站「{}」的条带结果没有任何会话基址", p.name);
        }
        // §6.4 的防线原样保留，只是换了个不涉及基址推导的方法来守：认不出
        // 的出站必须报错，绝不能套一个默认基址把流量发去另一台服务器。
        carrier
            .as_ref()
            .expect("出站非空时承载计划必然已构建")
            .window_label(&p.name)
            .ok_or_else(|| anyhow::anyhow!("承载计划里没有出站「{}」", p.name))?;

        // 未知写法必须报错。悄悄退回 auto 的话，用户配了 v4-only、流量照旧
        // 走 IPv6，而两端日志都显示一切正常——这正是 §6.4 要挡的静默改道。
        let ip_strategy = wsieve_proto::hello::IpStrategy::parse(&p.ip_strategy)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "出站「{}」的 ip-strategy「{}」无法识别，合法取值：auto / v4-only / v6-only / prefer-v4",
                    p.name,
                    p.ip_strategy
                )
            })?;

        outbound_cfgs.push(crate::outbound::instance::OutboundCfg {
            name: p.name.clone(),
            server_pub,
            client_priv,
            mux_prefs,
            session_bases,
            ip_strategy,
        });
    }

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
mod next_state_tests {
    use super::*;

    /// 一份最小可用的运行时快照。不碰 Tauri、不碰网络——`next_state` 与
    /// `outbound_changes_pending` 都是纯函数，这正是它们被切出来的理由
    /// （胶水层 `rebuild_and_swap` 需要 `AppHandle`，靠手动验证兜底）。
    fn snapshot(rules: &[&str], proxies: Vec<wsieve_config::Proxy>) -> RuntimeState {
        let lines: Vec<String> = rules.iter().map(|s| s.to_string()).collect();
        let known: std::collections::HashSet<String> =
            proxies.iter().map(|p| p.name.clone()).collect();
        let rule_set = Arc::new(
            wsieve_route::RuleSet::build(&lines, wsieve_route::Mode::Rule, "", &known).unwrap(),
        );
        let table = BTreeMap::new();
        let geo = Arc::new(wsieve_geo::GeoDb::new("geoip.dat".into(), "geosite.dat".into()));
        let env = crate::outbound::instance::SessionEnv {
            eval: Arc::new(|_, _| {}),
            on_status: Arc::new(|_, _| {}),
        };
        RuntimeState {
            router: Arc::new(crate::router::Router::without_resolver(
                rule_set.clone(),
                geo,
                table.clone(),
            )),
            rule_set,
            outbound_manager: crate::outbound::OutboundManager::new(
                table,
                Arc::new(crate::bridge::TransportCore::new()),
                env,
            ),
            groups: Arc::new(GroupTable),
            proxies: Arc::new(proxies),
        }
    }

    fn proxy(name: &str, url: &str) -> wsieve_config::Proxy {
        wsieve_config::Proxy {
            name: name.to_string(),
            kind: "websieve".to_string(),
            url: url.to_string(),
            server_pub: "11".repeat(32),
            client_priv: "22".repeat(32),
            extra_sessions: 0,
            mux_prefs: vec![1],
            ip_strategy: "auto".to_string(),
        }
    }

    fn config(mode: &str, rules: &[&str], proxies: Vec<wsieve_config::Proxy>) -> wsieve_config::Config {
        // `rules: Vec<Spanned<String>>` 手工构造要引入 serde-saphyr（src-tauri
        // 并不直接依赖它），走真实 YAML 解析更贴近生产输入形态。
        let mut text = format!("mode: {mode}\nrules:\n");
        for r in rules {
            text.push_str(&format!("  - {r}\n"));
        }
        let mut cfg = wsieve_config::load_str(&text).unwrap();
        cfg.proxies = proxies;
        cfg
    }

    /// 改一条规则只换规则表，**出站实例一个都不重建**——这是设计文档 §3
    /// 那条产品决策的核心断言：用户改了一条规则，已经连上的出站不该断。
    #[test]
    fn a_rule_edit_swaps_the_rule_set_and_reuses_every_outbound_instance() {
        let cur = snapshot(&["MATCH,REJECT"], vec![]);
        let next = next_state(&cur, &config("rule", &["MATCH,DIRECT"], vec![])).unwrap();
        assert!(
            Arc::ptr_eq(&next.outbound_manager, &cur.outbound_manager),
            "改规则绝不能顺手换掉出站管理器——那等于把全部连接断一遍"
        );
        assert!(
            !Arc::ptr_eq(&next.rule_set, &cur.rule_set),
            "规则表应当是新建的那一份"
        );
    }

    #[test]
    fn a_mode_change_is_picked_up() {
        let cur = snapshot(&["MATCH,REJECT"], vec![]);
        let next = next_state(&cur, &config("direct", &["MATCH,DIRECT"], vec![])).unwrap();
        // 换代/保存都不该动 groups 这个占位表（Part 3 才充实它）。
        assert!(Arc::ptr_eq(&next.groups, &cur.groups));
        assert!(next.rule_set.resolving_rule_count() == 0);
    }

    /// **规则引用一个「配置里有、但还没跑起来」的出站，规则表照样要建起来。**
    ///
    /// 这是 `known` 取配置里的出站名而不是正在跑的那一批的理由：用户在同一次
    /// 保存里加了出站 B 又写了 `MATCH,B`，若拿正在跑的那批（还没有 B）去校验，
    /// 整张规则表会被判「引用了不存在的出站」而拒绝，一条规则连累全部，
    /// 用户看到的是「保存后所有分流都失效了」。
    #[test]
    fn a_rule_naming_a_not_yet_running_outbound_still_builds() {
        let cur = snapshot(&["MATCH,REJECT"], vec![]);
        let cfg = config("rule", &["MATCH,B"], vec![proxy("B", "https://b.example/")]);
        assert!(
            next_state(&cur, &cfg).is_ok(),
            "新出站还没起来不该让整张规则表建不出来"
        );
    }

    /// 规则表建不出来时报错，调用方据此保留旧快照——半份新状态比一份旧状态
    /// 危险得多。
    #[test]
    fn an_unbuildable_rule_table_is_an_error_so_the_old_snapshot_survives() {
        let cur = snapshot(&["MATCH,REJECT"], vec![]);
        // 引用了配置里根本没有的出站
        let bad = config("rule", &["MATCH,幽灵节点"], vec![]);
        assert!(next_state(&cur, &bad).is_err());
    }

    #[test]
    fn an_invalid_mode_is_an_error_not_a_silent_fallback() {
        let cur = snapshot(&["MATCH,REJECT"], vec![]);
        let mut bad = config("rule", &["MATCH,DIRECT"], vec![]);
        bad.mode = "bogus".to_string();
        assert!(next_state(&cur, &bad).is_err());
    }

    #[test]
    fn an_untouched_outbound_section_reports_nothing_pending() {
        let p = proxy("A", "https://a.example/");
        let cur = snapshot(&["MATCH,A"], vec![p.clone()]);
        let cfg = config("rule", &["MATCH,A"], vec![p]);
        assert!(outbound_changes_pending(&cur, &cfg).is_empty());
    }

    #[test]
    fn added_and_removed_outbounds_are_both_reported() {
        let cur = snapshot(&["MATCH,REJECT"], vec![proxy("A", "https://a.example/")]);
        let cfg = config("rule", &["MATCH,REJECT"], vec![proxy("B", "https://b.example/")]);
        let pending = outbound_changes_pending(&cur, &cfg);
        assert!(pending.iter().any(|s| s.contains('B') && s.contains("新增")), "{pending:?}");
        assert!(pending.iter().any(|s| s.contains('A') && s.contains("删除")), "{pending:?}");
    }

    /// **只改 `url` 也要被报出来。** `url` 在 `OutboundCfg` 里根本没有对应项，
    /// 拿运行中的 `OutboundCfg` 去比会把这种改动整个漏掉——用户改了服务器
    /// 地址、保存成功、流量却仍然发去旧服务器，且没有任何提示。
    /// **换指针必须真的换掉。** 这条测试是冲着 `manage()` 那个坑来的：
    /// Tauri 的 `manage()` 在类型已注册时是空操作，靠它「更新」不报错、
    /// 只是静默无效，读到的永远是第一次注册的那份（`CurrentCore` 至今
    /// 如此）。`RuntimeHandle` 换成 `RwLock` 就是为了不重蹈覆辙，那就得
    /// 有一条测试真的去读一次换后的值。
    #[test]
    fn a_swap_is_visible_to_the_very_next_reader() {
        let h = RuntimeHandle(std::sync::RwLock::new(Arc::new(snapshot(
            &["MATCH,REJECT"],
            vec![],
        ))));
        let before = h.current();
        h.set(Arc::new(snapshot(&["MATCH,DIRECT"], vec![])));
        assert!(
            !Arc::ptr_eq(&before, &h.current()),
            "换进去的快照必须立刻被下一个读者看到，不能像 manage() 那样静默无效"
        );
    }

    /// **换代绝不能回滚刚保存的规则。** 承载页面死掉换一代时，只该换
    /// `outbound_manager`；若拿本代开头捕获的整份快照去覆盖，用户在这一代
    /// 期间保存的规则会被悄悄换回旧的，而控制窗口显示的仍是新规则——流量
    /// 按一份用户已经改掉的表在走，且没有任何迹象。
    #[test]
    fn a_generation_swap_keeps_the_rules_saved_during_that_generation() {
        let h = RuntimeHandle(std::sync::RwLock::new(Arc::new(snapshot(
            &["MATCH,REJECT"],
            vec![],
        ))));
        // 这一代跑着的期间，用户保存了新规则。
        let saved = next_state(&h.current(), &config("rule", &["MATCH,DIRECT"], vec![])).unwrap();
        let saved_rules = saved.rule_set.clone();
        h.set(Arc::new(saved));
        // 然后承载页面死了，换代。
        let fresh_manager = snapshot(&["MATCH,REJECT"], vec![]).outbound_manager;
        h.set_outbound_manager(fresh_manager.clone());

        let now = h.current();
        assert!(
            Arc::ptr_eq(&now.rule_set, &saved_rules),
            "换代把用户刚保存的规则回滚掉了"
        );
        assert!(Arc::ptr_eq(&now.outbound_manager, &fresh_manager), "新一代的管理器该就位");
    }

    #[test]
    fn a_url_only_edit_is_still_reported_as_pending() {
        let cur = snapshot(&["MATCH,A"], vec![proxy("A", "https://old.example/")]);
        let cfg = config("rule", &["MATCH,A"], vec![proxy("A", "https://new.example/")]);
        let pending = outbound_changes_pending(&cur, &cfg);
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert!(pending[0].contains("改动") && pending[0].contains('A'), "{pending:?}");
    }
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
            // 有效的 MuxId 取值。三方 mux 换成自研 wsmux 之后只剩
            // `Wsmux = 0x01` 一个，因此这里与 wsieve-config 的
            // `default_mux_prefs()` 恰好同值——但两者仍是**各写各的**：
            // 默认值本身归 `the_crate_default_mux_prefs_are_all_valid_mux_ids`
            // 守，这里只是给这些测试一份合法输入。
            mux_prefs: vec![1],
            ip_strategy: "auto".to_string(),
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

    /// 一份「hosts 劫持没生效」的条带结果：页面就是原始 URL，会话数按
    /// `extra_sessions + 1` 排开但都还没有各自的端口。这正是没有管理员
    /// 权限时的真实形态，也是这些测试唯一关心的输入形状。
    fn shard_for(cfg: &wsieve_config::Config) -> Vec<crate::shard_setup::ShardPlanEntry> {
        cfg.proxies
            .iter()
            .map(|p| crate::shard_setup::ShardPlanEntry {
                session_bases: vec![None; p.extra_sessions + 1],
                upstream: None,
                bypass_error: None,
            })
            .collect()
    }

    /// 测试入口：把 `shard_for` 的结果一并递进去，省得每个用例重复两行。
    /// 承载页 URL 统一用这个固定值——测试不关心它具体是什么端口，只关心
    /// 它被原样传给 `CarrierPlan::build`。
    fn plan_of(cfg: &wsieve_config::Config) -> anyhow::Result<StartupPlan> {
        build_startup_plan(cfg, &shard_for(cfg), "http://127.0.0.1:53119/")
    }

    #[test]
    fn empty_proxies_yield_no_outbounds_and_no_carrier() {
        let cfg = config_with(vec![]);
        let plan = plan_of(&cfg).unwrap();
        assert!(plan.outbound_cfgs.is_empty());
        assert!(plan.carrier.is_none(), "空出站不该调 CarrierPlan::build");
    }

    /// 坏掉的 DNS 配置**不能让应用起不来**。用户改坏一行 `nameserver` 就
    /// 再也打不开界面的话，他连改回去的地方都进不去。
    ///
    /// 这里逐个喂进真实会被 `parse_nameserver` 拒掉的写法，断言全部降级
    /// 而非 panic/退出。
    #[test]
    fn a_broken_dns_config_degrades_instead_of_failing_startup() {
        for bad in [
            vec![],                                    // 空列表
            vec!["not a url".to_string()],             // 语法垃圾
            vec!["https://example.com/dns-query".to_string()], // 域名而非 IP（纪律②）
        ] {
            let mut cfg = wsieve_config::Config::default();
            cfg.dns.nameserver = bad.clone();
            let (_resolver, hosts) = build_resolver(&cfg, 3);
            assert!(hosts.is_empty(), "没配 hosts 就该是空表");
            // 走到这里没 panic 即通过——降级本身在日志里，不在返回值里。
        }
    }

    /// `dns.enable: false` 是用户显式关掉，同样降级而不是报错。
    #[test]
    fn disabling_dns_is_honoured() {
        let mut cfg = wsieve_config::Config::default();
        cfg.dns.enable = false;
        let (_r, hosts) = build_resolver(&cfg, 0);
        assert!(hosts.is_empty());
    }

    /// hosts 表要真的从配置装进来，并且**同一张表**既回给调用方（转发用）
    /// 又叠进解析器（判决用）。
    #[tokio::test]
    async fn hosts_feed_both_the_forwarding_table_and_the_resolver() {
        let mut cfg = wsieve_config::Config::default();
        cfg.hosts
            .insert("Pinned.Example".into(), "1.2.3.4".into());
        // 上游故意配坏，逼底层降级成 NoResolver——这样解析器返回的任何
        // 非空结果都只可能来自 hosts，断言才有意义。
        cfg.dns.nameserver = vec![];
        let (resolver, hosts) = build_resolver(&cfg, 0);

        // 转发那一半：大小写归一后能查到。
        assert_eq!(
            hosts.lookup("pinned.example"),
            Some("1.2.3.4".parse().unwrap())
        );
        // 判决那一半：解析器直接吐出 hosts 里的值。
        assert_eq!(
            resolver.resolve("pinned.example").await,
            vec!["1.2.3.4".parse::<std::net::IpAddr>().unwrap()]
        );
        // 未命中的仍走下游（此处是 NoResolver）→ 空。
        assert!(resolver.resolve("other.example").await.is_empty());
    }

    /// 配置里的 `ip-strategy` 要真的落到 `OutboundCfg` 上，而且是**逐出站**
    /// 的——两个出站配不同策略，各拿各的。共用一个值的话，用户给某一台配
    /// v4-only 会把所有服务器一起改掉。
    #[test]
    fn ip_strategy_is_parsed_per_outbound() {
        let mut a = proxy("A", &valid_pub(), &valid_priv());
        a.ip_strategy = "v4-only".into();
        let mut b = proxy("B", &valid_pub(), &valid_priv());
        b.ip_strategy = "prefer-v4".into();
        let plan = plan_of(&config_with(vec![a, b])).unwrap();
        use wsieve_proto::hello::IpStrategy;
        assert_eq!(plan.outbound_cfgs[0].ip_strategy, IpStrategy::V4Only);
        assert_eq!(plan.outbound_cfgs[1].ip_strategy, IpStrategy::PreferV4);
        // 省略时是 auto，即加这个字段之前的行为。
        let plan = plan_of(&config_with(vec![proxy("C", &valid_pub(), &valid_priv())])).unwrap();
        assert_eq!(plan.outbound_cfgs[0].ip_strategy, IpStrategy::Auto);
    }

    /// 拼错要报错并点名出站，**不能静默退回 auto**：那样用户配的 v4-only
    /// 不生效而流量照旧走 IPv6，两端日志都显示一切正常。
    #[test]
    fn an_unknown_ip_strategy_is_an_error_naming_the_outbound() {
        let mut p = proxy("日本节点", &valid_pub(), &valid_priv());
        p.ip_strategy = "ipv4".into();
        let e = plan_of(&config_with(vec![p])).unwrap_err().to_string();
        assert!(e.contains("日本节点"), "要点名是哪个出站：{e}");
        assert!(e.contains("ipv4"), "要点名冒犯的值：{e}");
        assert!(e.contains("v4-only"), "要列出合法取值：{e}");
    }

    #[test]
    fn a_single_proxy_is_hex_decoded_into_the_outbound_cfg() {
        let cfg = config_with(vec![proxy("A", &valid_pub(), &valid_priv())]);
        let plan = plan_of(&cfg).unwrap();
        assert_eq!(plan.outbound_cfgs.len(), 1);
        let ob = &plan.outbound_cfgs[0];
        assert_eq!(ob.name, "A");
        assert_eq!(ob.server_pub, [0x11u8; 32]);
        assert_eq!(ob.client_priv, [0x22u8; 32]);
        assert_eq!(ob.mux_prefs.len(), 1, "mux 收敛成 wsmux 一种后偏好列表只有一项");
        // extra_sessions 默认 0 ⇒ 只有主会话
        assert_eq!(ob.session_bases, vec![None]);
    }

    #[test]
    fn session_bases_length_follows_extra_sessions() {
        let mut p = proxy("A", &valid_pub(), &valid_priv());
        p.extra_sessions = 3;
        let cfg = config_with(vec![p]);
        let plan = plan_of(&cfg).unwrap();
        assert_eq!(plan.outbound_cfgs[0].session_bases.len(), 4);
    }

    #[test]
    fn invalid_hex_in_server_pub_is_an_error_not_a_panic_or_zero_fill() {
        let cfg = config_with(vec![proxy("A", "不是十六进制", &valid_priv())]);
        let err = plan_of(&cfg).unwrap_err().to_string();
        assert!(err.contains('A'), "错误要点名是哪个出站：{err}");
    }

    #[test]
    fn invalid_hex_in_client_priv_is_an_error_not_a_panic_or_zero_fill() {
        let cfg = config_with(vec![proxy("A", &valid_pub(), "zz")]);
        assert!(plan_of(&cfg).is_err());
    }

    #[test]
    fn wrong_length_hex_is_an_error() {
        // 32 字节要求 64 个十六进制字符，短一位也不能悄悄放行。
        let cfg = config_with(vec![proxy("A", "11", &valid_priv())]);
        assert!(plan_of(&cfg).is_err());
    }

    /// **`wsieve-config` 的 `mux-prefs` 默认值必须全部是合法 `MuxId`。**
    ///
    /// 这是跨 crate 的一条契约，而两边谁都看不见对方：`wsieve-config` 依赖
    /// 不到 `wsieve-proto`，`wsieve-proto` 也不知道有人给它的枚举写了默认
    /// 列表。src-tauri 是唯一同时看得到两者的地方，所以守卫只能立在这里。
    ///
    /// 破了的后果不是「少一种复用器」：`ProxyForm` 添加服务器时从不写
    /// `mux-prefs`，每个从界面新建的出站都吃这个默认值，一旦其中有非法项，
    /// `build_startup_plan` 就会失败、`main` 随即 `exit(2)`——用户看到的是
    /// 「加完服务器，应用再也打不开了」，而配置文件本身看着一切正常。
    /// （这正是 2026-09-08 之前 `[0, 1, 2, 3, 4]` 那份默认值的真实行为。）
    #[test]
    fn the_crate_default_mux_prefs_are_all_valid_mux_ids() {
        let default_proxy: wsieve_config::Proxy = wsieve_config::load_str(
            "proxies:\n  - name: \"A\"\n    type: websieve\n    \
             url: https://a.example/\n    server-pub: \"x\"\n    client-priv: \"y\"\n",
        )
        .unwrap()
        .proxies
        .remove(0);
        for id in &default_proxy.mux_prefs {
            assert!(
                wsieve_proto::hello::MuxId::from_u8(*id).is_some(),
                "wsieve-config 的 mux-prefs 默认值含非法 MuxId {id}——\
                 从界面新建的出站会全部落到这份默认值上，下次启动直接 exit(2)"
            );
        }
        assert!(!default_proxy.mux_prefs.is_empty(), "空偏好列表握不了手");
    }

    /// 非法 `mux-prefs` 仍要报错（而不是静默丢弃那一项）——上面那条守的是
    /// 默认值本身，这条守的是「用户手写了一个非法值」时的行为。
    #[test]
    fn an_invalid_mux_pref_is_reported_with_the_outbound_name() {
        let mut p = proxy("A", &valid_pub(), &valid_priv());
        p.mux_prefs = vec![1, 99];
        let cfg = config_with(vec![p]);
        let err = plan_of(&cfg).unwrap_err().to_string();
        assert!(err.contains("mux-prefs"), "{err}");
        assert!(err.contains('A'), "错误要点名是哪个出站：{err}");
    }

    /// 出站 URL 合法性校验：这道校验以前长在 `outbound::carrier::validate`
    /// 里（承载页的 URL 曾经就是出站 URL 的 origin，非法 URL 在那一步现形）。
    /// 承载页挪到本机 http 壳之后，承载计划不再摸出站 URL，`build_startup_plan`
    /// 因此接手了这道校验，复用 `shard_setup::origin_of`——非法 URL 必须在
    /// 启动时就报错并点名是哪个出站，而不是被 `ShardPlanEntry::degraded` 悄悄
    /// 退回原始字符串、一路滑到运行时才在握手阶段炸给用户一个不知所云的错误。
    ///
    /// 四种畸形形状照抄被删掉的 `outbound::carrier::tests::
    /// malformed_url_is_an_error_not_an_empty_base`——那条测试连同它守的
    /// `carrier::origin_of` 一起被删，覆盖不能跟着丢：`shard_setup::origin_of`
    /// 复用的是同一份四条规则（缺 scheme / 不支持的 scheme / 缺主机 /
    /// 带 userinfo），四种都要在启动时被拒，而不是只剩「缺 scheme」这一种。
    #[test]
    fn a_malformed_proxy_url_is_rejected_with_the_outbound_name() {
        for bad in [
            "a.com",              // 缺 scheme
            "ftp://a.com/",       // 不支持的 scheme
            "https:///path",      // 缺主机
            "https://u:p@a.com/", // 带凭据
        ] {
            let mut p = proxy("A", &valid_pub(), &valid_priv());
            p.url = bad.to_string();
            let cfg = config_with(vec![p]);
            let err = plan_of(&cfg).unwrap_err().to_string();
            assert!(err.contains('A'), "{bad}: 错误要点名是哪个出站：{err}");
            assert!(err.contains("URL"), "{bad}: 错误要说明是 URL 不合法：{err}");
        }
    }

    #[test]
    fn two_proxies_with_shared_carrier_share_one_window_label() {
        let cfg = config_with(vec![
            proxy("A", &valid_pub(), &valid_priv()),
            proxy("B", &valid_pub(), &valid_priv()),
        ]);
        let plan = plan_of(&cfg).unwrap();
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
        let plan = plan_of(&cfg).unwrap();
        let carrier = plan.carrier.unwrap();
        assert_ne!(carrier.window_label("A"), carrier.window_label("B"));
    }

    /// **承载页面 URL 是调用方注入的 `carrier_page_url`，与条带完全无关。**
    ///
    /// 承载页挪到本机 http 壳之后，窗口加载的地址不再从条带推导——刻意让
    /// shard 的 session_bases 落在另一个域名上，确认承载窗口不会被这些
    /// 数据面地址污染，也不会去抄它们。
    #[test]
    fn the_carrier_page_is_the_injected_url_independent_of_the_shard() {
        let cfg = config_with(vec![proxy("A", &valid_pub(), &valid_priv())]);
        let shard = vec![crate::shard_setup::ShardPlanEntry {
            session_bases: vec![
                Some("https://a.example:18443".to_string()),
                Some("https://a.example:18444".to_string()),
            ],
            upstream: None,
            bypass_error: None,
        }];
        let plan = build_startup_plan(&cfg, &shard, "http://127.0.0.1:53119/").unwrap();
        let carrier = plan.carrier.expect("有出站就该有承载计划");
        assert_eq!(
            carrier.windows(),
            vec![("main".to_string(), "http://127.0.0.1:53119/".to_string())],
            "承载窗口该加载调用方注入的本机 http 壳地址，不是条带的数据面 origin"
        );
        // session_bases 原样透传，与承载页 URL 互不干扰。
        assert_eq!(
            plan.outbound_cfgs[0].session_bases,
            vec![
                Some("https://a.example:18443".to_string()),
                Some("https://a.example:18444".to_string()),
            ]
        );
    }

    /// 会话基址必须**逐字**来自条带，一项都不能被承载计划改写。
    ///
    /// 改写回来的后果不是「基址不好看」：承载页是本机那张空 HTML，被改写
    /// 的会话会把 Noise 握手发给它，握手拿到一段 HTML 后解析失败。
    #[test]
    fn session_bases_come_from_the_shard_verbatim() {
        let cfg = config_with(vec![
            proxy("A", &valid_pub(), &valid_priv()),
            proxy("B", &valid_pub(), &valid_priv()),
        ]);
        let shard = vec![
            crate::shard_setup::ShardPlanEntry {
                session_bases: vec![
                    Some("https://a.example:18443".to_string()),
                    Some("https://a.example:18444".to_string()),
                ],
                upstream: None,
                bypass_error: None,
            },
            crate::shard_setup::ShardPlanEntry {
                session_bases: vec![Some("https://b.example:18450".to_string())],
                upstream: None,
                bypass_error: None,
            },
        ];
        let plan = build_startup_plan(&cfg, &shard, "http://127.0.0.1:53119/").unwrap();
        assert_eq!(plan.outbound_cfgs[0].session_bases, shard[0].session_bases);
        assert_eq!(plan.outbound_cfgs[1].session_bases, shard[1].session_bases);
        for cfg in &plan.outbound_cfgs {
            for b in &cfg.session_bases {
                let b = b.as_ref().expect("没有任何会话还能吃相对路径");
                assert!(b.starts_with("https://"), "数据面必须留在 https：{b}");
            }
        }
    }

    /// 下标错位就是把流量发去另一台服务器，必须报错而不是按短的那个截断。
    #[test]
    fn a_shard_result_of_the_wrong_length_is_rejected() {
        let cfg = config_with(vec![
            proxy("A", &valid_pub(), &valid_priv()),
            proxy("B", &valid_pub(), &valid_priv()),
        ]);
        let short = vec![crate::shard_setup::ShardPlanEntry {
            session_bases: vec![None],
            upstream: None,
            bypass_error: None,
        }];
        assert!(build_startup_plan(&cfg, &short, "http://127.0.0.1:53119/").is_err());
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
             \x20\x20\x20\x20mux-prefs: [1]\n\
             rules:\n\
             \x20\x20- DOMAIN,a.com,A\n\
             \x20\x20- MATCH,A\n",
            valid_pub(),
            valid_priv()
        );
        let cfg = wsieve_config::load_str(&text).unwrap();
        let plan = plan_of(&cfg).unwrap();
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
        let plan = plan_of(&cfg).unwrap();
        assert_eq!(plan.rule_lines, vec!["MATCH,REJECT".to_string()]);
    }

    #[test]
    fn the_implicit_reject_fallback_actually_satisfies_rule_set_build() {
        // 这是本函数存在的核心动机：零规则的默认配置不能在
        // RuleSet::build 这一步把整个启动炸掉。
        let cfg = config_with(vec![]);
        let plan = plan_of(&cfg).unwrap();
        let known = std::collections::HashSet::new();
        wsieve_route::RuleSet::build(&plan.rule_lines, plan.mode, &plan.global_outbound, &known)
            .expect("空规则应当能通过隐式 REJECT 兜底通过 RuleSet::build");
    }

    #[test]
    fn an_invalid_mode_string_is_reported_not_panicked() {
        let mut cfg = config_with(vec![]);
        cfg.mode = "bogus".to_string();
        assert!(plan_of(&cfg).is_err());
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
            mux_prefs: vec![MuxId::Wsmux],
            session_bases: vec![None],
            ip_strategy: wsieve_proto::hello::IpStrategy::Auto,
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
