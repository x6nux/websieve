//! 首字节协议嗅探。
//!
//! 关键是用 `peek()` 而非 `read()` —— peek 不消耗数据，
//! 于是分派之后，真正的处理器还能读到完整的协议首字节，
//! 不需要把「已经读掉的那一段」再拼回去。
//!
//! 判决只看**第一个字节**，这是刻意的：SOCKS5 首字节是版本号 0x05，
//! HTTP 首字节是方法名的首字母（一律 ASCII 大写）。一个字节就足以分家，
//! 于是嗅探永远不会遇到「要 4 个字节却只到了 1 个」的短读困境 ——
//! 那类困境被结构性地消掉，而不是靠循环去兜。
//! 反过来说，若改成看多个字节，判决就会随 TCP 分段时机而变（同一份
//! 输入，字节一次到齐与分两次到齐可能得到不同结论），那种不确定性
//! 比多几个字节的校验强度更值得避免。

use std::time::Duration;

use tokio::net::TcpStream;

/// 嗅探等待首字节的默认上限。
///
/// 有些客户端连上来就干等（连接池预热、扫描器、半开的 NAT 残留）。
/// 没有上限的话，每一条这样的连接都会永久占住一个 task 与一个 fd，
/// 且完全不可见 —— 这是最典型的慢速资源泄漏。
pub const DEFAULT_SNIFF_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Socks5,
    Http,
}

#[derive(Debug, thiserror::Error)]
pub enum SniffError {
    #[error("读取首字节失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("连接未发送任何数据即关闭")]
    Empty,
    #[error("等待首字节超时（{0:?}）")]
    Timeout(Duration),
    #[error("SOCKS4 不受支持，请使用 SOCKS5")]
    UnsupportedSocks4,
    #[error("无法识别的协议，首字节为 {0:#04x}")]
    Unknown(u8),
}

/// 嗅探协议，等待首字节最多 [`DEFAULT_SNIFF_TIMEOUT`]。
///
/// 默认就带超时是刻意的：让「忘了加超时」不再是一种可能的写法。
/// 需要别的上限时用 [`sniff_within`]。
pub async fn sniff(stream: &TcpStream) -> Result<Protocol, SniffError> {
    sniff_within(stream, DEFAULT_SNIFF_TIMEOUT).await
}

/// 嗅探协议，自定义等待首字节的上限。
pub async fn sniff_within(stream: &TcpStream, within: Duration) -> Result<Protocol, SniffError> {
    let mut b = [0u8; 1];
    // peek 在至少有 1 字节可读时才返回，因此这里不存在「读到一半」的情形：
    // n 只可能是 1（有数据）或 0（对端已关闭写端）。
    let n = match tokio::time::timeout(within, stream.peek(&mut b)).await {
        Ok(r) => r?,
        Err(_) => return Err(SniffError::Timeout(within)),
    };
    if n == 0 {
        return Err(SniffError::Empty);
    }
    classify(b[0])
}

