//! v2 条带化运行时（协议对称，客户端/服务端共用一套实现）。
//!
//! 一个 `StripeConn` 管理一条逻辑连接的全部 lane：
//! - 发送方向（客户端=UP，服务端=DOWN）：单写者任务按 64KiB 分片、多 lane
//!   轮转、阈值触发自动加 lane、结束时在「最闲」lane 上发 CLOSE。
//! - 接收方向：每 lane 一个读任务，按绝对 offset 重组（空洞缓存 + 连续推进）；
//!   CLOSE(final_offset) 送达且全部字节交付后向上层报 EOF，有缺口 → 错误。
//!
//! `StripeDialer`（客户端）：conn_id 单调分配、mux.open 建 BIDI 首 lane
//! （OPEN 头 + TargetAddr 前缀）、后台 accept 任务归并对端新开 lane。
//! `StripeListener`（服务端）：accept 新 mux 流，按 conn_id 路由到已有 conn；
//! 未知 conn / 非 OPEN 首帧 → 丢流。新 conn 的 OPEN 携带 TargetAddr。

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
// `now_or_never`：非阻塞地收掉已完成的在途写入。用 `StreamExt::next().await`
// 会把「顺手收一下」变成「等齐」，那正是这次要去掉的队头阻塞。
use futures::{FutureExt, StreamExt};
use std::future::Future;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::mpsc::Sender;
use tokio::sync::Notify;

use crate::{Mux, MuxStream};
use wsieve_proto::addr::{decode_addr, AddrPort};
use wsieve_proto::stripe::{
    decode_close_payload, decode_frame, decode_header, encode_close_payload, encode_frame,
    encode_header, is_inline_header, CloseReason, Cmd, ConnHeader, Dir, HEADER_LEN, CHUNK,
};

/// 开 lane 抽象：给一条 conn 打开一条新 lane（写好 OPEN 头后返回流）。
/// 多会话时由 StripeDialer 提供（跨会话轮转）；单会话时退化为固定 mux。
pub type LaneOpener = Arc<
    dyn Fn(Dir) -> BoxFuture<'static, anyhow::Result<MuxStream>> + Send + Sync,
>;

/// 「这是一条追加 lane，不是新 conn」的线上标记。
///
/// 首 lane 的 OPEN 后面跟着 TargetAddr，追加 lane 没有。接收侧此前只能靠
/// 「conn 认不认识」来区分，而那正是竞态所在：追加 lane 抢在 conn 登记之前
/// 到达时，会被当成新 conn 去 `read_open_addr`——把分片数据当地址解析。
///
/// `lane_id` 本来就在线上（头部 11..13 字节），一直写 0 没派上用场，正好
/// 拿来做这个标记。数值本身不参与路由，只区分「首」与「追加」。
const LANE_ID_EXTRA: u16 = 1;

/// 在指定 mux 上开一条**追加** lane 并写好 OPEN 头。
///
/// 首 lane 不走这里（它要带 TargetAddr，见 `StripeDialer::connect`），
/// 因此这里恒填 [`LANE_ID_EXTRA`]。
async fn open_lane_on(
    mux: &Arc<dyn Mux>,
    conn_id: u64,
    dir: Dir,
) -> anyhow::Result<MuxStream> {
    let mut stream = mux.open().await?;
    let hdr = encode_header(&ConnHeader {
        conn_id,
        cmd: Cmd::Open,
        dir,
        lane_id: LANE_ID_EXTRA,
    })
    .to_vec();
    stream.write_all(&hdr).await?;
    Ok(stream)
}

/// 单 mux 版 opener（服务端 / 单会话客户端）。
fn single_mux_opener(conn_id: u64, mux: Arc<dyn Mux>) -> LaneOpener {
    Arc::new(move |dir| {
        let mux = mux.clone();
        Box::pin(async move { open_lane_on(&mux, conn_id, dir).await })
    })
}

/// 上行待发队列上限（字节）。超过 → poll_write Pending（写侧背压）。
const MAX_PENDING: usize = 4 * 1024 * 1024;

/// 升级阈值 / lane 目标数。默认即 pinned 值；测试可用环境变量调小：
/// `WSIEVE_STRIPE_LANES` / `WSIEVE_STRIPE_UPGRADE_BYTES` /
/// `WSIEVE_STRIPE_UPGRADE_RATE_BPS` / `WSIEVE_STRIPE_UPGRADE_WINDOW_MS`。
#[derive(Debug, Clone)]
pub struct StripeCfg {
    pub target_lanes: usize,
    pub upgrade_bytes: u64,
    pub upgrade_rate_bps: u64,
    pub upgrade_window: Duration,
    /// 额外 XHTTP 会话数（多 TCP 条带）。0 = 单会话（既有行为）。
    /// 由上层 wiring 负责实际建会话并 `StripeDialer::attach_session`。
    pub extra_sessions: usize,
}

impl Default for StripeCfg {
    fn default() -> Self {
        Self {
            target_lanes: 4,
            upgrade_bytes: 1024 * 1024,
            upgrade_rate_bps: 1024 * 1024,
            upgrade_window: Duration::from_secs(1),
            extra_sessions: 0,
        }
    }
}

impl StripeCfg {
    /// 默认值 + 环境变量覆盖（测试旋钮）。
    pub fn with_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(v) = std::env::var("WSIEVE_STRIPE_LANES") {
            if let Ok(n) = v.parse() {
                cfg.target_lanes = n;
            }
        }
        if let Ok(v) = std::env::var("WSIEVE_STRIPE_UPGRADE_BYTES") {
            if let Ok(n) = v.parse() {
                cfg.upgrade_bytes = n;
            }
        }
        if let Ok(v) = std::env::var("WSIEVE_STRIPE_UPGRADE_RATE_BPS") {
            if let Ok(n) = v.parse() {
                cfg.upgrade_rate_bps = n;
            }
        }
        if let Ok(v) = std::env::var("WSIEVE_STRIPE_UPGRADE_WINDOW_MS") {
            if let Ok(n) = v.parse() {
                cfg.upgrade_window = Duration::from_millis(n);
            }
        }
        if let Ok(v) = std::env::var("WSIEVE_EXTRA_SESSIONS") {
            if let Ok(n) = v.parse() {
                cfg.extra_sessions = n;
            }
        }
        cfg
    }
}

// ---------------- 内部状态 ----------------

/// 接收方向重组状态（std Mutex：所有临界区无 await，极短）。
struct RecvState {
    pending_early: Option<Vec<u8>>,
    buf: BytesMut,
    contig: u64,
    holes: BTreeMap<u64, Bytes>,
    closed: Option<(u64, CloseReason)>,
    failed: Option<io::Error>,
    live_lanes: usize,
    reader_waker: Option<Waker>,
}

fn wake_reader(st: &mut RecvState) {
    // 无条件 wake：Waker::wake_from_cloned 语义安全，且规避 tokio 多消费者
    // 场景下 permit 丢失的边角。
    if let Some(w) = st.reader_waker.take() {
        w.wake_by_ref();
        st.reader_waker = Some(w);
    }
}

/// 发送方向待发队列（写侧背压）。
struct OutState {
    queue: VecDeque<Vec<u8>>,
    pending: usize,
    writer_waker: Option<Waker>,
    /// 上层已 shutdown / 显式 close：排空后发 CLOSE。
    closing: bool,
    close_reason: CloseReason,
    /// 发送面已终结（lane 全死或 CLOSE 已发）。
    dead: bool,
    send_task_waker: Option<Waker>,
}

fn wake_send_task(out: &mut OutState) {
    if let Some(w) = out.send_task_waker.take() {
        w.wake();
    }
}

struct ConnInner {
    conn_id: u64,
    /// 接收方向终结（EOF/错误）时 notify_waiters（清表任务等它）。
    recv_terminal: Arc<tokio::sync::Notify>,
    /// 接收方向已终结的持久标记。`Notify::notify_waiters` 只唤醒「已注册」
    /// 的等待者，清表任务在 conn 建立之后才 spawn，中间存在丢失唤醒的窗口
    /// （短连接：CLOSE 先于清表任务注册到达 → 表项永久泄漏）。等待方必须
    /// 「先注册 Notified，再复查本标记」。
    recv_terminated: std::sync::atomic::AtomicBool,
    cfg: StripeCfg,
    lane_opener: LaneOpener,
    recv: Mutex<RecvState>,
    out: Mutex<OutState>,
    /// 控制 lane 集合变化 / 关闭（发送任务消费）。
    ctl: Sender<CtlMsg>,
    lane_count: Arc<AtomicUsize>,
}

impl ConnInner {
    /// 标记接收方向终结并唤醒等待者。顺序关键：先置标记再 notify，
    /// 配合等待端的「注册后复查」即可无窗口。
    fn mark_recv_terminal(&self) {
        self.recv_terminated.store(true, Ordering::Release);
        self.recv_terminal.notify_waiters();
    }
}

enum CtlMsg {
    AddLane(WriteHalf<MuxStream>),
}

fn io_closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "stripe conn send side closed")
}

impl ConnInner {
    fn request_close(&self, reason: CloseReason) {
        let mut out = self.out.lock().unwrap();
        if !out.closing && !out.dead {
            out.closing = true;
            out.close_reason = reason;
            wake_send_task(&mut out);
        }
    }
}

/// 一条逻辑连接：N 条 lane + 重组 + 调度。
pub struct StripeConn {
    inner: Arc<ConnInner>,
}

impl StripeConn {
    /// 建立一条 conn。`initial_lane` 为首 mux 流（客户端 = mux.open() 的
    /// BIDI 流；服务端 = accept 到的流），`initial_bytes` 为首帧前缀
    /// （客户端 = OPEN 头 + TargetAddr；服务端 = 空）。`send_dir` 是本端
    /// 发送方向（客户端 UP，服务端 DOWN）。`lane_opener` 负责后续加 lane
    /// （多会话时跨会话轮转）。
    fn new(
        conn_id: u64,
        cfg: StripeCfg,
        lane_opener: LaneOpener,
        send_dir: Dir,
        initial_lane: MuxStream,
        initial_bytes: Vec<u8>,
        early_inbound: Vec<u8>,
    ) -> Arc<Self> {
        let (ctl_tx, ctl_rx) = tokio::sync::mpsc::channel::<CtlMsg>(32);
        let lane_count = Arc::new(AtomicUsize::new(1));
        let inner = Arc::new(ConnInner {
            conn_id,
            recv_terminal: Arc::new(tokio::sync::Notify::new()),
            recv_terminated: std::sync::atomic::AtomicBool::new(false),
            cfg,
            lane_opener,
            recv: Mutex::new(RecvState {
                pending_early: None,
                buf: BytesMut::new(),
                contig: 0,
                holes: BTreeMap::new(),
                closed: None,
                failed: None,
                live_lanes: 1, // 首 lane
                reader_waker: None,
            }),
            out: Mutex::new(OutState {
                queue: VecDeque::new(),
                pending: 0,
                writer_waker: None,
                closing: false,
                close_reason: CloseReason::TargetEof,
                dead: false,
                send_task_waker: None,
            }),
            ctl: ctl_tx,
            lane_count,
        });
        // 服务端在解析 OPEN 时可能同批读到地址后的早期数据：直接喂入重组器
        if !early_inbound.is_empty() {
            let mut st = inner.recv.lock().unwrap();
            // 交给 lane_reader 解析：构造一个带缓冲的初始读取任务
            st.pending_early = Some(early_inbound.clone());
            drop(st);
        }
        let (r, w) = tokio::io::split(initial_lane);
        tokio::spawn(lane_reader(inner.clone(), r));
        tokio::spawn(send_task(inner.clone(), send_dir, initial_bytes, w, ctl_rx));
        Arc::new(Self { inner })
    }

