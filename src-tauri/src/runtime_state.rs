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
}

/// 代理组表——本计划只放占位结构，真正的构建逻辑属于 Part 3。
#[derive(Default)]
pub struct GroupTable;

/// 一次配置更新里，出站集合要如何从旧的过渡到新的。
///
/// 用 `Arc<OutboundInstance>` 而非 `OutboundCfg` 表示"复用"，是因为复用
/// 的重点就是**不重新构造实例**——上层拿到这个结构后，`reused` 里的每一项
/// 直接原样放进新 `OutboundManager`，`added` 里的每一项才需要真的
/// `OutboundInstance::new(cfg)`。
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
