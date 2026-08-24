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
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::mpsc::Sender;

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

/// 在指定 mux 上开一条 lane 并写好 OPEN 头。
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
        lane_id: 0,
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
        loop {
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
            return Poll::Pending;
        }
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
    let mut chunk = vec![0u8; CHUNK + 64];
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
                            drop(st);
                            inner.mark_recv_terminal();
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
                    let payload = Bytes::copy_from_slice(payload);
                    let _ = b.split_to(used);
                    feed(&inner, off, payload);
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
        // 补充字节
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => {
                lane_eof(&inner);
                return;
            }
            Ok(n) => b.extend_from_slice(&chunk[..n]),
        }
    }
}

/// 一条入站 lane 结束；最后一条 lane 结束时终结整条接收方向状态。
fn lane_eof(inner: &Arc<ConnInner>) {
    let mut st = inner.recv.lock().unwrap();
    st.live_lanes = st.live_lanes.saturating_sub(1);
    if st.live_lanes == 0 {
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
            // 对端没发 CLOSE 就全断了：已交付的内容按 EOF 处理（尽力而为）
            st.closed = Some((st.contig, CloseReason::TargetEof));
        }
    }
    wake_reader(&mut st);
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
        loop {
            let Some((&k, _)) = st.holes.first_key_value() else { break };
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
    wake_reader(&mut st);
}

// ---------------- 发送任务 ----------------

struct LaneW {
    w: Option<WriteHalf<MuxStream>>,
    last_write: Instant,
}

