//! muxado 适配器。
//!
//! `SessionBuilder::new(io).client()/.server().start()` 构造 `MuxadoSession`，
//! crate 自带的 `Accept`/`OpenClose` trait 方法是 `&mut self`，用 tokio Mutex
//! 串行化以满足我们的 `&self` 接口。
//!
//! ## muxado 的懒 SYN 问题
//!
//! muxado 只有在本地流**首次 poll_write 非空数据**时才发送 SYN 帧
//! （`stream.rs` 里 `needs_syn` 在 `poll_write` 中消费）。因此对端
//! `accept()` 会一直挂到本端写出第一个字节，这与我们 "open() 之后流即存在"
//! 的 Mux 语义不符。解决办法：open() 时写一个 sentinel 字节把 SYN 立刻
//! 冲出去；accept() 时读掉这个 sentinel 再把流交给上层。
//! 由于本协议两端都经过本适配器，sentinel 不会泄露给真实数据。

use anyhow::Result;
use muxado::{Accept, OpenClose};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::{Mux, MuxStream};

/// open 时用于触发 SYN 的哨兵字节。
const SYN_SENTINEL: u8 = 0;

pub struct MuxadoImpl {
    session: Mutex<muxado::MuxadoSession>,
}

impl MuxadoImpl {
    pub fn new(io: MuxStream, server: bool) -> Result<Self> {
        let builder = muxado::SessionBuilder::new(io);
        let builder = if server {
            builder.server()
        } else {
            builder.client()
        };
        let session = builder.start();
        Ok(Self {
            session: Mutex::new(session),
        })
    }
}

#[async_trait::async_trait]
impl Mux for MuxadoImpl {
    async fn open(&self) -> Result<MuxStream> {
        let mut stream = self.session.lock().await.open().await?;
        // 冲一个 sentinel 字节，让 SYN 立刻上 wire（对端 accept 不用等真实数据）
        stream.write_all(&[SYN_SENTINEL]).await?;
        stream.flush().await?;
        Ok(Box::new(stream))
    }

    async fn accept(&self) -> Result<MuxStream> {
        let mut stream = self
            .session
            .lock()
            .await
            .accept()
            .await
            .ok_or_else(|| anyhow::anyhow!("muxado session closed"))?;
        // 对端 open() 写了 sentinel 来触发 SYN：读掉它
        let mut buf = [0u8; 1];
        stream.read_exact(&mut buf).await?;
        Ok(Box::new(stream))
    }
}
