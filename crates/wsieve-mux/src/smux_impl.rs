//! smux (iberryful) 适配器。
//!
//! `Session::client/server` 自带后台任务，`open_stream`/`accept_stream` 均为 `&self`，
//! 是最薄的一个适配。transport 约束要求 `Send + Sync + Unpin + 'static`，
//! 由 `Duplex` trait 的 Sync 超 trait 满足。

use anyhow::Result;

use crate::{Mux, MuxStream};

pub struct SmuxImpl {
    session: smux::Session,
}

impl SmuxImpl {
    pub async fn new(io: MuxStream, server: bool) -> Result<Self> {
        // 回环测试拉长 keepalive，避免空闲 ping 噪音
        let config = smux::Config {
            keep_alive_interval: std::time::Duration::from_secs(3600),
            ..Default::default()
        };
        let session = if server {
            smux::Session::server(io, config).await?
        } else {
            smux::Session::client(io, config).await?
        };
        Ok(Self { session })
    }
}

#[async_trait::async_trait]
impl Mux for SmuxImpl {
    async fn open(&self) -> Result<MuxStream> {
        Ok(Box::new(self.session.open_stream().await?))
    }

    async fn accept(&self) -> Result<MuxStream> {
        Ok(Box::new(self.session.accept_stream().await?))
    }
}