    /// 上层流表面（可克隆；读写均为 conn 级）。
    pub fn stream(&self) -> StripeStreamHandle {
        StripeStreamHandle { inner: self.inner.clone() }
    }

    /// 接受一条入站 lane（首帧 ConnHeader 已由外层 accept 循环解析并消费）。
    ///
    /// 入站 lane 是**单向**的：对端 `maybe_upgrade` 开它是为了往我们这边发，
    /// 且对端把自己那一侧的读半交给了 `drain_read_half`（读完即丢）。所以
    /// 绝不能把它的写半并入本端发送集——写进去的数据会被对端静默丢弃，
    /// 接收端则永远等不到那些 offset 而挂死。本端要加发送 lane 只能自己
    /// 开（`maybe_upgrade` / `add_lanes`），双向各自升级、互不借用。
    ///
    /// 写半直接 drop：`tokio::io::split` 的底层流由读半继续持有，lane 不会
    /// 因此关闭。
    pub fn accept_lane(&self, stream: MuxStream) {
        let mut st = self.inner.recv.lock().unwrap();
        st.live_lanes += 1;
        drop(st);
        let (r, _w) = tokio::io::split(stream);
        tokio::spawn(lane_reader(self.inner.clone(), r));
    }

    /// 主动加发送 lane（发送任务自身也会按阈值自动加）。
    pub async fn add_lanes(&self, n: usize) {
        for _ in 0..n {
            if let Ok(stream) = (self.inner.lane_opener)(Dir::Up).await {
                let (r, w) = tokio::io::split(stream);
                tokio::spawn(drain_read_half(r));
                let _ = self.inner.ctl.send(CtlMsg::AddLane(w)).await;
            }
        }
    }

    pub(crate) fn inner_recv_terminal(&self) -> Arc<tokio::sync::Notify> {
        self.inner.recv_terminal.clone()
    }

    /// 接收方向是否已终结（EOF/错误/CLOSE 全交付）。
    pub(crate) fn recv_terminated(&self) -> bool {
        self.inner.recv_terminated.load(Ordering::Acquire)
    }

    /// 诊断：接收方向状态快照。
    pub fn dbg_recv_state(&self) -> (usize, u64, usize, Option<(u64, u8)>, bool) {
        let st = self.inner.recv.lock().unwrap();
        (
            st.buf.len(),
            st.contig,
            st.holes.len(),
            st.closed.map(|(o, r)| (o, r.to_u8())),
            st.failed.is_some(),
        )
    }

    /// 当前发送 lane 数。
    pub fn lane_count(&self) -> usize {
        self.inner.lane_count.load(Ordering::Relaxed)
    }

    /// **接收**方向存活的 lane 数（诊断/测试）。
    ///
    /// 与 [`Self::lane_count`]（发送侧）不是一回事：`accept_lane` 归并进来的
    /// lane 只增这一个，发送侧那个纹丝不动。两者混用会让「lane 归并成功了
    /// 没有」这类断言永远看着像失败。
    pub fn recv_lane_count(&self) -> usize {
        self.inner.recv.lock().unwrap().live_lanes
    }

    pub fn conn_id(&self) -> u64 {
        self.inner.conn_id
    }

    /// 本端发送方向关闭（原因写入 CLOSE 帧）。
    pub async fn close_send(&self, reason: CloseReason) {
        self.inner.request_close(reason);
    }
}

/// 上层 AsyncRead（接收重组）+ AsyncWrite（发送队列）句柄。
pub struct StripeStreamHandle {
    inner: Arc<ConnInner>,
}

