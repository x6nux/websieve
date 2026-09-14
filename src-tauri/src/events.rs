//! 事件聚合与节流（设计文档 §11.2）。
//!
//! 为什么必须聚合：实时连接每秒可达数百条，逐条 emit 会直接卡死 WebView。
//! 每个 IPC 事件都要序列化 + 跨进程边界 + 触发一次 JS 回调 + 一次 Svelte
//! 响应式更新，几百次/秒的量级下 WebView 会失去响应 —— 而那正是用户最想
//! 盯着看的负载。
//!
//! 三条设计决定：
//!   ① traffic 推**累计值 + 速率**，不只推增量。UI 从增量重建累计会漂移
//!      （丢一个 tick 就永久偏差），累计值幂等。速率由 Rust 算，因为只有
//!      Rust 知道真实采样间隔。
//!   ② connection 队列有上限，且溢出**必须可见**（dropped 标记）。无界队列
//!      在洪泛时吃光内存；静默丢弃会让 UI 的连接数说谎 —— 违反房规。
//!   ③ rule-hit 的键是**规则原文**而非下标。用户删一条规则会让后面所有下标
//!      整体错位，重启后恢复的热度就全错了。
//!
//! ## 节流窗口里的事件去哪了
//!
//! 三条通道的答案不同，各有各的理由：
//!
//! | 通道 | 窗口内的中间事件 | 为什么 |
//! |---|---|---|
//! | `traffic` | **合并**（计数器自累加） | 采的是单调计数器的快照，中间值天然被吸收，一个字节都不会丢 |
//! | `rule-hit` | **合并**（增量 = 本轮快照 − 上轮快照） | 同上；命中数只增不减，合并无损 |
//! | `connection` | **有界排队**，满了丢**最旧**的并置标记 | 每条连接的开/关是离散事实，合并不了；无界排队会吃光内存 |
//!
//! ## 尾事件必达
//!
//! 一次突发的**最后一条**事件必须到达 UI，否则界面会永久停在陈旧状态 ——
//! 连接明明已经关了，行还写着「活跃」。两条机制保证它：
//!
//! 1. **节流是「定期收口」而非「首事件后静音」。** `connection_loop` 每
//!    200ms 无条件醒一次，跳过的唯一条件是「队列真的空」（`connection_tick`
//!    返回 `None`），而不是「刚才推过了」。基于时间近因的抑制会把突发尾巴
//!    咽掉；这里没有那种抑制。守它的是
//!    `a_burst_tail_is_flushed_on_the_next_tick`。
//! 2. **溢出丢最旧而非最新。** 队列满时新事件仍然入队，被挤掉的是队头。
//!    反过来做的话，连接的 `Close` 会先于其 `Open` 被丢 —— UI 上那一行
//!    永远显示「活跃」。守它的是 `terminal_state_survives_a_burst_overflow`。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{Emitter, EventTarget, Manager};

/// 节流周期 —— 设计文档 §11.2 的表格。
pub const TRAFFIC_TICK: Duration = Duration::from_secs(1);
pub const CONNECTION_TICK: Duration = Duration::from_millis(200);
pub const RULE_HIT_TICK: Duration = Duration::from_secs(1);

// ── 流量 ────────────────────────────────────────────────────────

/// 用原子而非 Mutex：写方是代理数据路径（每个 chunk 都要记账），
/// 读方是采样循环。这是唯一一处真的会被高频并发触碰的计数。
#[derive(Debug, Default)]
pub struct Counters {
    pub up_total: AtomicU64,
    pub down_total: AtomicU64,
    pub active: AtomicU64,
}

impl Counters {
    /// 逐项 `allow(dead_code)` 而非整模块开：写方是代理数据路径
    /// （router / shard 的记账点，Task 9 接线），此刻还没接上。
    /// 读方（`sample`）与本模块的循环已经在用了，且这几个方法都有测试
    /// 覆盖 —— 这不是「写了没用的代码」，是「用它的那一头还没到」。
    #[allow(dead_code)]
    pub fn add_up(&self, n: u64) {
        self.up_total.fetch_add(n, Ordering::Relaxed);
    }
    #[allow(dead_code)]
    pub fn add_down(&self, n: u64) {
        self.down_total.fetch_add(n, Ordering::Relaxed);
    }
    #[allow(dead_code)]
    pub fn conn_opened(&self) {
        self.active.fetch_add(1, Ordering::Relaxed);
    }
    /// 用 CAS 而非 fetch_sub：连接计数下溢会变成 u64::MAX，
    /// UI 上显示「活跃连接 18446744073709551615」比显示 0 更糟。
    #[allow(dead_code)]
    pub fn conn_closed(&self) {
        let _ = self
            .active
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
    }
}

