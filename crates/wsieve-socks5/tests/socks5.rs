use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::{TcpListener, TcpStream};
use wsieve_proto::addr::{AddrPort, TargetAddr};
use wsieve_socks5::serve;

type Handler = Arc<dyn Fn(AddrPort) -> futures::future::BoxFuture<'static, std::io::Result<DuplexStream>> + Send + Sync>;

async fn spawn_server(handler: Handler) -> std::io::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    let h = handler.clone();
    tokio::spawn(async move {
        let _ = serve(listener, move |a| h(a)).await;
    });
    Ok(addr)
}

fn echo_handler() -> Handler {
    Arc::new(|_a: AddrPort| {
        Box::pin(async {
            let (client, mut server) = tokio::io::duplex(4096);
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    match server.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if server.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            Ok(client)
        })
    })
}

fn fail_handler() -> Handler {
    Arc::new(|_a: AddrPort| {
        Box::pin(async { Err(std::io::Error::other("dial failed")) })
    })
}

fn record_handler(sink: Arc<Mutex<Vec<AddrPort>>>) -> Handler {
    Arc::new(move |a: AddrPort| {
        let sink = sink.clone();
        Box::pin(async move {
            sink.lock().unwrap().push(a);
            let (client, mut server) = tokio::io::duplex(64);
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                let _ = server.read(&mut buf).await;
            });
            Ok(client)
        })
    })
}

async fn handshake(addr: &str) -> std::io::Result<TcpStream> {
    let mut s = TcpStream::connect(addr).await?;
    s.write_all(&[0x05, 0x01, 0x00]).await?;
    Ok(s)
}

#[tokio::test]
async fn connect_and_relay() {
    let addr = spawn_server(echo_handler()).await.unwrap();
    let mut s = TcpStream::connect(&addr).await.unwrap();
    s.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
    let mut m = [0u8; 2];
    s.read_exact(&mut m).await.unwrap();
    assert_eq!(&m, &[0x05, 0x00]);

    // CONNECT example.com:80 (atyp 3)
    let mut req = vec![0x05, 0x01, 0x00, 0x03, 11];
    req.extend_from_slice(b"example.com");
    req.extend_from_slice(&80u16.to_be_bytes());
    s.write_all(&req).await.unwrap();

    let mut reply = [0u8; 10];
    s.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply[..4], &[0x05, 0x00, 0x00, 0x01]);
    assert_eq!(&reply[4..], &[0, 0, 0, 0, 0, 0]);

    s.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    s.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");
}

#[tokio::test]
async fn unsupported_command_rejected() {
    let addr = spawn_server(echo_handler()).await.unwrap();
    let mut s = handshake(&addr).await.unwrap();
    let mut m = [0u8; 2];
    s.read_exact(&mut m).await.unwrap();

    // BIND (0x02) to 1.2.3.4:80
    let req = [0x05, 0x02, 0x00, 0x01, 1, 2, 3, 4, 0, 80];
    s.write_all(&req).await.unwrap();
    let mut reply = [0u8; 10];
    s.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[1], 0x07);
    // connection closed: subsequent read returns 0
    assert_closed(&mut s).await;
}

#[tokio::test]
async fn handler_failure_gets_0x01() {
    let addr = spawn_server(fail_handler()).await.unwrap();
    let mut s = handshake(&addr).await.unwrap();
    let mut m = [0u8; 2];
    s.read_exact(&mut m).await.unwrap();

    let req = [0x05, 0x01, 0x00, 0x01, 1, 2, 3, 4, 0, 80];
    s.write_all(&req).await.unwrap();
    let mut reply = [0u8; 10];
    s.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[1], 0x01);
    assert_closed(&mut s).await;
}

#[tokio::test]
async fn domain_and_ipv6_atyp_parsed() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let addr = spawn_server(record_handler(seen.clone())).await.unwrap();
    let mut s = handshake(&addr).await.unwrap();
    let mut m = [0u8; 2];
    s.read_exact(&mut m).await.unwrap();

    // domain
    let mut req = vec![0x05, 0x01, 0x00, 0x03, 7];
    req.extend_from_slice(b"foo.bar");
    req.extend_from_slice(&443u16.to_be_bytes());
    s.write_all(&req).await.unwrap();
    let mut reply = [0u8; 10];
    s.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[1], 0x00);

    // 第二个 CONNECT 走新 TCP 连接（RFC 1928：一条连接一个 CONNECT）
    drop(s);
    let mut s = handshake(&addr).await.unwrap();
    let mut m = [0u8; 2];
    s.read_exact(&mut m).await.unwrap();
    let mut req = vec![0x05, 0x01, 0x00, 0x04];
    req.extend_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    req.extend_from_slice(&8080u16.to_be_bytes());
    s.write_all(&req).await.unwrap();
    s.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[1], 0x00);

    let seen = seen.lock().unwrap();
    assert_eq!(
        seen[0],
        AddrPort { addr: TargetAddr::Domain("foo.bar".into()), port: 443 }
    );
    let mut v6 = [0u8; 16];
    v6.copy_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    assert_eq!(seen[1], AddrPort { addr: TargetAddr::V6(v6), port: 8080 });
}

#[tokio::test]
async fn bad_greeting_closed() {
    let addr = spawn_server(echo_handler()).await.unwrap();
    let mut s = TcpStream::connect(&addr).await.unwrap();
    s.write_all(&[0x04, 0x01, 0x00]).await.unwrap();
    assert_closed(&mut s).await;
}

/// 连接已关闭：EOF（0）或对端直接 reset 均算。
async fn assert_closed(s: &mut TcpStream) {
    let mut buf = [0u8; 16];
    match s.read(&mut buf).await {
        Ok(0) => {}
        Ok(_) => panic!("expected close, got data"),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        Err(e) => panic!("unexpected error: {e}"),
    }
}
