//! wsmux：本项目自己的流复用实现。
//!
//! 换掉三方 mux 的原因是实测出来的，不是偏好：yamux / smux / picomux / muxado /
//! h2mux 全部量过一遍，没有一个能同时满足吞吐和 CPU 两条线。具体是两类问题——
//!
//! 1. **窗口太小。**单流吞吐的硬上限是 `窗口 / RTT`。yamux 默认 256 KiB 在
//!    30 ms 上就是 8 MB/s 封顶，跟实现好坏无关。
//! 2. **每帧一次任务往返 + 忙等。**几个实现的 `poll_write`/`poll_read` 在通道满
//!    或空时写的是 `cx.waker().wake_by_ref(); Poll::Pending`——这会让执行器立刻
//!    重新轮询，把一个核烧满。实测每条流稳定吃掉约一个核。
//!
//! wsmux 针对这两点设计：4 MiB 默认窗口；出站是一块共享缓冲而非 channel，
//! 每帧一次拷贝、多流自动合并写；所有 `Pending` 路径都严格"先登记 waker 再复查
//! 条件"，没有任何一处自旋。
//!
//! 线格式见 `frame.rs`，性能设计的细节见 `shared.rs` 的模块注释。

pub mod frame;
pub mod session;
mod shared;
mod stream;

pub use session::{Config, Session};
pub use stream::Stream;