impl Clone for StripeStreamHandle {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

impl AsyncWrite for StripeStreamHandle {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut out = self.inner.out.lock().unwrap();
        if out.dead {
            return Poll::Ready(Err(io_closed()));
        }
        // 单次写入可能超过背压预算：接受部分写入（AsyncWrite 允许），
        // 否则大 buffer 会与空队列互相等死。
        let budget = MAX_PENDING.saturating_sub(out.pending);
        if budget == 0 {
            out.writer_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let n = buf.len().min(budget);
        out.pending += n;
        out.queue.push_back(buf[..n].to_vec());
        wake_send_task(&mut out);
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // 半关闭：发送侧数据排空后以 TargetEof 收尾
        self.inner.request_close(CloseReason::TargetEof);
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for StripeStreamHandle {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut st = self.inner.recv.lock().unwrap();
        if !st.buf.is_empty() {
            let n = st.buf.len().min(buf.remaining());
            buf.put_slice(&st.buf[..n]);
            let _ = st.buf.split_to(n);
            return Poll::Ready(Ok(()));
        }
        if let Some(e) = &st.failed {
            return Poll::Ready(Err(io::Error::new(e.kind(), e.to_string())));
        }
        if let Some((final_off, reason)) = st.closed {
            if st.contig >= final_off {
                return match reason {
                    CloseReason::TargetEof => Poll::Ready(Ok(())), // 干净 EOF
                    r => Poll::Ready(Err(io::Error::other(format!(
                        "stripe conn closed: {r:?}"
                    )))),
                };
            }
            // CLOSE 已到但仍有缺口：等 lane 读者终结状态
        }
        st.reader_waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

// ---------------- 接收侧 ----------------

fn apply_close(st: &mut RecvState, final_off: u64, reason: CloseReason) {
    match reason {
        CloseReason::Reset => {
            st.buf.clear();
            st.holes.clear();
            if st.failed.is_none() {
                st.failed = Some(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "stripe conn reset by peer",
                ));
            }
        }
        r => {
            if st.closed.is_none() {
                st.closed = Some((final_off, r));
            }
        }
    }
    wake_reader(st);
}

/// 单 lane 读任务：解析 DataFrame / 内联 CLOSE，喂给重组器；EOF/错误 → 终结。
async fn lane_reader(inner: Arc<ConnInner>, mut r: ReadHalf<MuxStream>) {
    let mut b = BytesMut::with_capacity(CHUNK + 32);
    // 首 lane 可能带有解析 OPEN 时同批读到的早期数据
    {
        let early = inner.recv.lock().unwrap().pending_early.take();
        if let Some(e) = early {
            b.extend_from_slice(&e);
        }
    }
    loop {
        // 解析缓冲内所有完整帧
        loop {
            if b.is_empty() {
                break;
            }
            if is_inline_header(b[0]) {
                if b.len() < HEADER_LEN {
                    break; // 需要更多字节
                }
                match decode_header(&b[..HEADER_LEN]) {
                    Ok(h) if h.conn_id == inner.conn_id && h.cmd == Cmd::Close => {
                        if b.len() < HEADER_LEN + 9 {
                            break;
                        }
                        if let Ok((final_off, reason)) =
                            decode_close_payload(&b[HEADER_LEN..HEADER_LEN + 9])
                        {
                            let _ = b.split_to(HEADER_LEN + 9);
                            let mut st = inner.recv.lock().unwrap();
                            apply_close(&mut st, final_off, reason);
                            // CLOSE 只是宣告总长度，**不等于** conn 结束：别的
                            // lane 上可能还有分片在路上，甚至还有中途加入的
                            // lane 没登记进来。这里就终结的话，注册表清理任务
                            // 会立刻把 conn 摘掉，那些 lane 回来时无处认领，
                            // 它们携带的数据就永久丢了。数据到齐才是收尾时机。
                            maybe_finish(&inner, &mut st);
                            wake_reader(&mut st);
                            drop(st);
                            continue; // 其它 lane 可能还有数据；本 lane 读到 EOF 为止
                        }
                        break;
                    }
                    Ok(_) | Err(_) => {
                        // 已建立 lane 上的非 CLOSE 内联头：协议违规，丢弃本 lane
                        lane_eof(&inner);
                        return;
                    }
                }
            }
            match decode_frame(&b) {
                Ok(Some((off, payload, used))) => {
                    // `decode_frame` 返回的 payload 是 `b` 的一段借用视图。旧代码
                    // 在这里 `Bytes::copy_from_slice` 把它整个抄一遍——满速下这是
                    // 一条与链路等速的 memcpy 带宽，剖析里 `_platform_memmove`
                    // 的大头。改成先量出它在 `b` 里的位置，再把整帧 `split_to`
                    // 走、`freeze` 成 `Bytes` 后切片：切片只加一次引用计数，零拷贝。
                    //
                    // 注意 `off` 是**流内逻辑偏移**（重组用），跟 payload 在缓冲里
                    // 的位置是两回事，不能混用。
                    let at = payload.as_ptr() as usize - b.as_ptr() as usize;
                    let len = payload.len();
                    let frame = b.split_to(used).freeze();
                    feed(&inner, off, frame.slice(at..at + len));
                }
                Ok(None) => break,
                Err(e) => {
                    let mut st = inner.recv.lock().unwrap();
                    if st.failed.is_none() {
                        st.failed =
                            Some(io::Error::new(io::ErrorKind::InvalidData, e.to_string()));
                        wake_reader(&mut st);
                    }
                    drop(st);
                    inner.mark_recv_terminal();
                    lane_eof(&inner);
                    return;
                }
            }
        }
        // 补充字节。
        //
        // 直接读进重组缓冲，不经中转数组。旧代码是 `read(&mut chunk)` 再
        // `extend_from_slice(&chunk[..n])`——那等于给每一个过路字节都加了一次
        // 完整的 memcpy，满速下就是一条与链路等宽的额外拷贝带宽。`read_buf`
        // 直接写进 `BytesMut` 的未初始化尾部，省掉的正是这一次。
        b.reserve(CHUNK + 64);
        match r.read_buf(&mut b).await {
            Ok(0) | Err(_) => {
                lane_eof(&inner);
                return;
            }
            Ok(_) => {}
        }
    }
}

/// 一条中途加入的 DOWN lane，从被 accept 到走完 `join_inbound` 登记进
/// `live_lanes`，中间有一段它还"不存在"的窗口。这个常量就是留给那段窗口的。
const LANE_JOIN_GRACE: Duration = Duration::from_millis(300);

/// 收尾阶段（排空在途写、发 CLOSE、flush/shutdown）的总时限。
///
/// 收尾路径上没有别的分支能救场：对端一旦不读，那里的每个 await 都会永久
/// Pending，而 `mark_dead` 排在它们之后——任务会停在倒数第二行，写端的 waker
/// 不被唤醒，conn 的资源一件都不回收。
///
/// 5 秒远长于任何正常的收尾（都是本地缓冲操作），又不至于让一条已经废掉的
/// conn 把回收拖到分钟级。超时就意味着放弃尾部数据，而那时对端已经不读了。
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// 一条入站 lane 结束。
///
/// 注意终结判据不是"所有 lane 都 EOF"——那只说明**已知的** lane 没数据了，
/// 不说明数据不会再来。条带允许对端中途加 lane，那条 lane 携带的分片完全可能
/// 在现有 lane 全部收尾之后才落地。判据是"数据到齐"，够不着时才退回到
/// "等一个宽限窗口仍然没有进展"。
fn lane_eof(inner: &Arc<ConnInner>) {
    let mut st = inner.recv.lock().unwrap();
    st.live_lanes = st.live_lanes.saturating_sub(1);
    if st.live_lanes > 0 {
        wake_reader(&mut st);
        return;
    }
    // 数据已齐，或对端压根没告诉过我们总长度：都可以当场收尾。
    let gap = matches!(st.closed, Some((final_off, _)) if st.contig < final_off);
    if !gap {
        finish_recv(inner, &mut st);
        wake_reader(&mut st);
        return;
    }
    drop(st);

    let inner = inner.clone();
    tokio::spawn(async move {
        tokio::time::sleep(LANE_JOIN_GRACE).await;
        let mut st = inner.recv.lock().unwrap();
        if st.live_lanes > 0 {
            // 宽限期内真的有新 lane 加进来了，终结判定交给它结束时那一轮。
            return;
        }
        finish_recv(&inner, &mut st);
        wake_reader(&mut st);
    });
}

/// 收尾接收方向：要么认定 EOF，要么认定缺口错误。
fn finish_recv(inner: &Arc<ConnInner>, st: &mut RecvState) {
    // 这里不去看 `recv_terminated` 提前返回：那个标志别处也会置，一旦提前返回，
    // 下面的缺口判定就永远不会执行，读者既等不到数据也等不到错误，只能挂死。
    // 幂等性由各分支自己的 `failed.is_none()` / `closed` 判断保证。
    inner.mark_recv_terminal();
    if let Some((final_off, _)) = st.closed {
        if st.contig < final_off && st.failed.is_none() {
            st.failed = Some(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "stripe conn ended with gap: delivered {}, final {}",
                    st.contig, final_off
                ),
            ));
        }
    } else if st.failed.is_none() {
        // 对端没发 CLOSE 就全断了：已交付的内容按 EOF 处理（尽力而为）。
        st.closed = Some((st.contig, CloseReason::TargetEof));
    }
}

/// 重组喂入：空洞缓存 + 连续推进；重叠帧按已有内容优先。
fn feed(inner: &Arc<ConnInner>, off: u64, payload: Bytes) {
    if payload.is_empty() {
        return;
    }
    let mut st = inner.recv.lock().unwrap();
    let end = off + payload.len() as u64;
    if off <= st.contig && end > st.contig {
        let skip = (st.contig - off) as usize;
        st.buf.extend_from_slice(&payload[skip..]);
        st.contig = end;
        // 用空洞推进连续区
        while let Some((&k, _)) = st.holes.first_key_value() {
            if k > st.contig {
                break;
            }
            let v = st.holes.remove(&k).unwrap();
            let vend = k + v.len() as u64;
            if vend > st.contig {
                let skip = (st.contig - k) as usize;
                st.buf.extend_from_slice(&v[skip..]);
                st.contig = vend;
            }
        }
    } else if off >= st.contig {
        st.holes.entry(off).or_insert_with(|| payload.clone());
        // 保留更长的重叠段不必要：协议保证 offset 唯一，重复帧直接忽略
        let _ = payload;
    }
    // off < contig && end <= contig：完全重复，丢弃
    // CLOSE 可能早于最后一批数据到达，补齐的这一刻就是收尾时机——
    // 走到这里就不必再等 lane EOF 和宽限窗口了。
    maybe_finish(inner, &mut st);
    wake_reader(&mut st);
}

/// 数据已达对端宣告的总长度时收尾接收方向。未收到 CLOSE 或仍有缺口则什么都不做。
fn maybe_finish(inner: &Arc<ConnInner>, st: &mut RecvState) {
    if let Some((final_off, _)) = st.closed {
        if st.contig >= final_off {
            finish_recv(inner, st);
        }
    }
}

// ---------------- 发送任务 ----------------

/// 病态慢的判据：EWMA 速率低于当前最快 lane 的 1/N 就停发。
///
/// 为什么光靠 work-stealing 不够：空闲轮转已经让分配与速率成正比（快 lane
/// 先写完先回来，自然拿到更多片），对**吞吐**是对的。但重组端是连续推进
/// ——发给一条慢 10 倍的 lane 的那一片，会把它后面所有数据一起堵住 10 倍
/// 时长。按比例少发解决不了这个，得干脆不发。
///
/// 8 是拍脑袋的初值：低于此倍数的差距在跨境链路上属于正常抖动，停发反而
/// 会把可用带宽白白丢掉；真正值得停的是那种「卡住了」级别的 lane。
/// `ponytail:` 待实测调参。
const LANE_SLOW_FACTOR: f64 = 8.0;

/// 停发的 lane 每隔这么久强制放行一片，用来刷新它的速率估计。
///
/// 没有这个，一条 lane 一旦被判慢就再也拿不到数据 ⇒ EWMA 永远停在那个旧
/// 值 ⇒ 永远出不来。链路恢复了也白搭，而跨境链路的抖动恰恰是分钟级的。
const LANE_REPROBE: Duration = Duration::from_secs(2);

/// 一条发送 lane 的健康度。
///
/// 只用**写入完成**这一个可观测量：完成一片 CHUNK 花了多久 ⇒ 瞬时速率。
/// 不去猜 RTT / 丢包——mux 层看不到它们，而写入完成时间本身就已经把窗口
/// 阻塞、重传、对端消费速度全都折进去了。
struct LaneHealth {
    /// 速率的指数滑动平均（字节/秒）。`None` = 还没有样本。
    ewma_bps: Option<f64>,
    /// 上一次真的发出数据的时刻（用于再探）。
    last_sent: Instant,
}

impl LaneHealth {
    fn new() -> Self {
        Self { ewma_bps: None, last_sent: Instant::now() }
    }

