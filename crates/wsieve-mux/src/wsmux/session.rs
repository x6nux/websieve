//! 会话：两个后台任务（reader / writer）+ 一张流表。
//!
//! 每条会话恒定两个任务，与流数无关。流本身是纯被动对象（见 `stream.rs`），
//! 开流不 spawn、不分配 channel，只往表里插一个 `Arc`。
//!
//! **读侧为什么是一大块缓冲而不是逐帧读。**底层是加密后的伪流，一次
//! `read_buf` 常常能带回好几个帧。先整块读进来再在内存里切帧，把"每帧一次
//! syscall"压成"每几十帧一次 syscall"；payload 用 `split_to` 切出来是零拷贝的，
//! 所以这条路径上除了内核那次拷贝之外没有别的拷贝。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::sync::Mutex as AsyncMutex;

use super::frame::{Cmd, Header, HEADER_LEN};
use super::shared::{Outbound, StreamState, StreamTable};
use super::stream::Stream;
use crate::{Duplex, Mux, MuxStream};

/// 每条流的接收窗口。
///
/// 这个值直接决定单流吞吐上限：`吞吐 ≤ 窗口 / RTT`。三方 mux 的默认值
/// （yamux 256 KiB）在 30 ms RTT 上只能跑到约 8 MB/s，是实测中单流上不去的
/// 主因。4 MiB 在 30 ms 上对应约 140 MB/s，越过千兆线速。
const DEFAULT_WINDOW: u32 = 4 * 1024 * 1024;

/// 开流方在收到对端窗口通告之前，先按这个额度发。
///
/// 开流的一方无从知道对端窗口有多大——SYN 是单向的，没有握手回合。拿本端窗口
/// 当发送信用是错的：对端窗口更小时就会超发。所以这里先给一个所有实现都能安全
/// 接收的小额度，对端在处理 SYN 时立刻回一个 WND 把差额补上，不额外花一个 RTT。
const INITIAL_CREDIT: u32 = 64 * 1024;

/// 读缓冲每轮保证的可写空间。取够大，让一次 `read_buf` 尽可能多带回几个帧。
const READ_CHUNK: usize = 256 * 1024;

/// 保活间隔。只在**完全空闲**时才发——出站缓冲非空说明链路本来就有活动，
/// 再插一个 NOP 纯属浪费。
const KEEPALIVE: Duration = Duration::from_secs(15);

pub struct Config {
    pub window: u32,
    pub keepalive: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            window: window_size(),
            keepalive: KEEPALIVE,
        }
    }
}

/// 单流接收窗口的硬上限。
///
/// 窗口是**授信**：通告 N 字节就是允许对端塞 N 字节未读数据过来，而本实现
/// **不限制并发流数**。总量 = 流数 × 窗口，那个乘数由对端决定；应用侧读取
/// 一停（磁盘慢、界面卡），这份额度就全部落成常驻内存。
///
/// 事后收不回：`Cmd::Wnd` 只有正增量，协议里没有负的窗口更新。所以唯一的
/// 闸口在**通告之前**，也就是这里——`window_size()` 与 `grow_window` 各自
/// clamp 一次。自适应那边的档位表也自带同一个上限
/// （`wsieve_xhttp::link_profile::MAX_WINDOW`），两处独立成立：档位表是
/// "别要求超限值"，这里是"要求了也不给"。
///
/// 16 MiB 的依据：实测单流吞吐约 20 MB/s，16 MiB 在 100 ms RTT 上对应
/// 160 MB/s，比实际能跑的高 8 倍——再大不会更快，只会更能被塞满。
const MAX_WINDOW: u32 = 16 * 1024 * 1024;

/// 接收窗口的实际取值，可用 `WSIEVE_WSMUX_WINDOW`（字节）覆盖。
///
/// 下限 64 KiB：再小的话一个满帧都放不下，流控会退化成逐帧停等。
/// 上限见 `MAX_WINDOW`——env 也不能越过它，那是内存安全的闸而不是调优旋钮。
fn window_size() -> u32 {
    std::env::var("WSIEVE_WSMUX_WINDOW")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|&n| n >= 64 * 1024)
        .unwrap_or(DEFAULT_WINDOW)
        .min(MAX_WINDOW)
}

struct Inner {
    out: Arc<Outbound>,
    streams: StreamTable,
    /// 下一个可用的 sid。客户端取奇数、服务端取偶数，两端各分一半空间，
    /// 不必为"谁先开流"做任何协商。
    next_sid: AtomicU32,
    /// 本端接收窗口。原子的，因为它会随链路画像在运行期变大
    /// （见 `Session::grow_window`），不再是建会话时定死的常量。
    window: Arc<AtomicU32>,
    dead: AtomicBool,
}

