//! 会话内的共享状态：出站缓冲与单流状态。
//!
//! 这里是整个 wsmux 的性能核心，所以把设计取舍写在前面。
//!
//! **为什么出站是一块共享缓冲而不是 channel。**
//! 三方 mux 普遍把每个帧当成一条 channel 消息发给 writer 任务。那条路上每帧
//! 要付两次拷贝（`&[u8]` → 消息、消息 → socket 缓冲）和一次任务唤醒。实测下来
//! 这两笔开销就是"每流吃掉一个核"的来源。改成共享缓冲后，`poll_write` 直接把
//! 帧头和数据拷进最终要写出去的那块内存，writer 只负责把它整块换走——每帧一次
//! 拷贝，而且多条流的并发写会自动合并成一次 `write_all`，流数越多合并率越高。
//!
//! **锁竞争。**临界区里只有 `extend_from_slice`，没有 await、没有分配（容量由
//! writer 归还时预留）。这种长度的临界区用 `std::sync::Mutex` 就够，不值得为它
//! 引一个 `parking_lot`。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Waker;

use bytes::{Buf, Bytes, BytesMut};
use futures::task::AtomicWaker;

use super::frame::{Cmd, Header, HEADER_LEN};

/// 出站缓冲的初始容量，也是 writer 每次归还时保证的容量。
/// 取 256 KiB：足够装下若干个满帧，又不至于让每条会话常驻太多内存。
const OUT_BUF_CAP: usize = 256 * 1024;

/// 出站缓冲的高水位。堆积超过它，`poll_write` 就挂起等 writer 排空。
///
/// 这是会话级的总背压，与流级的信用窗口是两回事：信用窗口防的是对端来不及收，
/// 高水位防的是**本端**来不及写（底层是 xhttp 伪流，写出速度受远端 POST 节奏
/// 约束）。少了它，一条快速的本地读源会把内存吃穿。
const OUT_HI_WATER_DEFAULT: usize = 1024 * 1024;

/// 高水位的实际取值，可用 `WSIEVE_WSMUX_HIWATER`（字节）覆盖。
///
/// 只读一次并缓存：`push_data` 在数据路径的最内层，每帧去查一次环境变量
/// 是纯粹的浪费。
fn hi_water() -> usize {
    static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("WSIEVE_WSMUX_HIWATER")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&n: &usize| n >= 64 * 1024)
            .unwrap_or(OUT_HI_WATER_DEFAULT)
    })
}

/// 会话的出站侧。所有流共享一份。
pub(crate) struct Outbound {
    buf: Mutex<BytesMut>,
    /// 缓冲里待发的字节数。单独用原子维护，是为了让 `poll_write` 的背压判断
    /// 不必先抢锁——绝大多数时候水位远低于阈值，这条快路径不该付锁的代价。
    queued: AtomicUsize,
    /// writer 任务的 waker。
    writer: AtomicWaker,
    /// 被高水位挡住的写者。可能同时有多条流在等，所以是一组而不是一个。
    blocked: Mutex<Vec<Waker>>,
    closed: AtomicBool,
    /// writer 写完后归还的空缓冲，等着被下一次 `take` 换进去。
    ///
    /// 少了它，每次 `take` 都要新分配一块 `OUT_BUF_CAP`，写完再释放——在满速
    /// 下就是每秒几百次大块分配/释放。剖析里 `madvise` / `stop_allocator` /
    /// `free_medium` 那一堆分配器符号加起来能占到一成 CPU，来源就是这里。
    /// 一发一还刚好构成双缓冲：writer 在写 A 的时候，生产者正往 B 里填。
    spare: Mutex<Option<BytesMut>>,
}

