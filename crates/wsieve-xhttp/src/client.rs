//! XhttpConn：xhttp 客户端核心。spec §6.3/§6.4/§6.5。
//!
//! 架构：共享状态（tokio Mutex，持有握手后的 Noise `TransportState`）+
//! 后台聚合/心跳循环 + 每个 POST 一个发送任务（含重试策略）+ 下行解密任务，
//! 经 mpsc 事件通道向 `XhttpConn` 上抛数据与会话死亡。
//!
//! snow 0.10 的 `TransportState` 在握手完成后同时持有发送/接收两个
//! cipherstate（nonce 独立计数），`write_message` / `read_message` 分别走
//! 各自一侧——上行加密与下行解密共用这一份状态，由 Mutex 串行化。

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{anyhow, Result};
use bytes::Bytes;
use futures::StreamExt;
use rand::RngCore;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, Mutex};
use tokio::time::{interval, Instant};

use snow::TransportState;
use wsieve_proto::crypto::build_client;
use wsieve_proto::hello::{decode_msg2, encode_msg1, IpStrategy, MuxId};
use wsieve_proto::tu::{decode_frame, encode_frame, Frame, TuDecoder, MAX_PAYLOAD};
use wsieve_transport::HttpTransport;

use crate::link_profile::{FlowKind, LinkProfile, Tier};

/// 这三个是上行的**形状参数**，最优值随链路 RTT 变化一个数量级：局域网
/// 0.5ms 与跨洲 66ms 的最优点完全不同。
///
/// 它们现在由 [`LinkProfile`] 按实测 BDP 自动定档（见 `link_profile.rs`），
/// env 只作为**显式覆盖**保留——那是排障和扫参用的旋钮，不能被自适应吃掉。
/// 优先级：env > 自适应 > 默认档（默认档逐字节等于改造前的硬编码值）。
/// 覆盖值**只解析一次**，之后从 `OnceLock` 读。
///
/// 缓存不是过早优化：这三个函数跑在 2ms 的 tick、每次 flush、每个 POST 上，
/// 而 `std::env::var` 每次都要遍历 environ 并分配一个 `String`。
///
/// 缓存也不损失语义——env 是启动时设好就不动的调试旋钮。真正需要随链路变的
/// 是**档位**那一路（`t.inflight` 等），它不走这里。这一点曾经被搞混：
/// `merge_window` 当初把整个函数（含档位取值）裹在 `OnceLock` 里，于是自适应
/// 对它完全失效；正确的粒度是「只缓存 env 这一侧」。
fn env_usize(cell: &'static OnceLock<Option<usize>>, key: &str) -> Option<usize> {
    *cell.get_or_init(|| std::env::var(key).ok().and_then(|v| v.parse().ok()))
}

/// 在途 POST 上限（上行窗口深度）。
fn max_inflight(t: &Tier) -> usize {
    static C: OnceLock<Option<usize>> = OnceLock::new();
    env_usize(&C, "WSIEVE_XHTTP_INFLIGHT").unwrap_or(t.inflight)
}
/// 攒批阈值：攒够这么多字节就发，不等 tick。
fn aggregate_bytes(t: &Tier) -> usize {
    static C: OnceLock<Option<usize>> = OnceLock::new();
    env_usize(&C, "WSIEVE_XHTTP_AGG_BYTES").unwrap_or(t.agg_bytes)
}

/// 聚合 tick 的**轮询**粒度。
///
/// 这不是聚合阈值本身——阈值是档位里的 `agg_wait`，判据在 tick 分支里用当前
/// 档的 `agg_wait` 比较。拆成两件事的原因是 tokio 的 `Interval` 没有 period
/// setter：阈值若和粒度共用一个常量，阈值一变就得重建 ticker。
///
/// 取 2ms 而非阈值本身的 4ms，只为把**检出延迟的最坏情况**折半：粒度等于阈值
/// 时，一笔只差 0.1ms 就够 4ms 的缓冲要再等整整一个 tick（最坏 ~8ms）；
/// 粒度取一半，最坏降到 ~6ms。
///
/// 不变量是「粒度 ≤ 任何档的 `agg_wait`」，由下面那条 `const _` 在编译期守住。
/// 眼下全部
/// 档位的 `agg_wait` 都是 4ms（它不随 BDP 变，见 `BASE_SHAPE`），所以这个
/// 常量与「最小档」无关——那个说法曾经写在这里，是错的。
const TICK_GRANULARITY_MS: u64 = 2;
const _: () = assert!(
    (TICK_GRANULARITY_MS as u128) * 1_000 <= crate::link_profile::MIN_AGG_WAIT.as_micros(),
    "tick 粒度大于某个档的 agg_wait：那个档的攒批会永远晚一个 tick 才被检出"
);
/// 一次从命令通道最多取多少条写入合并。取到这个数就先发一批，避免
/// 上行洪峰时 `agg_buffer` 无限涨大。
const RECV_BATCH: usize = 64;

