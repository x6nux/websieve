//! 链路画像与自适应档位：把实测的 RTT/带宽换算成传输参数。
//!
//! 为什么需要它：传输层的形状参数（在途 POST 数、攒批阈值、合并窗口、mux
//! 接收窗口）最优值随链路 RTT 变化一个数量级——局域网 0.5 ms 与跨洲 66 ms
//! 的最优点完全不同。此前这些值只能靠 env 手动扫，而用户不会去扫 env。
//!
//! 理论依据就一条，`wsmux/session.rs` 的注释里已经写着：`吞吐 ≤ 窗口 / RTT`。
//! 反过来，要让管道填满，窗口（以及在途深度）就得 ≈ 带宽 × 时延积（BDP）。
//!
//! 这里**不新增任何探测流量**。三个测量源全是现成的采样点：
//!   1. [`LinkProfile::observe_post`]——每个上行 POST 的往返。端到端，含 IPC
//!      开销与 JS 调度噪声，但任何时候都有。
//!   2. [`LinkProfile::observe_downlink`]——下行长流按时间窗口累计的吞吐。
//!      **带宽估计的主来源**：跑满管道的是下行，上行 POST 那条在下载时只有
//!      几百字节的窗口更新，算出来的数字与链路容量无关（差三个数量级，见
//!      `Inner::down_bps` 的注释）。
//!   3. [`LinkProfile::observe_peer`]——服务端经 `Server-Timing` 回传的对端
//!      视角：它自报的处理耗时（从 RTT 里扣掉才是纯网络往返），以及上行 seq
//!      的空洞/重复计数（客户端只知道自己重试了，不知道是请求丢了还是响应丢了）。
//!
//! # 没走承载页 `PerformanceResourceTiming` 这条路
//!
//! `emitter.js` 里那个 `PerformanceObserver` 能给出 WebKit 网络栈的真实时序
//! （`responseStart - requestStart` 是不含 IPC 的 TTFB），比第 1 条精确。没用
//! 它是因为**回帧里没有地方放**：服务端观测那三个数字已经占满了应答帧
//! 11..14 这四个字节（`wsieve-transport::pack_peer_observation`），再加
//! ttfb/body_time/bytes 就得扩帧格式、连带 `stripe::VER` 一起升版。
//!
//! 精度收益也不明确：第 1 条多出来的那部分是 IPC + JS 调度，实测在 0.2–0.4ms
//! 量级，对跨洲链路（几十到几百 ms）是噪声。等哪天真需要在回环这种量级上定档
//! 时再说，而那时要动的是帧格式，不是这里。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// `min_rtt` 的滑动窗口长度。
///
/// 取最小值而不是均值，是 BBR 的做法，也是 BDP 能算对的前提：排队时延不该
/// 被当成链路基线。管道一旦被填满，瞬时 RTT 会随队列增长，用它算 BDP 会得到
/// 一个正反馈——窗口越大测得的 RTT 越大，于是窗口继续变大，直到缓冲区爆掉。
/// 滑动窗口保证路径真的变差时（换网络、换路由）基线还能跟上去。
const MIN_RTT_WINDOW: Duration = Duration::from_secs(10);

/// 滑动窗口里最多留多少条样本。
///
/// 单靠时间窗口裁不住条数：`tier()` 在 2ms 的 tick、每次 flush、每个 POST 上
/// 都要调，而它内部 `min_rtt()` 是全扫。10 秒窗口在高频小包下能攒到几千条，
/// 于是一个持锁的全扫就压在最热的路径上。
///
/// 256 条对一个"取最小值"的估计器完全够用——它要的是这段时间里那一两个没
/// 排队的样本，不是分布。超出的从队头丢，与时间裁剪同向（丢最旧的）。
const MAX_RTT_SAMPLES: usize = 256;

/// 带宽 EWMA 的平滑系数。
///
/// 与 `stripe_runtime.rs` 的 `LaneHealth` 取同一个值，两处都是"对带宽做指数
/// 平滑"，没有理由用不同的响应速度。
const BW_ALPHA: f64 = 0.3;

/// 采样的最小时长，短于此的样本丢弃。
///
/// 微秒级的样本算出来的速率是纯噪声：一次内存拷贝也能"跑出" GB/s。
/// 同样的下限在 `LaneHealth::observe` 里也有，原因一致。
const MIN_SAMPLE_TIME: Duration = Duration::from_micros(50);

/// 换档需要连续多少次采样越界。
///
/// 没有这道滞回，BDP 在档位边界附近抖动就会让参数反复横跳，而每次换档都要
/// 付一次代价（窗口重算、在途深度变化）。沿用 `http3.rs` 降级判据的 `BAD_STREAK`
/// 范式：单次越界只是噪声，连续越界才是趋势。
const TIER_STREAK: u32 = 3;

/// 换档的重叠带：向上换档要超出阈值这么多倍，向下要低于这么多倍。
///
/// 纯阈值切分会让恰好落在边界上的链路反复换档（滞回只能延迟它，不能消除）。
/// 重叠带把"上去的线"和"下来的线"分开，边界附近就有了一个谁也不碰的稳定区。
///
/// **50% 是实测定出来的，不是拍的。** 最初取 20%，跨境链路上直接横跳：
/// 实测 BDP 序列 917 → 2499 → 1430 → 2477 KB（带宽估计本身波动 2.7 倍：
/// 21147 → 58309 → 33499 → 57246 KB/s），档位跟着 0→1→2→1→2 来回换。
/// 20% 对应的两条线是 2.5 MB（上）与 1.6 MB（下），那段波动把两条都跨过了。
/// 50% 把它们拉开到 3.0 MB 与 1.0 MB，整段波动落进稳定区。
///
/// 档位边界之间是 4 倍关系（512K / 2M / 8M），50% 的重叠带仍远小于档距，
/// 不会让相邻档粘连成一档。
const TIER_HYSTERESIS: f64 = 0.5;

/// 换档前至少要有这么多个带宽样本。
///
/// 2026-09-12 真机翻车：连接刚建立时只有握手那几个小 POST，带宽估成 3 KB/s、
/// BDP 算出来 0，于是**立刻从默认档降到最低档**——正好在连接初期最需要性能的
/// 时候变保守，然后再花几秒爬回来。轨迹是 `1 → 0 → 1 → 2`，那个 0 纯属自伤。
///
/// 8 个样本足以跨过握手阶段，又不会让真实的换档迟太久（下行每 200ms 一个
/// 样本，也就是一秒半）。
///
/// 这与 `http3.rs` 的 `MIN_SAMPLE` 是同一类判据：样本太少时算出来的比率
/// 不是测量，是噪声。区别只在那边防的是误降级，这边防的是误降档。
const MIN_BW_SAMPLES: u32 = 8;

/// 低于这个 RTT 就把合并窗口归零。
///
/// **合并窗口是个时间量，要跟 RTT 比，不能跟 BDP 比。** 400 µs 相对 1 ms 的
/// RTT 是 40%，相对 47 ms 只有 0.85%——同一个绝对值在两条链路上的分量差两个
/// 数量级，而 BDP（一个字节量）对此一无所知。最初我把它绑在 BDP 上，那是
/// 把一个无关的量塞进了公式。
///
/// 2026-09-12 单变量实测（其余参数全锁死，配对符号检验）：
///
/// | 环境 | RTT | 并发 | 赢家 | p |
/// |---|---|---|---|---|
/// | 本机回环 | 1ms | 16 | merge=0 | 0.0026 |
/// | 本机回环 | 1ms | 32 | merge=0 | 0.0064 |
/// | 本机回环 | 1ms | 4 | 打平（未触发）| 0.52 |
/// | 跨境 | 47ms | 16 | merge=0 | 0.0987 |
/// | 跨境 | 47ms | 8 | merge=400 | 0.1539 |
///
/// 低 RTT 那两组显著且同向；高 RTT 两组方向相反、都不显著。所以只在有证据的
/// 那一侧动手：RTT 低就归零，RTT 高保持实测调出来的 400 µs 不动。
///
/// 5 ms 这条线取在两组实测之间（1 ms 与 47 ms），离两边都远，不是在噪声里
/// 卡边界。真要精确定位这个拐点得扫一遍中间 RTT，那是后话。
const MERGE_RTT_FLOOR: Duration = Duration::from_millis(5);

/// 一次采样在画像里的形态。
#[derive(Debug, Clone, Copy)]
struct RttSample {
    at: Instant,
    rtt: Duration,
}