impl Outbound {
    pub(crate) fn new() -> Self {
        Self {
            buf: Mutex::new(BytesMut::with_capacity(OUT_BUF_CAP)),
            queued: AtomicUsize::new(0),
            writer: AtomicWaker::new(),
            blocked: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            spare: Mutex::new(None),
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// 拆会话。叫醒 writer（让它退出）和所有被背压挡住的写者（让它们看到错误）。
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.writer.wake();
        self.wake_blocked();
    }

    fn wake_blocked(&self) {
        let woken = std::mem::take(&mut *self.blocked.lock().unwrap());
        for w in woken {
            w.wake();
        }
    }

    /// 排一个控制帧（SYN/FIN/WND/NOP）。
    ///
    /// 控制帧**不受高水位约束**：它们体积固定为 12 字节，而且窗口更新被挡住会
    /// 直接把对端饿死——那是死锁，不是背压。
    pub(crate) fn push_control(&self, cmd: Cmd, sid: u32, arg: u32) {
        if self.is_closed() {
            return;
        }
        {
            let mut b = self.buf.lock().unwrap();
            b.extend_from_slice(&Header::new(cmd, sid, arg).encode());
        }
        self.queued.fetch_add(HEADER_LEN, Ordering::AcqRel);
        self.writer.wake();
    }

    /// 排一个数据帧。返回 `false` 表示水位已满，调用方应挂起。
    ///
    /// 水位检查在**拷贝之前**，所以拒绝是零成本的；而一旦开始拷贝就一定整帧写完，
    /// 不会出现半个帧留在缓冲里的情况——那会让对端的帧边界永久错位。
    pub(crate) fn push_data(&self, sid: u32, data: &[u8]) -> bool {
        if self.queued.load(Ordering::Acquire) >= hi_water() {
            return false;
        }
        {
            let mut b = self.buf.lock().unwrap();
            b.extend_from_slice(&Header::new(Cmd::Psh, sid, data.len() as u32).encode());
            b.extend_from_slice(data);
        }
        self.queued
            .fetch_add(HEADER_LEN + data.len(), Ordering::AcqRel);
        self.writer.wake();
        true
    }

    /// writer 写完之后把缓冲还回来复用。
    ///
    /// 只留一块。`BytesMut::clear` 不释放容量，所以归还的这块下次 `take` 时
    /// 直接就能用。容量异常大的（某次突发把它撑起来了）不收，免得一条闲置
    /// 会话长期占着一大块内存。
    pub(crate) fn recycle(&self, mut buf: BytesMut) {
        if buf.capacity() > OUT_BUF_CAP * 4 {
            return;
        }
        buf.clear();
        let mut slot = self.spare.lock().unwrap();
        if slot.is_none() {
            *slot = Some(buf);
        }
    }

    /// 登记一个被高水位挡住的写者。
    ///
    /// 登记完必须由调用方**重新检查一次水位**：writer 可能恰好在检查和登记之间
    /// 排空了缓冲，那次唤醒就落空了。这是 check-then-register 竞态的标准解法。
    pub(crate) fn register_blocked(&self, w: &Waker) {
        self.blocked.lock().unwrap().push(w.clone());
    }

    pub(crate) fn queued_bytes(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }

    pub(crate) fn at_hi_water(&self) -> bool {
        self.queued_bytes() >= hi_water()
    }

    /// writer 侧：把待发数据整块换出来。缓冲空时返回 `None` 并登记 writer 的 waker。
    pub(crate) fn take(&self, w: &Waker) -> Option<BytesMut> {
        self.writer.register(w);
        let mut b = self.buf.lock().unwrap();
        if b.is_empty() {
            return None;
        }
        // 优先用归还的那块；没有才新分配。
        let fresh = self
            .spare
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| BytesMut::with_capacity(OUT_BUF_CAP));
        let out = std::mem::replace(&mut *b, fresh);
        drop(b);
        self.queued.fetch_sub(out.len(), Ordering::AcqRel);
        // 水位刚降下来，把等着的写者全放出去。
        self.wake_blocked();
        Some(out)
    }
}

/// 会话的流表。`Session` 和每条 `Stream` 共享同一份。
///
/// 让 `Stream` 也持有它，是为了在 `Drop` 里把自己的表项摘掉。否则表项只能靠
/// 对端发 FIN 来回收，而对端未必会发（连接被 RST、进程被杀），长会话上就是
/// 一条稳定的内存泄漏。
pub(crate) type StreamTable = Arc<Mutex<HashMap<u32, Arc<StreamState>>>>;

