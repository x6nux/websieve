//! TargetAddr 编解码（Task 12）集成测试：roundtrip + 拒绝 + consumed。

use wsieve_proto::addr::{decode_addr, encode_addr, AddrError, AddrPort, TargetAddr};

fn roundtrip(a: AddrPort) {
    let bytes = encode_addr(&a);
    let (decoded, consumed) = decode_addr(&bytes).expect("decode");
    assert_eq!(decoded, a);
    assert_eq!(consumed, bytes.len(), "consumed must equal encoded length");
}

#[test]
fn roundtrip_v4() {
    roundtrip(AddrPort { addr: TargetAddr::V4([192, 168, 1, 1]), port: 8443 });
}

#[test]
fn roundtrip_domain() {
    roundtrip(AddrPort { addr: TargetAddr::Domain("example.com".into()), port: 443 });
}

#[test]
fn roundtrip_v6() {
    roundtrip(AddrPort {
        addr: TargetAddr::V6([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
        port: 22,
    });
}

#[test]
fn rejects_bad_atyp() {
    // 0x02 不是合法 atyp（SOCKS5 保留）
    let e = decode_addr(&[0x02, 1, 2, 3, 4, 0, 80]).unwrap_err();
    assert!(matches!(e, AddrError::BadAtyp(0x02)));
}

#[test]
fn rejects_truncated() {
    assert!(decode_addr(&[]).is_err());
    assert!(decode_addr(&[0x01, 1, 2, 3]).is_err()); // IPv4 差 1 字节
    assert!(decode_addr(&[0x03, 5, b'a']).is_err()); // 域名长度不足
    assert!(decode_addr(&[0x04, 0u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_err()); // IPv6 差 1 字节
    assert!(decode_addr(&[0x01, 1, 2, 3, 4, 0]).is_err()); // 缺端口低字节
}

#[test]
fn trailing_bytes_reported_in_consumed() {
    // mux 流首个 Data frame 里地址后可跟后续 payload：解码忽略但上报 consumed
    let mut bytes = encode_addr(&AddrPort { addr: TargetAddr::V4([9, 9, 9, 9]), port: 53 });
    bytes.extend_from_slice(b"extra payload");
    let (decoded, consumed) = decode_addr(&bytes).unwrap();
    assert_eq!(decoded, AddrPort { addr: TargetAddr::V4([9, 9, 9, 9]), port: 53 });
    assert_eq!(consumed, 7);
    assert_eq!(&bytes[consumed..], b"extra payload");
}