#[derive(Debug, Default)]
pub struct TrafficPrev {
    up: u64,
    down: u64,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct TrafficSample {
    /// 累计值（幂等，UI 随时能对齐）
    pub up_bytes: u64,
    pub down_bytes: u64,
    /// 本采样周期内的增量（速率）
    pub up_rate: u64,
    pub down_rate: u64,
    pub active: u64,
}

pub fn sample(c: &Counters, prev: &mut TrafficPrev) -> TrafficSample {
    let up = c.up_total.load(Ordering::Relaxed);
    let down = c.down_total.load(Ordering::Relaxed);
    // saturating_sub：计数器被重置（重连/配置重载）时不下溢成天文数字
    let s = TrafficSample {
        up_bytes: up,
        down_bytes: down,
        up_rate: up.saturating_sub(prev.up),
        down_rate: down.saturating_sub(prev.down),
        active: c.active.load(Ordering::Relaxed),
    };
    prev.up = up;
    prev.down = down;
    s
}

// ── 连接 ────────────────────────────────────────────────────────

/// 连接的三种状态。`Reject` 与 `Close` 都是终态（见 `is_terminal`）。
///
/// 整个 enum 一起 `allow(dead_code)`：三个变体是同一件事的三个面，
/// 构造方是路由分派（Task 9 接线）。测试里三个都构造并断言过。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnState {
    Open,
    Close,
    Reject,
}

impl ConnState {
    /// 终态 —— 这条连接不会再有后续事件了。
    ///
    /// 之所以要把它单独标出来：终态事件丢失的后果与中间事件丢失完全不同。
    /// 丢一条 `Open`，UI 少画一行；丢一条 `Close`，UI 上那一行**永远**
    /// 停在「活跃」，且没有任何后续事件能纠正它。
    #[allow(dead_code)]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Close | Self::Reject)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConnectionDelta {
    pub id: u64,
    pub target: String,
    /// 出站名，或 "DIRECT" / "REJECT"
    pub outbound: String,
    pub state: ConnState,
}

/// 有界队列。满了丢最旧的，并置溢出标记。
pub struct ConnectionQueue {
    buf: Mutex<Vec<ConnectionDelta>>,
    overflow: AtomicBool,
}

impl Default for ConnectionQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionQueue {
    /// 200ms 一批。CAP 定在 512 是「洪泛时 UI 还能画得动」与
    /// 「正常负载下永不触顶」的折衷 —— 512 条/200ms = 2560 条/秒，
    /// 远超正常上限。
    ///
    /// ponytail: 用 Vec + remove(0)（O(n)）而非 VecDeque。
    /// 上限：只有在持续触顶时才有 O(n) 开销，而那时 UI 本就画不过来。
    /// 升级路径：真成瓶颈就换 VecDeque，接口不变。
    #[allow(dead_code)]
    pub const CAP: usize = 512;

    pub fn new() -> Self {
        Self {
            buf: Mutex::new(Vec::new()),
            overflow: AtomicBool::new(false),
        }
    }

    /// 入队。满时挤掉**队头**（最旧的），新事件一定进得来。
    ///
    /// 方向是有讲究的：反过来「满了就丢新的」看似也合理，但那会让连接的
    /// 终态（Close / Reject）在洪泛时优先蒸发 —— 而终态恰恰是 UI 唯一
    /// 能靠它把一行从「活跃」改掉的东西。见模块注释「尾事件必达」。
    ///
    /// 入队方是路由分派（Task 9 接线）；出队方（`connection_tick`）已在用。
    #[allow(dead_code)]
    pub fn push(&self, d: ConnectionDelta) {
        let mut g = self.buf.lock().unwrap();
        if g.len() >= Self::CAP {
            self.overflow.store(true, Ordering::Relaxed);
            g.remove(0);
        }
        g.push(d);
    }

    /// 取走全部待推项，并把溢出标记一并取走（读后清零）。
    pub fn drain(&self) -> (Vec<ConnectionDelta>, bool) {
        let mut g = self.buf.lock().unwrap();
        let v = std::mem::take(&mut *g);
        (v, self.overflow.swap(false, Ordering::Relaxed))
    }
}

/// emit_to 要求 Serialize + **Clone**（实测：只 derive Serialize 会 E0277）
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConnectionBatch {
    /// 保持入队顺序：同一条连接的 Open 必须排在它的 Close 前面，
    /// 否则 UI 会先收到「关闭一个不存在的行」再收到「新增该行」，
    /// 那一行就此卡死在活跃态。
    pub items: Vec<ConnectionDelta>,
    /// true 表示这一批之前有连接因队列满被丢弃 —— UI 应显示「部分未展示」
    pub dropped: bool,
}