/// 流量形态。同一条链路上这两者要的参数是**反的**：合并窗口对交互式是毒药
/// （白等一个窗口的延迟），对批量传输是良药（省掉每个 POST 的固定开销）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowKind {
    /// 交互式：少量并发、每次字节数小、延迟敏感。
    Interactive,
    /// 批量：持续大流量、吞吐敏感。
    Bulk,
}

/// 一组传输参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier {
    /// 在途 POST 上限（上行管道深度）。
    pub inflight: usize,
    /// 攒批阈值：攒够这么多字节就发，不等 tick。
    pub agg_bytes: usize,
    /// tick 兜底阈值：距上次写入超过这么久就发。
    pub agg_wait: Duration,
    /// 小包合并窗口：密集时多等这么久，把紧邻的小包并进同一个 POST。
    pub merge: Duration,
    /// wsmux 每流接收窗口。
    pub window: u32,
}

/// 全部档位里最小的 `agg_wait`。
///
/// 导出它只为一个用途：让 `client.rs` 的 `TICK_GRANULARITY_MS` 能在**编译期**
/// 断言「轮询粒度 ≤ 任何档的聚合阈值」。粒度大于某个档的阈值，那个档的攒批
/// 就永远晚一个 tick 才被检出——一个纯粹的延迟回退，而且完全无声。
///
/// 写成编译期常量而不是一条测试：这个不变量的破坏方式是「有人往表里加一档、
/// 顺手给了更小的 `agg_wait`」，那种改动应该当场编译失败，而不是等谁去跑测试。
pub const MIN_AGG_WAIT: Duration = {
    let mut m = BULK_TIERS[0].agg_wait;
    let mut k = 1;
    while k < BULK_TIERS.len() {
        if BULK_TIERS[k].agg_wait.as_nanos() < m.as_nanos() {
            m = BULK_TIERS[k].agg_wait;
        }
        k += 1;
    }
    m
};

/// 批量型档位表，按 BDP 从小到大。
///
/// **只有 `inflight` 和 `window` 随 BDP 变。** 这两个是 BDP 公式直接支配的量：
/// `吞吐 ≤ 窗口 / RTT`，而管道深度同理。其余三个（`agg_bytes` / `agg_wait` /
/// `merge`）各档取值相同，一律沿用实测扫出来的默认值。
///
/// 这不是偷懒，是 2026-09-12 实测纠正过来的：最初我把 `merge` 也绑在 BDP 上，
/// 低 BDP 就归零。本机回环（RTT 1ms，理应是"400µs 白等 40% 的 RTT"的最坏
/// 情形）上配对 A/B 24 组，merge=0 反而略输给 merge=400µs（1.75ms vs 1.69ms，
/// p=0.54）。原因很清楚：合并窗口省的是**每个 POST 的 IPC 固定开销**，而
/// 回环上 IPC 占比更大，所以合并更划算——这个权衡与 BDP 无关，绑上去纯属
/// 把一个无关变量塞进公式。
///
/// **档 1 必须与改造前的硬编码默认值逐字节相同**，这是保底：数据不足、
/// 画像失效、或自适应本身出了问题时，行为原地不动而不是回退到一个没人
/// 验证过的组合。
const BULK_TIERS: &[Tier] = &[
    // 窗口**不向下调**：档 0 沿用默认的 4 MiB，而不是按 BDP 算出来的 1 MiB。
    // 2026-09-12 单变量实测（本机回环、大包 2 并发、40 对配对）：
    // 1 MiB 显著更慢，51.19ms vs 44.82ms，p=0.0007。
    //
    // BDP 公式在这里漏了一项：`take_ack` 攒够半个窗口才发一个 WND，窗口越小
    // ACK 越频繁，而每个 WND 都要走一次 IPC 往返。8 MB 的传输，1 MiB 窗口要
    // 发 16 个 WND，4 MiB 只要 4 个。IPC 开销主导时，这笔账远超小窗口省下的
    // 那点内存。
    Tier { inflight: 4, ..BASE_SHAPE },
    Tier { inflight: 8, ..BASE_SHAPE },
    Tier { inflight: 16, window: MAX_WINDOW, ..BASE_SHAPE },
    // 顶档只加深管道，不再加大窗口：窗口已经顶到 `MAX_WINDOW`。
    //
    // 这里曾经写 64 MiB。那个数字是按 BDP 公式推的，但公式算的是**单流**该多
    // 深，而窗口的实际含义是「对端最多能塞给我们多少还没被读走的字节」——
    // 会话里有多少条流，它就乘多少遍。`wsmux` 不限流数，于是 64 MiB 是一个
    // 由对端决定上限的内存额度，而且 `Cmd::Wnd` 只有正增量、发出去就收不回。
    //
    // 实测上也没有损失：窗口那一路的收益早就被 ACK 频率吃掉了（见档 0 的
    // 注释），真正把顶档和档 2 区分开的是 `inflight`。
    Tier { inflight: 32, window: MAX_WINDOW, ..BASE_SHAPE },
];

/// 单流接收窗口的硬上限。
///
/// 窗口是**授信**：通告 N 字节就是允许对端塞 N 字节未读数据给我们，而
/// `wsmux` 不限制并发流数。没有上限时，一条高 BDP 链路会让每条流都拿到
/// 按 BDP 推出来的大窗口，总量 = 流数 × 窗口，由对端说了算——应用侧读取
/// 一停（磁盘慢、界面卡），这份额度就全部落成常驻内存。
///
/// 协议上没法事后收回：`Cmd::Wnd` 只有正增量，没有负的。所以只能在**通告
/// 之前**封住，这个常量就是那道闸（`Session::grow_window` 与
/// `window_size()` 各自 clamp 一次）。
///
/// 16 MiB 的依据：这个栈实测单流吞吐约 20 MB/s，16 MiB 窗口在 100ms RTT 上
/// 对应 160 MB/s，比实际能跑的还高 8 倍——再大不会更快，只会更能被塞满。
pub const MAX_WINDOW: u32 = 16 * 1024 * 1024;

/// 不随 BDP 变的那三个形状参数，取值即改造前的硬编码默认值。
///
/// 它们是 2026-09 逐档扫出来的（400 µs 那个尤其：0/100/200/400 µs 都试过，
/// 串行小包 P50 从 1.8ms 掉到 3.0ms 的那次回归就踩在这上面）。没有实测证据
/// 说明它们该随链路变，就不该让一个未经验证的公式去动它们。
const BASE_SHAPE: Tier = Tier {
    inflight: 8,
    agg_bytes: 64_000,
    agg_wait: Duration::from_millis(4),
    merge: Duration::from_micros(400),
    window: 4 * 1024 * 1024,
};

/// 各档的 BDP 上界（字节）。最后一档没有上界。
const TIER_BDP_BOUNDS: &[f64] = &[512.0 * 1024.0, 2.0 * 1024.0 * 1024.0, 8.0 * 1024.0 * 1024.0];

/// 数据不足时用哪一档。取档 1 = 改造前的默认值，见 `BULK_TIERS` 的注释。
const DEFAULT_TIER: usize = 1;

/// 把批量档换算成交互档。
///
/// **目前是恒等的**，这是有意的：至今没有一项经过验证的差异化处理。
///
/// 两个曾经写在这里的差异都被实测拿掉了：
///   - `merge` 归零——它该看 RTT 而不是流型，见 `MERGE_RTT_FLOOR`；
///   - `inflight` 减半——理由是"交互式不必填满管道"，听起来合理，但从未被
///     单独验证过。唯一相关的实测（inflight 4 优于 8，p=0.0153）两臂都是
///     批量档，量的是"这条链路该用多深的管道"，不是"交互式该不该比批量浅"。
///     而这个减半还有实际代价：它让默认行为悄悄偏离了改造前的基线。
///
/// 函数留着不删，因为流型判据本身是有信息的（`client.rs` 的 `extra >= 2` 是
/// 实测调出来的）。等哪天有了单变量证据，差异化就加在这里，而不是散到各处。
fn to_interactive(t: &Tier) -> Tier {
    *t
}

/// 画像的可持久化快照。
///
/// 存起来是为了**冷启动直接落到正确档位**，跳过收敛期。先例是 Linux 的
/// `tcp_metrics` 与 RFC 2140：同一个对端的路径特征在会话之间是复用的。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProfileSnapshot {
    pub min_rtt_us: u64,
    pub delivery_bps: f64,
}

