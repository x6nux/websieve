//! 握手载荷 msg1/msg2（spec §6.3 + §7.4），大端序：
//!
//! ```text
//! msg1: u8 version=1 | u64 ts_ms | u8 mux_count(1..=5) | mux_count × u8 mux_id
//! msg2: u8 chosen_mux_id | u8 fallback(0/1)
//! ```

use thiserror::Error;

/// 时间戳窗口（毫秒）：|now - ts| <= 300_000 才接受。
pub const TS_WINDOW_MS: u64 = 300_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MuxId {
    Yamux = 0x01,
    Smux = 0x02,
    Muxado = 0x03,
    Picomux = 0x04,
    H2mux = 0x05,
}

impl MuxId {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0x01 => Self::Yamux,
            0x02 => Self::Smux,
            0x03 => Self::Muxado,
            0x04 => Self::Picomux,
            0x05 => Self::H2mux,
            _ => return None,
        })
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum HelloError {
    #[error("bad version: {0}")]
    BadVersion(u8),
    #[error("bad mux count: {0}")]
    BadMuxCount(u8),
    #[error("bad mux id: {0:#04x}")]
    BadMuxId(u8),
    #[error("bad length: expected {0}, got {1}")]
    BadLength(usize, usize),
    #[error("bad fallback flag: {0}")]
    BadFallback(u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Msg1 {
    pub version: u8,
    pub ts_ms: u64,
    pub mux_prefs: Vec<MuxId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Msg2 {
    pub chosen_mux_id: MuxId,
    pub fallback: bool,
}

/// |now_ms - ts_ms| <= TS_WINDOW_MS。
pub fn ts_in_window(ts_ms: u64, now_ms: u64) -> bool {
    ts_ms.abs_diff(now_ms) <= TS_WINDOW_MS
}

pub fn encode_msg1(ts_ms: u64, mux_prefs: &[MuxId]) -> Vec<u8> {
    let mut out = Vec::with_capacity(10 + mux_prefs.len());
    out.push(1);
    out.extend_from_slice(&ts_ms.to_be_bytes());
    out.push(mux_prefs.len() as u8);
    for m in mux_prefs {
        out.push(*m as u8);
    }
    out
}

pub fn decode_msg1(bytes: &[u8]) -> Result<Msg1, HelloError> {
    if bytes.len() < 10 {
        return Err(HelloError::BadLength(10, bytes.len()));
    }
    let version = bytes[0];
    if version != 1 {
        return Err(HelloError::BadVersion(version));
    }
    let ts_ms = u64::from_be_bytes(bytes[1..9].try_into().unwrap());
    let mux_count = bytes[9] as usize;
    if mux_count == 0 || mux_count > 5 {
        return Err(HelloError::BadMuxCount(bytes[9]));
    }
    if bytes.len() != 10 + mux_count {
        return Err(HelloError::BadLength(10 + mux_count, bytes.len()));
    }
    let mux_prefs = bytes[10..]
        .iter()
        .map(|&b| MuxId::from_u8(b).ok_or(HelloError::BadMuxId(b)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Msg1 {
        version,
        ts_ms,
        mux_prefs,
    })
}

pub fn encode_msg2(m: &Msg2) -> Vec<u8> {
    vec![m.chosen_mux_id as u8, m.fallback as u8]
}

pub fn decode_msg2(bytes: &[u8]) -> Result<Msg2, HelloError> {
    if bytes.len() != 2 {
        return Err(HelloError::BadLength(2, bytes.len()));
    }
    let chosen_mux_id = MuxId::from_u8(bytes[0]).ok_or(HelloError::BadMuxId(bytes[0]))?;
    let fallback = match bytes[1] {
        0 => false,
        1 => true,
        v => return Err(HelloError::BadFallback(v)),
    };
    Ok(Msg2 {
        chosen_mux_id,
        fallback,
    })
}
