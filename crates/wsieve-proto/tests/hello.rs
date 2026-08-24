use wsieve_proto::hello::*;

#[test]
fn msg1_roundtrip() {
    let prefs = vec![MuxId::Yamux, MuxId::Smux];
    let gid = 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210u128;
    let bytes = encode_msg1(1_000, gid, &prefs);
    let m1 = decode_msg1(&bytes).unwrap();
    assert_eq!(m1.version, 1);
    assert_eq!(m1.ts_ms, 1_000);
    assert_eq!(m1.group_id, gid);
    assert_eq!(m1.mux_prefs, prefs);
    // 严格布局：1 + 8 + 16 + 1 + n
    assert_eq!(bytes.len(), 26 + prefs.len());
    assert_eq!(&bytes[9..25], &gid.to_be_bytes());
}

/// group_id 极值（全 0 / 全 1）必须无损往返——0 是合法组 id，不是哨兵。
#[test]
fn msg1_group_id_edge_values() {
    for gid in [0u128, u128::MAX] {
        let bytes = encode_msg1(7, gid, &[MuxId::H2mux]);
        assert_eq!(decode_msg1(&bytes).unwrap().group_id, gid);
    }
}

/// 不同 group_id 编码出的字节必然不同（组区分度的最低保证）。
#[test]
fn msg1_group_id_distinguishes() {
    let a = encode_msg1(0, 1, &[MuxId::Yamux]);
    let b = encode_msg1(0, 2, &[MuxId::Yamux]);
    assert_ne!(a, b);
    assert_ne!(
        decode_msg1(&a).unwrap().group_id,
        decode_msg1(&b).unwrap().group_id
    );
}

#[test]
fn msg1_rejects_empty_mux_list() {
    assert!(decode_msg1(&encode_msg1(0, 0, &[])).is_err());
}

#[test]
fn msg1_rejects_bad_version() {
    let mut bytes = encode_msg1(0, 9, &[MuxId::Yamux]);
    bytes[0] = 2;
    assert!(decode_msg1(&bytes).is_err());
}

#[test]
fn msg1_rejects_wrong_length() {
    // mux_count says 2 but only 1 mux_id follows
    let mut bytes = encode_msg1(0, 9, &[MuxId::Yamux]);
    bytes[25] = 2;
    assert!(decode_msg1(&bytes).is_err());
    // trailing garbage
    let mut bytes = encode_msg1(0, 9, &[MuxId::Yamux]);
    bytes.push(0x01);
    assert!(decode_msg1(&bytes).is_err());
    // 头部被截断（旧 10 字节布局的长度已不再合法）
    let bytes = encode_msg1(0, 9, &[MuxId::Yamux]);
    assert!(decode_msg1(&bytes[..10]).is_err());
}

#[test]
fn msg1_rejects_bad_mux_id() {
    let mut bytes = encode_msg1(0, 9, &[MuxId::Yamux]);
    bytes[26] = 0x06;
    assert!(decode_msg1(&bytes).is_err());
}

#[test]
fn msg2_roundtrip() {
    for fallback in [false, true] {
        let m2 = Msg2 {
            chosen_mux_id: MuxId::Picomux,
            fallback,
        };
        let bytes = encode_msg2(&m2);
        assert_eq!(decode_msg2(&bytes).unwrap(), m2);
    }
}

#[test]
fn msg2_rejects_bad_mux_id() {
    assert!(decode_msg2(&[0x00, 0x00]).is_err());
    assert!(decode_msg2(&[0x06, 0x00]).is_err());
    assert!(decode_msg2(&[0x01]).is_err()); // wrong length
}

#[test]
fn ts_window() {
    let now = 10_000_000;
    assert!(ts_in_window(now, now));
    assert!(ts_in_window(now - 300_000, now));
    assert!(ts_in_window(now + 300_000, now));
    assert!(!ts_in_window(now - 300_001, now));
    assert!(!ts_in_window(now + 300_001, now));
}