/// 一次 connection 节流周期该推什么。`None` = 这一轮确实没有东西要推。
///
/// 判决抽成纯函数而不是埋在 `loop` 里，是为了让「跳过的条件是队列空、
/// 而不是刚才推过」这件事能被直接断言。基于时间近因的抑制会咽掉突发的
/// 尾批 —— 那正是本模块要避免的失败模式。
pub fn connection_tick(queue: &ConnectionQueue) -> Option<ConnectionBatch> {
    let (items, dropped) = queue.drain();
    if items.is_empty() && !dropped {
        return None;
    }
    Some(ConnectionBatch { items, dropped })
}

// ── 规则命中 ────────────────────────────────────────────────────

/// 键是**规则原文**，不是下标。见模块注释的决定 ③。
///
/// 值是裸 `u64` 而非 `AtomicU64`：每一次读写都在 `Mutex` 之下，原子性
/// 已由锁提供，再套一层原子只是噪音。
#[derive(Debug, Default)]
pub struct RuleHits {
    counters: Mutex<HashMap<String, u64>>,
}

impl RuleHits {
    /// 判决路径上调用 —— 必须便宜。快路径不分配 String。
    ///
    /// 调用方是规则判决（Task 9 接线）；读方（`hit_delta`）已在用。
    #[allow(dead_code)]
    pub fn bump(&self, rule: &str) {
        let mut g = self.counters.lock().unwrap();
        // 绝大多数调用命中已存在的键：只自增，不分配
        if let Some(c) = g.get_mut(rule) {
            *c += 1;
            return;
        }
        g.insert(rule.to_string(), 1);
    }

    pub fn snapshot(&self) -> HashMap<String, u64> {
        self.counters.lock().unwrap().clone()
    }

    /// 启动时从 stats.json 恢复。**覆盖**而非累加 —— 累加的话每重启一次
    /// 热度就翻一倍，几次之后数字就成了纯噪音。
    ///
    /// 调用方是命中计数持久化（Task 10 接线）。
    #[allow(dead_code)]
    pub fn restore(&self, saved: HashMap<String, u64>) {
        let mut g = self.counters.lock().unwrap();
        for (k, v) in saved {
            g.insert(k, v);
        }
    }
}

/// 算增量并更新 prev。返回空 map 表示这一轮没有新命中（调用方应跳过推送）。
///
/// 注意增量是从**单调计数器的两次快照**算出来的，不是从一个会被清空的
/// 缓冲区里取的：窗口内的多次命中天然合并成一个数，一次都不会丢。
pub fn hit_delta(h: &RuleHits, prev: &mut HashMap<String, u64>) -> HashMap<String, u64> {
    let now = h.snapshot();
    let delta = now
        .iter()
        .filter_map(|(k, v)| {
            let d = v.saturating_sub(prev.get(k).copied().unwrap_or(0));
            (d > 0).then(|| (k.clone(), d))
        })
        .collect();
    *prev = now;
    delta
}

// ── 聚合器 ──────────────────────────────────────────────────────

/// 三条节流循环的宿主。作为 Tauri managed state 供命令与代理侧写入。
pub struct Aggregator {
    pub counters: Arc<Counters>,
    pub hits: Arc<RuleHits>,
    pub queue: Arc<ConnectionQueue>,
}

impl Default for Aggregator {
    fn default() -> Self {
        Self::new()
    }
}

impl Aggregator {
    pub fn new() -> Self {
        Self {
            counters: Arc::new(Counters::default()),
            hits: Arc::new(RuleHits::default()),
            queue: Arc::new(ConnectionQueue::new()),
        }
    }

    /// 启动三条节流循环。三条都是 `loop { tick; drain; emit }`，
    /// 永不退出 —— 与代理主循环同生命周期。
    pub fn spawn(&self, app: tauri::AppHandle) {
        let counters = self.counters.clone();
        let a = app.clone();
        tauri::async_runtime::spawn(async move {
            traffic_loop(counters, move |s| emit_control(&a, "traffic", s)).await
        });

        let queue = self.queue.clone();
        let a = app.clone();
        tauri::async_runtime::spawn(async move {
            connection_loop(queue, move |b| emit_control(&a, "connection", b)).await
        });

        let hits = self.hits.clone();
        tauri::async_runtime::spawn(async move {
            rule_hit_loop(hits, move |d| emit_control(&app, "rule-hit", d)).await
        });
    }
}

/// 建一个按周期醒来的 interval。
///
/// `Skip`：机器休眠唤醒后不要把攒下的 tick 一次性补发，那会瞬间推几十条
/// 事件 —— 正是本模块要避免的事。注意它跳过的是**空转的时钟 tick**，
/// 不是数据：计数器仍在累加，队列仍在收，下一次醒来照样全带走。
fn throttle_interval(period: Duration) -> tokio::time::Interval {
    let mut t = tokio::time::interval(period);
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    t
}