    /// 记一次写入完成。`elapsed` 过短时不采样——除以一个接近 0 的数会得到
    /// 天文数字，一次就能把 EWMA 拉到没法再下来，之后所有 lane 都显得「慢」。
    fn observe(&mut self, bytes: usize, elapsed: Duration) {
        const MIN_SAMPLE: Duration = Duration::from_micros(50);
        if elapsed < MIN_SAMPLE {
            return;
        }
        let bps = bytes as f64 / elapsed.as_secs_f64();
        // α=0.3：够快地跟上链路变化，又不至于被单次抖动带飞。
        self.ewma_bps = Some(match self.ewma_bps {
            Some(prev) => prev * 0.7 + bps * 0.3,
            None => bps,
        });
    }
}

struct LaneW {
    w: Option<WriteHalf<MuxStream>>,
    last_write: Instant,
    health: LaneHealth,
}

/// 一笔在途写入完成时带回来的东西：`(lane 下标, 写半, 成功?, 字节数, 耗时)`。
type WriteDone = (usize, WriteHalf<MuxStream>, bool, usize, Duration);

/// 收回一笔完成的在途写入：把写半还给那条 lane，并记一笔健康样本。
///
/// 返回 `false` 表示那笔写入**失败**，调用方必须 `mark_dead` 并结束发送任务。
///
/// 抽出来是因为这三行在发送任务里原样出现了三次——分发内层的背压等待、
/// 分发之后的非阻塞顺手收、停车时的 `select!` 分支——而三处必须逐字一致：
/// 漏掉 `l.w = Some(w)` 那条 lane 就永久失踪（再也挑不到它，条带静默降级成
/// 更少的 lane），漏掉 `health.observe` 健康度就冻在旧值上（慢 lane 不再被
/// 识别出来）。两种漏法都不报错，只表现为"莫名其妙变慢了"。
///
/// 收尾时的那一处（排空在途写入）**有意不走这里**：那时不该再判死（对端
/// 已经不读了，那不是链路故障），也没人会再读健康度。
fn reclaim_write(lanes: &mut [LaneW], done: WriteDone) -> bool {
    let (i, w, ok, bytes, took) = done;
    if !ok {
        return false;
    }
    if let Some(l) = lanes.get_mut(i) {
        l.w = Some(w);
        l.last_write = Instant::now();
        l.health.observe(bytes, took);
    }
    true
}

/// 这条 lane 现在能不能发。
///
/// 判据是**相对**的，不是绝对阈值：整条链路一起变慢时谁都不该被停发（停了
/// 就是白扔带宽），只有明显掉队的那条才停。没有样本的一律放行——新加的
/// lane 必须先拿到数据才会有样本，一上来就判它慢会让它永远出不来。
fn lane_is_usable(h: &LaneHealth, best_bps: f64, now: Instant) -> bool {
    let Some(bps) = h.ewma_bps else {
        return true; // 还没测过，先给机会
    };
    if best_bps <= 0.0 {
        return true; // 谁都没样本
    }
    if bps * LANE_SLOW_FACTOR >= best_bps {
        return true;
    }
    // 已判慢：但要定期放行一片刷新估计，否则永远出不来（见 LANE_REPROBE）。
    now.duration_since(h.last_sent) >= LANE_REPROBE
}

/// 当前最快 lane 的 EWMA（没有任何样本时为 0）。
fn best_lane_bps(lanes: &[LaneW]) -> f64 {
    lanes
        .iter()
        .filter_map(|l| l.health.ewma_bps)
        .fold(0.0f64, f64::max)
}

/// 发送方向单写者任务：分片、轮转、按阈值加 lane、CLOSE 收尾。
async fn send_task(
    inner: Arc<ConnInner>,
    send_dir: Dir,
    initial_bytes: Vec<u8>,
    initial_w: WriteHalf<MuxStream>,
    mut ctl: tokio::sync::mpsc::Receiver<CtlMsg>,
) {
    let mut lanes = vec![LaneW { w: Some(initial_w), last_write: Instant::now(), health: LaneHealth::new() }];
    let mut up_off: u64 = 0;
    // lane 轮转游标：跨队列项持续（见分发处注释）。
    let mut lane_rr: u64 = 0;
    // 在途写入：每条 lane 至多一笔，写完把写半还回 `lanes[i].w`。
    // **跨队列项保留**——每项都排空的话就没有流水线了（见分发处注释②）。
    // 元组：(lane 下标, 归还的写半, 是否成功, 写了多少字节, 花了多久)。
    // 后两项喂给 `LaneHealth::observe`——健康度只用「写入完成」这一个可
    // 观测量，不去猜 RTT/丢包（mux 层看不到，而完成时间已经把窗口阻塞、
    // 重传、对端消费速度全折进去了）。
    type InflightWrite = Pin<Box<dyn Future<Output = WriteDone> + Send>>;
    let mut inflight: FuturesUnordered<InflightWrite> = FuturesUnordered::new();
    let start = Instant::now();
    let mut sent: u64 = 0;
    if !initial_bytes.is_empty() {
        let ok = match lanes[0].w.as_mut() {
            Some(w) => w.write_all(&initial_bytes).await.is_ok(),
            None => false,
        };
        if !ok {
            mark_dead(&inner);
            return;
        }
    }

    'dispatch: loop {
        // 1. 排空待发队列
        loop {
            let next = {
                let mut out = inner.out.lock().unwrap();
                out.queue.pop_front().inspect(|b| {
                    out.pending = out.pending.saturating_sub(b.len());
                    if out.pending < MAX_PENDING / 2 {
                        if let Some(w) = out.writer_waker.take() {
                            w.wake();
                        }
                    }
                })
            };
            let Some(bytes) = next else {
                break;
            };
            if lanes.is_empty() {
                mark_dead(&inner);
                return;
            }
            // 升级判定必须在分发**之前**。已经写出去的字节没法回收重分，等一批
            // 发完再加 lane，只会得到几条永远分不到数据的空 lane——上游一次
            // `write_all` 就可能把整段流量变成一个队列项，那一项发完时"已发送
            // 字节"才第一次越过阈值，而此时已经无货可分了。判据因此用**含这一批
            // 在内**的累计量。
            maybe_upgrade(
                &mut lanes,
                &inner,
                send_dir,
                sent + bytes.len() as u64,
                start,
            )
            .await;
            let mut off = up_off;
            // 分发策略：**谁空闲谁接下一片**（work-stealing），而不是固定轮转。
            //
            // 一条 lane 同一时刻最多一笔在途写入：它的写半被借进 `inflight`
            // 里的 future，回来了才算空闲。于是快的 lane 先写完、先回来、分到
            // 更多片；慢的 lane 只占着自己手里那一片，不挡别人。
            //
            // 这里换掉的是两个叠在一起的设计缺陷（2026-09-11 跨境实测，会话数
            // 1→2→4 时小包多线程延迟 50.6→69.3→91.1ms，大包吞吐同向变差，
            // 六轮含反序跑一致）：
            //
            //   ① 固定轮转不看 lane 快慢。每片按顺序发给下一条，于是 N 条
            //      lane 各分到 1/N，总耗时 = max_i(S/N / r_i)，聚合吞吐是
            //      **N × min(r_i)** 而不是 Σr_i。跨境链路上各连接速率相差
            //      十倍是常态，4 次抽样取最小值远低于单次抽样的中位数，
            //      N=4 补不回来。
            //   ② 每个队列项都 `join_all` 等所有 lane 写完才取下一项。而上游
            //      泵用 64KiB 缓冲、CHUNK 也是 64KiB，于是每项通常只有一片
            //      ——实际行为退化成「写 lane 0、等；写 lane 1、等；…」，
            //      **完全没有流水线**，只是把慢尾巴抽样得更频繁。这就是
            //      「lane 越多越慢」的直接来源。
            //
            // 在途写入跨队列项保留（不再每项 drain），流水线才成立。缓冲有界：
            // 每 lane 至多一笔 ≤CHUNK 的在途写入，叠加 conn 级的 MAX_PENDING。
            for chunk in bytes.chunks(CHUNK) {
                // 在**空闲** lane 之间轮转，全忙就等最先写完的那条回来。
                //
                // 是「轮转 + 跳过忙的」而不是「挑第一条空闲的」：后者在写入能
                // 被本地缓冲瞬间吸收时会退化成贪心——lane 0 永远空闲、永远
                // 被选中，8MB 上行有 6.5MB 压在第一条上（实测分布
                // [6489275, 655496, 655496, 589948]），多 lane 白做。轮转保住
                // 均摊，跳过忙的则让慢 lane 自动少分。
                // 再叠一层健康度：**病态慢的 lane 直接跳过**（见
                // `lane_is_usable`）。空闲轮转已经让分配与速率成正比，对吞吐
                // 够用；但重组是连续推进的，发给慢 10 倍那条的一片会把它后面
                // 所有数据一起堵住 10 倍时长——按比例少发解决不了，只能不发。
                let li = loop {
                    let n = lanes.len();
                    let best = best_lane_bps(&lanes);
                    let now = Instant::now();
                    let pick = |usable_only: bool| {
                        (0..n).find(|k| {
                            let i = (lane_rr as usize).wrapping_add(*k) % n;
                            let l = &lanes[i];
                            l.w.is_some()
                                && (!usable_only || lane_is_usable(&l.health, best, now))
                        })
                    };
                    // 先在「健康且空闲」里挑；一条都没有时**退回只看空闲**——
                    // 宁可发给一条慢 lane，也不能因为集体判慢而干脆不发
                    // （那是把活着的链路判死，比慢严重得多）。
                    if let Some(k) = pick(true).or_else(|| pick(false)) {
                        let i = (lane_rr as usize).wrapping_add(k) % n;
                        lane_rr = (i as u64).wrapping_add(1);
                        break i;
                    }
                    // 全忙 = 真正的背压：此刻不该再往任何 lane 塞东西。
                    //
                    // **等待必须能被收尾请求打断。** 对端一旦停止读取，所有
                    // lane 的写都永久 Pending，这个 `inflight.next()` 就成了
                    // 终点：任务再也回不到循环顶部去看 `closing`，`mark_dead`
                    // 永不调用，conn 的资源一件都不回收，而写端还在往无界队列
                    // 里塞东西、每次都返回 `Ok`。
                    //
                    // 这是整条发送路径上**最深**的那个卡点：它在分发循环内层，
                    // 比"停车前排空"和"收尾时排空"都更早触发（只要有一批数据
                    // 正在分发就会走到）。三处形状相同，都需要出口。
                    let interrupted = tokio::select! {
                        r = inflight.next() => {
                            match r {
                                Some(done) => {
                                    if !reclaim_write(&mut lanes, done) {
                                        mark_dead(&inner);
                                        return;
                                    }
                                    false
                                }
                                // inflight 空而又没有空闲 lane ⇒ 一条 lane 都没有了
                                None => {
                                    mark_dead(&inner);
                                    return;
                                }
                            }
                        }
                        _ = wait_closing(&inner) => true,
                    };
                    if interrupted {
                        break 'dispatch;
                    }
                };
                let mut frame = Vec::new();
                encode_frame(off, chunk, &mut frame);
                off += chunk.len() as u64;
                let n_bytes = frame.len();
                lanes[li].health.last_sent = Instant::now();
                let w = lanes[li].w.take().expect("刚判过 is_some");
                inflight.push(Box::pin(async move {
                    let mut w = w;
                    let t0 = Instant::now();
                    let ok = w.write_all(&frame).await.is_ok();
                    (li, w, ok, n_bytes, t0.elapsed())
                }));
            }
            // 让出一次执行器。
            //
            // 不是可有可无的礼貌：分发循环现在不再等写入完成，队列里有货时它
            // 可以一路跑到底一次都不让出，把同进程的 lane 读任务、重组任务全
            // 饿着——吞吐没涨，延迟先炸。旧代码靠每项 `join_all().await` 顺带
            // 完成了这件事，改成流水线就得显式做。
            tokio::task::yield_now().await;
            // 顺手收掉已经完成的，让下一项能挑到更多空闲 lane。不阻塞：
            // 这里要是 await，就又变回 join_all 那种「每项等齐」的老样子。
            while let Some(Some(done)) = inflight.next().now_or_never() {
                if !reclaim_write(&mut lanes, done) {
                    mark_dead(&inner);
                    return;
                }
            }
            up_off = off;
            sent += bytes.len() as u64;
            // 批后再查一次。批前那次可能因为 `upgrade_window` 还没跨过而放弃，
            // 而窗口往往正是在发这一批的过程中跨过的；少了这次兜底，升级会被
            // 推迟到下一批到来，短流量下就等于永不升级。`maybe_upgrade` 幂等。
            maybe_upgrade(&mut lanes, &inner, send_dir, sent, start).await;
        }
        // 2. 判断是否收尾。在途写入不在这里排空，见第 3 步。
        let (closing, _reason) = {
            let out = inner.out.lock().unwrap();
            (out.closing, out.close_reason)
        };
        if closing {
            break;
        }
        // 3. 等新工作：队列来数据 / 控制消息 / **在途写入完成**。
        //
        // `inflight` 里的写入**只有本任务会 poll**，所以停车时必须继续 poll
        // 它们，否则那些写入永远不再推进 → mux 窗口不推进 → 对端收不到剩余
        // 分片 → 整条流挂死（2026-09-11 真机：8MiB 下载 5 轮全部 30s 超时，
        // 小包因为每轮都能排空队列所以完全正常，症状只打在大流上）。
        //
        // 早先的写法是在停车**之前**用 `while inflight.next().await` 排空。
        // 那修掉了挂死，但换来一个更隐蔽的问题：那个 while 没有出口。一条
        // 永久卡住的 lane（对端不读、或被 orphan 驱逐后没人消费）会把整个
        // conn 冻在那一行上——健康的 lane 全都空闲，任务却再也不接新工作、
        // 不发 CLOSE、不 `mark_dead`。而 `lane_is_usable` 的跳过逻辑对此无能
        // 为力，它只管"下一项派给谁"。
        //
        // 放进 `select!` 两个问题一起解决：在途写照样被推进，而任何一个分支
        // 就绪都能让任务继续往前走。
        // 注意：waker 注册与队列检查在同一锁临界区内完成（无丢失唤醒窗口）。
        let notified = std::future::poll_fn::<(), _>(|cx| {
            let mut out = inner.out.lock().unwrap();
            if !out.queue.is_empty() || out.closing {
                return Poll::Ready(());
            }
            if out.send_task_waker.is_none() {
                out.send_task_waker = Some(cx.waker().clone());
            }
            Poll::Pending
        });
        tokio::select! {
            _ = notified => {},
            // 在途写完成：收回 WriteHalf 并记一笔健康样本，然后回到循环顶部。
            // guard 是必需的——`FuturesUnordered::next` 在空集上立刻返回
            // `Ready(None)`，没有它这个分支会抢占 `select!` 空转。
            Some(done) = inflight.next(), if !inflight.is_empty() => {
                if !reclaim_write(&mut lanes, done) {
                    mark_dead(&inner);
                    return;
                }
            }
            msg = ctl.recv() => match msg {
                Some(CtlMsg::AddLane(w)) => {
                    lanes.push(LaneW { w: Some(w), last_write: Instant::now(), health: LaneHealth::new() });
                    inner.lane_count.store(lanes.len(), Ordering::Relaxed);
                }
                None => {
                    let mut out = inner.out.lock().unwrap();
                    out.closing = true;
                }
            },
        }
    }