#[derive(Debug)]
struct Inner {
    /// 滑动窗口内的 RTT 样本，按时间升序。
    rtt_samples: VecDeque<RttSample>,
    /// 上行方向的平滑交付速率（字节/秒）。
    up_bps: Option<f64>,
    /// 下行方向的平滑交付速率（字节/秒）。
    ///
    /// **必须与上行分开记。** 上行 POST 的 `bytes/rtt` 测的是"这次发了多少"，
    /// 下载时那只是几百字节的窗口更新除以一整个 RTT——2026-09-12 真机上算出
    /// 8.7 KB/s，而同一时刻实测吞吐 22 MiB/s，差三个数量级。照那个数定档会
    /// 把每条链路都钉死在最低档，比不做自适应还糟。
    down_bps: Option<f64>,
    /// 当前档位。
    tier: usize,
    /// 朝某个方向连续越界了几次。正数向上、负数向下。
    streak: i32,
    /// 历史快照（来自上次运行），尚未被实测确认或否决。
    pending_history: Option<ProfileSnapshot>,
    /// 服务端报告的上行 seq 空洞累计数。
    peer_gaps: u32,
    /// 至今收到的带宽样本数，用于挡住冷启动期间的误换档。
    bw_samples: u32,
    /// 整个会话期间见过的最小 RTT。
    ///
    /// **与滑动窗口的 `min_rtt()` 是两件事，别合并。** 滑动窗口服务于实时
    /// 定档：路径真的变差时基线要能跟上去，所以它必须会遗忘。而持久化要存的
    /// 是这条链路的**代表性基线**，遗忘在这里是有害的。
    ///
    /// 2026-09-12 真机上撞见过：流量停了 70 秒后落盘，窗口里只剩空闲期的心跳
    /// 样本，存下来的 min_rtt 是 87ms，而同一条链路跑起来时的基线是 45ms。
    /// 下次冷启动拿 87ms 去算 BDP，档位一上来就偏低。
    lifetime_min_rtt: Option<Duration>,
}

/// 一条链路的画像。内部自带锁，克隆 `Arc` 共享即可。
#[derive(Debug)]
pub struct LinkProfile {
    inner: Mutex<Inner>,
}

impl Default for LinkProfile {
    fn default() -> Self {
        Self::new()
    }
}

/// 自适应总开关。`WSIEVE_ADAPTIVE=off` 关掉，画像照常测量但**不换档**。
///
/// 两个用途，都不是可选的：
///   1. 排障对照——出了问题要能一键确认"是不是自适应引入的"，否则只能靠
///      改代码重编来二分，而那会同时改变别的东西。
///   2. 用户的逃生口。一个会自己改传输参数的东西必须能被关掉。
///
/// 关掉时停在默认档（= 改造前的硬编码值），行为与改造前一致。
pub fn adaptive_enabled() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        !matches!(
            std::env::var("WSIEVE_ADAPTIVE").as_deref(),
            Ok("off") | Ok("0") | Ok("false")
        )
    })
}

impl LinkProfile {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                rtt_samples: VecDeque::new(),
                up_bps: None,
                down_bps: None,
                tier: DEFAULT_TIER,
                streak: 0,
                pending_history: None,
                peer_gaps: 0,
                bw_samples: 0,
                lifetime_min_rtt: None,
            }),
        }
    }

    /// 注入历史快照（磁盘上的记录读出来之后）。
    ///
    /// **不做成构造函数参数**，是因为画像的 `Arc` 在出站实例建好时就分发给了
    /// xhttp 会话和窗口驱动器，那之后换不掉；而磁盘读取属于 I/O，不该塞进
    /// 构造函数。内部可变正是为这种"结构已定、数据后到"的情形准备的。
    ///
    /// **已经有实测样本时不覆盖**：实测永远比历史可信，让一次晚到的磁盘读
    /// 把已经学到的东西盖掉是纯倒退。
    pub fn adopt_history(&self, snap: ProfileSnapshot) {
        let mut i = self.inner.lock().unwrap();
        if !i.rtt_samples.is_empty() {
            return;
        }
        i.tier = tier_for_bdp(snap.min_rtt_us as f64 / 1e6 * snap.delivery_bps);
        i.pending_history = Some(snap);
    }

    /// 喂一个上行 POST 的往返样本。
    ///
    /// 这是端到端耗时，含 IPC 往返与 JS 调度。更精确的浏览器侧时序为什么
    /// 没用上，见模块文档末尾那一节。
    pub fn observe_post(&self, rtt: Duration, bytes: usize, retried: bool) {
        if rtt < MIN_SAMPLE_TIME {
            return;
        }
        let mut i = self.inner.lock().unwrap();
        // 重试过的样本只更新 RTT 不更新带宽：重试把退避时间也算进了耗时，
        // 拿它算速率会把链路严重低估，反而触发向下换档——正好和"链路不稳
        // 时更该保守"的直觉相反，但错在数值上而不是方向上，不能将就。
        i.push_rtt(rtt);
        if !retried {
            i.push_up_bw(bytes as f64 / rtt.as_secs_f64());
        }
        i.judge_history(rtt);
        i.retier();
    }

    /// 喂一段下行流的实测吞吐。
    ///
    /// **这是带宽估计的主来源。** 上行 POST 那条（`observe_post`）测的是
    /// "这次发了多少字节"，下载时上行只有几百字节的窗口更新，除以一整个 RTT
    /// 得到的是个与链路容量无关的小数字。真正跑满管道的是下行长流，所以
    /// 容量要从这里读。
    ///
    /// 调用方按**时间窗口**累计后再报，不要每个 chunk 报一次：单个 chunk 的
    /// 间隔常在微秒级，那个尺度上算出来的速率是调度噪声不是链路速率。
    pub fn observe_downlink(&self, bytes: u64, elapsed: Duration) {
        if elapsed < MIN_SAMPLE_TIME || bytes == 0 {
            return;
        }
        let mut i = self.inner.lock().unwrap();
        i.push_down_bw(bytes as f64 / elapsed.as_secs_f64());
        i.retier();
    }

    /// 喂服务端经 `Server-Timing` 回传的观测。
    ///
    /// `srv_dur` 是服务端自报的处理耗时：把它从 RTT 里扣掉，剩下的才是纯网络
    /// 往返。服务端偶尔的处理抖动（GC、锁等待）此前会被整个算进 RTT，让画像
    /// 误以为链路变差了。
    ///
    /// `gaps` 是服务端看到的上行 seq 空洞数——客户端自己看不到这个：它只知道
    /// "我重试了"，分不清是请求没到还是响应丢了。
    pub fn observe_peer(&self, srv_dur: Duration, gaps: u32) {
        let mut i = self.inner.lock().unwrap();
        // **服务端报的是会话累计值，这里取 max 而不是累加。**
        //
        // `server.rs` 的 `session.gaps` 只增不减、永不重置，每个 204 都带着
        // 那个running total。再累加一次就是把同一个事实数 N 遍：一次真实的
        // 重排在 1000 个 POST 之后会读成 1000。这与 `http3.rs::judge` 修掉的
        // 累计-vs-增量是同一个错，只是换了一层。
        //
        // 取 max 而非直接赋值：会话重建时服务端计数从 0 起，而客户端关心的是
        // "这条链路至今见过多少空洞"，不该被一次重连抹掉。顺带也消掉了 u8
        // 饱和的影响（max(255,255) 仍是 255，累加则会每个 POST 涨 255）。
        i.peer_gaps = i.peer_gaps.max(gaps);
        // 把最近一个 RTT 样本扣掉服务端处理时间。只修最近这条而不是全部，
        // 是因为 srv_dur 本来就只属于那一次请求。
        // 扣不出一个**可用样本**就整条丢掉，不钳位。
        //
        // 服务端自报的耗时不可能接近、更不可能超过客户端看到的整个往返；真出现
        // 了就说明这个数字不可信（服务端计量有问题，或者对端在给一个大数）。
        // 此前的写法是 `saturating_sub(..).max(MIN_SAMPLE_TIME)`——那等于凭空
        // 造出一个 50µs 的 RTT 样本，而 `min_rtt` 是**取最小值**的估计器：一个
        // 假的地板值会把滑动窗口和长期基线一起按到地上，此后 BDP 全线偏小、
        // 档位永久偏低，而且这个假值还会跟着快照落盘、污染下一次冷启动。
        let fixed = match i.rtt_samples.back_mut() {
            Some(last) if last.rtt.saturating_sub(srv_dur) >= MIN_SAMPLE_TIME => {
                last.rtt -= srv_dur;
                Some(last.rtt)
            }
            _ => None,
        };
        if let Some(fixed) = fixed {
            // 长期基线也得跟着修正。`push_rtt` 折进去的是**未扣服务端耗时**的
            // 原始值，比真实网络往返大；只修滑动窗口的话，落盘的那份快照就带
            // 着服务端处理时间，下次冷启动照它算 BDP 会偏高、直接跳到偏大的档。
            //
            // 折的方向是安全的：`fixed <= last.rtt`，取 min 只会把基线往下拉，
            // 不存在"把已经记下的好样本抹掉"这回事。
            i.lifetime_min_rtt = Some(match i.lifetime_min_rtt {
                None => fixed,
                Some(m) => m.min(fixed),
            });
        }
        i.retier();
    }

    /// 当前档位下该用的参数。
    ///
    /// **总开关就卡在这一个出口**。全部使用方（`client.rs` 的三个形状参数、
    /// `instance.rs` 的窗口驱动器）都经这里取值，所以关掉自适应只需要在这里
    /// 返回默认档。分散地在每个适应点各判一次，是这个函数曾经的写法，代价是
    /// `adopt_history` 忘了判——`WSIEVE_ADAPTIVE=off` 却读到了历史档位，得到
    /// 一份"窗口不动但管道深度是历史值"的混合配置，既不是自适应也不是基线，
    /// 于是排障对照这个开关的唯一用途就没了。
    pub fn tier(&self, flow: FlowKind) -> Tier {
        if !adaptive_enabled() {
            return BULK_TIERS[DEFAULT_TIER];
        }
        let i = self.inner.lock().unwrap();
        let mut t = BULK_TIERS[i.tier];
        // 合并窗口单独按 RTT 判，不走 BDP 档位——理由见 `MERGE_RTT_FLOOR`。
        // 没有 RTT 样本时不动它：默认值是实测调出来的，缺数据就别猜。
        if i.min_rtt().is_some_and(|r| r < MERGE_RTT_FLOOR) {
            t.merge = Duration::ZERO;
        }
        match flow {
            FlowKind::Bulk => t,
            FlowKind::Interactive => to_interactive(&t),
        }
    }

    /// 当前估计的 BDP（字节）。数据不足时返回 `None`。
    pub fn bdp(&self) -> Option<f64> {
        let i = self.inner.lock().unwrap();
        i.bdp()
    }

    /// 取快照用于持久化。数据不足时返回 `None`——存一个没依据的快照，
    /// 下次冷启动就会拿它去跳档，比没有历史更糟。
    pub fn snapshot(&self) -> Option<ProfileSnapshot> {
        let i = self.inner.lock().unwrap();
        Some(ProfileSnapshot {
            // 存长期最小值而非窗口内最小值，见 `lifetime_min_rtt` 的注释。
            min_rtt_us: i.lifetime_min_rtt?.as_micros() as u64,
            delivery_bps: i.delivery_bps()?,
        })
    }

    /// 服务端累计报告的上行 seq 空洞数。测试用；生产侧它只出现在换档日志里
    /// （见 `retier`）——它不参与定档，理由写在那条日志的注释上。
    #[cfg(test)]
    fn peer_gaps(&self) -> u32 {
        self.inner.lock().unwrap().peer_gaps
    }
}

