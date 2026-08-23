use wsieve_proto::tu::*;

#[test]
fn frame_roundtrip_data() {
    let f = Frame::Data(vec![1, 2, 3, 4, 5]);
    let plain = encode_frame(&f, &mut rand::rng()).unwrap();
    assert!(plain.len() >= 3 + 5 && plain.len() <= 3 + 5 + MAX_PADDING);
    assert_eq!(decode_frame(&plain).unwrap(), f);
}

#[test]
fn frame_roundtrip_padding() {
    let plain = encode_frame(&Frame::Padding, &mut rand::rng()).unwrap();
    assert_eq!(decode_frame(&plain).unwrap(), Frame::Padding);
}

#[test]
fn payload_limit_enforced() {
    let big = vec![0u8; MAX_PAYLOAD + 1];
    assert!(encode_frame(&Frame::Data(big), &mut rand::rng()).is_err());
    let ok = vec![0u8; MAX_PAYLOAD];
    assert!(encode_frame(&Frame::Data(ok), &mut rand::rng()).is_ok());
}

#[test]
fn decoder_splits_stream_at_boundaries() {
    let mut dec = TuDecoder::new();
    let c1 = b"\x00\x05hello".to_vec();
    let c2 = b"\x00\x03abc".to_vec();
    let mut wire = c1.clone();
    wire.extend_from_slice(&c2);
    assert_eq!(dec.push(&wire[..7]), vec![c1]);
    assert_eq!(dec.push(&wire[7..]), vec![c2]);
    assert!(dec.push(b"\x00").is_empty());
}

#[test]
fn decoder_accepts_max_len_header() {
    let mut dec = TuDecoder::new();
    let wire = [0xffu8, 0xff];
    assert!(dec.push(&wire).is_empty());
}