/// 小包合并窗口：请求密集时，收到一批写入后再多等这么久，把紧邻的小包并进
/// 同一个 POST 再发。
///
/// 协议这边本来就支持——一个 POST body 可以装多个 TU（`MAX_TUS_PER_POST`），
/// 服务端按长度前缀逐个拆，不用改。缺的只是发送时机：`recv_many` 是"取空队列
/// 就走"，几个并发请求只要稍微错开一点，就各自成一个 POST，每个都要付一整套
/// 跨进程 + HTTP 栈的固定开销。
///
/// 只在**密集**时才等（见调用处的判据）：孤立的请求立刻发，延迟不受影响。
///
/// 这里曾经把**整个函数**裹进 `OnceLock`：读一次就连档位一起永久定死。自适应
/// 要求它能随链路变，那样缓存等于自适应对这个参数完全失效——而它恰恰是三个
/// 参数里对小包延迟影响最大的那个。现在只缓存 env 覆盖值那一侧。
fn merge_window(t: &Tier) -> Duration {
    static C: OnceLock<Option<usize>> = OnceLock::new();
    match env_usize(&C, "WSIEVE_XHTTP_MERGE_US") {
        Some(us) => Duration::from_micros(us as u64),
        None => t.merge,
    }
}
/// 下行吞吐的采样窗口。
///
/// 200ms 是个折中：短到能跟上链路变化（一次换档要连续 3 个样本，也就是
/// 0.6 秒），长到足以盖过 chunk 到达的调度抖动（单个 chunk 间隔常在微秒级，
/// 那个尺度上算出来的速率是噪声不是带宽）。
const DOWNLINK_WINDOW: Duration = Duration::from_millis(200);

const IDLE_HEARTBEAT_MS: u64 = 60_000;
/// 单 POST body 上限 1 MB（spec §6.4）。单个 TU 密文 ≤ 65537 字节，
/// 15 个 TU（≤ 983 055 B）必然落在 1 MB 内。
const MAX_TUS_PER_POST: usize = 15;
const SID_LEN: usize = 16;

const RETRY_MAX: u8 = 2;
const RETRY_BACKOFF_INITIAL: Duration = Duration::from_millis(100);

pub struct UpstreamCfg {
    pub server_pub: [u8; 32],
    pub client_priv: [u8; 32],
    pub mux_prefs: Vec<MuxId>,
    /// 会话组 id：同一客户端开的全部会话必须用同一值，服务端据此把它们
    /// 归为一组并跨会话铺下行 lane。单会话场景随便一个随机值即可
    /// （见 `random_group_id`）。
    pub group_id: u128,
    /// 服务端解析域名目标时用哪个地址族。双栈服务器按 RFC 6724 通常优先
    /// IPv6，配 `V4Only`/`PreferV4` 可以把出口拉回 IPv4。
    pub ip_strategy: IpStrategy,
    /// 这条链路的实测画像。**与同一条链路上的 mux 会话共用同一个 `Arc`**：
    /// 画像在这里测（上行 POST 的往返），在 mux 那边用（接收窗口），
    /// 分成两份就等于两边各学各的，谁也拿不到完整信息。
    pub profile: Arc<LinkProfile>,
}

/// 生成一个会话组 id。客户端在启动时调用一次，之后所有会话复用。
pub fn random_group_id() -> u128 {
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    u128::from_be_bytes(b)
}

#[derive(Debug, Clone, Copy)]
pub struct Negotiated {
    pub mux_id: MuxId,
    pub fallback: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("session dead")]
pub struct SessionDead;

/// 后台任务命令
enum Command {
    WriteData(Vec<u8>),
}

/// 后台任务事件
enum Event {
    DataReceived(Vec<u8>),
    SessionDead,
}

pub struct XhttpConn {
    /// 写入命令发送器。
    ///
    /// **必须是 `PollSender` 而不是裸 `mpsc::Sender`**：`poll_write` 在通道满时
    /// 要返回 `Pending`，而 `Pending` 的契约是「已经注册了 waker」。
    /// `try_send` 给不出 waker，`reserve()` 是 async 函数在 poll 上下文里用不了；
    /// `PollSender::poll_reserve(cx)` 两者兼得。
    ///
    /// 这里曾经是裸 `Sender` + `try_send`，满了直接 `Poll::Pending`。
    ///
    /// **今天这条路走不到**：`background_task` 会把 `cmd_rx` 飞快排空进无界的
    /// `agg_buffer`（窗口占满时 `flush` 直接返回、数据留在缓冲里），所以通道
    /// 实际上填不满——这也是为什么没有针对它的测试：构造不出那个状态的测试
    /// 是假测试。但它是个装好的地雷：谁给上行加背压（该加，`agg_buffer`
    /// 无界本身就是个问题），写任务就会停在一个**没有 waker 的 `Pending`** 上，
    /// 运行时再也没有理由碰它——表现为会话静默挂死、零错误日志。
    cmd_tx: tokio_util::sync::PollSender<Command>,
    /// 读取数据接收器
    event_rx: mpsc::Receiver<Event>,
    /// 读取缓冲
    read_buffer: Vec<u8>,
    /// 会话是否死亡
    dead: bool,
}