impl Session {
    /// 把本端接收窗口扩大到 `target`，并把增量通告给对端。
    ///
    /// **只增不减**，这是有意的：缩小需要一个负的 WND，协议里没有这个东西。
    /// 真要收缩，停发 delta 让它自然耗尽即可（软收缩），但那是另一件事——
    /// 自适应只会因为链路变好而调大，链路变差时深窗口本身不造成损害。
    ///
    /// 两步的**顺序不能反**：先把每条流的入站闸提高，再通告新窗口。反过来的话
    /// 对端一收到更大的窗口就会多发，而本端的闸还卡在旧值上，于是它一边守规矩
    /// 一边被判越窗（见 `StreamState::grow_in_limit`）。
    pub fn grow_window(&self, target: u32) {
        // 先封顶再谈别的：见 `MAX_WINDOW`。调用方是自适应驱动器，它按 BDP
        // 公式算"单流该多深"，那个公式里没有"会话里有多少条流"这一项。
        let target = target.min(MAX_WINDOW);
        // **整个操作在 streams 锁内完成**，包括 `fetch_max`。
        //
        // 建流的一侧（`open` / `dispatch` 的 SYN 分支）也必须在这把锁内读窗口
        // 并插表，否则有一个会永久停滞的缝：建流方读到旧窗口 → 这里 fetch_max
        // 并遍历（那个 sid 还没插进表，收不到 delta）→ 建流方插表并按旧窗口发
        // SYN。结果是对端的发送信用停在旧窗口，而本端的 ACK 阈值（`take_ack`
        // 读共享的 `Arc<AtomicU32>`）已经是新窗口的一半——对端发满旧窗口就停，
        // 本端 unacked 永远够不到阈值，WND 再也不会发出。没有错误，只有静默卡死。
        let streams = self.inner.streams.lock().unwrap();
        let prev = self.inner.window.fetch_max(target, Ordering::AcqRel);
        if target <= prev {
            return;
        }
        let delta = target - prev;
        for (sid, st) in streams.iter() {
            st.grow_in_limit(target);
            self.inner.out.push_control(Cmd::Wnd, *sid, delta);
        }
    }

    /// 当前的本端接收窗口。
    pub fn window(&self) -> u32 {
        self.inner.window.load(Ordering::Relaxed)
    }
}

impl Inner {
    /// 拆会话：关出站、叫醒所有流。之后所有读返回 EOF、所有写返回错误。
    fn shutdown(&self) {
        if self.dead.swap(true, Ordering::AcqRel) {
            return;
        }
        self.out.close();
        for (_, st) in self.streams.lock().unwrap().drain() {
            st.kill();
        }
    }
}

pub struct Session {
    inner: Arc<Inner>,
    /// 待 accept 的入站流。accept 每条连接只调用一次，频率远低于数据路径，
    /// 用 channel 换取代码简单是划算的。
    incoming: AsyncMutex<mpsc::UnboundedReceiver<Stream>>,
    /// 只有读侧的句柄。写侧**故意不留**——见 `Drop` 的注释。
    read_task: tokio::task::JoinHandle<()>,
}

impl Session {
    pub fn new(io: MuxStream, is_server: bool) -> Self {
        Self::with_config(io, is_server, Config::default())
    }

    pub fn with_config(io: MuxStream, is_server: bool, cfg: Config) -> Self {
        let inner = Arc::new(Inner {
            out: Arc::new(Outbound::new()),
            streams: StreamTable::default(),
            next_sid: AtomicU32::new(if is_server { 2 } else { 1 }),
            window: Arc::new(AtomicU32::new(cfg.window)),
            dead: AtomicBool::new(false),
        });

        let (tx, rx) = mpsc::unbounded_channel();
        let (rd, wr) = tokio::io::split(io);

        let read_task = tokio::spawn(read_loop(inner.clone(), rd, tx));
        tokio::spawn(write_loop(inner.clone(), wr, cfg.keepalive));

        Self {
            inner,
            incoming: AsyncMutex::new(rx),
            read_task,
        }
    }