/// 三条循环都把「往哪推」做成参数。这样循环体本身（周期、跳过条件、
/// 尾批处理）能在测试里被真的驱动一遍 —— `tauri::async_runtime` 用的是
/// 自己的全局运行时，不受 `tokio::time::pause()` 影响，循环若写死在
/// spawn 里就只能靠肉眼看界面有没有在跳。
async fn traffic_loop<F>(counters: Arc<Counters>, mut emit: F)
where
    F: FnMut(TrafficSample) + Send,
{
    let mut prev = TrafficPrev::default();
    let mut t = throttle_interval(TRAFFIC_TICK);
    loop {
        t.tick().await;
        // 即使没人看也要采样：prev 必须持续推进，否则窗口一打开
        // 第一个速率会是「关窗期间的总量」这种荒谬值。
        emit(sample(&counters, &mut prev));
    }
}

async fn connection_loop<F>(queue: Arc<ConnectionQueue>, mut emit: F)
where
    F: FnMut(ConnectionBatch) + Send,
{
    let mut t = throttle_interval(CONNECTION_TICK);
    loop {
        t.tick().await;
        // 跳过的条件只有「队列空」这一个。没有任何「刚推过就静音」的分支 ——
        // 那种抑制会把突发的尾批咽掉，UI 就停在陈旧状态了。
        if let Some(batch) = connection_tick(&queue) {
            emit(batch);
        }
    }
}

async fn rule_hit_loop<F>(hits: Arc<RuleHits>, mut emit: F)
where
    F: FnMut(HashMap<String, u64>) + Send,
{
    let mut prev = HashMap::new();
    let mut t = throttle_interval(RULE_HIT_TICK);
    loop {
        t.tick().await;
        let delta = hit_delta(&hits, &mut prev);
        if delta.is_empty() {
            continue;
        }
        emit(delta);
    }
}

/// 推给控制窗口，且**只推给它**。
///
/// 用 `emit_to(webview_window("control"))` 而非 `emit()` 不是优化，是安全：
/// `emit()` 无条件广播给所有 webview，包括加载远端服务器页面的 main 窗口。
/// 那等于把流量统计、规则命中、出站名单白送给一台可能已被攻破的服务器。
///
/// **但要把话说准：`emit_to` 本身不是一道密不透风的墙。** 实测确认
/// （`an_any_target_listener_still_receives_targeted_events` 记下了这个事实）：
/// `manager/mod.rs` 的过滤走 `match_any_or_filter`，第一个分支是
/// `*target == EventTarget::Any || ...` —— 也就是说以 `Any` 为 target 注册的
/// 监听者会**绕过定向过滤**收到事件。而 JS 侧 `listen(event, handler)` 不传
/// `options.target` 时，注册的正是 `{kind:"Any"}`（tauri 的 bundle.global.js
/// 里 `null!==t?.target ? ... : {kind:"Any"}`）。
///
/// 所以真正挡住远端页面的是**分层**的，`emit_to` 只是最外面一层：
///   1. `emit_to` 定向 —— 挡住以具名 target 注册的监听者（含 Rust 侧
///      `WebviewWindow::listen`，它注册的是 `WebviewWindow{label}`）；
///   2. **传输 capability 里没有 `core:event:default`** —— 那三个
///      `wsieve_*` 之外一个命令都没有，远端页面连
///      `plugin:event|listen` 都调不动，根本注册不上任何监听者。
///      这一层由 `capability_isolation.rs` 的
///      `transport_can_only_reach_the_three_binary_channels` 守着；
///   3. `ui/emitter.js` 只 invoke 不 listen。
///
/// 第 2 层才是承重的。真要给传输窗口加 `core:event` 权限时，第 1 层挡不住
/// `listen()` 的默认 `Any` target —— 那时必须同时改这里。
///
/// 实测注意：控制窗口不存在时 `emit_to` **静默返回 Ok(())**，不报错。
/// 因此这里显式短路 —— 让「窗口关着」真的等于零开销，而不是白白序列化一轮。
///
/// 对 `R: Runtime` 泛型（而非写死 `Wry`）是为了让上面那些断言能在
/// MockRuntime 上真的建两个窗口跑一遍，而不是只靠肉眼。
pub fn emit_control<R: tauri::Runtime, S: Serialize + Clone>(
    app: &tauri::AppHandle<R>,
    event: &str,
    payload: S,
) {
    if app.get_webview_window(crate::control::LABEL).is_none() {
        return;
    }
    if let Err(e) = app.emit_to(
        EventTarget::webview_window(crate::control::LABEL),
        event,
        payload,
    ) {
        // 房规：错误绝不静默吞掉
        tracing::warn!("事件 {event} 推送失败：{e}");
    }
}