/// 发送任务、聚合循环与下行任务共享的会话状态。
struct SharedState {
    sid_b64: String,
    /// 下一个上行 seq（握手用 0，数据/心跳从 1 起）
    next_seq: u64,
    /// 在途窗口：seq -> 完整 POST 字节。重试原样重发同一字节，
    /// 不重新加密（nonce 序 = TU 加密顺序 = seq 顺序，spec §6.4）。
    in_flight: HashMap<u64, Bytes>,
    dead: bool,
    last_write: Instant,
    last_flush: Instant,
    /// 握手后的 Noise 状态：write_message = 上行加密，read_message = 下行解密
    noise: TransportState,
    /// 这条链路的实测画像，决定当前该用哪一档形状参数。
    profile: Arc<LinkProfile>,
    /// 最近一次观察到**真并发**的时刻（聚合循环里 `extra >= 2` 的判据）。
    ///
    /// 流型不能每轮翻转：一次孤立请求就把批量流打回交互档，会让大文件传输
    /// 的参数在两档之间反复横跳。给它一个粘性窗口，见 `flow_kind`。
    last_concurrent: Option<Instant>,
}

/// 流型的粘性窗口：最后一次观察到并发之后，这么久内仍算批量流。
///
/// 取 1 秒是因为批量传输的间隙（一次 GC、一次磁盘写、对端的一次慢响应）
/// 通常在百毫秒量级，比这短得多；而真正的交互式请求之间的间隔远长于此。
const FLOW_STICKY: Duration = Duration::from_secs(1);

impl SharedState {
    /// 当前流型。见 `last_concurrent` 的注释。
    fn flow_kind(&self) -> FlowKind {
        match self.last_concurrent {
            // **从未观察到并发 → 用默认档，不是交互式。**
            //
            // "还没看到并发"和"这是交互式流量"是两件事。原先这里落到
            // `Interactive`，于是每条会话从建立那一刻起就吃减半后的参数——
            // 而"默认行为与改造前逐字节相同"这条保底原则正是被它破掉的，
            // 且完全无声（`window_capped_at_8` 断言的是 `<= 8`，4 也满足）。
            None => FlowKind::Bulk,
            Some(t) if t.elapsed() < FLOW_STICKY => FlowKind::Bulk,
            Some(_) => FlowKind::Interactive,
        }
    }

    /// 当前该用的一组形状参数。
    fn tier(&self) -> Tier {
        self.profile.tier(self.flow_kind())
    }
}

