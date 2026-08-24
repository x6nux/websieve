//! 多路复用层（spec §7.1-§7.3）：`Mux` trait + 四个三方 crate 的薄适配 + h2mux 适配器。
//! 注意：muxado 适配器含 sentinel 字节 workaround（懒 SYN），仅限本系统
//! factory 配对使用，不可与第三方 raw muxado 端点互通。
//!
//! `Mux` 必须是 object-safe（运行时协商决定用哪种 mux）。

pub mod h2mux_impl;
pub mod muxado_impl;
pub mod picomux_impl;
pub mod smux_impl;
pub mod stripe_runtime;
pub mod yamux_impl;

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

/// 客户端角色工厂（h2mux 在 Task 10 加入）。
pub async fn mux_factory(id: MuxId, io: MuxStream) -> anyhow::Result<Box<dyn Mux>> {
    match id {
        MuxId::Yamux => Ok(Box::new(yamux_impl::TokioYamux::new(io, false)?)),
        MuxId::Smux => Ok(Box::new(smux_impl::SmuxImpl::new(io, false).await?)),
        MuxId::Muxado => Ok(Box::new(muxado_impl::MuxadoImpl::new(io, false)?)),
        MuxId::Picomux => Ok(Box::new(picomux_impl::PicomuxImpl::new(io)?)),
        MuxId::H2mux => Ok(Box::new(h2mux_impl::H2ClientImpl::new(io).await?)),
    }
}

/// 服务端角色工厂。五个 crate：前四个为对称双端 API，h2mux 客户端/服务端各有专用半，与客户端工厂一一对应。
pub async fn mux_server_factory(id: MuxId, io: MuxStream) -> anyhow::Result<Box<dyn Mux>> {
    match id {
        MuxId::Yamux => Ok(Box::new(yamux_impl::TokioYamux::new(io, true)?)),
        MuxId::Smux => Ok(Box::new(smux_impl::SmuxImpl::new(io, true).await?)),
        MuxId::Muxado => Ok(Box::new(muxado_impl::MuxadoImpl::new(io, true)?)),
        MuxId::Picomux => Ok(Box::new(picomux_impl::PicomuxImpl::new(io)?)),
        MuxId::H2mux => Ok(Box::new(h2mux_impl::H2ServerImpl::new(io).await?)),
    }
}
