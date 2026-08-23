//! tokio-yamux 适配器。
//!
//! `Session` 是 `futures::Stream<Item = Result<StreamHandle>>`，只有被 poll 才会处理帧，
//! 因此需要一个常驻驱动任务。`Control` 是克隆出来的控制句柄，持有 `open_stream()`。
//! 服务端 accept 由驱动任务把到来的 `StreamHandle` 推进 channel。

use anyhow::Result;
use tokio::sync::{mpsc, Mutex};

use crate::{Mux, MuxStream};

pub struct TokioYamux {
    control: tokio_yamux::Control,
    accepted: Mutex<mpsc::Receiver<tokio_yamux::StreamHandle>>,
    /// 持有驱动任务；Session 逻辑上归该任务所有。
    _driver: tokio::task::JoinHandle<()>,
}

impl TokioYamux {
    pub fn new(io: MuxStream, server: bool) -> Result<Self> {
        // 纯内存回环不需要 keepalive
        let config = tokio_yamux::Config {
            enable_keepalive: false,
            ..Default::default()
        };
        let session = if server {
            tokio_yamux::Session::new_server(io, config)
        } else {
            tokio_yamux::Session::new_client(io, config)
        };
        let control = session.control();
        let (tx, rx) = mpsc::channel(64);
        let driver = tokio::spawn(async move {
            use futures::StreamExt;
            let mut session = session;
            while let Some(item) = session.next().await {
                match item {
                    Ok(handle) => {
                        if tx.send(handle).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            control,
            accepted: Mutex::new(rx),
            _driver: driver,
        })
    }
}

#[async_trait::async_trait]
impl Mux for TokioYamux {
    async fn open(&self) -> Result<MuxStream> {
        let mut control = self.control.clone();
        let handle = control.open_stream().await?;
        Ok(Box::new(handle))
    }

    async fn accept(&self) -> Result<MuxStream> {
        let handle = self
            .accepted
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("yamux session driver terminated"))?;
        Ok(Box::new(handle))
    }
}