impl XhttpConn {
    pub async fn connect<T: HttpTransport + 'static>(
        transport: Arc<T>,
        cfg: &UpstreamCfg,
    ) -> Result<(Self, Negotiated)> {
        let mut sid = [0u8; SID_LEN];
        rand::rng().fill_bytes(&mut sid);
        let sid_b64 = base64_url::encode(&sid);

        let mut client = build_client(&cfg.server_pub, &cfg.client_priv)?;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;

        let hello = encode_msg1(now_ms, cfg.group_id, &cfg.mux_prefs, cfg.ip_strategy);

        let mut msg1_buf = vec![0u8; 65535];
        let msg1_len = client.write_message(&hello, &mut msg1_buf)?;
        let msg1_cipher = &msg1_buf[..msg1_len];

        let mut msg1_tu = Vec::with_capacity(2 + msg1_cipher.len());
        msg1_tu.extend_from_slice(&(msg1_cipher.len() as u16).to_be_bytes());
        msg1_tu.extend_from_slice(msg1_cipher);

        let path = format!("/api/sync?n=0&sid={}", sid_b64);
        let reply = transport.post(&path, Bytes::from(msg1_tu)).await?;

        if reply.status != 200 {
            return Err(anyhow!("handshake failed: status {}", reply.status));
        }

        if reply.body.len() < 2 {
            return Err(anyhow!("msg2 body too short"));
        }
        let msg2_len = u16::from_be_bytes([reply.body[0], reply.body[1]]) as usize;
        if reply.body.len() < 2 + msg2_len {
            return Err(anyhow!("msg2 truncated"));
        }
        let msg2_cipher = &reply.body[2..2 + msg2_len];

        let mut msg2_buf = vec![0u8; 65535];
        let msg2_plain_len = client.read_message(msg2_cipher, &mut msg2_buf)?;
        let msg2_plain = &msg2_buf[..msg2_plain_len];
        let msg2 = decode_msg2(msg2_plain)?;

        let shared = Arc::new(Mutex::new(SharedState {
            sid_b64,
            next_seq: 1,
            in_flight: HashMap::new(),
            dead: false,
            last_write: Instant::now(),
            last_flush: Instant::now(),
            noise: client.into_transport_mode()?,
            profile: cfg.profile.clone(),
            last_concurrent: None,
        }));

        let (cmd_tx, cmd_rx) = mpsc::channel(128);
        let (event_tx, event_rx) = mpsc::channel(128);

        if msg2.fallback {
            tracing::warn!(
                "服务端不支持 {:?}，已回退到基线 mux（性能可能下降）",
                cfg.mux_prefs.first()
            );
        }

        // 启动后台任务
        let transport_clone = transport.clone();
        tokio::spawn(async move {
            Self::background_task(shared, transport_clone, cmd_rx, event_tx).await;
        });

        Ok((
            Self {
                cmd_tx: tokio_util::sync::PollSender::new(cmd_tx),
                event_rx,
                read_buffer: Vec::new(),
                dead: false,
            },
            Negotiated {
                mux_id: msg2.chosen_mux_id,
                fallback: msg2.fallback,
            },
        ))
    }

    async fn background_task<T: HttpTransport + 'static>(
        shared: Arc<Mutex<SharedState>>,
        transport: Arc<T>,
        mut cmd_rx: mpsc::Receiver<Command>,
        event_tx: mpsc::Sender<Event>,
    ) {
        let mut agg_buffer = Vec::new();
        // 合并窗口的到期时刻；None = 当前没有待合并的批次。
        // `recv_many` 的收件篮，循环里复用，不每轮重新分配。
        let mut cmd_batch: Vec<Command> = Vec::with_capacity(RECV_BATCH);
        let mut ticker = interval(Duration::from_millis(TICK_GRANULARITY_MS));
        let mut heartbeat_interval = interval(Duration::from_millis(IDLE_HEARTBEAT_MS));

        // 启动下行任务
        let downlink_path = format!("/api/events?sid={}", shared.lock().await.sid_b64);
        let transport_clone = transport.clone();
        let shared_clone = shared.clone();
        let downlink_event_tx = event_tx.clone();
        tokio::spawn(async move {
            Self::downlink_task(
                transport_clone,
                downlink_path,
                shared_clone,
                downlink_event_tx,
            )
            .await;
        });

        loop {
            if shared.lock().await.dead {
                let _ = event_tx.try_send(Event::SessionDead);
                return;
            }

            let mut collected = false;
            tokio::select! {
                // 一次把排队的写入全取走，然后立刻发。
                //
                // 聚合的目的是"别为几十字节单开一个 POST"，而不是"等一等看还有没有
                // 更多数据"——后者是纯粹的延迟。队列被取空就说明上层此刻已经没有
                // 别的东西要写了，再等只是让请求干等着。
                //
                // 之前的判据是"距上次 flush 超过 N 毫秒才发"。它在**连续写入**上
                // 恰好失效：一个 HTTP 请求会连着写两次（stripe 的 OPEN 头 + 请求
                // 本身），第一次写触发了 flush，第二次紧跟其后、距离刚才那次 flush
                // 不到阈值，于是被扣下来等满 `AGGREGATE_MS` 的 tick。实测这一等就是
                // 端到端延迟的大头：阈值 0/1/4ms 对应 P50 4.5/12.0/12.2ms。
                //
                // `recv_many` 同时解决聚合与延迟：有多少收多少（合成一个 POST），
                // 收完就走（不等 tick）。
                n = cmd_rx.recv_many(&mut cmd_batch, RECV_BATCH) => {
                    if n == 0 {
                        return; // 通道关闭
                    }
                    let mut st = shared.lock().await;
                    st.last_write = Instant::now();
                    for c in cmd_batch.drain(..) {
                        match c {
                            Command::WriteData(data) => agg_buffer.extend_from_slice(&data),
                        }
                    }
                    // 距上次发出还不到两个窗口 → 这会儿请求是密集的，八成还有
                    // 邻近的小包正在路上，值得多等一个窗口把它们并进来。孤立请求
                    // （上次发出已经很久）直接走，延迟一点不加。
                    drop(st);
                    collected = true;
                }
                // 合并窗口到期：把这一窗攒下的小包作为一个 POST 发出去。

                _ = ticker.tick() => {
                    let mut st = shared.lock().await;
                    let tier = st.tier();
                    let elapsed = st.last_write.elapsed();
                    let should_send =
                        agg_buffer.len() >= aggregate_bytes(&tier) || elapsed >= tier.agg_wait;
                    if should_send {
                        Self::flush(&shared, &mut st, &mut agg_buffer, &transport, &event_tx);
                    }
                }
                _ = heartbeat_interval.tick() => {
                    let mut st = shared.lock().await;
                    let idle = st.last_write.elapsed() >= Duration::from_millis(IDLE_HEARTBEAT_MS);
                    if idle {
                        // 心跳 PADDING TU：与数据完全相同的窗口/seq 路径，无旁路（spec §6.5）
                        let tu = Self::encode_single_tu(&mut st, &Frame::Padding);
                        let seq = st.next_seq;
                        st.next_seq += 1;
                        st.in_flight.insert(seq, tu.clone());
                        let sid_b64 = st.sid_b64.clone();
                        drop(st);
                        Self::spawn_send(transport.clone(), shared.clone(), sid_b64, seq, tu, event_tx.clone());
                    }
                }
            }

            // 出了 select!，`cmd_rx` 的借用结束，这里才能再捞一轮。
            //
            // 让出一次调度再捞，是为了把**同一个请求的连续写入**并进一个 POST：
            // stripe 开一条 conn 会连着写两次（OPEN 头 + 请求数据），`recv_many`
            // 只要队列里有一条就返回，于是这两次被拆成两个串行 POST——第二个还得
            // 等前一个的往返，实测串行小包 P50 从 1.8ms 掉到 3.0ms。
            //
            // 这里不用时间窗口：实测 100/200/400µs 三档下串行延迟同样都是 3.0ms，
            // 说明代价根本不在"等多久"，而在"拆成了两个 POST"。让出一次是微秒级的，
            // 却正好给了对端任务把第二次写入排进队列的机会。
            if collected {
                tokio::task::yield_now().await;
                let tier = shared.lock().await.tier();
                let mut extra = 0usize;
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        Command::WriteData(data) => agg_buffer.extend_from_slice(&data),
                    }
                    extra += 1;
                    if agg_buffer.len() >= aggregate_bytes(&tier) {
                        break;
                    }
                }

                // 让出一次就捞到两条以上，说明**真的有并发**在排队：单个请求最多
                // 也就是 OPEN 头 + 请求数据这两笔，捞到第二笔以上必然来自别的连接。
                // 这时候再等一个短窗口，把陆续到达的兄弟请求并进同一个 POST——
                // 每个 POST 都要付一整套跨进程 + HTTP 栈的固定开销，能并就别分开。
                //
                // 判据卡在 `>= 2` 上，正是为了不误伤串行：串行场景 `extra` 恒为 1，
                // 一秒都不会多等（早先按"最近是否发过"判，串行 P50 从 1.8ms 掉到
                // 3.0ms，就是被这个误伤的）。
                //
                // 这个判据同时是**流型的来源**：捞到两笔以上就是真并发，也就是
                // 批量流。流型定了用哪一套档位参数（交互式把合并窗口归零），
                // 所以这里记一笔时刻，由 `SharedState::flow_kind` 加粘性窗口读取。
                // 不另造一套流型识别——这个判据是实测扫出来的，换一个等于把
                // 「串行不误伤」那条结论重新赌一遍。
                if extra >= 2 {
                    shared.lock().await.last_concurrent = Some(Instant::now());
                }
                if extra >= 2 && agg_buffer.len() < aggregate_bytes(&tier) {
                    // 用上面那份 `tier`，不重新取。
                    //
                    // 这里曾经再锁一次，理由写的是「上面刚把流型置成批量，合并
                    // 窗口的取值跟着变」。那个理由今天不成立：`to_interactive`
                    // 是恒等的，流型不改变任何字段（见它的注释——两处差异都被
                    // 实测拿掉了）。于是那次加锁是一个可证明的空操作，还在
                    // 每一批并发写入上多抢一次共享锁。
                    //
                    // 上面的 `last_concurrent` 记账要留着：它是流型判据的输入，
                    // 等哪天真有了单变量证据、差异化加回 `to_interactive` 时，
                    // 要变的是那一个函数，而不是重新在这里散一次取档。
                    tokio::time::sleep(merge_window(&tier)).await;
                    while let Ok(cmd) = cmd_rx.try_recv() {
                        match cmd {
                            Command::WriteData(data) => agg_buffer.extend_from_slice(&data),
                        }
                        if agg_buffer.len() >= aggregate_bytes(&tier) {
                            break;
                        }
                    }
                }

                let mut st = shared.lock().await;
                Self::flush(&shared, &mut st, &mut agg_buffer, &transport, &event_tx);
            }
        }
    }

    /// 把聚合缓冲编码为一个 POST 的 TU 串并发送（窗口允许时）。
    /// 单 POST ≤ MAX_TUS_PER_POST 个 TU（≤ 1 MB），剩余留给下次 flush。
    fn flush<T: HttpTransport + 'static>(
        shared: &Arc<Mutex<SharedState>>,
        st: &mut SharedState,
        agg_buffer: &mut Vec<u8>,
        transport: &Arc<T>,
        event_tx: &mpsc::Sender<Event>,
    ) {
        if agg_buffer.is_empty() || st.dead {
            return;
        }
        // 窗口占满时数据留在**无界**的 agg_buffer 里等下一次 tick。缓冲堆多少、
        // 堵了多久，此前完全不可见——而它正是「请求莫名卡 10 秒」这类现象的
        // 首要嫌疑。
        if st.in_flight.len() >= max_inflight(&st.tier()) {
            tracing::debug!(
                pending_bytes = agg_buffer.len(),
                in_flight = st.in_flight.len(),
                "上行窗口占满，本次 flush 跳过"
            );
            return;
        }
        let cap = MAX_TUS_PER_POST * MAX_PAYLOAD;
        let take = agg_buffer.len().min(cap);
        let data: Vec<u8> = agg_buffer.drain(..take).collect();

        let body = Self::encode_tus(&mut st.noise, &data);
        let seq = st.next_seq;
        st.next_seq += 1;
        st.in_flight.insert(seq, body.clone());
        st.last_flush = Instant::now();
        let sid_b64 = st.sid_b64.clone();

        Self::spawn_send(
            transport.clone(),
            shared.clone(),
            sid_b64,
            seq,
            body,
            event_tx.clone(),
        );
    }

    fn spawn_send<T: HttpTransport + 'static>(
        transport: Arc<T>,
        shared: Arc<Mutex<SharedState>>,
        sid_b64: String,
        seq: u64,
        body: Bytes,
        event_tx: mpsc::Sender<Event>,
    ) {
        tokio::spawn(async move {
            if send_post(transport, sid_b64, seq, body, shared).await == PostOutcome::Fatal {
                let _ = event_tx.try_send(Event::SessionDead);
            }
        });
    }

    async fn downlink_task<T: HttpTransport + 'static>(
        transport: Arc<T>,
        path: String,
        shared: Arc<Mutex<SharedState>>,
        event_tx: mpsc::Sender<Event>,
    ) {
        let stream = match transport.get_stream(&path).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("downlink GET failed: {}", e);
                kill_session(&shared).await;
                let _ = event_tx.try_send(Event::SessionDead);
                return;
            }
        };

        let mut decoder = TuDecoder::new();
        let mut stream = stream;
        // 解密缓冲**在循环外**分配一次。曾经写在每个 TU 的循环体里，于是
        // 每个 TU 都要 `vec![0u8; 65535]` —— 一次 64KB 分配加一次 64KB 清零。
        // 100 MB/s 下这就是每秒一千多次、约 100 MB/s 的纯 memset，剖析里
        // `_platform_memmove` 与 `free_medium` 双双进前列正是这里。
        // 单个 TU 明文上限 65535，容量够，`read_message` 只写不读旧内容，
        // 不清零也不会泄漏上一轮的数据（返回的 n 之外的字节永远不被读）。
        let mut plain_buf = vec![0u8; 65535];
        // 下行的**静默间隔**是最关键的一个数：请求卡住时，到底是我们没发出去，
        // 还是发出去了但对端半天不回，只有这个数分得开。
        let mut last_chunk = Instant::now();
        // 下行吞吐的累计器：**按时间窗口报，不按 chunk 报**。
        //
        // 单个 chunk 的到达间隔常在微秒级，那个尺度上 `bytes/gap` 量的是
        // 调度抖动而不是链路速率——一次内存拷贝也能"跑出" GB/s。攒够一个
        // 窗口再除，才是这段时间里真实的交付速率。
        let mut win_bytes = 0u64;
        let mut win_start = Instant::now();

        while let Some(chunk) = stream.next().await {
            let gap = last_chunk.elapsed();
            last_chunk = Instant::now();
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(?gap, error = %e, "下行流出错");
                    eprintln!("downlink stream error: {}", e);
                    break;
                }
            };
            if gap > Duration::from_secs(1) {
                tracing::warn!(?gap, bytes = chunk.len(), "下行静默后才收到数据");
            } else {
                // `since_up` = 距最近一次上行发出的时间。小请求是严格一来一回的，
                // 所以它近似就是"上行离开本机 → 对应下行回到本机"的整段往返：
                // 服务端处理 + 目标往返 + 下行经 WebKit/IPC 回来。端到端延迟减掉
                // 它，剩下的才是本机协议栈（SOCKS/stripe/mux）自己花掉的。
                let since_up = shared.lock().await.last_flush.elapsed();
                tracing::debug!(?gap, ?since_up, bytes = chunk.len(), "下行分片");
            }

            // 累计这一窗的下行字节，够一窗就报给链路画像。
            //
            // 静默超过一窗时直接重开：那段空闲不是"传得慢"，把它算进分母会
            // 让一条空闲链路看起来带宽极低，进而被降档——恰恰在它随时可能
            // 要提速的时候。
            win_bytes += chunk.len() as u64;
            let win = win_start.elapsed();
            if gap >= DOWNLINK_WINDOW {
                win_bytes = chunk.len() as u64;
                win_start = Instant::now();
            } else if win >= DOWNLINK_WINDOW {
                shared
                    .lock()
                    .await
                    .profile
                    .observe_downlink(win_bytes, win);
                win_bytes = 0;
                win_start = Instant::now();
            }

            // TuDecoder 返回的每个元素是完整 TU（含 2 字节长度前缀）：
            // 先剥前缀取密文体，再走 Noise transport-state read_message 解密
            //（spec §6.1，生产路径，禁止跳过解密直接解帧）。
            for tu in decoder.push(&chunk) {
                if tu.len() < 2 {
                    continue;
                }
                let tu_cipher = &tu[2..];
                let frame = {
                    let mut st = shared.lock().await;
                    if st.dead {
                        return;
                    }
                    let n = match st.noise.read_message(tu_cipher, &mut plain_buf) {
                        Ok(n) => n,
                        Err(_) => {
                            eprintln!("downlink TU decrypt failed");
                            drop(st);
                            kill_session(&shared).await;
                            let _ = event_tx.try_send(Event::SessionDead);
                            return;
                        }
                    };
                    match decode_frame(&plain_buf[..n]) {
                        Ok(f) => f,
                        Err(_) => {
                            eprintln!("downlink frame decode failed");
                            drop(st);
                            kill_session(&shared).await;
                            let _ = event_tx.try_send(Event::SessionDead);
                            return;
                        }
                    }
                };

                match frame {
                    Frame::Data(data) => {
                        // **必须是 `send().await`（背压），不能是 `try_send` + return。**
                        //
                        // 曾经这里是「通道满就 return」——通道容量 128，读侧
                        // （`XhttpConn::poll_read`）稍微跟不上一点，整个下行任务
                        // 就自杀，会话随之判死重连。表现是吞吐**断崖**而非渐变：
                        // 实测总在途从 2MB 涨到 4MB 时，8 流吞吐从 102 MB/s 掉到
                        // 22 MB/s，而 CPU 反而更低（大半时间在重连）。
                        //
                        // 通道满是**正常的流控信号**，不是错误：此处阻塞会一路
                        // 反压到 TCP 接收窗口，正是它该有的样子。只有对端真的
                        // 关闭（接收端已销毁）才是终止条件。
                        if event_tx.send(Event::DataReceived(data)).await.is_err() {
                            return;
                        }
                    }
                    Frame::Padding => {}
                }
            }
        }

        // 流结束/错误 → 断会话（spec §9.1）
        kill_session(&shared).await;
        let _ = event_tx.try_send(Event::SessionDead);
    }

    fn encode_single_tu(st: &mut SharedState, frame: &Frame) -> Bytes {
        let plain = encode_frame(frame, &mut rand::rng()).expect("padding frame encodes");
        let mut cipher_buf = vec![0u8; 65535];
        let cipher_len = st
            .noise
            .write_message(&plain, &mut cipher_buf)
            .expect("transport encrypt");
        let mut tu = Vec::with_capacity(2 + cipher_len);
        tu.extend_from_slice(&(cipher_len as u16).to_be_bytes());
        tu.extend_from_slice(&cipher_buf[..cipher_len]);
        Bytes::from(tu)
    }

    fn encode_tus(noise: &mut TransportState, data: &[u8]) -> Bytes {
        // 输出容量一次算够：密文比明文长一个 16 字节 tag，再加 2 字节长度前缀。
        // 不预留的话 `out` 要在一次 1MB 的 flush 里反复扩容重拷。
        let n_tus = data.len().div_ceil(MAX_PAYLOAD).max(1);
        let mut out = Vec::with_capacity(data.len() + n_tus * 64);
        let mut remaining = data;
        // 密文缓冲同样只分配一次（理由见 downlink_task 里那份的注释）。
        let mut cipher_buf = vec![0u8; 65535];

        while !remaining.is_empty() {
            let chunk_size = remaining.len().min(MAX_PAYLOAD);
            let chunk = &remaining[..chunk_size];
            remaining = &remaining[chunk_size..];

            let frame = Frame::Data(chunk.to_vec());
            let plain = encode_frame(&frame, &mut rand::rng()).unwrap();

            let cipher_len = noise.write_message(&plain, &mut cipher_buf).unwrap();
            let cipher = &cipher_buf[..cipher_len];

            out.extend_from_slice(&(cipher_len as u16).to_be_bytes());
            out.extend_from_slice(cipher);
        }

        Bytes::from(out)
    }
}

