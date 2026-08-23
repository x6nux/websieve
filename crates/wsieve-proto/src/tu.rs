//! TU（Transport Unit）分帧。spec §6.1。
//! 外层：u16 大端长度 + Noise 密文；内层（解密后）：u8 type + u16 p_len + payload + padding。

pub const MAX_CIPHERTEXT: usize = 65535;
pub const TAG_LEN: usize = 16;
pub const MAX_PLAINTEXT: usize = MAX_CIPHERTEXT - TAG_LEN;
pub const FRAME_HEADER: usize = 3;
pub const MAX_PADDING: usize = 1000;
pub const MAX_PAYLOAD: usize = MAX_PLAINTEXT - FRAME_HEADER - MAX_PADDING; // 64516

pub const TYPE_DATA: u8 = 0x01;
pub const TYPE_PADDING: u8 = 0x02;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Data(Vec<u8>),
    Padding,
}

pub fn encode_frame(frame: &Frame, rng: &mut impl rand::Rng) -> Result<Vec<u8>, TuError> {
    let payload = match frame {
        Frame::Data(p) => {
            if p.len() > MAX_PAYLOAD {
                return Err(TuError::PayloadTooLarge(p.len(), MAX_PAYLOAD));
            }
            p.as_slice()
        }
        Frame::Padding => &[],
    };
    let ty = match frame {
        Frame::Data(_) => TYPE_DATA,
        Frame::Padding => TYPE_PADDING,
    };
    let pad = rng.random_range(0..=MAX_PADDING);
    let mut out = Vec::with_capacity(FRAME_HEADER + payload.len() + pad);
    out.push(ty);
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out.resize(out.len() + pad, 0);
    Ok(out)
}

pub fn decode_frame(plain: &[u8]) -> Result<Frame, TuError> {
    if plain.len() < FRAME_HEADER {
        return Err(TuError::Truncated);
    }
    let ty = plain[0];
    let p_len = u16::from_be_bytes([plain[1], plain[2]]) as usize;
    if plain.len() < FRAME_HEADER + p_len {
        return Err(TuError::Truncated);
    }
    match ty {
        TYPE_DATA => Ok(Frame::Data(plain[FRAME_HEADER..FRAME_HEADER + p_len].to_vec())),
        TYPE_PADDING if p_len == 0 => Ok(Frame::Padding),
        other => Err(TuError::UnknownType(other)),
    }
}

pub struct TuDecoder {
    buf: Vec<u8>,
}

impl TuDecoder {
    pub fn new() -> Self {
        Self { buf: Vec::with_capacity(MAX_CIPHERTEXT + 2) }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while self.buf.len() >= 2 {
            let len = u16::from_be_bytes([self.buf[0], self.buf[1]]) as usize;
            if self.buf.len() < 2 + len {
                break;
            }
            let tu = self.buf[..2 + len].to_vec();
            self.buf.drain(..2 + len);
            out.push(tu);
        }
        out
    }
}

impl Default for TuDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TuError {
    #[error("payload {0} exceeds limit {1}")]
    PayloadTooLarge(usize, usize),
    #[error("truncated frame")]
    Truncated,
    #[error("unknown frame type {0}")]
    UnknownType(u8),
}
