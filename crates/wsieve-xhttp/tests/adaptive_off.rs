//! `WSIEVE_ADAPTIVE=off` 必须精确回到改造前的那一套硬编码值。
//!
//! 单独一个文件（= 单独一个测试二进制）且只放一个 `#[test]`，是因为
//! `adaptive_enabled()` 用 `OnceLock` 缓存。同一个二进制里再放第二个测试，
//! 谁先跑就决定了那个缓存值，另一个随机失败——这类测试的顺序依赖比它想
//! 防的 bug 更难查。

use std::time::Duration;

use wsieve_xhttp::link_profile::{FlowKind, LinkProfile, ProfileSnapshot};

#[test]
fn the_kill_switch_pins_every_parameter_to_the_pre_refactor_defaults() {
    // 必须在任何一次 `adaptive_enabled()` 之前设好，否则 OnceLock 已经定了。
    unsafe { std::env::set_var("WSIEVE_ADAPTIVE", "off") };

    let p = LinkProfile::new();

    // 1) 历史快照：这一份的 BDP = 0.2s × 50MB/s = 10MB，足够跳到最高档。
    //    `adopt_history` 自己不判开关（它只改内部状态），关掉时必须读不出来。
    p.adopt_history(ProfileSnapshot {
        min_rtt_us: 200_000,
        delivery_bps: 50e6,
    });

    // 2) 实测样本：高带宽 + 低 RTT，两个方向都往上顶，且足够越过
    //    MIN_BW_SAMPLES 的门。开着的话这里一定会换档。
    for _ in 0..64 {
        p.observe_post(Duration::from_millis(200), 2_000_000, false);
        p.observe_downlink(50_000_000, Duration::from_secs(1));
    }

    // 3) 低 RTT 样本：开着的话会把 merge 清零（MERGE_RTT_FLOOR 那条规则）。
    for _ in 0..8 {
        p.observe_post(Duration::from_micros(300), 1_000, false);
    }

    // 逐字段写死字面量，而不是和 `BULK_TIERS[DEFAULT_TIER]` 比。
    // 和表里的某一行比只能证明"和表一致"；改造前的基线是一组具体数字，
    // 有人顺手改了那一行的话，两边会一起动而测试照样绿。
    for flow in [FlowKind::Bulk, FlowKind::Interactive] {
        let t = p.tier(flow);
        assert_eq!(t.inflight, 8, "{flow:?}: max_inflight 的改造前默认值");
        assert_eq!(t.agg_bytes, 64_000, "{flow:?}: aggregate_bytes 的改造前默认值");
        assert_eq!(t.agg_wait, Duration::from_millis(4), "{flow:?}: AGGREGATE_MS");
        assert_eq!(t.merge, Duration::from_micros(400), "{flow:?}: merge_window");
        assert_eq!(t.window, 4 * 1024 * 1024, "{flow:?}: wsmux 窗口");
    }

    // 对照：测量本身不该被开关停掉。开关关的是"换档"，不是"采样"——
    // 排障时还得靠日志里的画像数字，而持久化的快照下次开着时要能用。
    assert!(
        p.bdp().is_some(),
        "关掉自适应不该连测量一起停：快照与日志都还要用"
    );
}