/// `read_into` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadOut {
    /// 填了 n 字节（n > 0）。
    Filled(usize),
    /// 队列已排干且对端已关闭。
    Eof,
    /// 暂时无数据，登记 waker 后挂起。
    Blocked,
}

/// 单条流的入站队列。
///
/// 队首用 `Bytes::advance` 做部分消费而不是 `split_to`：前者只挪一个指针，
/// 后者要动引用计数并产生一个新的 `Bytes` 头。读侧每次 `poll_read` 都会走这条
/// 路径，差别累积起来不小。
struct Inbox {
    q: VecDeque<Bytes>,
    /// 对端已发 FIN，或会话已死。队列排空后 `poll_read` 返回 EOF。
    eof: bool,
}

/// 单条流的全部状态。`Session` 和 `Stream` 各持一份 `Arc`。
pub(crate) struct StreamState {
    pub(crate) sid: u32,
    inbox: Mutex<Inbox>,
    reader: AtomicWaker,

    /// 还能往对端发多少字节。对端每发一个 WND 就加回来。
    ///
    /// 用有符号类型是为了让"扣减"和"判负"能在一次 `fetch_sub` 里完成，
    /// 不必先读再比再写（那中间有竞态窗口）。
    credit: AtomicI64,
    credit_waker: AtomicWaker,

    /// 本端已消费、但还没告诉对端的字节数。攒够半个窗口才发 WND——
    /// 每帧一个窗口更新会让控制帧数量和数据帧一样多，那是纯粹的浪费。
    unacked: AtomicU32,

    /// 本端已发 FIN。
    pub(crate) fin_sent: AtomicBool,
    /// 会话或流已失效，读写都应立刻报错。
    dead: AtomicBool,

    /// 入站队列里积压的字节数，以及它的上限。
    ///
    /// 守规矩的对端永远不会越过我们给它的窗口，所以这条线正常情况下碰不到。
    /// 它防的是一个坏掉或恶意的对端把我们的内存吃穿——`VecDeque` 本身无界，
    /// 少了这道闸就是一个远端可触发的 OOM。
    queued_in: AtomicUsize,
    in_limit: usize,
}

impl StreamState {
    /// 两个窗口是**不同的东西**，别合并成一个参数：
    ///
    /// - `peer_window`：对端愿意收多少 → 本端的发送信用（`credit`）。
    /// - `local_window`：本端愿意收多少 → 入站积压上限（`in_limit`）。
    ///
    /// 早先这里只有一个参数、两处都用它，隐含假定了"两端配置相同"。一旦两端
    /// 窗口不一致，入站上限就会按对端的数值来算，于是一条完全守规矩的流会被
    /// 误判成越窗而断链——实测表现为链路握手成功、然后一个字节都传不动。
    pub(crate) fn new(sid: u32, peer_window: u32, local_window: u32) -> Self {
        Self {
            sid,
            inbox: Mutex::new(Inbox {
                q: VecDeque::new(),
                eof: false,
            }),
            reader: AtomicWaker::new(),
            credit: AtomicI64::new(peer_window as i64),
            queued_in: AtomicUsize::new(0),
            in_limit: local_window.max(64 * 1024) as usize * 2,
            credit_waker: AtomicWaker::new(),
            unacked: AtomicU32::new(0),
            fin_sent: AtomicBool::new(false),
            dead: AtomicBool::new(false),
        }
    }

    pub(crate) fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    /// 流失效：叫醒读者和写者，让它们各自看到错误/EOF 并退出。
    pub(crate) fn kill(&self) {
        self.dead.store(true, Ordering::Release);
        self.inbox.lock().unwrap().eof = true;
        self.reader.wake();
        self.credit_waker.wake();
    }

    /// reader 任务侧：投递一个 payload。返回 `false` 表示对端越窗，会话应终止。
    pub(crate) fn deliver(&self, data: Bytes) -> bool {
        {
            let mut ib = self.inbox.lock().unwrap();
            if ib.eof {
                return true; // 流已关，丢弃即可——对端迟早会看到我们的 FIN。
            }
            let now = self.queued_in.fetch_add(data.len(), Ordering::AcqRel) + data.len();
            if now > self.in_limit {
                return false;
            }
            ib.q.push_back(data);
        }
        self.reader.wake();
        true
    }