    fn alloc_sid(&self) -> u32 {
        // 步长 2 保持奇偶不变。u32 回绕后仍然保持奇偶，所以长会话也安全。
        self.inner.next_sid.fetch_add(2, Ordering::Relaxed)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // 最后一个句柄没了就没人能再 open/accept，会话该收摊——但要**优雅**收。
        //
        // 写侧不能 abort。`Stream::poll_write` 返回 Ok 只表示帧已进出站缓冲，
        // 不表示已经写到 io 上；直接 abort 会把缓冲里排队的帧连同刚发的 FIN
        // 一起扔掉，对端看到的是数据凭空少一截。所以这里只置关闭标志，让
        // write_loop 把缓冲排干、shutdown 掉 io 之后自己退出（`next_chunk` 里
        // 取数据优先于查关闭标志，正是为了这一刻）。
        //
        // 读侧相反，可以直接 abort：此后到达的入站数据没有任何人会去读。
        self.inner.shutdown();
        self.read_task.abort();
    }
}

#[async_trait::async_trait]
impl Mux for Session {
    fn grow_window(&self, target: u32) {
        Session::grow_window(self, target);
    }

    async fn open(&self) -> anyhow::Result<MuxStream> {
        if self.inner.dead.load(Ordering::Acquire) {
            anyhow::bail!("wsmux 会话已关闭，无法开流");
        }
        let sid = self.alloc_sid();
        // 发送信用先按保守额度起步，真实额度等对端的 WND；接收上限用本端窗口。
        //
        // 读窗口与插表必须在**同一把锁**内，与 `grow_window` 互斥——否则扩窗
        // 会漏掉这条正在建的流，见 `grow_window` 的注释。
        let (win, st) = {
            let mut streams = self.inner.streams.lock().unwrap();
            let win = self.inner.window.load(Ordering::Relaxed);
            let st = Arc::new(StreamState::new(sid, INITIAL_CREDIT, win));
            streams.insert(sid, st.clone());
            (win, st)
        };
        // SYN 先于任何数据发出。arg 带本端接收窗口，对端据此设定它的发送信用。
        self.inner.out.push_control(Cmd::Syn, sid, win);
        Ok(Box::new(Stream::new(
            st,
            self.inner.out.clone(),
            self.inner.window.clone(),
            self.inner.streams.clone(),
        )))
    }

    async fn accept(&self) -> anyhow::Result<MuxStream> {
        let mut rx = self.incoming.lock().await;
        match rx.recv().await {
            Some(s) => Ok(Box::new(s)),
            None => anyhow::bail!("wsmux 会话已关闭，不会再有新流"),
        }
    }
}

/// 读侧主循环。任何解析错误都直接拆会话——复用层没有安全的单帧恢复点，
/// 一旦帧边界错位，后面读到的全是垃圾。
async fn read_loop(
    inner: Arc<Inner>,
    mut rd: tokio::io::ReadHalf<Box<dyn Duplex + Send + Unpin>>,
    tx: mpsc::UnboundedSender<Stream>,
) {
    let mut buf = BytesMut::with_capacity(READ_CHUNK);
    loop {
        // 先把缓冲里已经完整的帧全部消化掉，再去读下一批。
        while let Some(done) = try_parse_one(&inner, &mut buf, &tx) {
            if !done {
                inner.shutdown();
                return;
            }
        }

        buf.reserve(READ_CHUNK);
        match rd.read_buf(&mut buf).await {
            Ok(0) | Err(_) => break, // 对端关闭或链路出错，两种都是会话终结。
            Ok(_) => {}
        }
    }
    inner.shutdown();
}

/// 尝试从缓冲头部取出一个完整帧并派发。
///
/// - `None`：数据不够，需要继续读。
/// - `Some(true)`：处理了一帧。
/// - `Some(false)`：对端发来的是垃圾，会话必须终止。
fn try_parse_one(
    inner: &Arc<Inner>,
    buf: &mut BytesMut,
    tx: &mpsc::UnboundedSender<Stream>,
) -> Option<bool> {
    if buf.len() < HEADER_LEN {
        return None;
    }
    let Some(h) = Header::decode(&buf[..HEADER_LEN]) else {
        return Some(false);
    };
    let body = if h.cmd == Cmd::Psh { h.arg as usize } else { 0 };
    if buf.len() < HEADER_LEN + body {
        return None;
    }
    buf.advance(HEADER_LEN);
    // `split_to` 把这段内存的所有权直接交给 payload，没有拷贝。
    let payload = if body > 0 {
        buf.split_to(body).freeze()
    } else {
        Bytes::new()
    };
    Some(dispatch(inner, h, payload, tx))
}