    // 4. 排空在途写入，再发 CLOSE。
    //
    // 顺序不能反：CLOSE 带的是**最终偏移** `up_off`，对端收到它就知道「到这个
    // 偏移为止是全部数据」。在途的那几片偏移都小于它，CLOSE 抢先到达的话，
    // 对端会在数据还没收齐时判定流已结束——尾部静默丢几十 KiB，且不报错。
    //
    // 但排空必须**有出口**：对端不读时那些写入永远完不成，而这条收尾路径
    // 上没有任何别的分支能救它——任务会永久停在这里，`mark_dead` 不被调用，
    // 写端的 waker 不被唤醒，conn 的资源一件都不回收。宁可丢掉尾部（对端
    // 反正已经不读了）也不能把任务钉死在这儿。
    let _ = tokio::time::timeout(TEARDOWN_TIMEOUT, async {
        // 这里**有意不走 `reclaim_write`**：收尾时不该再判死（对端已经不读了，
        // 那不是链路故障），也没人会再读健康度。只把写半还回去，好让下面发
        // CLOSE 时还能挑到 lane。
        while let Some((i, w, _ok, _n, _took)) = inflight.next().await {
            if let Some(l) = lanes.get_mut(i) {
                l.w = Some(w);
                l.last_write = Instant::now();
            }
        }
    })
    .await;
    // 发 CLOSE 帧（最闲 lane）并关闭全部 lane
    if !lanes.is_empty() {
        let mut best = 0usize;
        for (i, l) in lanes.iter().enumerate() {
            if l.last_write < lanes[best].last_write {
                best = i;
            }
        }
        let mut frame = encode_header(&ConnHeader {
            conn_id: inner.conn_id,
            cmd: Cmd::Close,
            dir: send_dir,
            lane_id: 0,
        })
        .to_vec();
        frame.extend_from_slice(&encode_close_payload(up_off, inner.out.lock().unwrap().close_reason));
        if let Some(w) = lanes[best].w.as_mut() {
            let _ = w.write_all(&frame).await;
        }
    }
    // flush/shutdown 同样要有出口：对端不读时它们也会永久 Pending，而
    // `mark_dead` 在它们之后——少了这个超时，收尾会卡在倒数第二行。
    let _ = tokio::time::timeout(TEARDOWN_TIMEOUT, async {
        for l in &mut lanes {
            if let Some(w) = l.w.as_mut() {
                let _ = w.flush().await;
                let _ = w.shutdown().await;
            }
        }
    })
    .await;
    mark_dead(&inner);
}

/// 等到收尾被请求。
///
/// 与第 3 步停车用的那个 `poll_fn` 同构（也复用同一个 waker 槽），只是这里
/// 只关心 `closing`：调用点是"等一条 lane 空出来"，队列里有没有新数据无关。
///
/// 存在的理由：发送路径上有三处会 `await` 在途写入，对端一停止读取它们全都
/// 永久 Pending。少了这个出口，任务就再也回不到能看见 `closing` 的地方。
async fn wait_closing(inner: &Arc<ConnInner>) {
    std::future::poll_fn::<(), _>(|cx| {
        let mut out = inner.out.lock().unwrap();
        if out.closing || out.dead {
            return Poll::Ready(());
        }
        if out.send_task_waker.is_none() {
            out.send_task_waker = Some(cx.waker().clone());
        }
        Poll::Pending
    })
    .await
}

fn mark_dead(inner: &Arc<ConnInner>) {
    let mut out = inner.lock_out();
    out.dead = true;
    if let Some(w) = out.writer_waker.take() {
        w.wake();
    }
}

impl ConnInner {
    fn lock_out(&self) -> std::sync::MutexGuard<'_, OutState> {
        self.out.lock().unwrap()
    }
}

/// 发送量 + 速率超阈值 → 自动加 lane 到目标数。
async fn maybe_upgrade(
    lanes: &mut Vec<LaneW>,
    inner: &Arc<ConnInner>,
    send_dir: Dir,
    sent: u64,
    start: Instant,
) {
    let cfg = &inner.cfg;
    if lanes.len() >= cfg.target_lanes || sent < cfg.upgrade_bytes {
        return;
    }
    // 时间窗只用来给速率一个像样的分母，**不作为策略门**。
    //
    // 曾经是「elapsed < upgrade_window 就直接返回」，于是窗口没跨过之前一条
    // lane 都不加：5 MiB/s 的链路上等满 1 秒，就是 5 MiB 白白挤在首 lane 上。
    // 发送侧改成流水线之后更明显——8MB 上行里 6.7MB 在窗口跨过前就发完了
    // （实测分布 [6751467, 589948, 524400, 524400]）。
    //
    // 分母不够长时，「已经攒够 `upgrade_bytes`」本身就足以判定这是个大流。
    // 速率门留给**慢流**：花了超过一个窗口才攒够那些字节的，才真的不值得
    // 加 lane。lane 是 mux 流不是 TCP 连接，多开几条代价很小，判早了远比
    // 判晚了划算。
    //
    // 放开这道门的前提是接收侧**不再丢弃未知 conn 的 lane**（见
    // `ConnRegistry::park_orphan`）：升级得早时，lane 1..N 的 OPEN 会赶在
    // conn 登记之前到达，此前那条路径直接丢流，而发送侧照样往它们写分片，
    // 重组端就此静默卡死（实测 `read timeout at 65536/...`，恰好停在首
    // lane 之后）。两处改动是配套的，不能只做一半。
    let elapsed = start.elapsed();
    if elapsed >= cfg.upgrade_window {
        let rate = sent as f64 / elapsed.as_secs_f64();
        if (rate as u64) < cfg.upgrade_rate_bps {
            return;
        }
    }
    let want = cfg.target_lanes - lanes.len();
    for _ in 0..want {
        let Ok(stream) = (inner.lane_opener)(send_dir).await else { break };
        let (r, w) = tokio::io::split(stream);
        // 自己开的 lane 上对端不会有数据，但读半须有人消费（EOF 检测）
        tokio::spawn(drain_read_half(r));
        lanes.push(LaneW { w: Some(w), last_write: Instant::now(), health: LaneHealth::new() });
        inner.lane_count.store(lanes.len(), Ordering::Relaxed);
    }
}