impl Inner {
    fn push_rtt(&mut self, rtt: Duration) {
        let now = Instant::now();
        self.lifetime_min_rtt = Some(match self.lifetime_min_rtt {
            None => rtt,
            Some(m) => m.min(rtt),
        });
        self.rtt_samples.push_back(RttSample { at: now, rtt });
        // 滑出窗口的样本必须真的删掉，不能只是"算的时候跳过"：这个队列在
        // 长连接上会一直涨，而它每次算 min 都要全扫。
        while let Some(f) = self.rtt_samples.front() {
            if now.duration_since(f.at) > MIN_RTT_WINDOW {
                self.rtt_samples.pop_front();
            } else {
                break;
            }
        }
        // 时间窗口裁的是"多旧"，这里裁的是"多少条"。见 `MAX_RTT_SAMPLES`。
        while self.rtt_samples.len() > MAX_RTT_SAMPLES {
            self.rtt_samples.pop_front();
        }
    }

    fn push_up_bw(&mut self, bps: f64) {
        if !self.count_sample(bps) {
            return;
        }
        self.up_bps = ewma(self.up_bps, bps);
    }

    fn push_down_bw(&mut self, bps: f64) {
        if !self.count_sample(bps) {
            return;
        }
        self.down_bps = ewma(self.down_bps, bps);
    }

    /// 记一个有效样本，返回它是否有效。
    ///
    /// 计数必须看"这个样本有没有效"，**不能看"EWMA 的值有没有变"**：一串
    /// 相同的样本喂进 EWMA 后值是不动的，照那个判据计数会永远停在 1，
    /// `MIN_BW_SAMPLES` 那道门就再也跨不过去——表现为档位永久冻在默认档，
    /// 而且完全无声。
    fn count_sample(&mut self, bps: f64) -> bool {
        if !bps.is_finite() || bps <= 0.0 {
            return false;
        }
        self.bw_samples = self.bw_samples.saturating_add(1);
        true
    }

    /// 链路容量 = 两个方向里**观察到的较大者**。
    ///
    /// 取 max 而不是求和或取某一个方向：一条链路的容量是它能达到的最大交付
    /// 速率，而任一时刻通常只有一个方向在真正跑满（下载时上行近乎空闲，
    /// 反之亦然）。取那个空闲方向的数字会把链路判得极慢。
    fn delivery_bps(&self) -> Option<f64> {
        match (self.up_bps, self.down_bps) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0).max(b.unwrap_or(0.0))),
        }
    }

    fn min_rtt(&self) -> Option<Duration> {
        self.rtt_samples.iter().map(|s| s.rtt).min()
    }

    fn bdp(&self) -> Option<f64> {
        Some(self.min_rtt()?.as_secs_f64() * self.delivery_bps()?)
    }

    /// 用第一批实测裁决历史快照：偏差太大就丢弃，把档位退回默认。
    ///
    /// 历史是按"上次那条路径"记的。换了 WiFi、换了地点、运营商换了出口，
    /// 路径特征可能差一个数量级，此时照着历史跳档比没有历史更糟——它会让
    /// 一条慢链路一上来就开 64 MiB 窗口和 32 深度的管道。
    ///
    /// 判据只用 RTT 不用带宽：RTT 是路径的固有属性（光速 + 跳数），一两个
    /// 样本就能测准；带宽则要看当时有没有在传数据，冷启动阶段根本测不出来。
    fn judge_history(&mut self, observed: Duration) {
        let Some(h) = self.pending_history else { return };
        let hist = Duration::from_micros(h.min_rtt_us);
        let (a, b) = (observed.as_secs_f64(), hist.as_secs_f64());
        if b <= 0.0 {
            self.pending_history = None;
            return;
        }
        let ratio = a / b;
        if !(0.5..=2.0).contains(&ratio) {
            // 路径变了，历史作废，退回默认档重新学。
            self.pending_history = None;
            self.tier = DEFAULT_TIER;
            self.streak = 0;
        } else {
            // 历史被实测确认，从此由实测接管，不用再裁决。
            self.pending_history = None;
        }
    }

    /// 按当前 BDP 决定是否换档，带滞回与重叠带。
    fn retier(&mut self) {
        if !adaptive_enabled() {
            return;
        }
        // 样本不够就按兵不动。默认档是经过实测的保底值，偏离它需要证据，
        // 而握手阶段那几个小 POST 不构成证据。见 `MIN_BW_SAMPLES`。
        if self.bw_samples < MIN_BW_SAMPLES {
            return;
        }
        let Some(bdp) = self.bdp() else { return };
        let want = tier_for_bdp_from(bdp, self.tier);
        let before = self.tier;
        self.tier = advance_tier(self.tier, &mut self.streak, want);
        // 换档要留痕。这套东西对使用者是完全不可见的——参数自己在变，
        // 而"当前在哪一档"没有任何别的观察点。出了问题（跑得莫名其妙地慢、
        // 或者两组参数测出来没差别）时，第一个要回答的就是"它当时到底在
        // 哪一档"，没有这条日志就只能猜。
        //
        // 只在**真的换档**时打：档位是分钟级变化的，这不会刷屏。
        if self.tier != before {
            let t = &BULK_TIERS[self.tier];
            tracing::info!(
                从 = before,
                到 = self.tier,
                BDP_KB = (bdp / 1024.0) as u64,
                min_rtt_ms = self.min_rtt().map(|d| d.as_millis() as u64),
                带宽_KBps = self.delivery_bps().map(|b| (b / 1024.0) as u64),
                // 对端报的上行 seq 空洞数。它**不参与**定档——BDP 公式假设
                // 管道无损，拿丢包去调档是另一套控制律，没有实测证据就不该
                // 上。放在这里是因为它恰好回答排障的下一个问题：档位看着不
                // 对，是链路真的变了，还是上行在丢包。
                对端空洞 = self.peer_gaps,
                inflight = t.inflight,
                窗口_KB = t.window / 1024,
                "链路画像换档"
            );
        }
    }
}

