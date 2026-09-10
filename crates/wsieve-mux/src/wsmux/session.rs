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

/// 接收窗口的实际取值，可用 `WSIEVE_WSMUX_WINDOW`（字节）覆盖。
///
/// 下限 64 KiB：再小的话一个满帧都放不下，流控会退化成逐帧停等。
/// 上限 `i64::MAX` 由 `credit` 的类型保证，实践中受内存约束，不另设。
fn window_size() -> u32 {
    std::env::var("WSIEVE_WSMUX_WINDOW")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|&n| n >= 64 * 1024)
        .unwrap_or(DEFAULT_WINDOW)
}

struct Inner {
    out: Arc<Outbound>,
    streams: StreamTable,
    /// 下一个可用的 sid。客户端取奇数、服务端取偶数，两端各分一半空间，
    /// 不必为"谁先开流"做任何协商。
    next_sid: AtomicU32,
    window: u32,
    dead: AtomicBool,
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
            window: cfg.window,
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
    async fn open(&self) -> anyhow::Result<MuxStream> {
        if self.inner.dead.load(Ordering::Acquire) {
            anyhow::bail!("wsmux 会话已关闭，无法开流");
        }
        let sid = self.alloc_sid();
        // 发送信用先按保守额度起步，真实额度等对端的 WND；接收上限用本端窗口。
        let st = Arc::new(StreamState::new(sid, INITIAL_CREDIT, self.inner.window));
        self.inner.streams.lock().unwrap().insert(sid, st.clone());
        // SYN 先于任何数据发出。arg 带本端接收窗口，对端据此设定它的发送信用。
        self.inner.out.push_control(Cmd::Syn, sid, self.inner.window);
        Ok(Box::new(Stream::new(
            st,
            self.inner.out.clone(),
            self.inner.window,
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
            let st = Arc::new(StreamState::new(h.sid, h.arg, inner.window));
            inner.streams.lock().unwrap().insert(h.sid, st.clone());
            // 对端此刻只给自己留了 `INITIAL_CREDIT`，立刻把本端真实窗口补给它。
            // 不补的话它每发 64 KiB 就要停下来等一次窗口更新。
            if inner.window > INITIAL_CREDIT {
                inner
                    .out
                    .push_control(Cmd::Wnd, h.sid, inner.window - INITIAL_CREDIT);
            }
            let s = Stream::new(
                st,
                inner.out.clone(),
                inner.window,
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
