//! 目标地址编解码（spec §7.4）：每条 mux 子流首个 Frame::Data 携带
//! SOCKS5 地址格式：`u8 atyp`（0x01 IPv4 / 0x03 域名 / 0x04 IPv6）+ 地址 + `u16 be port`。

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetAddr {
    V4([u8; 4]),
    Domain(String),
    V6([u8; 16]),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddrPort {
    pub addr: TargetAddr,
    pub port: u16,
}

#[derive(Debug, thiserror::Error)]
pub enum AddrError {
    #[error("buffer too short: need {need}, have {have}")]
    Truncated { need: usize, have: usize },
    #[error("bad atyp: {0:#04x}")]
    BadAtyp(u8),
    #[error("bad domain length: {0}")]
    BadDomainLen(u8),
    #[error("domain is not valid UTF-8")]
    BadDomainUtf8,
}

impl AddrPort {
    /// 形如 `example.com:443` / `1.2.3.4:80` 的展示串（用于日志/错误信息）。
    pub fn display(&self) -> String {
        match &self.addr {
            TargetAddr::V4(o) => format!("{}.{}.{}.{}:{}", o[0], o[1], o[2], o[3], self.port),
            TargetAddr::Domain(d) => format!("{}:{}", d, self.port),
            TargetAddr::V6(a) => {
                use std::fmt::Write;
                let mut s = String::new();
                for (i, seg) in a.chunks(2).enumerate() {
                    if i > 0 {
                        s.push(':');
                    }
                    let _ = write!(s, "{:02x}{:02x}", seg[0], seg[1]);
                }
                format!("[{}]:{}", s, self.port)
            }
        }
    }
}

/// 编码为 SOCKS5 地址格式字节。
pub fn encode_addr(a: &AddrPort) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 255 + 2);
    match &a.addr {
        TargetAddr::V4(o) => {
            out.push(0x01);
            out.extend_from_slice(o);
        }
        TargetAddr::Domain(d) => {
            out.push(0x03);
            out.push(d.len() as u8);
            out.extend_from_slice(d.as_bytes());
        }
        TargetAddr::V6(a) => {
            out.push(0x04);
            out.extend_from_slice(a);
        }
    }
    out.extend_from_slice(&a.port.to_be_bytes());
    out
}

/// 解码。返回地址与消耗的字节数；尾部多余字节（同一 frame 里的后续数据）被忽略但通过
/// `consumed` 上报，调用方自行切片。
pub fn decode_addr(b: &[u8]) -> Result<(AddrPort, usize), AddrError> {
    let need = |have: usize, need: usize| -> AddrError {
        AddrError::Truncated { need, have }
    };
    if b.is_empty() {
        return Err(need(0, 1));
    }
    let mut i = 1usize;
    let addr = match b[0] {
        0x01 => {
            if b.len() < i + 4 {
                return Err(need(b.len(), i + 4));
            }
            let mut o = [0u8; 4];
            o.copy_from_slice(&b[i..i + 4]);
            i += 4;
            TargetAddr::V4(o)
        }
        0x03 => {
            if b.len() < i + 1 {
                return Err(need(b.len(), i + 1));
            }
            let dlen = b[i] as usize;
            i += 1;
            if dlen == 0 {
                return Err(AddrError::BadDomainLen(0));
            }
            if b.len() < i + dlen {
                return Err(need(b.len(), i + dlen));
            }
            let s = std::str::from_utf8(&b[i..i + dlen]).map_err(|_| AddrError::BadDomainUtf8)?;
            i += dlen;
            TargetAddr::Domain(s.to_owned())
        }
        0x04 => {
            if b.len() < i + 16 {
                return Err(need(b.len(), i + 16));
            }
            let mut a = [0u8; 16];
            a.copy_from_slice(&b[i..i + 16]);
            i += 16;
            TargetAddr::V6(a)
        }
        other => return Err(AddrError::BadAtyp(other)),
    };
    if b.len() < i + 2 {
        return Err(need(b.len(), i + 2));
    }
    let port = u16::from_be_bytes([b[i], b[i + 1]]);
    i += 2;
    Ok((AddrPort { addr, port }, i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_v4() {
        let a = AddrPort { addr: TargetAddr::V4([1, 2, 3, 4]), port: 80 };
        let b = encode_addr(&a);
        assert_eq!(&b, &[0x01, 1, 2, 3, 4, 0, 80]);
        let (d, n) = decode_addr(&b).unwrap();
        assert_eq!(d, a);
        assert_eq!(n, 7);
    }

    #[test]
    fn roundtrip_domain() {
        let a = AddrPort { addr: TargetAddr::Domain("example.com".into()), port: 443 };
        let (d, n) = decode_addr(&encode_addr(&a)).unwrap();
        assert_eq!(d, a);
        assert_eq!(n, 1 + 1 + 11 + 2);
    }

    #[test]
    fn roundtrip_v6() {
        let a = AddrPort { addr: TargetAddr::V6([0x20, 0x01, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]), port: 22 };
        let (d, n) = decode_addr(&encode_addr(&a)).unwrap();
        assert_eq!(d, a);
        assert_eq!(n, 19);
    }

    #[test]
    fn rejects_bad_atyp() {
        assert!(matches!(decode_addr(&[0x02, 1, 2, 3, 4, 0, 80]), Err(AddrError::BadAtyp(0x02))));
    }

    #[test]
    fn rejects_truncated() {
        assert!(decode_addr(&[]).is_err());
        assert!(decode_addr(&[0x01, 1]).is_err());
        assert!(decode_addr(&[0x03, 5, b'a']).is_err());
        assert!(decode_addr(&vec![0x04].iter().copied().chain(std::iter::repeat(0u8).take(15)).collect::<Vec<u8>>()[..]).is_err());
        assert!(decode_addr(&[0x01, 1, 2, 3, 4, 0]).is_err()); // 缺 port 低字节
    }

    #[test]
    fn trailing_bytes_reported_in_consumed() {
        let mut b = encode_addr(&AddrPort { addr: TargetAddr::V4([9, 9, 9, 9]), port: 53 });
        b.extend_from_slice(b"extra payload");
        let (d, n) = decode_addr(&b).unwrap();
        assert_eq!(d, AddrPort { addr: TargetAddr::V4([9, 9, 9, 9]), port: 53 });
        assert_eq!(n, 7);
        assert_eq!(&b[n..], b"extra payload");
    }
}