/// 滞回的核心决策：在 `want` 的方向上累计，攒够 `TIER_STREAK` 才真的挪一档。
///
/// 单独提成纯函数，是为了能脱开 EWMA 和 `min_rtt` 直接测它。经由 `observe_*`
/// 间接构造"方向交替"的样本序列是做不到的——低 RTT 样本会永久拉低 `min_rtt`
/// 基线，于是后续样本全都指向同一个方向，测出来的根本不是交替。
fn advance_tier(cur: usize, streak: &mut i32, want: usize) -> usize {
    if want == cur {
        *streak = 0;
        return cur;
    }
    let dir = if want > cur { 1 } else { -1 };
    // 方向一变就重新计数：一次向上、一次向下、再一次向上，不该攒成
    // "连续 3 次"。滞回要的是同向趋势，不是越界次数。
    if streak.signum() != dir {
        *streak = 0;
    }
    *streak += dir;
    if streak.unsigned_abs() >= TIER_STREAK {
        *streak = 0;
        // 一次只挪一档：BDP 突然跳变多半是测量异常（一次超大响应、一次长 GC），
        // 逐档爬比直接跳过去安全，而真的持续变化时下一轮滞回会继续推。
        return (cur as i32 + dir).clamp(0, BULK_TIERS.len() as i32 - 1) as usize;
    }
    cur
}

/// 指数滑动平均。`None` 时直接采纳首个样本，不从 0 起步——从 0 起步要好几个
/// 样本才爬到真值，那段时间里档位是错的。
fn ewma(prev: Option<f64>, sample: f64) -> Option<f64> {
    if !sample.is_finite() || sample <= 0.0 {
        return prev;
    }
    Some(match prev {
        None => sample,
        Some(p) => p * (1.0 - BW_ALPHA) + sample * BW_ALPHA,
    })
}

/// BDP → 档位（无滞回，用于冷启动定档）。
fn tier_for_bdp(bdp: f64) -> usize {
    TIER_BDP_BOUNDS
        .iter()
        .position(|&b| bdp < b)
        .unwrap_or(BULK_TIERS.len() - 1)
}

