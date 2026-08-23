use std::collections::HashSet;
use sealed::Seal;
use wsieve_proto::crypto::*;

fn seal<T: sealed::Seal>(h: &mut T, payload: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; 65535];
    let n = h.write(payload, &mut buf).unwrap();
    buf.truncate(n);
    buf
}

fn open<T: sealed::Seal>(h: &mut T, msg: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; 65535];
    let n = h.read(msg, &mut buf).unwrap();
    buf.truncate(n);
    buf
}

#[test]
fn handshake_roundtrip_ik() {
    let (s_priv, s_pub) = gen_keypair();
    let (c_priv, c_pub) = gen_keypair();
    let _allow: HashSet<[u8; 32]> = [c_pub].into_iter().collect();

    let mut c = build_client(&s_pub, &c_priv).unwrap();
    let msg1 = seal(&mut c, b"hello-0rtt");

    let mut s = build_server(&s_priv).unwrap();
    assert_eq!(open(&mut s, &msg1), b"hello-0rtt");

    let msg2 = seal(&mut s, b"ack");
    assert_eq!(open(&mut c, &msg2), b"ack");

    let mut t1 = c.into_transport_mode().unwrap();
    let mut t2 = s.into_transport_mode().unwrap();
    let ct = seal(&mut t1, b"data");
    assert_eq!(open(&mut t2, &ct), b"data");
    // wrong-key decrypt must fail:
    let garbage = vec![0u8; ct.len()];
    assert!(t2.read(&garbage, &mut vec![0u8; 65535]).is_err());
}

#[test]
fn whitelist_rejection_is_caller_side() {
    // rogue client (valid keys, not whitelisted): handshake SUCCEEDS at noise level,
    // but get_remote_static() returns a key NOT in the empty whitelist.
    let (s_priv, s_pub) = gen_keypair();
    let (rogue_priv, rogue_pub) = gen_keypair();
    let mut c = build_client(&s_pub, &rogue_priv).unwrap();
    let msg1 = seal(&mut c, b"x");
    let mut s = build_server(&s_priv).unwrap();
    let _ = open(&mut s, &msg1); // succeeds at Noise level
    let remote = s.get_remote_static().unwrap();
    assert_eq!(remote, &rogue_pub[..]);
    let allowed: HashSet<[u8; 32]> = HashSet::new();
    let remote_arr: [u8; 32] = remote.try_into().unwrap();
    assert!(!allowed.contains(&remote_arr)); // caller-side rejection point
}

#[test]
fn wire_name_parses() {
    let p: snow::params::NoiseParams = WIRE_NAME.parse().unwrap();
    assert_eq!(p.name, WIRE_NAME);
}

mod sealed {
    pub trait Seal {
        fn write(&mut self, payload: &[u8], message: &mut [u8]) -> Result<usize, snow::Error>;
        fn read(&mut self, message: &[u8], payload: &mut [u8]) -> Result<usize, snow::Error>;
    }
    impl Seal for snow::HandshakeState {
        fn write(&mut self, p: &[u8], m: &mut [u8]) -> Result<usize, snow::Error> {
            snow::HandshakeState::write_message(self, p, m)
        }
        fn read(&mut self, m: &[u8], p: &mut [u8]) -> Result<usize, snow::Error> {
            snow::HandshakeState::read_message(self, m, p)
        }
    }
    impl Seal for snow::TransportState {
        fn write(&mut self, p: &[u8], m: &mut [u8]) -> Result<usize, snow::Error> {
            snow::TransportState::write_message(self, p, m)
        }
        fn read(&mut self, m: &[u8], p: &mut [u8]) -> Result<usize, snow::Error> {
            snow::TransportState::read_message(self, m, p)
        }
    }
}
