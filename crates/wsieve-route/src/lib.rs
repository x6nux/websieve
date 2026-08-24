//! Clash 语法的分流规则解析与判决引擎。
//!
//! 全部同步、无网络、无 IO —— 唯一的外部交互是查 GeoDb，而那是只读的。
//! 这条纪律让整个判决逻辑可被穷举单测，也让 UI 的「规则试算」能复用
//! 同一份代码，保证试算结果与真实判决永远一致
//! （见设计文档 §4.2 纪律①）。
//!
//! DNS 解析不在本 crate 内发生。需要解析时，evaluate 会返回
//! Verdict::NeedResolve 把需求抛给调用方，由调用方在 async 上下文里
//! 解析后再调一轮。

pub mod engine;
pub mod rule;

pub use engine::{Decision, RuleSet, Verdict};
pub use rule::{Mode, Rule, RuleError, RuleKind, Target};
