//! picomux 适配器。
//!
//! `PicoMux::new(read_half, write_half)` 接受拆分的两半（`AsyncRead` / `AsyncWrite`
//! 分开传），内部自己 spawn 任务（geph5-rt，tokio 后端）。`open(metadata)` /
//! `accept()` 均为 `&self`，天然对称，无角色之分。
//!
//! 拆分用 `tokio::io::split`（内部 BiLock，安全）。
//! `Box<dyn Duplex>` 因 trait 对象自动实现超 trait 而满足 tokio IO traits。

use anyhow::Result;

use crate::{Mux, MuxStream};

pub struct PicomuxImpl {
    mux: picomux::PicoMux,
}

impl PicomuxImpl {
    pub fn new(io: MuxStream) -> Result<Self> {
        let (read, write) = tokio::io::split(io);
        let mux = picomux::PicoMux::new(read, write);
        Ok(Self { mux })
    }
}

#[async_trait::async_trait]
impl Mux for PicomuxImpl {
    async fn open(&self) -> Result<MuxStream> {
        Ok(Box::new(self.mux.open(b"").await?))
    }

    async fn accept(&self) -> Result<MuxStream> {
        Ok(Box::new(self.mux.accept().await?))
    }
}