/// 由首字节判定协议族。单独抽出来便于直接对全部字节取值做穷举测试。
fn classify(first: u8) -> Result<Protocol, SniffError> {
    match first {
        0x05 => Ok(Protocol::Socks5),
        0x04 => Err(SniffError::UnsupportedSocks4),
        // HTTP 方法名一律是 ASCII 大写字母开头：GET / POST / CONNECT / PUT / …
        c if c.is_ascii_uppercase() => Ok(Protocol::Http),
        other => Err(SniffError::Unknown(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::{TcpListener, TcpStream};

    /// 起一个监听器，把 `payload` 写进去，返回服务端侧的连接。
    async fn conn_with(payload: &[u8]) -> TcpStream {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let payload = payload.to_vec();
        tokio::spawn(async move {
            let mut c = TcpStream::connect(addr).await.unwrap();
            c.write_all(&payload).await.unwrap();
            // 保持连接开着，否则 peek 可能读到 EOF
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        });
        l.accept().await.unwrap().0
    }

    #[tokio::test]
    async fn socks5_greeting_is_detected() {
        // SOCKS5 握手首字节是版本号 0x05
        let s = conn_with(&[0x05, 0x01, 0x00]).await;
        assert_eq!(sniff(&s).await.unwrap(), Protocol::Socks5);
    }

    #[tokio::test]
    async fn http_verbs_are_detected() {
        for verb in [
            "GET / HTTP/1.1\r\n",
            "CONNECT a.com:443 HTTP/1.1\r\n",
            "POST /x HTTP/1.1\r\n",
        ] {
            let s = conn_with(verb.as_bytes()).await;
            assert_eq!(sniff(&s).await.unwrap(), Protocol::Http, "verb: {verb}");
        }
    }

    #[tokio::test]
    async fn socks4_is_rejected_explicitly() {
        // 不静默当成 HTTP —— 那会产生一个莫名其妙的 400
        let s = conn_with(&[0x04, 0x01]).await;
        assert!(matches!(sniff(&s).await, Err(SniffError::UnsupportedSocks4)));
    }

    #[tokio::test]
    async fn garbage_is_rejected() {
        let s = conn_with(&[0xFF, 0xFE]).await;
        assert!(sniff(&s).await.is_err());
    }

    #[tokio::test]
    async fn peek_does_not_consume() {
        // 嗅探之后，真正的处理器必须还能读到完整的首字节
        let s = conn_with(&[0x05, 0x01, 0x00]).await;
        assert_eq!(sniff(&s).await.unwrap(), Protocol::Socks5);
        let mut buf = [0u8; 3];
        use tokio::io::AsyncReadExt;
        let mut s = s;
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, [0x05, 0x01, 0x00], "peek 不能消耗数据");
    }

    // —— 以下四组覆盖嗅探真正会出事的边界 ——

    /// 失效模式一：客户端连上来但一个字节都不发。
    /// 没有超时的话这条连接会永久占住一个 task 与一个 fd。
    #[tokio::test]
    async fn silent_client_times_out_instead_of_hanging() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        // 连上就不说话，且不关闭 —— 与「发完就走」是两种不同的情形
        let hold = tokio::spawn(async move {
            let c = TcpStream::connect(addr).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            drop(c);
        });
        let s = l.accept().await.unwrap().0;

        let started = std::time::Instant::now();
        let r = sniff_within(&s, Duration::from_millis(150)).await;
        assert!(
            matches!(r, Err(SniffError::Timeout(_))),
            "静默连接必须超时，实际：{r:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "超时必须真的生效，而不是一直等下去"
        );
        hold.abort();
    }

    /// 失效模式一之二：连上立刻关闭，一个字节都没有。
    /// 这条路径 peek 返回 0（EOF）而非超时，要报 Empty 而不是被当成某种协议。
    #[tokio::test]
    async fn immediate_eof_is_empty_not_a_protocol() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let c = TcpStream::connect(addr).await.unwrap();
            drop(c); // 什么都不发就关
        });
        let s = l.accept().await.unwrap().0;
        let r = sniff_within(&s, Duration::from_secs(5)).await;
        assert!(matches!(r, Err(SniffError::Empty)), "实际：{r:?}");
    }

    /// 失效模式二：首个 TCP 分段只带来一个字节。
    /// read() 返回 1 字节是正常现象而非错误；判决只依赖首字节，
    /// 因此逐字节到达与一次到齐必须得出同一结论。
    #[tokio::test]
    async fn one_byte_at_a_time_still_detected() {
        for (payload, want) in [
            (b"GET / HTTP/1.1\r\n".to_vec(), Protocol::Http),
            (vec![0x05, 0x01, 0x00], Protocol::Socks5),
        ] {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            let feeder = tokio::spawn(async move {
                let mut c = TcpStream::connect(addr).await.unwrap();
                for byte in payload {
                    c.write_all(&[byte]).await.unwrap();
                    c.flush().await.unwrap();
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            });
            let s = l.accept().await.unwrap().0;
            assert_eq!(sniff_within(&s, Duration::from_secs(5)).await.unwrap(), want);
            feeder.abort();
        }
    }

    /// 失效模式四：既非 SOCKS5 亦非 HTTP 的输入，要给出**具体**的错误，
    /// 不能含糊地成功（那会让下游报出一个与真实原因无关的错）。
    /// 这里穷举全部 256 个首字节取值，把分类规则钉死。
    #[test]
    fn every_first_byte_is_classified_deterministically() {
        for b in 0u8..=0xFF {
            match (b, classify(b)) {
                (0x05, Ok(Protocol::Socks5)) => {}
                (0x04, Err(SniffError::UnsupportedSocks4)) => {}
                (c, Ok(Protocol::Http)) if c.is_ascii_uppercase() => {}
                (c, Err(SniffError::Unknown(got))) if got == c => {
                    assert!(
                        !c.is_ascii_uppercase() && c != 0x04 && c != 0x05,
                        "{c:#04x} 不该落到 Unknown"
                    );
                }
                (c, other) => panic!("首字节 {c:#04x} 的分类出人意料：{other:?}"),
            }
        }
        // TLS（0x16）与 SOCKS4a（0x04）是最常被误投到代理口的两种流量，
        // 它们必须落在错误一侧而不是被当成 HTTP。
        assert!(matches!(classify(0x16), Err(SniffError::Unknown(0x16))));
        assert!(matches!(classify(0x04), Err(SniffError::UnsupportedSocks4)));
        // 小写字母不是合法的 HTTP 方法首字母（RFC 7231 方法名区分大小写）
        assert!(matches!(classify(b'g'), Err(SniffError::Unknown(_))));
    }
}