/// send_post 的结局
#[derive(Debug, PartialEq, Eq)]
enum PostOutcome {
    /// 约定的成功响应（n≥1: 204+空body；n=0: 200+合法 msg2）→ 已释放窗口槽
    Delivered,
    /// 会话已死亡：传输层错误/超时/5xx 重试 ≤2 次（指数退避）仍失败，
    /// 或收到非约定响应（立即，不重试）→ 调用方据此上抛 SessionDead
    Fatal,
}

/// 发送单个 POST（含完整重试策略，spec §6.4）。窗口槽持有完整 POST 字节，
/// 重试原样重发同一 seq 同一字节——绝不重新加密，nonce 序不乱。
async fn send_post<T: HttpTransport + 'static>(
    transport: Arc<T>,
    sid_b64: String,
    seq: u64,
    body: Bytes,
    shared: Arc<Mutex<SharedState>>,
) -> PostOutcome {
    let path = format!("/api/sync?n={}&sid={}", seq, sid_b64);

    let mut attempt: u8 = 0;
    let mut backoff = RETRY_BACKOFF_INITIAL;

    loop {
        // 每个 POST 的往返耗时。上行卡顿到底卡在「等窗口」还是「等服务端应答」，
        // 只有这两个数字并排才分得开。
        let t0 = Instant::now();
        let outcome = transport.post(&path, body.clone()).await;
        let rtt = t0.elapsed();
        // 喂给链路画像。这个测量点本来就在（为了那条"上行 POST 异常慢"的
        // 日志），此前只进 tracing——自适应需要的信号一直都有人采，只是没人用。
        //
        // `attempt > 0` 要告诉画像：重试过的样本里含了退避时间，拿它算带宽
        // 会把链路严重低估。
        {
            let st = shared.lock().await;
            st.profile
                .observe_post(rtt, body.len(), attempt > 0);
            // 服务端自报的处理耗时要从这次往返里扣掉，剩下的才是纯网络 RTT。
            // 顺序不能反：`observe_peer` 修的是**刚刚**那条 RTT 样本。
            if let Ok(ref reply) = outcome {
                if let Some(p) = reply.peer {
                    st.profile
                        .observe_peer(Duration::from_micros(p.server_us as u64), p.gaps as u32);
                }
            }
        }
        if rtt > Duration::from_secs(1) {
            tracing::warn!(seq, ?rtt, bytes = body.len(), attempt, "上行 POST 异常慢");
        } else {
            tracing::debug!(seq, ?rtt, bytes = body.len(), "上行 POST");
        }
        let retryable = match outcome {
            // 传输层错误/超时 → 不知是否送达 → 重试
            Err(ref e) => {
                tracing::warn!(seq, ?rtt, attempt, error = %e, "上行 POST 失败，将重试");
                true
            }
            Ok(reply) => {
                if seq == 0 {
                    // n=0 约定响应：200 + 合法 msg2（msg2 合法性在 connect 里验证）
                    if reply.status == 200 {
                        let mut st = shared.lock().await;
                        st.in_flight.remove(&seq);
                        return PostOutcome::Delivered;
                    }
                    return fatal(&shared).await;
                }
                if reply.status == 204 && reply.body.is_empty() {
                    // 送达（或被去重，等价）→ 释放窗口槽
                    let mut st = shared.lock().await;
                    st.in_flight.remove(&seq);
                    return PostOutcome::Delivered;
                }
                // 5xx → 不知是否送达 → 重试；其余一切 → 会话失效，不重试
                reply.status >= 500
            }
        };

        if retryable && attempt < RETRY_MAX {
            attempt += 1;
            tokio::time::sleep(backoff).await;
            backoff *= 4; // 100ms → 400ms
            continue;
        }

        // 重试耗尽或非约定响应 → 断会话
        return fatal(&shared).await;
    }
}