/// BDP → 档位，带相对当前档的重叠带。
///
/// 向上换档要越过 `上界 × (1 + h)`，向下要跌破 `下界 × (1 - h)`。两条线分开，
/// 边界附近就有一个谁也不碰的稳定区——否则恰好落在阈值上的链路会一直
/// 在两档之间横跳，滞回只能延缓这件事，消不掉它。
fn tier_for_bdp_from(bdp: f64, cur: usize) -> usize {
    // 先看能不能往上：当前档的上界抬高一个重叠带。
    if cur < BULK_TIERS.len() - 1 {
        let up = TIER_BDP_BOUNDS[cur] * (1.0 + TIER_HYSTERESIS);
        if bdp >= up {
            return cur + 1;
        }
    }
    // 再看要不要往下：前一档的上界压低一个重叠带。
    if cur > 0 {
        let down = TIER_BDP_BOUNDS[cur - 1] * (1.0 - TIER_HYSTERESIS);
        if bdp < down {
            return cur - 1;
        }
    }
    cur
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 喂 n 个 (rtt, bytes) 相同的样本。
    fn feed(p: &LinkProfile, n: usize, rtt_ms: u64, bytes: usize) {
        for _ in 0..n {
            p.observe_post(Duration::from_millis(rtt_ms), bytes, false);
        }
    }

    /// **保底**：默认档必须逐字段等于改造前的硬编码值。
    ///
    /// 这些数字不是从公式里推出来的，是 2026-09 实测扫出来的（400 µs 合并
    /// 窗口尤其：0/100/200/400 µs 四档扫过，串行小包 P50 从 1.8ms 掉到 3.0ms
    /// 的那次回归就是踩在这上面）。自适应在数据不足时必须落回这一组，而不是
    /// 落到某个没人验证过的组合——否则这套改造的下限比改造前还低。
    #[test]
    fn the_default_tier_is_byte_for_byte_the_old_hardcoded_values() {
        let t = BULK_TIERS[DEFAULT_TIER];
        assert_eq!(t.inflight, 8, "WSIEVE_XHTTP_INFLIGHT 的原默认值");
        assert_eq!(t.agg_bytes, 64_000, "WSIEVE_XHTTP_AGG_BYTES 的原默认值");
        assert_eq!(t.agg_wait, Duration::from_millis(4), "AGGREGATE_MS 的原值");
        assert_eq!(t.merge, Duration::from_micros(400), "WSIEVE_XHTTP_MERGE_US 的原默认值");
        assert_eq!(t.window, 4 * 1024 * 1024, "wsmux DEFAULT_WINDOW 的原值");
    }

    /// 没有任何样本时就是默认档——不能因为"没数据"去猜一个激进档位。
    #[test]
    fn a_profile_with_no_samples_stays_on_the_default_tier() {
        let p = LinkProfile::new();
        assert_eq!(p.bdp(), None);
        assert_eq!(p.tier(FlowKind::Bulk), BULK_TIERS[DEFAULT_TIER]);
        assert_eq!(p.snapshot(), None, "没依据的快照不该被存下来毒害下次冷启动");
    }

    /// 高 BDP 链路要能爬上去，但必须**逐档**爬，不能一步跳到顶。
    #[test]
    fn a_fat_link_climbs_one_tier_at_a_time() {
        let p = LinkProfile::new();
        // 先喂够 `MIN_BW_SAMPLES`，跨过冷启动那道门（见
        // `a_handshake_burst_does_not_drag_the_tier_down`）。
        // 100ms RTT × 200 MB/s = 20 MB BDP，明确越过顶档门槛（8MiB × 1.2）。
        // 门槛期间 `streak` 不累积，所以要 `门槛 + 滞回` 个样本才走完第一次换档。
        feed(&p, (MIN_BW_SAMPLES + TIER_STREAK - 1) as usize, 100, 20_000_000);
        assert_eq!(p.tier(FlowKind::Bulk).inflight, BULK_TIERS[2].inflight, "一次只挪一档");
        feed(&p, TIER_STREAK as usize, 100, 20_000_000);
        assert_eq!(p.tier(FlowKind::Bulk).inflight, BULK_TIERS[3].inflight);
    }

    /// 换档需要**连续同向**越界；方向一变就重新计数。
    ///
    /// 没有这条，一次向上、一次向下、再一次向上会被攒成"连续 3 次"，
    /// 参数就会在噪声里横跳。
    #[test]
    fn alternating_directions_never_accumulate_into_a_tier_change() {
        let mut streak = 0i32;
        let mut cur = DEFAULT_TIER;
        for _ in 0..10 {
            cur = advance_tier(cur, &mut streak, DEFAULT_TIER + 1); // 想上
            cur = advance_tier(cur, &mut streak, DEFAULT_TIER - 1); // 想下
        }
        assert_eq!(cur, DEFAULT_TIER, "方向来回摆不该换档");
    }

    /// 同向连续攒够才换，且一次只挪一档——即使 `want` 指向更远的档位。
    #[test]
    fn a_sustained_direction_moves_exactly_one_tier() {
        let mut streak = 0i32;
        let mut cur = 0usize;
        for _ in 0..TIER_STREAK - 1 {
            cur = advance_tier(cur, &mut streak, 3);
            assert_eq!(cur, 0, "没攒够就不该动");
        }
        cur = advance_tier(cur, &mut streak, 3);
        assert_eq!(cur, 1, "攒够了也只挪一档，不能直接跳到 want");
    }

    /// BDP 停在档位边界的重叠带里时，两个方向都不动。
    ///
    /// 这是重叠带存在的全部意义：滞回只能延缓边界抖动，消不掉它。
    #[test]
    fn a_bdp_inside_the_overlap_band_holds_its_tier() {
        let bound = TIER_BDP_BOUNDS[DEFAULT_TIER];
        // 在上界之上、但没越过重叠带 → 不该上
        assert_eq!(tier_for_bdp_from(bound * (1.0 + TIER_HYSTERESIS) * 0.9, DEFAULT_TIER), DEFAULT_TIER);
        // 在下界之下、但没跌破重叠带 → 不该下
        let low = TIER_BDP_BOUNDS[DEFAULT_TIER - 1];
        assert_eq!(tier_for_bdp_from(low * (1.0 - TIER_HYSTERESIS) * 1.1, DEFAULT_TIER), DEFAULT_TIER);
        // 真的越过了重叠带 → 该动。取值要明确超出 TIER_HYSTERESIS，
        // 否则测的是边界本身而不是"越界后会动"。
        assert_eq!(tier_for_bdp_from(bound * (1.0 + TIER_HYSTERESIS) * 1.1, DEFAULT_TIER), DEFAULT_TIER + 1);
        assert_eq!(tier_for_bdp_from(low * (1.0 - TIER_HYSTERESIS) * 0.9, DEFAULT_TIER), DEFAULT_TIER - 1);
    }

    /// `min_rtt` 取滑动最小值：排队时延不得把 BDP 抬上去。
    ///
    /// 这是 BDP 能算对的前提。管道填满后瞬时 RTT 会随队列增长，若用均值，
    /// 窗口越大测得 RTT 越大 → BDP 越大 → 窗口继续变大，正反馈直到缓冲爆掉。
    #[test]
    fn queueing_delay_does_not_inflate_the_bdp() {
        let p = LinkProfile::new();
        feed(&p, 5, 50, 100_000); // 基线 50ms
        let base = p.bdp().unwrap();
        // 同样的带宽，但 RTT 因排队涨到 10 倍
        for _ in 0..20 {
            p.observe_post(Duration::from_millis(500), 1_000_000, false);
        }
        let after = p.bdp().unwrap();
        let min = p.inner.lock().unwrap().min_rtt().unwrap();
        assert_eq!(min, Duration::from_millis(50), "基线必须还是那个最小值");
        // 带宽相同的前提下 BDP 不该被 RTT 抬高
        assert!(
            after < base * 3.0,
            "排队时延把 BDP 从 {base:.0} 抬到了 {after:.0}——min_rtt 没起作用"
        );
    }

    /// 路径没变时历史被采纳，档位保持。
    #[test]
    fn history_survives_when_the_measured_path_matches() {
        // 100ms × 100MB/s = 10MiB BDP → 顶档
        let snap = ProfileSnapshot { min_rtt_us: 100_000, delivery_bps: 100e6 };
        let p = LinkProfile::new();
        p.adopt_history(snap);
        let started_at = p.tier(FlowKind::Bulk);
        assert_eq!(started_at, BULK_TIERS[3], "历史应当让冷启动直接落到顶档");
        // 实测与历史相符（100ms 上下浮动）
        p.observe_post(Duration::from_millis(110), 1_000_000, false);
        assert_eq!(p.tier(FlowKind::Bulk), started_at, "路径没变就不该退档");
    }

    /// 路径变了（RTT 偏差超过 2×）历史必须作废并退回默认档。
    ///
    /// 换 WiFi、换地点、运营商换出口都会走到这里。照着旧历史跳档，会让一条
    /// 慢链路一上来就开 64 MiB 窗口和 32 深的管道——比没有历史更糟。
    #[test]
    fn history_is_discarded_when_the_path_clearly_changed() {
        let snap = ProfileSnapshot { min_rtt_us: 100_000, delivery_bps: 100e6 };
        let p = LinkProfile::new();
        p.adopt_history(snap);
        assert_eq!(p.tier(FlowKind::Bulk), BULK_TIERS[3]);
        // 实测 400ms，是历史的 4 倍：这不是同一条路径
        p.observe_post(Duration::from_millis(400), 1_000, false);
        assert_eq!(
            p.tier(FlowKind::Bulk),
            BULK_TIERS[DEFAULT_TIER],
            "历史作废后必须退回默认档重新学，而不是留在顶档"
        );
    }

    /// **下载时带宽必须从下行读，不能从上行 POST 读。**
    ///
    /// 2026-09-12 真机翻车：下载 8 MiB 的同时，画像存下来的 `delivery_bps`
    /// 是 8769（8.7 KB/s），而实测吞吐 22 MiB/s——差三个数量级。原因是上行
    /// POST 在下载时只携带几百字节的窗口更新，`bytes/rtt` 量的是"这次发了
    /// 多少"，与链路容量无关。照那个数定档，每条链路都会被钉死在最低档。
    #[test]
    fn download_bandwidth_comes_from_the_downlink_not_the_tiny_uplink_posts() {
        let p = LinkProfile::new();
        // 下载中的典型形态：上行每 45ms 发 400 字节的窗口更新……
        for _ in 0..10 {
            p.observe_post(Duration::from_millis(45), 400, false);
        }
        // ……而下行在同一时间里真的在跑 100 MiB/s。
        for _ in 0..10 {
            p.observe_downlink(20 * 1024 * 1024, Duration::from_millis(200));
        }
        let bps = p.snapshot().unwrap().delivery_bps;
        assert!(
            bps > 80e6,
            "带宽估计 {bps:.0} B/s 被上行的小包拖垮了——链路容量要看跑满的那个方向"
        );
        // 两条路算出来的 BDP 差着三个数量级，档位方向正好相反：
        //   从下行读：45ms × 100MiB/s ≈ 4.7 MB → 该往**上**走到档 2
        //   从上行读：45ms × 8.9KB/s  ≈ 400 B  → 会往**下**掉到档 0
        // 断言升档而不只是"没降档"，这个测试才真的能分辨两者。
        assert_eq!(
            p.tier(FlowKind::Bulk),
            BULK_TIERS[2],
            "4.7 MB 的 BDP 该升到档 2；停在默认档或更低说明带宽读错了方向"
        );
    }

    /// 上传方向同理：这时跑满的是上行，下行只有 ACK。
    ///
    /// 取两个方向的 max 而不是只信某一个，就是为了这两种场景都不误判。
    #[test]
    fn upload_bandwidth_still_counts_when_the_downlink_is_idle() {
        let p = LinkProfile::new();
        for _ in 0..10 {
            p.observe_post(Duration::from_millis(45), 900_000, false);
        }
        // 下行只有零星的小应答
        for _ in 0..10 {
            p.observe_downlink(200, Duration::from_millis(200));
        }
        let bps = p.snapshot().unwrap().delivery_bps;
        assert!(bps > 15e6, "上行跑满时不该被空闲的下行拉低，实得 {bps:.0} B/s");
    }

    /// **冷启动不得降档。**
    ///
    /// 2026-09-12 真机轨迹：`1 → 0 → 1 → 2`。那个 0 是连接刚建立时打出来的
    /// ——当时只有握手的几个小 POST，带宽估成 3 KB/s、BDP 算出 0，于是立刻从
    /// 默认档掉到最低档。偏偏连接初期是最需要性能的时候，而它反而在那里变
    /// 保守，然后花几秒爬回来。
    ///
    /// 默认档是实测出来的保底值，偏离它需要证据；握手阶段那几笔不是证据。
    #[test]
    fn a_handshake_burst_does_not_drag_the_tier_down() {
        let p = LinkProfile::new();
        // 握手形态：几个小 POST，每个几百字节、一整个 RTT
        for _ in 0..4 {
            p.observe_post(Duration::from_millis(42), 300, false);
        }
        assert_eq!(
            p.tier(FlowKind::Bulk),
            BULK_TIERS[DEFAULT_TIER],
            "握手阶段的小 POST 把档位拖下去了——连接初期反而最需要性能"
        );
    }

    /// 但样本够了之后，真慢的链路还是要降下去——上面那道门不能变成"永不降档"。
    #[test]
    fn a_genuinely_slow_link_still_gets_demoted_once_samples_accumulate() {
        let p = LinkProfile::new();
        for _ in 0..40 {
            p.observe_post(Duration::from_millis(42), 300, false);
            p.observe_downlink(400, Duration::from_millis(200));
        }
        assert_eq!(
            p.tier(FlowKind::Bulk),
            BULK_TIERS[0],
            "样本充足的慢链路必须降到最低档，否则那道门就成了永不降档"
        );
    }

    /// 流型目前**不改变任何参数**——这是有意的，也是被实测逼出来的。
    ///
    /// 曾经写在 `to_interactive` 里的两个差异都拿掉了：
    ///   - `merge` 归零：该看 RTT 而非流型（见 `MERGE_RTT_FLOOR`）；
    ///   - `inflight` 减半：从未被单独验证，而且它让默认行为悄悄偏离了
    ///     改造前的基线（`window_capped_at_8` 断言 `<= 8`，4 也通过）。
    ///
    /// 这条测试守的是那个纪律：没有单变量证据，就不要凭直觉给流型加差异。
    /// 真有了证据，差异加在 `to_interactive` 里，这条测试自然会红。
    #[test]
    fn flow_kind_currently_changes_nothing() {
        let p = LinkProfile::new();
        assert_eq!(
            p.tier(FlowKind::Interactive),
            p.tier(FlowKind::Bulk),
            "流型带来了未经单变量验证的参数差异"
        );
    }

    /// **只有 BDP 公式支配的那两个参数随档位变，而且窗口只增不减。**
    ///
    /// 其余三个（攒批阈值、tick 兜底、合并窗口）没有随 BDP 变的实测依据：
    /// `merge` 该看 RTT（见 `MERGE_RTT_FLOOR`），另两个至今无证据。这条测试
    /// 守的是那个纪律——别再凭直觉把它们塞回档位表。
    #[test]
    fn only_bdp_driven_parameters_differ_across_tiers() {
        for w in BULK_TIERS.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            assert_ne!(a.inflight, b.inflight, "管道深度该随 BDP 变");
            assert!(a.window <= b.window, "窗口必须随档位单调不减");
            assert_eq!(a.agg_bytes, b.agg_bytes, "攒批阈值没有随 BDP 变的依据");
            assert_eq!(a.agg_wait, b.agg_wait, "tick 兜底没有随 BDP 变的依据");
            assert_eq!(a.merge, b.merge, "合并窗口该看 RTT 而非 BDP");
        }
    }

    /// 重试过的样本不得参与带宽估计——退避时间会把链路严重低估。
    #[test]
    fn a_retried_sample_does_not_poison_the_bandwidth_estimate() {
        let p = LinkProfile::new();
        feed(&p, 5, 50, 1_000_000);
        let good = p.bdp().unwrap();
        // 同样的字节数，但耗时含了 100ms 退避
        for _ in 0..5 {
            p.observe_post(Duration::from_millis(2000), 1_000_000, true);
        }
        assert_eq!(p.bdp().unwrap(), good, "重试样本不该改动带宽估计");
    }

    /// 服务端自报的处理耗时要从 RTT 里扣掉，剩下的才是纯网络往返。
    #[test]
    fn peer_reported_server_time_is_subtracted_from_the_rtt() {
        let p = LinkProfile::new();
        p.observe_post(Duration::from_millis(100), 1_000, false);
        p.observe_peer(Duration::from_millis(60), 2);
        let min = p.inner.lock().unwrap().min_rtt().unwrap();
        assert_eq!(min, Duration::from_millis(40), "100ms 里有 60ms 是服务端处理");
        assert_eq!(p.peer_gaps(), 2);
    }

    /// 服务端自报耗时 ≥ 整个往返时，这条扣减必须整条丢掉。
    ///
    /// 钳到 `MIN_SAMPLE_TIME` 是凭空造一个 50µs 的 RTT 样本，而 `min_rtt` 是
    /// 取最小值的估计器——一个假地板会把滑动窗口和长期基线一起按到地上，
    /// 之后 BDP 全线偏小、档位永久偏低，假值还会跟着快照落盘污染下次冷启动。
    #[test]
    fn an_implausible_server_duration_is_discarded_rather_than_clamped() {
        for srv_ms in [100u64, 200] {
            let p = LinkProfile::new();
            p.observe_post(Duration::from_millis(100), 1_000, false);
            p.observe_peer(Duration::from_millis(srv_ms), 3);
            assert_eq!(
                p.inner.lock().unwrap().min_rtt().unwrap(),
                Duration::from_millis(100),
                "srv_dur={srv_ms}ms 不可信，RTT 样本该原样保留而不是被钳成地板值"
            );
            assert_eq!(
                p.snapshot().unwrap().min_rtt_us,
                100_000,
                "落盘的长期基线也不能被那个假地板污染"
            );
            // gaps 与 RTT 扣减是两件独立的信息，前者照收。
            assert_eq!(p.peer_gaps(), 3);
        }
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    /// 快照存的是**长期**基线，不是当前滑动窗口里的最小值。
    ///
    /// 2026-09-12 真机：流量停止 70 秒后落盘，滑动窗口里只剩空闲期的心跳样本，
    /// 存下的 min_rtt 是 87ms，而这条链路跑起来时的基线是 45ms。下次冷启动
    /// 拿 87ms 算 BDP，档位一上来就偏低——历史启发式反而帮了倒忙。
    ///
    /// 滑动窗口仍然服务于**实时定档**（路径变差时基线要能跟上去），两者
    /// 职责不同，不能合并成一个数。
    #[test]
    fn the_snapshot_keeps_the_lifetime_baseline_not_the_recent_window() {
        let p = LinkProfile::new();
        // 跑起来时的基线
        p.observe_post(Duration::from_millis(45), 100_000, false);
        // 之后一段空闲期的慢样本（首包、心跳）
        for _ in 0..5 {
            p.observe_post(Duration::from_millis(88), 500, false);
        }
        assert_eq!(
            p.snapshot().unwrap().min_rtt_us,
            45_000,
            "快照被空闲期的慢样本顶掉了链路真实基线"
        );
    }

    /// 快照里的基线必须是**扣掉服务端处理时间之后**的那个数。
    ///
    /// `push_rtt` 折进长期基线的是原始往返（含服务端处理），`observe_peer`
    /// 随后只修了滑动窗口里那一条。两处失步的后果全在下一次冷启动：落盘的
    /// min_rtt 带着服务端耗时、偏大，`BDP = min_rtt × 带宽` 跟着偏大，档位
    /// 一上来就跳高——而本次运行一切正常，故障要等到重启才现身。
    #[test]
    fn the_snapshot_baseline_excludes_the_server_side_processing_time() {
        let p = LinkProfile::new();
        p.observe_post(Duration::from_millis(100), 100_000, false);
        p.observe_peer(Duration::from_millis(60), 0);
        assert_eq!(
            p.snapshot().unwrap().min_rtt_us,
            40_000,
            "长期基线还是 100ms 的原始值——服务端那 60ms 没从快照里扣掉"
        );
        // 与滑动窗口一致：两者本来就该看同一个校正后的数字。
        assert_eq!(
            p.inner.lock().unwrap().min_rtt().unwrap(),
            Duration::from_millis(40)
        );
    }

    /// 滑动窗口的样本条数必须有上限。
    ///
    /// 时间窗口只裁"多旧"，裁不住"多少条"：10 秒窗口在高频小包下能攒几千条，
    /// 而 `min_rtt()` 是全扫且持锁，跑在 2ms 的 tick、每次 flush、每个 POST 上。
    #[test]
    fn the_rtt_window_is_bounded_in_count_not_just_in_age() {
        let p = LinkProfile::new();
        // 10 秒窗口内塞远超上限的样本：一条都不会因为"太旧"被裁掉。
        for k in 0..(MAX_RTT_SAMPLES * 4) {
            p.observe_post(Duration::from_millis(20 + (k % 7) as u64), 1_000, false);
        }
        let i = p.inner.lock().unwrap();
        assert!(
            i.rtt_samples.len() <= MAX_RTT_SAMPLES,
            "样本数 {} 超过上限 {MAX_RTT_SAMPLES}",
            i.rtt_samples.len()
        );
        // 裁剪不能把估计器搞坏：最小值仍要是喂进去的那个最小值。
        assert_eq!(i.min_rtt().unwrap(), Duration::from_millis(20));
    }
}