/// 低频事件直接推，不进聚合器（§11.2：「变化时推，天然低频」）。
#[allow(dead_code)]
pub fn emit_status<R: tauri::Runtime>(app: &tauri::AppHandle<R>, msg: &str) {
    tracing::info!("status: {msg}");
    emit_control(app, "status", msg.to_string());
}

#[derive(Serialize, Clone)]
pub struct OutboundState {
    pub name: String,
    /// "connecting" | "live" | "failed" | "disabled"
    pub state: String,
    pub latency_ms: Option<u64>,
}

/// 出站状态变化。调用方是出站实例的状态回调（Task 9 接线），
/// 逐项 `allow(dead_code)` 而非整模块开 —— 用它的那一头还没到。
#[allow(dead_code)]
pub fn emit_outbound_state<R: tauri::Runtime>(app: &tauri::AppHandle<R>, s: OutboundState) {
    emit_control(app, "outbound-state", s);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(id: u64) -> ConnectionDelta {
        ConnectionDelta {
            id,
            target: "example.com:443".into(),
            outbound: "日本节点".into(),
            state: ConnState::Open,
        }
    }

    fn closing(id: u64) -> ConnectionDelta {
        ConnectionDelta {
            state: ConnState::Close,
            ..delta(id)
        }
    }

    // ── 有界队列 ────────────────────────────────────────────────

    #[test]
    fn queue_drops_oldest_and_flags_overflow() {
        let q = ConnectionQueue::new();
        for i in 0..(ConnectionQueue::CAP as u64 + 10) {
            q.push(delta(i));
        }
        let (items, dropped) = q.drain();
        assert_eq!(items.len(), ConnectionQueue::CAP, "队列必须有上限");
        assert!(dropped, "溢出必须可见，不能静默丢弃");
        assert_eq!(items[0].id, 10, "丢的应该是最旧的");

        // drain 之后队列与标记都清空
        let (empty, flag) = q.drain();
        assert!(empty.is_empty());
        assert!(!flag, "溢出标记只报一次");
    }

    /// **这条是节流设计里最容易搞砸的一处。**
    ///
    /// 洪泛把队列冲爆时，一条连接的终态（Close / Reject）绝不能优先于
    /// 中间事件被丢弃 —— 终态是 UI 把一行从「活跃」改掉的唯一依据，
    /// 丢了就永久卡在陈旧状态。丢最旧保证了这一点：突发的尾巴总在。
    ///
    /// 把 `push` 里的 `g.remove(0)` 改成「满了直接 return（丢最新）」，
    /// 这条立刻变红。
    #[test]
    fn terminal_state_survives_a_burst_overflow() {
        let q = ConnectionQueue::new();
        // 先灌满：一大批中间事件
        for i in 0..(ConnectionQueue::CAP as u64 * 3) {
            q.push(delta(i));
        }
        // 突发的最后一件事：这条连接关了
        let last_id = 99_999;
        q.push(closing(last_id));

        let (items, dropped) = q.drain();
        assert!(dropped, "溢出必须可见");
        let tail = items.last().expect("批次不该为空");
        assert_eq!(tail.id, last_id, "突发的最后一条必须还在批尾");
        assert!(
            tail.state.is_terminal(),
            "终态事件被挤掉了 —— UI 上那一行会永远显示「活跃」"
        );
    }

    /// 批内顺序必须是入队顺序。同一连接的 Open 排在 Close 之后的话，
    /// UI 先收到「关一个不存在的行」再收到「新增该行」，那行就卡活跃态了。
    #[test]
    fn batch_preserves_enqueue_order() {
        let q = ConnectionQueue::new();
        q.push(delta(1));
        q.push(delta(2));
        q.push(closing(1));
        let (items, _) = q.drain();
        let seq: Vec<(u64, ConnState)> = items.iter().map(|d| (d.id, d.state)).collect();
        assert_eq!(
            seq,
            vec![
                (1, ConnState::Open),
                (2, ConnState::Open),
                (1, ConnState::Close)
            ],
            "批内必须保持入队顺序"
        );
    }

    /// 空队列不推空批 —— 否则 UI 每 200ms 白吃一次 IPC 往返。
    /// 但溢出标记本身就是必须送达的信息，即使这一轮没有任何条目。
    #[test]
    fn empty_tick_pushes_nothing_but_overflow_alone_still_does() {
        let q = ConnectionQueue::new();
        assert!(connection_tick(&q).is_none(), "空队列不该推空批");

        // 造一个「条目全被取走、只剩溢出标记」的局面
        for i in 0..(ConnectionQueue::CAP as u64 + 1) {
            q.push(delta(i));
        }
        {
            let mut g = q.buf.lock().unwrap();
            g.clear(); // 条目没了，但 overflow 标记还在
        }
        let batch = connection_tick(&q).expect("只剩溢出标记时也必须推");
        assert!(batch.items.is_empty());
        assert!(batch.dropped, "「有连接未显示」这件事不能咽掉");
    }

    // ── 节流循环：突发下的实际行为 ──────────────────────────────

    /// 突发期间的中间事件**合并成一批**，不是逐条推 —— 这是整个模块的
    /// 存在理由。1000 条连接事件挤在一个 200ms 窗口里，UI 只收到一次推送。
    #[tokio::test(start_paused = true)]
    async fn a_burst_is_coalesced_into_one_push_not_a_thousand() {
        let q = Arc::new(ConnectionQueue::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let qc = q.clone();
        let task = tokio::spawn(async move {
            connection_loop(qc, move |b| {
                let _ = tx.send(b);
            })
            .await
        });

        // interval 的第一个 tick 立即完成，先让它过去
        tokio::time::sleep(CONNECTION_TICK / 2).await;
        while rx.try_recv().is_ok() {}

        // 一个节流窗口内灌 1000 条
        for i in 0..1000u64 {
            q.push(delta(i));
        }
        tokio::time::sleep(CONNECTION_TICK * 2).await;

        let mut batches = Vec::new();
        while let Ok(b) = rx.try_recv() {
            batches.push(b);
        }
        assert_eq!(
            batches.len(),
            1,
            "1000 条事件必须合并成一次推送，实际推了 {} 次",
            batches.len()
        );
        assert_eq!(
            batches[0].items.len(),
            ConnectionQueue::CAP,
            "一批的条目数受 CAP 约束"
        );
        assert!(batches[0].dropped, "被削掉的部分必须让 UI 知道");
        task.abort();
    }

    /// **尾事件必达。**
    ///
    /// 突发在某个节流窗口的中途结束，此时队列里还压着最后几条 —— 下一次
    /// tick 必须把它们送出去。若节流实现成「推过一次就静音一段时间」，
    /// 这批尾巴就永远出不来，UI 停在陈旧状态。
    ///
    /// 验证过它会红：给 `connection_loop` 加一个「距上次推送不足 1s 就
    /// continue」的近因抑制分支，本测试立刻失败（收不到尾批）。
    #[tokio::test(start_paused = true)]
    async fn a_burst_tail_is_flushed_on_the_next_tick() {
        let q = Arc::new(ConnectionQueue::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let qc = q.clone();
        let task = tokio::spawn(async move {
            connection_loop(qc, move |b| {
                let _ = tx.send(b);
            })
            .await
        });

        tokio::time::sleep(CONNECTION_TICK / 2).await;
        while rx.try_recv().is_ok() {}

        // 第一段突发
        for i in 0..10u64 {
            q.push(delta(i));
        }
        tokio::time::sleep(CONNECTION_TICK).await;
        let first = rx.try_recv().expect("第一批必须到达");
        assert_eq!(first.items.len(), 10);

        // 突发的尾巴：紧接着又来一条，然后就没了
        let tail_id = 4242;
        q.push(closing(tail_id));
        tokio::time::sleep(CONNECTION_TICK).await;

        let tail = rx
            .try_recv()
            .expect("突发的尾批必须在下一个周期到达 —— 否则 UI 永远停在陈旧状态");
        assert_eq!(tail.items.len(), 1);
        assert_eq!(tail.items[0].id, tail_id);
        assert!(tail.items[0].state.is_terminal(), "尾巴正是那条终态事件");

        // 之后彻底安静：不该有空批继续冒出来
        tokio::time::sleep(CONNECTION_TICK * 3).await;
        assert!(rx.try_recv().is_err(), "队列空了就不该再推");
        task.abort();
    }

    /// 节流窗口内的 traffic 中间值是**合并**的，不是丢弃的：
    /// 一个窗口内加了三次，下一次采样的速率必须是三次之和，一个字节不差。
    #[tokio::test(start_paused = true)]
    async fn traffic_within_a_window_is_coalesced_not_dropped() {
        let c = Arc::new(Counters::default());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cc = c.clone();
        let task = tokio::spawn(async move {
            traffic_loop(cc, move |s| {
                let _ = tx.send(s);
            })
            .await
        });

        tokio::time::sleep(TRAFFIC_TICK / 2).await;
        while rx.try_recv().is_ok() {}

        // 同一个 1s 窗口内的三次记账
        c.add_up(100);
        c.add_up(200);
        c.add_up(700);
        tokio::time::sleep(TRAFFIC_TICK).await;

        let s = rx.try_recv().expect("必须收到一次采样");
        assert_eq!(s.up_rate, 1000, "窗口内的中间值必须合并，不能丢");
        assert_eq!(s.up_bytes, 1000, "累计值必须对得上");
        task.abort();
    }

    // ── 纯函数 ──────────────────────────────────────────────────

    #[test]
    fn rule_hits_accumulate_and_snapshot() {
        let h = RuleHits::default();
        h.bump("GEOSITE,cn,DIRECT");
        h.bump("GEOSITE,cn,DIRECT");
        h.bump("MATCH,日本节点");
        let s = h.snapshot();
        assert_eq!(s["GEOSITE,cn,DIRECT"], 2);
        assert_eq!(s["MATCH,日本节点"], 1);
    }

    #[test]
    fn rule_hits_restore_replaces_not_adds() {
        let h = RuleHits::default();
        h.bump("a");
        h.restore(HashMap::from([("a".to_string(), 100u64)]));
        assert_eq!(h.snapshot()["a"], 100, "恢复是覆盖，不是累加");
    }

    #[test]
    fn traffic_rate_is_difference_not_total() {
        let c = Counters::default();
        c.add_up(1000);
        let mut prev = TrafficPrev::default();
        let s1 = sample(&c, &mut prev);
        assert_eq!(s1.up_bytes, 1000);
        assert_eq!(s1.up_rate, 1000, "第一轮速率 = 全部累计");

        c.add_up(300);
        let s2 = sample(&c, &mut prev);
        assert_eq!(s2.up_bytes, 1300, "累计值必须单调");
        assert_eq!(s2.up_rate, 300, "速率是增量");
    }

    #[test]
    fn traffic_rate_never_goes_negative_on_counter_reset() {
        // 计数器被重置（重连、配置重载）时，saturating_sub 保证速率不下溢
        let c = Counters::default();
        c.add_up(500);
        let mut prev = TrafficPrev::default();
        sample(&c, &mut prev);
        c.up_total.store(0, Ordering::Relaxed);
        let s = sample(&c, &mut prev);
        assert_eq!(s.up_rate, 0, "计数器归零不该产生天文数字的速率");
    }

    /// 活跃连接数不会下溢。u64 的 0-1 是 18446744073709551615，
    /// UI 上显示那个数字比显示 0 更糟。
    #[test]
    fn active_connection_count_never_underflows() {
        let c = Counters::default();
        c.conn_closed(); // 没开过就关，比如启动时的残留清理
        assert_eq!(c.active.load(Ordering::Relaxed), 0);
        c.conn_opened();
        c.conn_closed();
        c.conn_closed();
        assert_eq!(c.active.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn rule_hit_delta_skips_unchanged_rules() {
        let h = RuleHits::default();
        h.bump("a");
        h.bump("b");
        let mut prev = HashMap::new();

        let d1 = hit_delta(&h, &mut prev);
        assert_eq!(d1.len(), 2);

        // 没有新命中 → 空增量（调用方据此跳过整次推送）
        let d2 = hit_delta(&h, &mut prev);
        assert!(d2.is_empty(), "没变化就不该推");

        h.bump("a");
        let d3 = hit_delta(&h, &mut prev);
        assert_eq!(d3, HashMap::from([("a".to_string(), 1u64)]), "只推变了的");
    }

    // ── 只推控制窗口 ────────────────────────────────────────────

    /// **这是安全断言，不是功能断言。**
    ///
    /// `emit()` 会广播给所有 webview，包括加载远端服务器页面的传输窗口。
    /// 那等于把流量统计、规则命中、出站名单白送给一台可能已被攻破的
    /// 服务器。这里真的建两个窗口、真的在两边挂监听、真的推一次，
    /// 断言只有控制侧收到。
    ///
    /// 覆盖的是**以具名 target 注册**的监听者（`WebviewWindow::listen`
    /// 注册的是 `WebviewWindow{label}`）。`Any` target 是另一回事，
    /// 见下面那条 characterization 测试。
    ///
    /// 把 `emit_control` 的 `emit_to(...)` 换成 `emit(...)`，本测试变红
    /// （已实测）。
    #[test]
    fn events_reach_the_control_window_and_never_the_transport_one() {
        use std::sync::atomic::AtomicUsize;
        use tauri::test::{mock_builder, mock_context, noop_assets};
        use tauri::Listener;

        let app = mock_builder()
            .build(mock_context(noop_assets()))
            .expect("mock app");
        let handle = app.handle().clone();

        // 控制窗口走真实的 open()，标签与 capability 才对得上
        crate::control::open(&handle).expect("控制窗口");
        // 传输窗口：与 main.rs 里承载 WebView 同标签
        tauri::webview::WebviewWindowBuilder::new(
            &handle,
            "main",
            tauri::WebviewUrl::App("index.html".into()),
        )
        .build()
        .expect("传输窗口");

        let on_control = Arc::new(AtomicUsize::new(0));
        let on_transport = Arc::new(AtomicUsize::new(0));

        let c = on_control.clone();
        handle
            .get_webview_window(crate::control::LABEL)
            .unwrap()
            .listen("traffic", move |_| {
                c.fetch_add(1, Ordering::SeqCst);
            });
        let t = on_transport.clone();
        handle
            .get_webview_window("main")
            .unwrap()
            .listen("traffic", move |_| {
                t.fetch_add(1, Ordering::SeqCst);
            });

        emit_control(&handle, "traffic", TrafficSample::default());

        assert_eq!(
            on_control.load(Ordering::SeqCst),
            1,
            "控制窗口必须收到 —— 收不到就等于整个界面是死的"
        );
        assert_eq!(
            on_transport.load(Ordering::SeqCst),
            0,
            "传输窗口收到了统计事件 —— 那台服务器被攻破时，它能读到全部流量与规则命中"
        );
    }

    /// 控制窗口不存在时短路，不白白序列化一轮。
    /// （实测：此时 `emit_to` 会静默返回 Ok(())，不报错，所以只能这样验。）
    #[test]
    fn emitting_without_a_control_window_is_a_no_op() {
        use tauri::test::{mock_builder, mock_context, noop_assets};

        let app = mock_builder()
            .build(mock_context(noop_assets()))
            .expect("mock app");
        let handle = app.handle().clone();
        assert!(handle.get_webview_window(crate::control::LABEL).is_none());
        // 不 panic、不报错即通过
        emit_control(&handle, "traffic", TrafficSample::default());
    }

    /// **Characterization 测试：记录一个反直觉的 Tauri 行为，不是在庆祝它。**
    ///
    /// 以 `EventTarget::Any` 注册的监听者会收到 `emit_to` 的**定向**事件。
    /// 来源是 `event/listener.rs` 的 `match_any_or_filter`：
    /// `*target == EventTarget::Any || filter(...)` —— `Any` 短路在过滤之前。
    ///
    /// 为什么这件事必须写成测试而不是注释：JS 侧 `listen(event, handler)`
    /// 不传 `options.target` 时注册的正是 `{kind:"Any"}`。也就是说
    /// **`emit_to` 单独并不能阻止一个远端页面收到定向事件** —— 真正挡住它的
    /// 是传输 capability 里根本没有 `core:event` 权限（见 `emit_control`
    /// 的文档注释与 `capability_isolation.rs`）。
    ///
    /// 若哪天 Tauri 改掉这个语义，本测试会红，那是好事：说明第 1 层
    /// 真的收紧了，可以把上面那段「第 2 层才是承重的」重新评估一遍。
    #[test]
    fn an_any_target_listener_still_receives_targeted_events() {
        use std::sync::atomic::AtomicUsize;
        use tauri::test::{mock_builder, mock_context, noop_assets};
        use tauri::Listener;

        let app = mock_builder()
            .build(mock_context(noop_assets()))
            .expect("mock app");
        let handle = app.handle().clone();
        crate::control::open(&handle).expect("控制窗口");

        let seen = Arc::new(AtomicUsize::new(0));
        let s = seen.clone();
        // listen_any 注册 EventTarget::Any —— 与 JS 侧不传 target 的 listen() 同构
        handle.listen_any("traffic", move |_| {
            s.fetch_add(1, Ordering::SeqCst);
        });

        emit_control(&handle, "traffic", TrafficSample::default());

        assert_eq!(
            seen.load(Ordering::SeqCst),
            1,
            "Any target 的监听者绕过了 emit_to 的定向过滤 —— 这是当前 Tauri 的真实行为。\
             它变了的话，emit_control 的分层安全论证需要重写"
        );
    }

    /// 承重的那一层：传输 capability 里**没有** `core:event` 的任何权限，
    /// 因此远端页面调不动 `plugin:event|listen`，注册不上任何监听者 ——
    /// 上面那条 `Any` 绕过在这里就无从触发。
    ///
    /// 与 `capability_isolation.rs` 的集合相等断言互补：那条守「不膨胀」，
    /// 这条把「为什么事件推送是安全的」这个具体论证钉在代码里。
    #[test]
    fn the_transport_capability_grants_no_event_listening() {
        let cap: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("capabilities/transport.json"),
            )
            .expect("读取 transport.json"),
        )
        .expect("解析 transport.json");

        let perms = cap["permissions"]
            .as_array()
            .expect("transport.json 缺 permissions");
        for p in perms {
            let id = p.as_str().or_else(|| p["identifier"].as_str()).unwrap_or("");
            assert!(
                !id.starts_with("core:event") && id != "core:default",
                "传输 capability 拿到了事件权限（{id}）—— 远端页面就能 listen('traffic') \
                 收走全部流量统计与规则命中。emit_to 的定向过滤挡不住默认的 Any target"
            );
        }
    }
}