async fn drain_read_half(mut r: ReadHalf<MuxStream>) {
    let mut buf = vec![0u8; 4096];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

// ---------------- conn 表 ----------------

/// 孤儿 lane 的暂存上限（条）与有效期。
///
/// 有界是硬要求：`conn_id` 来自对端，攒未知 conn 的流等于给对方一个用随机
/// conn_id 灌内存的口子。超过上限就丢最旧的，超过有效期的同样丢——正常的
/// 乱序只差一个 RTT 级别的量，秒级窗口绰绰有余。
const ORPHAN_CAP: usize = 64;
const ORPHAN_TTL: Duration = Duration::from_secs(5);

/// conn_id → conn 表（双端共用；接受侧据此归并 lane）。
#[derive(Default)]
pub struct ConnRegistry {
    map: Mutex<std::collections::HashMap<u64, Arc<StripeConn>>>,
    /// 先于自己的 conn 到达的 lane（见 [`ConnRegistry::park_orphan`]）。
    orphans: Mutex<VecDeque<(Instant, u64, MuxStream)>>,
}

impl ConnRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn insert(&self, id: u64, conn: Arc<StripeConn>) {
        // **锁顺序固定为 orphans → map**，与 `park_orphan` 一致。
        //
        // 两者必须在同一个临界区里完成"看一眼对面、再动自己这边"，否则有个
        // 会静默丢流的窗口：`park_orphan` 查 map 查不到 → 这里 insert 进 map
        // 并 `take_orphans`（队列里还没有那条）→ `park_orphan` 才把它塞进队列。
        // 那条 lane 就此烂在队列里直到被驱逐，而发送侧照样往它写分片，重组端
        // 永远等不齐（实测症状 `read timeout at 65536/...`）。
        //
        // 顺序统一是为了避免 ABBA 死锁：两个函数都先 orphans 后 map。
        let mut q = self.orphans.lock().unwrap();
        self.map.lock().unwrap().insert(id, conn.clone());
        let now = Instant::now();
        q.retain(|(t, _, _)| now.duration_since(*t) < ORPHAN_TTL);
        let mut pending = Vec::new();
        let mut keep = VecDeque::with_capacity(q.len());
        while let Some(item) = q.pop_front() {
            if item.1 == id {
                pending.push(item.2);
            } else {
                keep.push_back(item);
            }
        }
        *q = keep;
        drop(q);
        // conn 刚登记，把此前先到的 lane 接回来。
        for stream in pending {
            conn.accept_lane(stream);
        }
    }

    /// 暂存一条「conn 还不存在」的 lane。
    ///
    /// 这不是理论竞态：升级出来的 lane 1..N 与首 lane 之间**没有顺序保证**
    /// ——多会话下它们走的是不同的 TCP，单会话下 conn 的登记也发生在被
    /// spawn 的处理任务里。此前这些流一律直接丢弃，而发送侧并不知情，照样
    /// 往它们写分片：那些分片成了重组端永远填不上的洞，整条流静默卡死
    /// （实测症状是 `read timeout at 65536/...`，恰好停在首 lane 之后）。
    ///
    /// 暂存而非丢弃之后，晚到的 conn 登记会把它们接回去。
    pub fn park_orphan(&self, id: u64, stream: MuxStream) {
        // 锁顺序 orphans → map，与 `insert` 一致（避免 ABBA 死锁）。
        let mut q = self.orphans.lock().unwrap();
        // **在同一个临界区里再查一次 map**：调用方的 `get` 与这里之间存在
        // 窗口，conn 可能刚刚被登记。查不到才 park，查到就直接交付——否则
        // 这条 lane 会烂在队列里，而发送侧照样往它写（见 `insert` 的注释）。
        if let Some(conn) = self.map.lock().unwrap().get(&id).cloned() {
            drop(q);
            conn.accept_lane(stream);
            return;
        }
        let now = Instant::now();
        let mut evicted: Vec<MuxStream> = Vec::new();
        q.retain(|(t, _, _)| now.duration_since(*t) < ORPHAN_TTL);
        while q.len() >= ORPHAN_CAP {
            if let Some((_, _, s)) = q.pop_front() {
                evicted.push(s);
            }
        }
        q.push_back((now, id, stream));
        drop(q);
        // **被驱逐的流要显式关掉，不能只是 drop。**
        //
        // wsmux 的流是信用制的：单纯 drop 只摘掉本端的 sid，对端的 `write_all`
        // 照样返回 `Ok`，而 dispatch 会把那些分片丢给一个不存在的 sid——重组端
        // 出现一个永远填不上的洞，两侧都不报错。显式 shutdown 让对端拿到
        // EOF，它的写入随之失败，上层才有机会重建。
        for s in evicted {
            tokio::spawn(async move {
                let mut s = s;
                let _ = s.shutdown().await;
            });
        }
    }

    /// 取走该 conn 的全部暂存 lane（顺带清掉过期项）。
    ///
    /// 仅留给外部调用方；`insert` 自己在锁内完成同样的事，因为它必须与
    /// `park_orphan` 互斥（见 `insert` 的注释）。
    #[allow(dead_code)]
    fn take_orphans(&self, id: u64) -> Vec<MuxStream> {
        let mut q = self.orphans.lock().unwrap();
        let now = Instant::now();
        q.retain(|(t, _, _)| now.duration_since(*t) < ORPHAN_TTL);
        let mut out = Vec::new();
        let mut keep = VecDeque::with_capacity(q.len());
        while let Some(item) = q.pop_front() {
            if item.1 == id {
                out.push(item.2);
            } else {
                keep.push_back(item);
            }
        }
        *q = keep;
        out
    }
    pub fn remove(&self, id: u64) {
        self.map.lock().unwrap().remove(&id);
    }
    pub fn get(&self, id: u64) -> Option<Arc<StripeConn>> {
        self.map.lock().unwrap().get(&id).cloned()
    }
    /// 当前表项数（诊断/测试）。
    pub fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 注册表清理任务：conn 接收方向终结后从表中摘除。双端共用（客户端
/// `connect`、服务端 `route_inbound_with_cfg` 的新 conn 路径）——没有它
/// 表项永久泄漏。
///
/// 竞态处理：`Notify::notify_waiters` 只唤醒「已注册」的等待者，而本任务
/// 是在 conn 建立之后才 spawn 的，终结完全可能先于注册发生（短连接常见）。
/// 因此顺序必须是「先建 Notified（注册），再复查持久标记」：
///   - 终结先发生 → 复查命中 → 立即清理；
///   - 终结后发生 → Notified 已注册 → 被唤醒 → 清理。
///
/// 两种顺序都无窗口。
fn spawn_registry_cleanup(registry: Arc<ConnRegistry>, conn: &Arc<StripeConn>) {
    let conn_id = conn.conn_id();
    let term = conn.inner_recv_terminal();
    let conn = conn.clone();
    tokio::spawn(async move {
        let notified = term.notified();
        tokio::pin!(notified);
        // 注册（Notified 首次 poll 才入队），随后复查已终结标记。
        notified.as_mut().enable();
        if !conn.recv_terminated() {
            notified.await;
        }
        registry.remove(conn_id);
    });
}

// ---------------- 服务端：会话组 ----------------

/// 一个客户端的全部 XHTTP 会话（同一 msg1.group_id）。服务端据此把下行
/// lane 铺到组内任意会话上——每个会话是独立 TCP，即独立拥塞窗口
/// （aria2 效应）。组只在服务端存在，客户端侧由 `StripeDialer::sessions`
/// 承担同样职责。
///
/// 成员用 `Weak<dyn Mux>`：会话拆除时其 `Arc<dyn Mux>` 由 session_loop 释放，
/// 组表内的 Weak 自然失效，不会把已死会话的 mux（及其驱动任务、缓冲区）
/// 钉在内存里。取用时 upgrade 失败即摘除，无需依赖 deregister 的及时性
/// （deregister 仍然做，只是不再是唯一回收路径）。
#[derive(Default)]
pub struct SessionGroup {
    members: Mutex<Vec<std::sync::Weak<dyn Mux>>>,
    rr: AtomicU64,
}

impl SessionGroup {
    pub fn new() -> Self {
        Self::default()
    }

    /// 加入一个会话 mux（幂等）。
    pub fn insert(&self, mux: &Arc<dyn Mux>) {
        let mut m = self.members.lock().unwrap();
        m.retain(|w| w.strong_count() > 0);
        if m.iter().any(|w| w.upgrade().is_some_and(|a| Arc::ptr_eq(&a, mux))) {
            return;
        }
        m.push(Arc::downgrade(mux));
    }

    /// 摘除一个会话 mux（会话拆除时调用）。
    pub fn remove(&self, mux: &Arc<dyn Mux>) {
        let mut m = self.members.lock().unwrap();
        m.retain(|w| match w.upgrade() {
            Some(a) => !Arc::ptr_eq(&a, mux),
            None => false, // 顺带清理已失效的 Weak
        });
    }

    /// 当前存活成员数。
    pub fn len(&self) -> usize {
        let mut m = self.members.lock().unwrap();
        m.retain(|w| w.strong_count() > 0);
        m.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 存活成员快照（按轮转起点旋转，供逐个尝试）。
    fn live_rotated(&self) -> Vec<Arc<dyn Mux>> {
        let mut m = self.members.lock().unwrap();
        m.retain(|w| w.strong_count() > 0);
        let live: Vec<Arc<dyn Mux>> = m.iter().filter_map(|w| w.upgrade()).collect();
        drop(m);
        if live.is_empty() {
            return live;
        }
        let start = (self.rr.fetch_add(1, Ordering::Relaxed) as usize) % live.len();
        let mut out = Vec::with_capacity(live.len());
        out.extend_from_slice(&live[start..]);
        out.extend_from_slice(&live[..start]);
        out
    }
}

/// group_id → 会话组表（AppState 级）。
#[derive(Default)]
pub struct SessionGroups {
    map: Mutex<std::collections::HashMap<u128, Arc<SessionGroup>>>,
}

impl SessionGroups {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取（或建）一个组，并把 mux 登记进去。返回组句柄，会话的 accept
    /// 循环把它传给 `StripeListener`，新 conn 的下行 lane 即可跨组开。
    pub fn join(&self, group_id: u128, mux: &Arc<dyn Mux>) -> Arc<SessionGroup> {
        let g = {
            let mut map = self.map.lock().unwrap();
            map.entry(group_id)
                .or_insert_with(|| Arc::new(SessionGroup::new()))
                .clone()
        };
        g.insert(mux);
        g
    }

    /// 会话拆除：从组内摘除该 mux；组空则删除组条目（否则 group_id 表
    /// 随客户端重连无限增长）。
    pub fn leave(&self, group_id: u128, mux: &Arc<dyn Mux>) {
        let g = self.map.lock().unwrap().get(&group_id).cloned();
        let Some(g) = g else { return };
        g.remove(mux);
        if g.is_empty() {
            let mut map = self.map.lock().unwrap();
            // 复查：leave 与 join 并发时可能刚有新成员进来
            if map
                .get(&group_id)
                .is_some_and(|e| Arc::ptr_eq(e, &g) && e.is_empty())
            {
                map.remove(&group_id);
            }
        }
    }

    /// 当前组数（诊断/测试）。
    pub fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 组感知的 lane opener（服务端下行）：在组内存活会话上轮转开 lane，
/// 逐个尝试直到成功。组为空 / 全部失败 → 回落到 conn 到达的那个会话
/// （`fallback`），它至少是这条 conn 已经在用的那条 TCP。
fn group_lane_opener(
    conn_id: u64,
    group: Arc<SessionGroup>,
    fallback: Arc<dyn Mux>,
) -> LaneOpener {
    Arc::new(move |dir| {
        let group = group.clone();
        let fallback = fallback.clone();
        Box::pin(async move {
            for mux in group.live_rotated() {
                match open_lane_on(&mux, conn_id, dir).await {
                    Ok(s) => return Ok(s),
                    // 开失败 = 该会话已死：摘除后继续试下一个
                    Err(_) => group.remove(&mux),
                }
            }
            open_lane_on(&fallback, conn_id, dir).await
        })
    })
}

// ---------------- 客户端：StripeDialer ----------------

/// 客户端拨号器：分配 conn_id，建首 lane（OPEN + TargetAddr），后台归并
/// 服务端新开的 DOWN lane。多会话：`attach_session` 挂额外会话 mux，
/// lane 打开跨会话轮转（每 lane 独立 TCP 拥塞窗口，aria2 效应）。
pub struct StripeDialer {
    mux: Arc<dyn Mux>,
    sessions: RwLock<Vec<Arc<dyn Mux>>>,
    rr: AtomicU64,
    cfg: StripeCfg,
    next_conn_id: Arc<AtomicU64>,
    registry: Arc<ConnRegistry>,
    /// 还活着的会话数。**必须与 `sessions` 的长度分开记**：`sessions` 刻意
    /// 保留最后一个死条目（好让 `connect` 干净失败而不是对空 vec 取模 panic），
    /// 所以它的长度永远 ≥ 1，读不出「全死了」。
    live_sessions: AtomicUsize,
    /// 最后一条会话死亡时触发一次。
    all_dead: Notify,
}

impl StripeDialer {
    /// 等到本拨号器的**全部** mux 会话都死掉。
    ///
    /// 这是 `prune_session` 注释里那句「由外层代理重连循环负责重建会话」
    /// 缺失的另一半：会话死亡此前只被记在 `sessions` 表里，没有任何出口
    /// 通知外层，于是 mux 内部死亡（如 smux 的 keep-alive 超时只退出
    /// `recv_loop`、`send_loop` 仍持有写半边）时，代理层的 `DeathWatch`
    /// 既不会被 poll 到错误、也不会被 drop——出站永久停在「已连接」，
    /// 全部新连接瞬间失败，且**一条日志都没有**。
    pub async fn all_sessions_dead(&self) {
        loop {
            // 先登记再检查：反过来的话，两步之间到来的通知会被永久错过。
            let notified = self.all_dead.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.live_sessions.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }

    /// 一条会话的 accept 循环退出即为该会话死亡。只减一次由调用方保证
    /// （每个 accept 循环只会走到 `Err` 分支一次，随后 `return`）。
    fn mark_session_dead(&self) {
        if self.live_sessions.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.all_dead.notify_waiters();
        }
    }

    pub fn dbg_conn(&self, id: u64) -> Option<Arc<StripeConn>> {
        self.registry.get(id)
    }

    /// 本拨号器的 conn 表。额外会话的入站 lane 若不经 `attach_session`
    /// 而由调用方自建 accept 泵（诊断/测试），需要用它调 `join_inbound`。
    pub fn registry(&self) -> Arc<ConnRegistry> {
        self.registry.clone()
    }

    /// 当前挂载的会话 mux 数（含主会话）。
    pub fn session_count(&self) -> usize {
        self.sessions.read().unwrap().len()
    }

    /// 主会话 mux（诊断/测试用）。
    pub fn primary_mux(&self) -> Arc<dyn Mux> {
        self.mux.clone()
    }

    fn pick(&self) -> Arc<dyn Mux> {
        let sessions = self.sessions.read().unwrap();
        let n = self.rr.fetch_add(1, Ordering::Relaxed);
        sessions[(n as usize) % sessions.len()].clone()
    }

    /// 从会话表摘除一个已死会话（其 accept 循环退出即为死亡证据）。
    /// 不变量：`sessions` 永不为空——最后一个条目即使已死也保留，让
    /// `connect` 干净地返回错误（而非对空 vec 取模 panic），由外层
    /// 代理重连循环负责重建会话。
    fn prune_session(&self, mux: &Arc<dyn Mux>) {
        let mut s = self.sessions.write().unwrap();
        if s.len() <= 1 {
            return;
        }
        if let Some(i) = s.iter().position(|m| Arc::ptr_eq(m, mux)) {
            s.remove(i);
        }
    }

    /// lane opener：跨会话轮转开 lane（写好 OPEN 头）。单次 pick 落到死会话
    /// 时在剩余会话上重试（open 失败即死亡证据）。拨号器已销毁时退化到主
    /// 会话 mux。
    fn rr_lane_opener(self: &Arc<Self>, conn_id: u64) -> LaneOpener {
        let dialer = Arc::downgrade(self);
        let primary = self.mux.clone();
        Arc::new(move |dir| {
            let dialer = dialer.clone();
            let primary = primary.clone();
            Box::pin(async move {
                let Some(d) = dialer.upgrade() else {
                    // 拨号器已销毁：主会话是唯一还能取到的句柄
                    return open_lane_on(&primary, conn_id, dir).await;
                };
                let attempts = d.session_count();
                let mut last: Option<anyhow::Error> = None;
                for _ in 0..attempts {
                    let mux = d.pick();
                    match open_lane_on(&mux, conn_id, dir).await {
                        Ok(s) => return Ok(s),
                        Err(e) => {
                            d.prune_session(&mux);
                            last = Some(e);
                        }
                    }
                }
                Err(last.unwrap_or_else(|| anyhow::anyhow!("no live session to open lane on")))
            })
        })
    }

    /// 挂载一个额外会话（其 mux 的后台 accept 循环由本方法启动）。
    /// 返回 false 表示该会话已挂载过（幂等拒绝）。
    pub fn attach_session(self: &Arc<Self>, mux: Arc<dyn Mux>) -> bool {
        {
            let mut s = self.sessions.write().unwrap();
            if s.iter().any(|m| Arc::ptr_eq(m, &mux)) {
                return false;
            }
            s.push(mux.clone());
        }
        // 挂载成功才计数，与下面 accept 循环退出时的减一严格配对。
        self.live_sessions.fetch_add(1, Ordering::AcqRel);
        let d = self.clone();
        tokio::spawn(async move {
            loop {
                let stream = match mux.accept().await {
                    Ok(s) => s,
                    // accept 循环退出 = 会话已死：先摘表，避免后续 pick
                    // 继续把 lane 往死会话上开。
                    Err(_) => {
                        d.prune_session(&mux);
                        d.mark_session_dead();
                        return;
                    }
                };
                let dd = d.clone();
                tokio::spawn(async move {
                    let _ = join_inbound(&dd.registry, stream).await;
                });
            }
        });
        true
    }

    pub fn new(mux: Arc<dyn Mux>, cfg: StripeCfg) -> Arc<Self> {
        let dialer = Arc::new(Self {
            mux: mux.clone(),
            sessions: RwLock::new(vec![mux.clone()]),
            rr: AtomicU64::new(0),
            cfg,
            // conn_id 随机 epoch + 单调递增：服务端 registry 跨会话/跨客户端
            // 共享（多 TCP 条带），不同 dialer 必须几乎不可能撞 conn_id。
            // 高 32 位随机、低 32 位计数；撞上的后果是该流被对端按未知 conn
            // 丢弃，概率 2^-32 级（可忽略）。
            next_conn_id: Arc::new(AtomicU64::new(
                (rand::random::<u32>() as u64) << 32 | 1,
            )),
            registry: Arc::new(ConnRegistry::new()),
            // 主会话即第一条存活会话。
            live_sessions: AtomicUsize::new(1),
            all_dead: Notify::new(),
        });
        // 后台 accept：服务端发起的 DOWN lane 归并；未知 conn / 非 OPEN → 丢流
        let d = dialer.clone();
        tokio::spawn(async move {
            loop {
                let stream = match d.mux.accept().await {
                    Ok(s) => s,
                    // 主会话也在 sessions 表里：死了同样要摘（prune 保证表
                    // 不会变空，最后一个死条目让 connect 干净失败）。
                    Err(_) => {
                        let primary = d.mux.clone();
                        d.prune_session(&primary);
                        d.mark_session_dead();
                        return;
                    }
                };
                let dd = d.clone();
                tokio::spawn(async move {
                    if let Err(_e) = join_inbound(&dd.registry, stream).await {
                        // 坏头/未知 conn：流已在函数内丢弃
                    }
                });
            }
        });
        dialer
    }

    /// 开一条新逻辑连接（首 lane = BIDI OPEN + TargetAddr）。
    /// 轮转落到死会话时在剩余会话上重试（最多 `session_count()` 次）。
    pub async fn connect(
        self: &Arc<Self>,
        target: &AddrPort,
    ) -> io::Result<StripeStreamHandle> {
        let conn_id = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
        let attempts = self.session_count().max(1);
        let mut last: Option<io::Error> = None;
        let mut opened = None;
        for _ in 0..attempts {
            let mux = self.pick();
            match mux.open().await {
                Ok(s) => {
                    opened = Some(s);
                    break;
                }
                Err(e) => {
                    // open 失败 = 该会话已死：摘表后换一条再试。
                    self.prune_session(&mux);
                    last = Some(io::Error::other(e.to_string()));
                }
            }
        }
        let stream = match opened {
            Some(s) => s,
            None => {
                return Err(last.unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::NotConnected, "no live session")
                }))
            }
        };
        let mut prefix = encode_header(&ConnHeader {
            conn_id,
            cmd: Cmd::Open,
            dir: Dir::Bidi,
            lane_id: 0,
        })
        .to_vec();
        prefix.extend_from_slice(&wsieve_proto::addr::encode_addr(target));
        let conn = StripeConn::new(
            conn_id,
            self.cfg.clone(),
            self.rr_lane_opener(conn_id),
            Dir::Up,
            stream,
            prefix,
            Vec::new(),
        );
        self.registry.insert(conn_id, conn.clone());
        // conn 接收方向终结（EOF/错误）后清表
        spawn_registry_cleanup(self.registry.clone(), &conn);
        Ok(conn.stream())
    }
}

