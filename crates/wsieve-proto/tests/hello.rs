use wsieve_proto::hello::*;

#[test]
fn msg1_roundtrip() {
    let prefs = vec![MuxId::Wsmux, MuxId::Wsmux];
    let gid = 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210u128;
    let bytes = encode_msg1(1_000, gid, &prefs, IpStrategy::Auto);
    let m1 = decode_msg1(&bytes).unwrap();
    assert_eq!(m1.version, 2);
    assert_eq!(m1.ts_ms, 1_000);
    assert_eq!(m1.group_id, gid);
    assert_eq!(m1.mux_prefs, prefs);
    assert_eq!(m1.ip_strategy, IpStrategy::Auto);
    // 严格布局：1 + 8 + 16 + 1 + n + 1(ip_strategy)
    assert_eq!(bytes.len(), 27 + prefs.len());
    assert_eq!(&bytes[9..25], &gid.to_be_bytes());
}

/// 四个策略都要能无损往返，且**编码出的字节互不相同**——线上值一旦写错，
/// 服务端会按另一个地址族出网，而两端日志都显示自己正常。
#[test]
fn msg1_ip_strategy_roundtrips_and_is_distinguishable() {
    let all = [
        IpStrategy::Auto,
        IpStrategy::V4Only,
        IpStrategy::V6Only,
        IpStrategy::PreferV4,
    ];
    let mut seen = std::collections::HashSet::new();
    for s in all {
        let bytes = encode_msg1(1, 2, &[MuxId::Wsmux], s);
        assert_eq!(decode_msg1(&bytes).unwrap().ip_strategy, s);
        assert!(seen.insert(bytes), "两个策略编出了相同的字节：{s:?}");
    }
}

/// 未知策略值必须报错，不能悄悄当成 Auto——那会让用户以为自己配的
/// v4-only 生效了，而流量照旧走 IPv6。
#[test]
fn msg1_rejects_unknown_ip_strategy() {
    let mut bytes = encode_msg1(0, 9, &[MuxId::Wsmux], IpStrategy::Auto);
    let last = bytes.len() - 1;
    bytes[last] = 0x7f;
    assert!(decode_msg1(&bytes).is_err());
}

/// 配置文件里的写法必须能解析，且未知写法返回 None 由调用方报错。
#[test]
fn ip_strategy_parses_config_spellings() {
    assert_eq!(IpStrategy::parse("auto"), Some(IpStrategy::Auto));
    assert_eq!(IpStrategy::parse("v4-only"), Some(IpStrategy::V4Only));
    assert_eq!(IpStrategy::parse("V4-Only"), Some(IpStrategy::V4Only), "大小写不敏感");
    assert_eq!(IpStrategy::parse(" prefer-v4 "), Some(IpStrategy::PreferV4), "两边空白要容忍");
    assert_eq!(IpStrategy::parse("v6-only"), Some(IpStrategy::V6Only));
    assert_eq!(IpStrategy::parse("ipv4"), None, "拼错要报错而不是猜");
    assert_eq!(IpStrategy::parse(""), None);
    // 别名一律不收：合法写法必须与 wsieve-config 的 check_enum 列表逐字相同。
    assert_eq!(IpStrategy::parse("ipv4-only"), None);
    assert_eq!(IpStrategy::parse("prefer-ipv4"), None);
}

/// group_id 极值（全 0 / 全 1）必须无损往返——0 是合法组 id，不是哨兵。
#[test]
fn msg1_group_id_edge_values() {
    for gid in [0u128, u128::MAX] {
        let bytes = encode_msg1(7, gid, &[MuxId::Wsmux], IpStrategy::Auto);
        assert_eq!(decode_msg1(&bytes).unwrap().group_id, gid);
    }
}

/// 不同 group_id 编码出的字节必然不同（组区分度的最低保证）。
#[test]
fn msg1_group_id_distinguishes() {
    let a = encode_msg1(0, 1, &[MuxId::Wsmux], IpStrategy::Auto);
    let b = encode_msg1(0, 2, &[MuxId::Wsmux], IpStrategy::Auto);
    assert_ne!(a, b);
    assert_ne!(
        decode_msg1(&a).unwrap().group_id,
        decode_msg1(&b).unwrap().group_id
    );
}

#[test]
fn msg1_rejects_empty_mux_list() {
    assert!(decode_msg1(&encode_msg1(0, 0, &[], IpStrategy::Auto)).is_err());
}

/// 版本号不认识就拒。**1 也要拒**——它是加 ip_strategy 之前的布局，
/// 尾部少一个字节；放行的话末位 mux_id 会被当成策略字节读走，属于静默错位。
#[test]
fn msg1_rejects_bad_version() {
    for v in [0u8, 1, 3, 255] {
        let mut bytes = encode_msg1(0, 9, &[MuxId::Wsmux], IpStrategy::Auto);
        bytes[0] = v;
        assert!(decode_msg1(&bytes).is_err(), "版本 {v} 不该被接受");
    }
}

#[test]
fn msg1_rejects_wrong_length() {
    // mux_count says 2 but only 1 mux_id follows
    let mut bytes = encode_msg1(0, 9, &[MuxId::Wsmux], IpStrategy::Auto);
    bytes[25] = 2;
    assert!(decode_msg1(&bytes).is_err());
    // trailing garbage
    let mut bytes = encode_msg1(0, 9, &[MuxId::Wsmux], IpStrategy::Auto);
    bytes.push(0x01);
    assert!(decode_msg1(&bytes).is_err());
    // 头部被截断（旧 10 字节布局的长度已不再合法）
    let bytes = encode_msg1(0, 9, &[MuxId::Wsmux], IpStrategy::Auto);
    assert!(decode_msg1(&bytes[..10]).is_err());
}

#[test]
fn msg1_rejects_bad_mux_id() {
    let mut bytes = encode_msg1(0, 9, &[MuxId::Wsmux], IpStrategy::Auto);
    bytes[26] = 0x06;
    assert!(decode_msg1(&bytes).is_err());
}

#[test]
fn msg2_roundtrip() {
    for fallback in [false, true] {
        let m2 = Msg2 {
            chosen_mux_id: MuxId::Wsmux,
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
