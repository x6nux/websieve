//! 多路复用层（spec §7.1-§7.3）：`Mux` trait + 本项目自己的实现 `wsmux`。
//!
//! 这里曾经并列挂着五个三方 crate 的适配层（yamux / smux / muxado / picomux /
//! h2mux）。它们全部被实测淘汰了——原因记在 `wsmux/mod.rs` 的模块注释里。
//!
//! `Mux` 必须是 object-safe（运行时协商决定用哪种 mux）。

pub mod stripe_runtime;
pub mod wsmux;

pub use wsieve_proto::hello::MuxId;

/// 任何满足 tokio AsyncRead + AsyncWrite 的流。
pub type MuxStream = Box<dyn Duplex + Send + Unpin>;

pub trait Duplex: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Sync + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Sync + Unpin> Duplex for T {}

/// 多路复用会话：open 发起子流，accept 接收子流。
#[async_trait::async_trait]
pub trait Mux: Send + Sync {
    async fn open(&self) -> anyhow::Result<MuxStream>;
    async fn accept(&self) -> anyhow::Result<MuxStream>;

    /// 把本端接收窗口扩大到 `target`（只增不减）。
    ///
    /// 默认空实现：窗口是流控细节，不是每种 mux 都能在运行期调。调用方
    /// （自适应流控）只管按实测 BDP 提要求，支持的实现照做，不支持的忽略。
    ///
    /// 之所以挂在 trait 上而不是让调用方持有具体类型：`mux_factory` 按协商
    /// 结果返回 `Box<dyn Mux>`，类型在那一步就被擦掉了。
    fn grow_window(&self, _target: u32) {}
}

/// 客户端角色工厂。
pub async fn mux_factory(id: MuxId, io: MuxStream) -> anyhow::Result<Box<dyn Mux>> {
    match id {
        MuxId::Wsmux => Ok(Box::new(wsmux::Session::new(io, false))),
    }
}

/// 服务端角色工厂。wsmux 两端对称，只有 `is_server` 决定 sid 的奇偶分配。
pub async fn mux_server_factory(id: MuxId, io: MuxStream) -> anyhow::Result<Box<dyn Mux>> {
    match id {
        MuxId::Wsmux => Ok(Box::new(wsmux::Session::new(io, true))),
    }
}