/// 派发一个已解析出的帧。返回 `false` 表示对端违反协议，会话必须终止。
fn dispatch(
    inner: &Arc<Inner>,
    h: Header,
    payload: Bytes,
    tx: &mpsc::UnboundedSender<Stream>,
) -> bool {
    match h.cmd {
        Cmd::Syn => {
            // SYN 的 arg 是对端的接收窗口，正是我们能往它发多少字节。
            //
            // 与 `open` 同理：读窗口与插表要在同一把锁内，才能与 `grow_window`
            // 互斥。少了这一层，扩窗会漏掉这条流并让它永久停滞。
            let (win, st) = {
                let mut streams = inner.streams.lock().unwrap();
                let win = inner.window.load(Ordering::Relaxed);
                let st = Arc::new(StreamState::new(h.sid, h.arg, win));
                streams.insert(h.sid, st.clone());
                (win, st)
            };
            // 对端此刻只给自己留了 `INITIAL_CREDIT`，立刻把本端真实窗口补给它。
            // 不补的话它每发 64 KiB 就要停下来等一次窗口更新。
            if win > INITIAL_CREDIT {
                inner
                    .out
                    .push_control(Cmd::Wnd, h.sid, win - INITIAL_CREDIT);
            }
            let s = Stream::new(
                st,
                inner.out.clone(),
                inner.window.clone(),
                inner.streams.clone(),
            );
            // 发送失败说明 Session 已被丢弃，没人再来 accept 了。
            let _ = tx.send(s);
        }
        Cmd::Psh => {
            // 锁在 `deliver` 之前就放掉：`deliver` 会拿流自己的 inbox 锁，
            // 在持有表锁时再去拿另一把锁是在给自己埋锁序问题。
            let st = inner.streams.lock().unwrap().get(&h.sid).cloned();
            if let Some(st) = st {
                if !st.deliver(payload) {
                    return false; // 对端越窗，按协议违规处理。
                }
            }
            // 查不到 sid 是正常的：本端刚关掉这条流，对端的数据还在路上。
        }
        Cmd::Fin => {
            // 只标记 EOF，**不**摘表项。FIN 是半关闭——对端不再发数据，但它
            // 还在收我们的数据，还会继续回 WND。这时候把表项删掉，那些 WND 就
            // 会找不到 sid 被丢弃，本端信用再也涨不回来，写侧永久挂死。
            // 表项的回收统一交给 `Stream::drop`。
            let st = inner.streams.lock().unwrap().get(&h.sid).cloned();
            if let Some(st) = st {
                st.mark_eof();
            }
        }
        Cmd::Wnd => {
            let st = inner.streams.lock().unwrap().get(&h.sid).cloned();
            if let Some(st) = st {
                st.grant(h.arg);
            }
        }
        Cmd::Nop => {}
    }
    true
}

/// 写侧主循环。把出站缓冲整块换出来一次写完。
///
/// 这里没有"每帧一次 write"——多条流在两次换出之间排进来的帧会被合并成一次
/// `write_all`，流越多合并率越高。这是本实现相对三方 mux 的主要 CPU 优势。
async fn write_loop(
    inner: Arc<Inner>,
    mut wr: tokio::io::WriteHalf<Box<dyn Duplex + Send + Unpin>>,
    keepalive: Duration,
) {
    loop {
        let chunk = tokio::select! {
            biased;
            c = next_chunk(&inner) => c,
            _ = tokio::time::sleep(keepalive) => {
                // 真空闲才发保活。缓冲非空说明链路上本来就有流量。
                if inner.out.queued_bytes() == 0 {
                    inner.out.push_control(Cmd::Nop, 0, 0);
                }
                continue;
            }
        };
        let Some(chunk) = chunk else { break };
        if wr.write_all(&chunk).await.is_err() {
            break;
        }
        // 底层未必是 socket。本项目里它是 xhttp 伪流——一个带自己缓冲的东西，
        // `write_all` 只把字节交给它，不代表已经上线。少了这次 flush，数据会
        // 一直压在下层，直到某个无关的定时器顺手把它带出去；实测表现为吞吐
        // 看着正常、延迟却精确等于保活周期。
        //
        // 放在这里而不是每帧一次：`take` 一次就把排队的帧全换走了，所以这正好
        // 是一个天然的批边界，flush 的次数等于实际写 syscall 的次数。
        if wr.flush().await.is_err() {
            break;
        }
        // 缓冲已经写出去了，还回去给下一轮用，省掉一次大块分配。
        inner.out.recycle(chunk);
    }
    inner.shutdown();
    let _ = wr.shutdown().await;
}

/// 等到出站缓冲里有东西，整块取走。会话关闭时返回 `None`。
async fn next_chunk(inner: &Arc<Inner>) -> Option<BytesMut> {
    std::future::poll_fn(|cx| {
        // `take` 内部会先登记 waker 再检查缓冲，所以这里不会漏唤醒。
        if let Some(b) = inner.out.take(cx.waker()) {
            return Poll::Ready(Some(b));
        }
        if inner.out.is_closed() {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await
}