    /// reader 任务侧：对端发来 FIN。
    pub(crate) fn mark_eof(&self) {
        self.inbox.lock().unwrap().eof = true;
        self.reader.wake();
    }

    /// reader 任务侧：对端发来 WND，放行等在信用上的写者。
    pub(crate) fn grant(&self, n: u32) {
        self.credit.fetch_add(n as i64, Ordering::AcqRel);
        self.credit_waker.wake();
    }

    /// 取一段入站数据填进 `dst`。
    ///
    /// 三态而不是 `Option<usize>`：`Filled(0)` 和 `Eof` 在 `AsyncRead` 里是
    /// 截然不同的意思（前者是"这次没读到"，后者是"流结束了"），用同一个
    /// `Some(0)` 表示会让上层把一次空读当成对端关闭。
    pub(crate) fn read_into(&self, dst: &mut [u8]) -> ReadOut {
        let mut ib = self.inbox.lock().unwrap();
        let mut n = 0;
        while n < dst.len() {
            let Some(front) = ib.q.front_mut() else { break };
            let take = front.len().min(dst.len() - n);
            dst[n..n + take].copy_from_slice(&front[..take]);
            front.advance(take);
            if front.is_empty() {
                ib.q.pop_front();
            }
            n += take;
        }
        if n > 0 {
            self.queued_in.fetch_sub(n, Ordering::AcqRel);
            ReadOut::Filled(n)
        } else if ib.q.is_empty() && ib.eof {
            // 注意是"队列空**且**EOF"：FIN 到达时队列里可能还压着数据，
            // 那些字节必须先交给上层，不能因为看到 eof 标记就丢掉。
            ReadOut::Eof
        } else {
            ReadOut::Blocked
        }
    }

    pub(crate) fn register_reader(&self, w: &Waker) {
        self.reader.register(w);
    }

    pub(crate) fn register_credit(&self, w: &Waker) {
        self.credit_waker.register(w);
    }

    /// 消费 `n` 字节信用。调用前必须确认信用足够。
    pub(crate) fn spend(&self, n: usize) {
        self.credit.fetch_sub(n as i64, Ordering::AcqRel);
    }

    pub(crate) fn credit(&self) -> i64 {
        self.credit.load(Ordering::Acquire)
    }

    /// 记账本端消费掉的字节；攒够 `window/2` 就返回该回给对端的窗口增量。
    pub(crate) fn take_ack(&self, n: usize, window: u32) -> Option<u32> {
        let before = self.unacked.fetch_add(n as u32, Ordering::AcqRel);
        let now = before + n as u32;
        if now >= window / 2 {
            // 用 swap 而不是 store(0)：并发读者可能在这中间又加了几笔，
            // swap 能把它们一起带走，不会漏账。
            let total = self.unacked.swap(0, Ordering::AcqRel);
            if total > 0 {
                return Some(total);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop_waker() -> Waker {
        futures::task::noop_waker()
    }

    #[test]
    fn a_data_frame_lands_in_the_buffer_as_header_plus_payload() {
        let out = Outbound::new();
        assert!(out.push_data(7, b"hello"));
        let taken = out.take(&noop_waker()).expect("刚写进去就该拿得到");
        assert_eq!(taken.len(), HEADER_LEN + 5);
        let h = Header::decode(&taken).unwrap();
        assert_eq!((h.cmd, h.sid, h.arg), (Cmd::Psh, 7, 5));
        assert_eq!(&taken[HEADER_LEN..], b"hello");
        assert_eq!(out.queued_bytes(), 0, "换出后水位必须归零");
    }

    #[test]
    fn the_high_water_mark_rejects_data_but_never_control_frames() {
        let out = Outbound::new();
        let chunk = vec![0u8; 64 * 1024];
        while !out.at_hi_water() {
            assert!(out.push_data(1, &chunk));
        }
        assert!(!out.push_data(1, &chunk), "过了高水位数据帧必须被挡");

        // 窗口更新在这时候被挡住就是死锁：对端等我们放窗口，我们等对端收数据。
        let before = out.queued_bytes();
        out.push_control(Cmd::Wnd, 1, 4096);
        assert_eq!(
            out.queued_bytes(),
            before + HEADER_LEN,
            "控制帧必须无视高水位"
        );
    }

    #[test]
    fn a_blocked_writer_is_woken_the_moment_the_buffer_drains() {
        let out = Arc::new(Outbound::new());
        let woken = Arc::new(AtomicBool::new(false));
        let w = {
            let woken = woken.clone();
            futures::task::waker(Arc::new(FlagWake(woken)))
        };
        out.register_blocked(&w);
        out.push_data(1, b"x");
        let _ = out.take(&noop_waker());
        assert!(woken.load(Ordering::Acquire), "排空后必须放行被挡的写者");
    }

    struct FlagWake(Arc<AtomicBool>);
    impl futures::task::ArcWake for FlagWake {
        fn wake_by_ref(arc: &Arc<Self>) {
            arc.0.store(true, Ordering::Release);
        }
    }

    #[test]
    fn a_partially_read_chunk_keeps_its_place_instead_of_being_dropped() {
        let st = StreamState::new(1, 1024, 1024);
        assert!(st.deliver(Bytes::from_static(b"abcdefgh")));
        let mut buf = [0u8; 3];
        assert_eq!(st.read_into(&mut buf), ReadOut::Filled(3));
        assert_eq!(&buf, b"abc");
        let mut rest = [0u8; 16];
        assert_eq!(st.read_into(&mut rest), ReadOut::Filled(5));
        assert_eq!(&rest[..5], b"defgh");
    }

    #[test]
    fn an_empty_inbox_blocks_until_eof_then_reports_end_of_stream() {
        let st = StreamState::new(1, 1024, 1024);
        let mut buf = [0u8; 8];
        assert_eq!(st.read_into(&mut buf), ReadOut::Blocked, "没数据没 EOF 应当挂起");
        st.mark_eof();
        assert_eq!(st.read_into(&mut buf), ReadOut::Eof, "EOF 后应当报流结束");
    }

    #[test]
    fn data_already_queued_when_fin_arrives_is_still_delivered() {
        // 对端"发完数据立刻关流"是最常见的收尾方式。如果看到 eof 就直接报
        // 流结束，这些字节会被静默吞掉——表现为响应体尾部莫名截断。
        let st = StreamState::new(1, 1024, 1024);
        assert!(st.deliver(Bytes::from_static(b"tail")));
        st.mark_eof();
        let mut buf = [0u8; 16];
        assert_eq!(st.read_into(&mut buf), ReadOut::Filled(4));
        assert_eq!(&buf[..4], b"tail");
        assert_eq!(st.read_into(&mut buf), ReadOut::Eof, "排干之后才是 EOF");
    }

    #[test]
    fn window_updates_are_batched_until_half_the_window_is_consumed() {
        let window = 1000u32;
        let st = StreamState::new(1, window, window);
        assert_eq!(st.take_ack(100, window), None);
        assert_eq!(st.take_ack(300, window), None, "累计 400 < 500，还不该回窗");
        assert_eq!(
            st.take_ack(100, window),
            Some(500),
            "跨过半窗时应当把攒的账一次回清"
        );
        assert_eq!(st.take_ack(1, window), None, "回清后重新攒");
    }

    #[test]
    fn a_peer_that_ignores_the_window_is_cut_off_instead_of_eating_our_memory() {
        // 入站队列本身无界。少了这道闸，一个不守窗口的对端就是远端可触发的 OOM。
        let st = StreamState::new(1, 64 * 1024, 64 * 1024);
        let chunk = Bytes::from(vec![0u8; 64 * 1024]);
        assert!(st.deliver(chunk.clone()), "窗口之内必须收下");
        assert!(st.deliver(chunk.clone()), "到两倍窗口仍在容忍范围");
        assert!(!st.deliver(chunk), "越过上限必须拒绝，由会话侧断链");
    }

    #[test]
    fn credit_is_spent_and_replenished_across_the_zero_boundary() {
        let st = StreamState::new(1, 100, 100);
        st.spend(100);
        assert_eq!(st.credit(), 0, "花光后应当正好归零");
        st.grant(250);
        assert_eq!(st.credit(), 250);
    }
}
