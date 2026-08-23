//! XhttpConn 客户端测试。

use std::sync::Arc;
use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt};
use tokio::time::Duration;

use wsieve_transport::{HttpTransport, PostReply};
use wsieve_proto::crypto::{gen_keypair, build_client, build_server};
use wsieve_proto::hello::{MuxId, encode_msg1, decode_msg2, encode_msg2};
use wsieve_proto::tu::Frame;
use wsieve_xhttp::client::{XhttpConn, UpstreamCfg};

struct FakeTransport {
    handshake_handler: std::sync::Mutex<Box<dyn Fn(&[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> + Send>>,
    stream_handler: std::sync::Mutex<Box<dyn Fn() -> BoxStream<'static, Result<Bytes, anyhow::Error>> + Send>>,
}

impl FakeTransport {
    fn new() -> Self {
        Self {
            handshake_handler: std::sync::Mutex::new(Box::new(|_body| {
                // 默认返回错误
                Err("no handler".into())
            })),
            stream_handler: std::sync::Mutex::new(Box::new(|| {
                Box::pin(futures::stream::empty())
            })),
        }
    }

    fn set_handshake<F>(&self, f: F)
    where
        F: Fn(&[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> + Send + 'static,
    {
        *self.handshake_handler.lock().unwrap() = Box::new(f);
    }

    fn set_stream<F>(&self, f: F)
    where
        F: Fn() -> BoxStream<'static, Result<Bytes, anyhow::Error>> + Send + 'static,
    {
        *self.stream_handler.lock().unwrap() = Box::new(f);
    }
}

#[async_trait::async_trait]
impl HttpTransport for FakeTransport {
    async fn post(&self, path: &str, body: Bytes) -> anyhow::Result<PostReply> {
        let handler = self.handshake_handler.lock().unwrap();
        let result = handler(&body);

        match result {
            Ok(msg2_tu) => {
                // 编码为 TU
                let mut reply_body = vec![0u8; 2 + msg2_tu.len()];
                reply_body[..2].copy_from_slice(&(msg2_tu.len() as u16).to_be_bytes());
                reply_body[2..].copy_from_slice(&msg2_tu);

                Ok(PostReply { status: 200, body: Bytes::from(reply_body) })
            }
            Err(_) => Ok(PostReply { status: 404, body: Bytes::from("not found") }),
        }
    }

    async fn get_stream(&self, _path: &str) -> anyhow::Result<BoxStream<'static, anyhow::Result<Bytes>>> {
        let handler = self.stream_handler.lock().unwrap();
        Ok(handler())
    }
}

#[tokio::test]
async fn handshake_success() {
    let (server_priv, server_pub) = gen_keypair();
    let (client_priv, _client_pub) = gen_keypair();

    let transport = Arc::new(FakeTransport::new());

    // 设置握手处理器
    let server_pub_clone = server_pub;
    let server_priv_clone = server_priv;
    transport.set_handshake(move |msg1_tu| {
        // 解析 TU
        if msg1_tu.len() < 2 {
            return Err("TU too short".into());
        }
        let msg1_len = u16::from_be_bytes([msg1_tu[0], msg1_tu[1]]) as usize;
        if msg1_tu.len() < 2 + msg1_len {
            return Err("TU truncated".into());
        }
        let msg1_cipher = &msg1_tu[2..2 + msg1_len];

        // 服务端处理
        let mut server = build_server(&server_priv_clone)?;
        let mut payload_buf = vec![0u8; 65535];
        server.read_message(msg1_cipher, &mut payload_buf)?;

        // 生成 msg2
        let msg2 = encode_msg2(&wsieve_proto::hello::Msg2 {
            chosen_mux_id: MuxId::Yamux,
            fallback: false,
        });

        let mut msg2_buf = vec![0u8; 65535];
        let msg2_len = server.write_message(&msg2, &mut msg2_buf)?;

        Ok(msg2_buf[..msg2_len].to_vec())
    });

    // 设置空流
    transport.set_stream(|| Box::pin(futures::stream::empty()));

    let cfg = UpstreamCfg {
        server_pub,
        client_priv,
        mux_prefs: vec![MuxId::Yamux],
    };

    let (conn, negotiated) = XhttpConn::connect(transport, &cfg).await.unwrap();

    assert_eq!(negotiated.mux_id, MuxId::Yamux);
    assert_eq!(negotiated.fallback, false);

    drop(conn);
}

#[tokio::test]
async fn handshake_garbage_reply_kills() {
    let (_server_priv, server_pub) = gen_keypair();
    let (client_priv, _client_pub) = gen_keypair();

    let transport = Arc::new(FakeTransport::new());

    // 设置返回错误的处理器
    transport.set_handshake(|_| Err("bad handshake".into()));

    let cfg = UpstreamCfg {
        server_pub,
        client_priv,
        mux_prefs: vec![MuxId::Yamux],
    };

    let result = XhttpConn::connect(transport, &cfg).await;
    assert!(result.is_err());
}
