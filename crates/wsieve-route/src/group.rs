//! 代理组的挑选逻辑（设计文档「代理组与首页视图」§2）。
//!
//! **不依赖 `wsieve-config`**——与 `RuleSet::build` 只吃 `&[String]` /
//! `&HashSet<String>` 是同一条解耦纪律：本 crate 只认识「规则语法」与
//! 「一组带延迟的候选名字」，不认识 YAML schema。调用方（未来真正接线时）
//! 自己把 `ProxyGroup` 拆成这里要的原始类型。
//!
//! **暂时没有运行时调用点**——main.rs 还不消费 config.yaml，与
//! `rule::CHINA_PRESET_RULES` 同一处境。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LbStrategy {
    ConsistentHash,
    RoundRobin,
}

/// auto：挑延迟最小的非 `None` 成员。全 `None`（一个都没测过延迟）时
/// 退回第一个成员——给一个可预测的默认值，而不是让调用方处理「挑不出」。
///
/// `members` 为空时 panic：一个零成员的组本该在配置校验阶段就被拒绝
/// （`wsieve-config` 的 `validate()`），这里把「非空」当成调用方保证的前提，
/// 而不是再报一次错——两层各管一段，不重复。
pub fn auto_pick(members: &[(String, Option<u64>)]) -> &str {
    members
        .iter()
        .filter(|(_, lat)| lat.is_some())
        .min_by_key(|(_, lat)| lat.unwrap())
        .or_else(|| members.first())
        .map(|(name, _)| name.as_str())
        .expect("空成员列表——上游 validate() 应已挡住零成员的组")
}

/// load-balance：
///   `ConsistentHash` 对 `key`（目标 host）取哈希取模，同一 host 稳定落在
///   同一个成员上，不消耗 `rr_counter`。
///   `RoundRobin` 用调用方传入的可变计数器递增取模，`rr_counter` 的初值
///   由调用方决定（可以是每次从 0 开始，也可以是上一次留下的状态）。
pub fn load_balance_pick(
    members: &[String],
    strategy: LbStrategy,
    key: &str,
    rr_counter: &mut usize,
) -> String {
    assert!(!members.is_empty(), "空成员列表——上游 validate() 应已挡住零成员的组");
    match strategy {
        LbStrategy::ConsistentHash => {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            key.hash(&mut hasher);
            let idx = (hasher.finish() as usize) % members.len();
            members[idx].clone()
        }
        LbStrategy::RoundRobin => {
            let idx = *rr_counter % members.len();
            *rr_counter += 1;
            members[idx].clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(name: &str, latency: Option<u64>) -> (String, Option<u64>) {
        (name.to_string(), latency)
    }

    #[test]
    fn auto_pick_chooses_the_lowest_latency() {
        let members = [m("a", Some(80)), m("b", Some(30)), m("c", Some(120))];
        assert_eq!(auto_pick(&members), "b");
    }

    #[test]
    fn auto_pick_ignores_unmeasured_members() {
        let members = [m("a", None), m("b", Some(30)), m("c", None)];
        assert_eq!(auto_pick(&members), "b");
    }

    #[test]
    fn auto_pick_falls_back_to_the_first_member_when_nothing_is_measured() {
        // 全 None 时给一个可预测的默认值，而不是让调用方处理「挑不出」。
        let members = [m("a", None), m("b", None)];
        assert_eq!(auto_pick(&members), "a");
    }

    #[test]
    fn auto_pick_breaks_ties_by_the_first_minimum() {
        let members = [m("a", Some(50)), m("b", Some(50))];
        assert_eq!(auto_pick(&members), "a");
    }

    #[test]
    fn load_balance_consistent_hash_is_stable_for_the_same_key() {
        let members = ["a".to_string(), "b".to_string(), "c".to_string()];
        let mut rr = 0usize;
        let first = load_balance_pick(&members, LbStrategy::ConsistentHash, "example.com", &mut rr);
        let second = load_balance_pick(&members, LbStrategy::ConsistentHash, "example.com", &mut rr);
        assert_eq!(first, second, "同一个目标 host 每次都该落到同一个成员上");
    }

    #[test]
    fn load_balance_consistent_hash_does_not_touch_the_round_robin_counter() {
        let members = ["a".to_string(), "b".to_string()];
        let mut rr = 0usize;
        load_balance_pick(&members, LbStrategy::ConsistentHash, "x.com", &mut rr);
        load_balance_pick(&members, LbStrategy::ConsistentHash, "y.com", &mut rr);
        assert_eq!(rr, 0, "consistent-hash 不消耗 round-robin 状态");
    }

    #[test]
    fn load_balance_round_robin_cycles_through_every_member() {
        let members = ["a".to_string(), "b".to_string(), "c".to_string()];
        let mut rr = 0usize;
        let picks: Vec<String> = (0..5)
            .map(|_| load_balance_pick(&members, LbStrategy::RoundRobin, "irrelevant", &mut rr))
            .collect();
        assert_eq!(picks, vec!["a", "b", "c", "a", "b"], "轮询应严格按顺序回绕");
    }

    #[test]
    fn load_balance_round_robin_starts_fresh_from_whatever_counter_it_is_given() {
        let members = ["a".to_string(), "b".to_string()];
        let mut rr = 3usize; // 调用方可能带着上一次的状态进来
        let pick = load_balance_pick(&members, LbStrategy::RoundRobin, "x", &mut rr);
        assert_eq!(pick, "b"); // 3 % 2 == 1
        assert_eq!(rr, 4);
    }
}