/// 发送方向单写者任务：分片、轮转、按阈值加 lane、CLOSE 收尾。
async fn send_task(
    inner: Arc<ConnInner>,
    send_dir: Dir,
    initial_bytes: Vec<u8>,
    initial_w: WriteHalf<MuxStream>,
    mut ctl: tokio::sync::mpsc::Receiver<CtlMsg>,
) {
    let mut lanes = vec![LaneW { w: Some(initial_w), last_write: Instant::now() }];
    let mut up_off: u64 = 0;
    // lane 轮转游标：跨队列项持续（见分发处注释）。
    let mut lane_rr: u64 = 0;
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

    loop {
        // 1. 排空待发队列
        loop {
            let next = {
                let mut out = inner.out.lock().unwrap();
                out.queue.pop_front().map(|b| {
                    out.pending = out.pending.saturating_sub(b.len());
                    if out.pending < MAX_PENDING / 2 {
                        if let Some(w) = out.writer_waker.take() {
                            w.wake();
                        }
                    }
                    b
                })
            };
            let Some(bytes) = next else {
                break;
            };
            if lanes.is_empty() {
                mark_dead(&inner);
                return;
            }
            let mut off = up_off;
            // 并行分发：每条 lane 攒好自己的帧批次，然后各 lane 的写入并发执行
            // （join_all + 每批次 move 进独立 future）。串行 await 会让窗口满的
            // lane 阻塞其他 lane（队头阻塞），多车道退化成单车道——这正是
            // 分片要解决的问题。每 lane 内部帧顺序天然保持（批次内顺序 write_all）。
            //
            // 轮转游标 `lane_rr` 必须跨队列项持续，不能每项从 0 重来：上游泵
            // 用 64KiB 缓冲读取，而 CHUNK 也是 64KiB，于是每个队列项通常只切出
            // 一个 chunk。游标每项归零 ⇒ 恒取 lane 0 ⇒ 全部流量压在一条 lane 上，
            // 多 lane / 多会话形同虚设（实测 64MB 下载里 68MB 走了第一条 TCP，
            // 其余每条只有几百字节）。
            let mut batches: Vec<Vec<u8>> = vec![Vec::new(); lanes.len()];
            for chunk in bytes.chunks(CHUNK) {
                let li = (lane_rr % lanes.len() as u64) as usize;
                lane_rr = lane_rr.wrapping_add(1);
                encode_frame(off, chunk, &mut batches[li]);
                off += chunk.len() as u64;
            }
            // 把每条 lane 的写半 move 到独立 future 再 join——所有权出借问题
            // 用「写完放回」解决：lane 写半包成 Option，future 归还。
            let mut lane_ws: Vec<Option<WriteHalf<MuxStream>>> =
                lanes.iter_mut().map(|l| l.w.take()).collect();
            let mut futs = Vec::new();
            for (li, buf) in batches.into_iter().enumerate() {
                if buf.is_empty() {
                    continue;
                }
                let Some(mut w) = lane_ws[li].take() else { continue };
                futs.push(async move {
                    let r = w.write_all(&buf).await.is_ok();
                    (li, w, r)
                });
            }
            let results = futures::future::join_all(futs).await;
            let mut failed = false;
            for (li, w, ok) in results {
                if let Some(l) = lanes.get_mut(li) {
                    l.w = Some(w);
                    l.last_write = Instant::now();
                }
                if !ok {
                    failed = true;
                }
            }
            // 本轮没分到数据的 lane：写半还留在 lane_ws 里，必须放回。
            // 否则它被 drop → 该 lane 的写侧关闭 → 对端 lane_reader 读到 EOF
            // → live_lanes 归零后整条 conn 被判终结。轮转游标持续化之前，
            // 每批总是从 lane 0 开始填，靠后的 lane 几乎必然分到空批次，
            // 于是升级出来的 lane 刚建好就被这里关掉——多 lane 从未真正成立。
            for (li, w) in lane_ws.into_iter().enumerate() {
                if let (Some(w), Some(l)) = (w, lanes.get_mut(li)) {
                    l.w = Some(w);
                }
            }
            if failed {
                mark_dead(&inner);
                return;
            }
            up_off = off;
            sent += bytes.len() as u64;
            maybe_upgrade(
                &mut lanes,
                &inner,
                send_dir,
                sent,
                start,
            )
            .await;
        }
        // 2. 判断是否收尾
        let (closing, _reason) = {
            let out = inner.out.lock().unwrap();
            (out.closing, out.close_reason)
        };
        if closing {
            break;
        }
        // 3. 等新工作：队列来数据 / 控制消息。
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
            msg = ctl.recv() => match msg {
                Some(CtlMsg::AddLane(w)) => {
                    lanes.push(LaneW { w: Some(w), last_write: Instant::now() });
                    inner.lane_count.store(lanes.len(), Ordering::Relaxed);
                }
                None => {
                    let mut out = inner.out.lock().unwrap();
                    out.closing = true;
                }
            },
        }
    }

    // 4. 发 CLOSE 帧（最闲 lane）并关闭全部 lane
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
    for l in &mut lanes {
        if let Some(w) = l.w.as_mut() {
            let _ = w.flush().await;
            let _ = w.shutdown().await;
        }
    }
    mark_dead(&inner);
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
    let elapsed = start.elapsed();
    if elapsed < cfg.upgrade_window {
        return;
    }
    let rate = sent as f64 / elapsed.as_secs_f64();
    if (rate as u64) < cfg.upgrade_rate_bps {
        return;
    }
    let want = cfg.target_lanes - lanes.len();
    for _ in 0..want {
        let Ok(stream) = (inner.lane_opener)(send_dir).await else { break };
        let (r, w) = tokio::io::split(stream);
        // 自己开的 lane 上对端不会有数据，但读半须有人消费（EOF 检测）
        tokio::spawn(drain_read_half(r));
        lanes.push(LaneW { w: Some(w), last_write: Instant::now() });
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

/// conn_id → conn 表（双端共用；接受侧据此归并 lane）。
#[derive(Default)]
pub struct ConnRegistry {
    map: Mutex<std::collections::HashMap<u64, Arc<StripeConn>>>,
}

impl ConnRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn insert(&self, id: u64, conn: Arc<StripeConn>) {
        self.map.lock().unwrap().insert(id, conn);
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
}

impl StripeDialer {
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
        let d = self.clone();
        tokio::spawn(async move {
            loop {
                let stream = match mux.accept().await {
                    Ok(s) => s,
                    // accept 循环退出 = 会话已死：先摘表，避免后续 pick
                    // 继续把 lane 往死会话上开。
                    Err(_) => {
                        d.prune_session(&mux);
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
    }
    // 未知 conn / 非 OPEN 首帧：丢流
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