// ---------------- 服务端：StripeListener ----------------

/// 服务端监听器：accept mux 流 → 按 conn_id 路由 / 建新 conn（OPEN 带 addr）。
/// `registry` 由外部注入——多会话条带要求跨会话共享同一张表（conn 的 lane
/// 可能来自任意会话）。`group` 为本会话所属的会话组（同一客户端的全部
/// XHTTP 会话）：新 conn 的下行 lane 在组内轮转开，从而跨多条 TCP。
pub struct StripeListener {
    mux: Arc<dyn Mux>,
    cfg: StripeCfg,
    registry: Arc<ConnRegistry>,
    group: Option<Arc<SessionGroup>>,
}

/// 路由一条入站 mux 流的结果。
pub enum InboundLane {
    /// 加入已有 conn。
    Join(Arc<StripeConn>),
    /// 新 conn：首 lane 携带 TargetAddr。
    New(Arc<StripeConn>, AddrPort),
    /// 未知 conn / 非 OPEN 首帧 → 已丢弃。
    Dropped,
}

/// 读首帧 ConnHeader（阻塞至 16 字节或 EOF/错误）。EOF（流上无任何字节）
/// → Ok(None)（对端开流即关，静默丢弃）。
async fn read_header(stream: &mut MuxStream) -> io::Result<Option<ConnHeader>> {

    let mut b = [0u8; HEADER_LEN];
    let mut got = 0usize;
    while got < HEADER_LEN {
        match stream.read(&mut b[got..]).await {
            Ok(0) => {
                return if got == 0 {
                    Ok(None)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated conn header",
                    ))
                };
            }
            Ok(n) => got += n,
            Err(e) => return Err(e),
        }
    }
    decode_header(&b)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// 读 OPEN 首 lane 的 TargetAddr（header 之后的 addr 帧）。
/// 读 OPEN 首 lane 的 TargetAddr。返回地址与同批读到的剩余字节（可能是
/// 紧随地址的早期 DataFrame，必须转交给 conn，不得丢弃）。
async fn read_open_addr(stream: &mut MuxStream) -> io::Result<(AddrPort, Vec<u8>)> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    let mut chunk = [0u8; 256];
    loop {
        if let Ok((addr, consumed)) = decode_addr(&buf) {
            let rest = buf.split_off(consumed);
            return Ok((addr, rest));
        }
        if buf.len() > 512 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "address frame too large",
            ));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "eof before address",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// 客户端侧入站处理：只归并已知 conn 的 OPEN lane；其余一律丢弃。
