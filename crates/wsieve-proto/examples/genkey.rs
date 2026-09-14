fn main() {
    let (priv_, pub_) = wsieve_proto::crypto::gen_keypair();
    println!("server_priv={}", hex(&priv_));
    println!("server_pub={}", hex(&pub_));
    let (cp, cpub) = wsieve_proto::crypto::gen_keypair();
    println!("client_priv={}", hex(&cp));
    println!("client_pub={}", hex(&cpub));
}
fn hex(b: &[u8; 32]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }
