use wsieve_proto::hello::*;

#[test]
fn msg1_roundtrip() {
    let prefs = vec![MuxId::Yamux, MuxId::Smux];
    let bytes = encode_msg1(1_000, &prefs);
    let m1 = decode_msg1(&bytes).unwrap();
    assert_eq!(m1.version, 1);
    assert_eq!(m1.ts_ms, 1_000);
    assert_eq!(m1.mux_prefs, prefs);
}

#[test]
fn msg1_rejects_empty_mux_list() {
    assert!(decode_msg1(&encode_msg1(0, &[])).is_err());
}

#[test]
fn msg1_rejects_bad_version() {
    let mut bytes = encode_msg1(0, &[MuxId::Yamux]);
    bytes[0] = 2;
    assert!(decode_msg1(&bytes).is_err());
}

#[test]
fn msg1_rejects_wrong_length() {
    // mux_count says 2 but only 1 mux_id follows
    let bytes = [1u8, 0, 0, 0, 0, 0, 0x0f, 0x42, 0x02, 0x01];
    assert!(decode_msg1(&bytes).is_err());
    // trailing garbage
    let mut bytes = encode_msg1(0, &[MuxId::Yamux]);
    bytes.push(0x01);
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