#[cfg(test)]
mod kill_switch_tests {
    use super::*;

    /// 关掉自适应后必须停在默认档——而默认档逐字节等于改造前的硬编码值。
    ///
    /// 这是逃生口的意义：出问题时一个环境变量就能回到改造前的行为，不需要
    /// 回滚代码、不需要重编。
    ///
    /// 用 `advance_tier` 而不是跑一遍 `LinkProfile`：开关是 `OnceLock` 缓存的
    /// （每个 POST 都要读，不能每次查 env），进程内改不了，测不了两种取值。
    /// 这里守的是另一半——关掉之后落在哪一档。
    #[test]
    fn the_default_tier_is_the_pre_adaptive_behaviour() {
        let t = BULK_TIERS[DEFAULT_TIER];
        assert_eq!(t, BASE_SHAPE, "默认档必须与不随 BDP 变的那组基准值完全一致");
    }
}

#[cfg(test)]
mod merge_window_tests {
    use super::*;

    /// 低 RTT 链路上合并窗口必须归零。
    ///
    /// 本机回环（RTT 1ms）16/32 并发各一组配对 A/B，merge=0 都显著更快
    /// （p=0.0026 / p=0.0064）。400 µs 在 1 ms 的 RTT 上是 40% 的额外等待，
    /// 而它想省的那点 IPC 开销远不值这个价。
    #[test]
    fn a_low_rtt_link_drops_the_merge_window() {
        let p = LinkProfile::new();
        for _ in 0..10 {
            p.observe_post(Duration::from_micros(900), 10_000, false);
        }
        assert_eq!(
            p.tier(FlowKind::Bulk).merge,
            Duration::ZERO,
            "1ms RTT 上还等 400µs，那是 40% 的纯额外延迟"
        );
    }