async fn fatal(shared: &Arc<Mutex<SharedState>>) -> PostOutcome {
    let mut st = shared.lock().await;
    st.dead = true;
    st.in_flight.clear();
    PostOutcome::Fatal
}

async fn kill_session(shared: &Arc<Mutex<SharedState>>) {
    let mut st = shared.lock().await;
    st.dead = true;
    st.in_flight.clear();
}

impl AsyncRead for XhttpConn {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // 先检查事件通道
        loop {
            match std::pin::Pin::new(&mut self.event_rx).poll_recv(cx) {
                Poll::Ready(Some(Event::DataReceived(data))) => {
                    self.read_buffer.extend_from_slice(&data);
                }
                Poll::Ready(Some(Event::SessionDead)) => {
                    self.dead = true;
                }
                Poll::Ready(None) => {
                    self.dead = true;
                }
                Poll::Pending => break,
            }
        }

        // 先吐缓冲里的数据再报死亡（会话死亡前到达的数据仍可读）
        if !self.read_buffer.is_empty() {
            let n = self.read_buffer.len().min(buf.remaining());
            let data = self.read_buffer.drain(..n).collect::<Vec<_>>();
            buf.put_slice(&data);
            return Poll::Ready(Ok(()));
        }

        if self.dead {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "session dead",
            )));
        }

        Poll::Pending
    }
}

impl AsyncWrite for XhttpConn {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.dead {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "session dead",
            )));
        }

        // 先抢名额再拷贝：反过来的话每次通道满都白分配一个立刻被丢弃的 Vec，
        // 而通道满恰恰是高负载下最常走的分支。
        match self.cmd_tx.poll_reserve(cx) {
            // Pending 由 poll_reserve 自己注册 waker——名额腾出来时会被叫醒，
            // 这正是原来那版 `try_send` 缺的东西。
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(_)) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "channel closed",
            ))),
            Poll::Ready(Ok(())) => match self.cmd_tx.send_item(Command::WriteData(buf.to_vec())) {
                Ok(()) => Poll::Ready(Ok(buf.len())),
                // poll_reserve 刚返回 Ok，名额是我们的；能失败只剩「接收端已关闭」。
                Err(_) => Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "channel closed",
                ))),
            },
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.dead = true;
        Poll::Ready(Ok(()))
    }
}

mod base64_url {
    pub fn encode(data: &[u8]) -> String {
        use base64::prelude::*;
        BASE64_URL_SAFE_NO_PAD.encode(data)
    }
}
