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