/// （conn 均由客户端发起，服务端不可能合法开新 conn。）
pub async fn join_inbound(
    registry: &Arc<ConnRegistry>,
    mut stream: MuxStream,
) -> io::Result<()> {
    let Some(header) = read_header(&mut stream).await? else {
        return Ok(()); // 空流：静默丢
    };
    if header.cmd == Cmd::Open {
        if let Some(conn) = registry.get(header.conn_id) {
            conn.accept_lane(stream);
            return Ok(());
        }
        // conn 还没登记：**暂存**而不是丢弃。升级出来的 lane 与首 lane 之间
        // 没有顺序保证（多会话下走不同 TCP），丢掉它们会让发送侧继续往一条
        // 不存在的 lane 写分片，重组端静默卡死。见 `park_orphan`。
        registry.park_orphan(header.conn_id, stream);
        return Ok(());
    }
    // 非 OPEN 首帧：丢流
    let _ = stream.shutdown().await;
    Ok(())
}

impl StripeListener {
    pub fn new(mux: Arc<dyn Mux>, cfg: StripeCfg) -> Arc<Self> {
        Arc::new(Self {
            mux,
            cfg,
            registry: Arc::new(ConnRegistry::new()),
            group: None,
        })
    }

    /// 共享 registry 版（多会话条带）：多个会话的监听器归并到同一张表。
    /// 无会话组 → 下行 lane 只能开在本会话上（单会话行为）。
    pub fn with_registry(mux: Arc<dyn Mux>, cfg: StripeCfg, registry: Arc<ConnRegistry>) -> Arc<Self> {
        Arc::new(Self { mux, cfg, registry, group: None })
    }

    /// 共享 registry + 会话组版：新 conn 的下行 lane 在组内会话上轮转开，
    /// 从而跨多条 TCP（多拥塞窗口）。组只有一个成员时等价于单会话。
    pub fn with_group(
        mux: Arc<dyn Mux>,
        cfg: StripeCfg,
        registry: Arc<ConnRegistry>,
        group: Arc<SessionGroup>,
    ) -> Arc<Self> {
        Arc::new(Self { mux, cfg, registry, group: Some(group) })
    }

    /// accept 循环（每会话一个）。`on_new(conn, addr)`：新 conn 建立时拨目标。
    pub async fn run<F>(self: Arc<Self>, on_new: F)
    where
        F: Fn(Arc<StripeConn>, AddrPort) + Send + Sync + 'static,
    {
        let on_new = Arc::new(on_new);
        loop {
            let stream = match self.mux.accept().await {
                Ok(s) => s,
                Err(_) => return, // 会话终结
            };
            let registry = self.registry.clone();
            let mux = self.mux.clone();
            let cfg = self.cfg.clone();
            let group = self.group.clone();
            let on_new = on_new.clone();
            tokio::spawn(async move {
                let _ = route_inbound_full(&registry, mux, group, cfg, stream, |conn, addr| {
                    on_new(conn, addr)
                })
                .await;
            });
        }
    }
}

/// route_inbound 的 cfg 可注入版本（无会话组：下行 lane 固定在本会话）。
pub async fn route_inbound_with_cfg<F>(
    registry: &Arc<ConnRegistry>,
    mux: Arc<dyn Mux>,
    cfg: StripeCfg,
    stream: MuxStream,
    on_new: F,
) -> io::Result<InboundLane>
where
    F: FnOnce(Arc<StripeConn>, AddrPort),
{
    route_inbound_full(registry, mux, None, cfg, stream, on_new).await
}

/// 完整版路由：`group` 为 Some 时新 conn 的下行 lane 在组内会话上轮转开
/// （多 TCP 条带）；None 时退化为固定在本会话（单会话行为）。
pub async fn route_inbound_full<F>(
    registry: &Arc<ConnRegistry>,
    mux: Arc<dyn Mux>,
    group: Option<Arc<SessionGroup>>,
    cfg: StripeCfg,
    mut stream: MuxStream,
    on_new: F,
) -> io::Result<InboundLane>
where
    F: FnOnce(Arc<StripeConn>, AddrPort),
{
    let Some(header) = read_header(&mut stream).await? else {
        return Ok(InboundLane::Dropped);
    };
    if let Some(conn) = registry.get(header.conn_id) {
        if header.cmd == Cmd::Open {
            conn.accept_lane(stream);
            return Ok(InboundLane::Join(conn));
        }
        return Ok(InboundLane::Dropped);
    }
    if header.cmd != Cmd::Open {
        return Ok(InboundLane::Dropped);
    }
    // 追加 lane 抢在它的 conn 登记之前到达：**暂存**，等 conn 建好再接回去。
    //
    // 不能接着往下走——下面那行 `read_open_addr` 假定 OPEN 后面跟着
    // TargetAddr，而追加 lane 没有，会把分片数据当地址解析出来，凭空造一个
    // 指向乱七八糟目标的 conn。这个竞态在多会话下尤其真实：追加 lane 走的是
    // 另一条 TCP，与首 lane 之间毫无顺序保证；即便单会话，conn 的登记也发生
    // 在被 spawn 的处理任务里，与 accept 顺序无关。
    //
    // 判据是 `!= 0` 而不是 `== LANE_ID_EXTRA`：`lane_id` 的**数值不参与路由**
    // （见那个常量的注释），它只区分"首"与"追加"。按具体数值判会留一个错位
    // 的隐患——任何填了别的非零值的追加 lane（换一个常量、对端版本不同、
    // 或将来真的用 lane_id 编号）都会被当成首 lane 送去 `read_open_addr`，
    // 把分片数据当 TargetAddr 解析，凭空造一个指向乱七八糟目标的 conn。
    if header.lane_id != 0 {
        registry.park_orphan(header.conn_id, stream);
        return Ok(InboundLane::Dropped);
    }
    let (addr, early_data) = read_open_addr(&mut stream).await?;
    // 关键：下行 lane 不再钉死在 conn 到达的那条会话上。钉死意味着下载
    // 全程只吃一个 TCP 拥塞窗口，上行条带、下行不条带——而基准测的是下载。
    let opener = match group {
        Some(g) => group_lane_opener(header.conn_id, g, mux),
        None => single_mux_opener(header.conn_id, mux),
    };
    let conn = StripeConn::new(header.conn_id, cfg, opener, Dir::Down, stream, Vec::new(), early_data);
    registry.insert(header.conn_id, conn.clone());
    // conn 接收方向终结后清表。registry 现在是 AppState 级全局表（跨会话
    // 共享），不再随会话拆除而整体回收——没有这一步每条 conn 都永久泄漏。
    spawn_registry_cleanup(registry.clone(), &conn);
    on_new(conn.clone(), addr.clone());
    Ok(InboundLane::New(conn, addr))
}

#[cfg(test)]
mod health_tests {
    use super::*;

    fn h(bps: Option<f64>, sent_ago: Duration) -> LaneHealth {
        LaneHealth {
            ewma_bps: bps,
            last_sent: Instant::now() - sent_ago,
        }
    }

    /// 没有样本的 lane 必须放行。
    ///
    /// 新加进来的 lane 一个样本都没有——一上来就判它慢，它就永远拿不到数据、
    /// 永远产生不了样本，升级出来的 lane 全是摆设。
    #[test]
    fn a_lane_without_samples_is_always_usable() {
        let now = Instant::now();
        assert!(lane_is_usable(&h(None, Duration::ZERO), 10_000_000.0, now));
    }

    /// 判据是**相对**的：整条链路一起变慢时谁都不该被停发。
    ///
    /// 换成绝对阈值的话，链路整体劣化时全部 lane 会被同时判死，表现为
    /// 「网络一慢就彻底不通」——比慢严重得多。
    #[test]
    fn a_uniformly_slow_link_keeps_every_lane() {
        let now = Instant::now();
        // 三条都只有 10 KB/s，但彼此相当 ⇒ 全部可用
        for _ in 0..3 {
            assert!(lane_is_usable(&h(Some(10_000.0), Duration::ZERO), 10_000.0, now));
        }
    }

    /// 明显掉队的那条才停发。
    #[test]
    fn a_pathologically_slow_lane_is_skipped() {
        let now = Instant::now();
        // 比最快的慢 100 倍，且刚发过（不在再探窗口里）
        assert!(!lane_is_usable(&h(Some(100_000.0), Duration::ZERO), 10_000_000.0, now));
    }

    /// 差距没到阈值的不停发——正常抖动停发只会白扔带宽。
    #[test]
    fn a_moderately_slower_lane_is_still_used() {
        let now = Instant::now();
        // 慢 4 倍 < LANE_SLOW_FACTOR(8) ⇒ 仍然用
        assert!(lane_is_usable(&h(Some(2_500_000.0), Duration::ZERO), 10_000_000.0, now));
    }

    /// **被停发的 lane 必须能回来。**
    ///
    /// 停发之后它拿不到数据 ⇒ EWMA 永远停在那个旧值 ⇒ 永远出不来。链路恢复
    /// 了也白搭，而跨境链路的抖动恰恰是分钟级的。所以要定期强制放行一片。
    #[test]
    fn a_skipped_lane_is_reprobed_so_it_can_recover() {
        let now = Instant::now();
        let stale = h(Some(100_000.0), LANE_REPROBE + Duration::from_millis(1));
        assert!(
            lane_is_usable(&stale, 10_000_000.0, now),
            "过了再探间隔必须放行一片刷新估计，否则判慢即终身"
        );
    }

    /// 谁都没样本时不得据此判死。
    #[test]
    fn no_samples_anywhere_means_everyone_is_usable() {
        let now = Instant::now();
        assert!(lane_is_usable(&h(Some(1.0), Duration::ZERO), 0.0, now));
    }

    /// 过短的样本不入账。
    ///
    /// 除以一个接近 0 的耗时会得到天文数字，一次就能把「最快」拉到没法企及，
    /// 之后所有 lane 都显得慢 —— 全员停发。内存 duplex 上写入常是几微秒，
    /// 这条不是理论担忧。
    #[test]
    fn an_absurdly_short_sample_is_ignored() {
        let mut hh = LaneHealth::new();
        hh.observe(65536, Duration::from_nanos(10));
        assert!(hh.ewma_bps.is_none(), "过短的样本必须丢弃，否则会把基准拉到天上");
        hh.observe(65536, Duration::from_millis(10));
        assert!(hh.ewma_bps.is_some(), "正常样本要入账");
    }

    /// EWMA 要真的跟得上变化，又不被单次抖动带飞。
    #[test]
    fn the_ewma_tracks_change_without_overreacting() {
        let mut hh = LaneHealth::new();
        // 稳定在 ~6.5 MB/s
        for _ in 0..5 {
            hh.observe(65536, Duration::from_millis(10));
        }
        let steady = hh.ewma_bps.unwrap();
        // 单次掉到 1/10
        hh.observe(65536, Duration::from_millis(100));
        let after = hh.ewma_bps.unwrap();
        assert!(after < steady, "变慢要反映出来");
        assert!(
            after > steady * 0.5,
            "单次抖动不该把估计砍掉一半以上（α=0.3）：steady={steady}, after={after}"
        );
    }
}
