//! Noise_IK + AES-256-GCM + BLAKE3（spec §6.6）。
//!
//! snow 0.10 的 `NoiseParams::from_str` 只认已注册的哈希名，"BLAKE3" 会在
//! 解析阶段 panic（早于 resolver 介入），因此：
//! - 线名用可解析的壳 `Noise_IK_25519_AESGCM_SHA256`（AESGCM 是已注册密码名）；
//! - `SieveResolver::resolve_hash` 无视 HashChoice，恒返回 BLAKE3 实现，
//!   实际跑的哈希是 BLAKE3（协议文档名见 [`PROTOCOL_NAME`]）。

use aes_gcm::aead::AeadInPlace;
use aes_gcm::{Aes256Gcm, Key, KeyInit};
use snow::params::{CipherChoice, HashChoice, NoiseParams};
use snow::resolvers::{CryptoResolver, DefaultResolver, FallbackResolver};
use snow::types::Cipher;
use snow::{Builder, Error, HandshakeState};
use x25519_dalek::{PublicKey, StaticSecret};

/// 文档标识（真实算法组合），从不被 snow 解析。
pub const PROTOCOL_NAME: &str = "Noise_IK_25519_AESGCM_BLAKE3";
/// snow 可解析的线名壳（哈希名仅为占位，被 resolver 覆盖为 BLAKE3）。
pub const WIRE_NAME: &str = "Noise_IK_25519_AESGCM_SHA256";

/// GCM 12 字节 nonce：4 零字节 ‖ u64 BE 计数器（spec §6.6 固定）。
fn gcm_nonce(counter: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_be_bytes());
    n
}

/// AES-256-GCM（detached 16 字节 tag 追加在密文后，与 snow 约定一致）。
#[derive(Default)]
pub struct AesGcmCipher {
    cipher: Option<Aes256Gcm>,
}

impl Cipher for AesGcmCipher {
    fn name(&self) -> &'static str {
        "AESGCM"
    }

    fn set(&mut self, key: &[u8; 32]) {
        self.cipher = Some(Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key)));
    }

    fn encrypt(&self, nonce: u64, authtext: &[u8], plaintext: &[u8], out: &mut [u8]) -> usize {
        let cipher = self.cipher.as_ref().expect("AesGcmCipher used before set()");
        out[..plaintext.len()].copy_from_slice(plaintext);
        let tag = cipher
            .encrypt_in_place_detached(&gcm_nonce(nonce).into(), authtext, &mut out[..plaintext.len()])
            .expect("aes-gcm encrypt cannot fail on valid inputs");
        out[plaintext.len()..plaintext.len() + 16].copy_from_slice(&tag);
        plaintext.len() + 16
    }

    fn decrypt(
        &self,
        nonce: u64,
        authtext: &[u8],
        ciphertext: &[u8],
        out: &mut [u8],
    ) -> Result<usize, Error> {
        let cipher = self.cipher.as_ref().expect("AesGcmCipher used before set()");
        if ciphertext.len() < 16 {
            return Err(Error::Decrypt);
        }
        let (body, tag) = ciphertext.split_at(ciphertext.len() - 16);
        out[..body.len()].copy_from_slice(body);
        cipher
            .decrypt_in_place_detached(
                &gcm_nonce(nonce).into(),
                authtext,
                &mut out[..body.len()],
                tag.into(),
            )
            .map_err(|_| Error::Decrypt)?;
        Ok(body.len())
    }
}

/// BLAKE3（hash_len 32，块长 64，供 Noise HMAC 使用）。
#[derive(Default)]
pub struct Blake3Hash {
    hasher: blake3::Hasher,
}

impl snow::types::Hash for Blake3Hash {
    fn name(&self) -> &'static str {
        "BLAKE3"
    }

    fn block_len(&self) -> usize {
        64
    }

    fn hash_len(&self) -> usize {
        32
    }

    fn reset(&mut self) {
        self.hasher.reset();
    }

    fn input(&mut self, data: &[u8]) {
        self.hasher.update(data);
    }

    fn result(&mut self, out: &mut [u8]) {
        // snow 以 MAXHASHLEN(64) 缓冲调用，仅前 hash_len(32) 字节有效。
        out[..32].copy_from_slice(self.hasher.finalize().as_bytes());
    }
}

/// 自定义 resolver：cipher/hash 走 AESGCM/BLAKE3；dh/rng 委托 DefaultResolver。
pub struct SieveResolver;

impl CryptoResolver for SieveResolver {
    fn resolve_rng(&self) -> Option<Box<dyn snow::types::Random>> {
        DefaultResolver.resolve_rng()
    }

    fn resolve_dh(&self, choice: &snow::params::DHChoice) -> Option<Box<dyn snow::types::Dh>> {
        DefaultResolver.resolve_dh(choice)
    }

    fn resolve_hash(&self, _choice: &HashChoice) -> Option<Box<dyn snow::types::Hash>> {
        Some(Box::new(Blake3Hash::default()))
    }

    fn resolve_cipher(&self, _choice: &CipherChoice) -> Option<Box<dyn Cipher>> {
        Some(Box::new(AesGcmCipher::default()))
    }
}

fn builder<'a>() -> Builder<'a> {
    let params: NoiseParams = WIRE_NAME.parse().expect("WIRE_NAME is parseable");
    Builder::with_resolver(
        params,
        Box::new(FallbackResolver::new(
            Box::new(SieveResolver),
            Box::new(DefaultResolver),
        )),
    )
}

/// IK 发起方：需要服务端静态公钥 + 本地静态私钥。
pub fn build_client(
    server_static_pub: &[u8],
    client_priv: &[u8],
) -> Result<HandshakeState, Error> {
    builder()
        .local_private_key(client_priv)?
        .remote_public_key(server_static_pub)?
        .build_initiator()
}

/// IK 响应方：需要本地静态私钥；发起方公钥由 `read_message` 后经
/// `get_remote_static()` 取出，白名单比对在调用方完成（spec §6.3）。
pub fn build_server(server_priv: &[u8]) -> Result<HandshakeState, Error> {
    builder().local_private_key(server_priv)?.build_responder()
}

/// 生成 X25519 静态密钥对：(私钥 32B, 公钥 32B)。
pub fn gen_keypair() -> ([u8; 32], [u8; 32]) {
    use rand::RngCore;
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    let priv_key = StaticSecret::from(seed);
    let pub_key = PublicKey::from(&priv_key);
    (priv_key.to_bytes(), pub_key.to_bytes())
}
