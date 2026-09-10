//! 子流：把 `StreamState` 包成 `AsyncRead + AsyncWrite`。
//!
//! 这里没有任何后台任务。一条流就是一组原子量加两个 waker 槽，开销只有一次
//! `Arc` 分配——所以开几千条流也不会给调度器添负担。
//!
//! **绝不自旋。**每一条返回 `Poll::Pending` 的路径，都必须在返回之前把 waker
//! 登记到某个将来一定会被叫醒的地方，然后**再检查一次**条件。先登记后检查这个
//! 顺序不能反：反了就会漏掉登记与检查之间发生的那次唤醒，轻则卡顿重则永久挂起。
//! （`cx.waker().wake_by_ref(); Poll::Pending` 那种写法功能上是对的，代价是把一个
//! 核烧光——本项目实测过，那正是换掉三方 mux 的直接原因。）

use std::io;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::frame::{Cmd, MAX_FRAME_PAYLOAD};
use super::shared::{Outbound, ReadOut, StreamState, StreamTable};

pub struct Stream {
    st: Arc<StreamState>,
    out: Arc<Outbound>,
    /// 本端接收窗口，用于判断该在什么时候回 WND。会话级配置，各流一致。
    window: u32,
    /// 会话的流表，只为了在 `Drop` 里摘掉自己的表项。
    table: StreamTable,
}

impl Stream {
    pub(crate) fn new(
        st: Arc<StreamState>,
        out: Arc<Outbound>,
        window: u32,
        table: StreamTable,
    ) -> Self {
        Self {
            st,
            out,
            window,
            table,
        }
    }

    pub fn id(&self) -> u32 {
        self.st.sid
    }

    fn broken() -> io::Error {
        io::Error::new(io::ErrorKind::BrokenPipe, "wsmux 会话已关闭")
    }

    /// 收尾一次成功的读：推进 `ReadBuf`，并按需把接收窗口还给对端。
    fn finish_read(&self, n: usize, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        buf.advance(n);
        // 数据已交给上层，对应的接收窗口可以还了。`take_ack` 攒够半个窗口才
        // 真的发帧，所以这不是每读一次发一个 WND。
        if let Some(delta) = self.st.take_ack(n, self.window) {
            self.out.push_control(Cmd::Wnd, self.st.sid, delta);
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.st.read_into(buf.initialize_unfilled()) {
            ReadOut::Filled(n) => return this.finish_read(n, buf),
            ReadOut::Eof => return Poll::Ready(Ok(())),
            ReadOut::Blocked => {}
        }
        // 先登记再复查。反过来会漏掉登记之前那一瞬间到达的数据，
        // 表现为流莫名其妙地卡住直到下一个包把它撞醒。
        this.st.register_reader(cx.waker());
        match this.st.read_into(buf.initialize_unfilled()) {
            ReadOut::Filled(n) => this.finish_read(n, buf),
            ReadOut::Eof => Poll::Ready(Ok(())),
            ReadOut::Blocked => Poll::Pending,
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.st.is_dead() || this.out.is_closed() {
            return Poll::Ready(Err(Self::broken()));
        }
        if data.is_empty() {
            return Poll::Ready(Ok(0));
        }

        // 一次最多写一个满帧。上层是 `copy_bidirectional` 之类的循环，
        // 返回部分写入完全合法，它会接着调下一次。
        let mut want = data.len().min(MAX_FRAME_PAYLOAD as usize);

        // ① 流级背压：对端的接收窗口。
        let credit = this.st.credit();
        if credit <= 0 {
            this.st.register_credit(cx.waker());
            if this.st.credit() <= 0 {
                if this.st.is_dead() {
                    return Poll::Ready(Err(Self::broken()));
                }
                return Poll::Pending;
            }
        }
        want = want.min(this.st.credit().max(0) as usize);
        if want == 0 {
            this.st.register_credit(cx.waker());
            return Poll::Pending;
        }

        // ② 会话级背压：本端出站缓冲的水位。
        if !this.out.push_data(this.st.sid, &data[..want]) {
            this.out.register_blocked(cx.waker());
            if this.out.at_hi_water() {
                if this.out.is_closed() {
                    return Poll::Ready(Err(Self::broken()));
                }
                return Poll::Pending;
            }
            // 登记后水位刚好降下来了，再试一次；这次失败就老实等下一轮唤醒。
            if !this.out.push_data(this.st.sid, &data[..want]) {
                return Poll::Pending;
            }
        }
        this.st.spend(want);
        Poll::Ready(Ok(want))
    }

    /// 数据一进出站缓冲就归 writer 任务管，这里没有本地缓冲需要冲刷。
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // FIN 只发一次。`swap` 保证并发调用下也只有一个发出去。
        if !this.st.fin_sent.swap(true, Ordering::AcqRel) && !this.out.is_closed() {
            this.out.push_control(Cmd::Fin, this.st.sid, 0);
        }
        Poll::Ready(Ok(()))
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // 上层直接丢弃流而没有 shutdown 是常态（比如代理侧连接被对端 RST）。
        // 补一个 FIN，否则对端会一直等一个永远不来的结束标记。
        if !self.st.fin_sent.swap(true, Ordering::AcqRel) && !self.out.is_closed() {
            self.out.push_control(Cmd::Fin, self.st.sid, 0);
        }
        self.st.kill();
        self.table.lock().unwrap().remove(&self.st.sid);
    }
}