    /// 高 RTT 链路保持实测调出来的默认值不动。
    ///
    /// 跨境（47ms）两组实测方向相反且都不显著（p=0.0987 / p=0.1539），
    /// 也就是**没有证据**要改它。没有证据就不动——默认值是扫出来的，
    /// 而这个公式还没被验证过。
    #[test]
    fn a_high_rtt_link_keeps_the_measured_default() {
        let p = LinkProfile::new();
        for _ in 0..10 {
            p.observe_post(Duration::from_millis(47), 100_000, false);
        }
        assert_eq!(
            p.tier(FlowKind::Bulk).merge,
            BASE_SHAPE.merge,
            "高 RTT 侧没有实测依据支持改动，必须保持默认"
        );
    }

    /// 没有 RTT 样本时不动合并窗口——缺数据不是归零的理由。
    #[test]
    fn no_rtt_samples_means_no_change_to_the_merge_window() {
        let p = LinkProfile::new();
        assert_eq!(p.tier(FlowKind::Bulk).merge, BASE_SHAPE.merge);
    }

    /// 合并窗口**不**随 BDP 档位变——它是时间量，跟 RTT 比才有意义。
    ///
    /// 这条守的是别再把它塞回档位表：同一个 RTT 下，无论 BDP 落在哪一档，
    /// 合并窗口都该是同一个值。
    #[test]
    fn the_merge_window_does_not_follow_the_bdp_tier() {
        for w in BULK_TIERS.windows(2) {
            assert_eq!(
                w[0].merge, w[1].merge,
                "合并窗口跟着 BDP 档位变了——它该看 RTT，见 MERGE_RTT_FLOOR"
            );
        }
    }
}

#[cfg(test)]
mod window_floor_tests {
    use super::*;

    /// **接收窗口永远不低于实测出来的默认值。**
    ///
    /// 最初档 0 按 BDP 算出 1 MiB，实测直接证伪：本机回环大包 2 并发 40 对
    /// 配对 A/B，1 MiB 比 4 MiB 慢 14%（51.19ms vs 44.82ms，p=0.0007）。
    ///
    /// BDP 公式漏掉的是 ACK 频率：`take_ack` 攒够半窗才发 WND，窗口减半就意味着
    /// WND 帧翻倍，而每个 WND 都要付一次 IPC 往返。这一项与带宽时延积无关。
    #[test]
    fn no_tier_shrinks_the_receive_window_below_the_default() {
        for (i, t) in BULK_TIERS.iter().enumerate() {
            assert!(
                t.window >= BASE_SHAPE.window,
                "档 {i} 的窗口 {} 低于实测默认值 {}——窗口只能往上调",
                t.window,
                BASE_SHAPE.window
            );
        }
    }

    /// 窗口仍然要随 BDP 往上走，否则高 BDP 链路会被 4 MiB 卡住。
    #[test]
    fn high_bdp_tiers_still_grow_the_window() {
        assert!(
            BULK_TIERS.last().unwrap().window > BASE_SHAPE.window,
            "顶档没有放大窗口，高 BDP 链路会被默认值限住"
        );
    }

    /// 没有哪一档能通告超过 `MAX_WINDOW` 的窗口。
    ///
    /// 窗口是给对端的授信，而 `wsmux` 不限并发流数——总量 = 流数 × 窗口，
    /// 由对端说了算。`Cmd::Wnd` 只有正增量，通告出去就收不回，所以唯一的
    /// 闸口在通告之前。档位表能给出超限值的话，`Session::grow_window` 那边
    /// 的 clamp 会静默把它压下来，表里写的数字就成了一句空话。
    #[test]
    fn no_tier_advertises_more_than_the_hard_window_cap() {
        for (i, t) in BULK_TIERS.iter().enumerate() {
            assert!(
                t.window <= MAX_WINDOW,
                "档 {i} 的窗口 {} 超过硬上限 {MAX_WINDOW}",
                t.window
            );
        }
    }
}

#[cfg(test)]
mod hysteresis_field_tests {
    use super::*;

    /// 真机观测到的 BDP 波动序列**不得**引起反复换档。
    ///
    /// 这四个数是 2026-09-12 跨境链路上原样抄下来的（KB）。当时重叠带是 20%，
    /// 档位跟着走了 0→1→2→1→2。带宽估计本身在 21–58 MB/s 之间摆了 2.7 倍，
    /// 而 BDP 正好骑在档 1/2 的边界（2 MB）上。
    ///
    /// 每次换档都要重算窗口、改管道深度，在噪声上反复付这个代价是纯亏损。
    #[test]
    fn the_observed_bdp_jitter_does_not_cause_repeated_tier_changes() {
        let observed_kb = [917.0, 2499.0, 1430.0, 2477.0, 1500.0, 2400.0];
        let mut cur = DEFAULT_TIER;
        let mut streak = 0i32;
        let mut changes = 0;
        // 每个 BDP 重复喂 TIER_STREAK 次，模拟"这个值持续了一会儿"——
        // 比真实情况更苛刻，真实序列里每个值只出现一两次。
        for kb in observed_kb {
            for _ in 0..TIER_STREAK {
                let want = tier_for_bdp_from(kb * 1024.0, cur);
                let next = advance_tier(cur, &mut streak, want);
                if next != cur {
                    changes += 1;
                }
                cur = next;
            }
        }
        assert!(
            changes <= 1,
            "这段波动引起了 {changes} 次换档——重叠带没能盖住带宽估计的抖动"
        );
    }

    /// 但重叠带不能宽到让相邻档粘连：真的跨越一整档时仍要换。
    #[test]
    fn a_genuine_tier_jump_still_happens() {
        let mut cur = 0usize;
        let mut streak = 0i32;
        for _ in 0..TIER_STREAK {
            let want = tier_for_bdp_from(50.0 * 1024.0 * 1024.0, cur);
            cur = advance_tier(cur, &mut streak, want);
        }
        assert_eq!(cur, 1, "50 MB 的 BDP 远超任何重叠带，必须换档");
    }
}

#[cfg(test)]
mod peer_gaps_tests {
    use super::*;

    /// 服务端报的是**累计**值，反复上报同一个数不得把它累加。
    ///
    /// `server.rs` 的 `session.gaps` 永不重置，每个 204 都带着 running total。
    /// 原先这里是 `saturating_add`，于是一次真实的重排在 1000 个 POST 之后会
    /// 读成 1000 —— 与 `http3.rs::judge` 修掉的累计-vs-增量是同一个错。
    #[test]
    fn a_repeated_cumulative_gap_count_is_not_added_up() {
        let p = LinkProfile::new();
        for _ in 0..50 {
            p.observe_post(Duration::from_millis(40), 1_000, false);
            // 服务端一直报"累计 1 次空洞"
            p.observe_peer(Duration::from_micros(200), 1);
        }
        assert_eq!(
            p.peer_gaps(),
            1,
            "把同一个累计值数了 {} 遍",
            p.peer_gaps()
        );
    }

    /// 累计值真的涨了要跟上。
    #[test]
    fn a_growing_cumulative_count_is_tracked() {
        let p = LinkProfile::new();
        for g in [1u32, 1, 3, 3, 7] {
            p.observe_post(Duration::from_millis(40), 1_000, false);
            p.observe_peer(Duration::from_micros(200), g);
        }
        assert_eq!(p.peer_gaps(), 7);
    }

    /// 会话重建（服务端计数归零）不该抹掉客户端记住的历史峰值。
    #[test]
    fn a_session_restart_does_not_erase_the_observed_peak() {
        let p = LinkProfile::new();
        p.observe_post(Duration::from_millis(40), 1_000, false);
        p.observe_peer(Duration::from_micros(200), 5);
        // 新会话，服务端从 0 开始报
        p.observe_post(Duration::from_millis(40), 1_000, false);
        p.observe_peer(Duration::from_micros(200), 0);
        assert_eq!(p.peer_gaps(), 5);
    }
}
