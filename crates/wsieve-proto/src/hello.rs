//! 握手载荷 msg1/msg2（spec §6.3 + §7.4），大端序：
//!
//! ```text
//! msg1: u8 version=1 | u64 ts_ms | u128 group_id | u8 mux_count(1..=5) | mux_count × u8 mux_id
//! msg2: u8 chosen_mux_id | u8 fallback(0/1)
//! ```
//!
//! `group_id`：客户端启动时随机生成一次，其发起的全部 XHTTP 会话（主会话
//! + 全部额外会话）共用同一值。服务端据此把同一客户端的多条会话归为一组，
//! 从而能把下行 lane 铺到组内任意会话上（多 TCP 条带 / aria2 效应）。
//! 单会话客户端即「只有一个成员的组」，行为与改动前一致。

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
    /// 会话组 id：同一客户端的全部会话共用（服务端据此跨会话开下行 lane）。
    pub group_id: u128,
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

/// msg1 固定头长度：version(1) + ts_ms(8) + group_id(16) + mux_count(1)。
const MSG1_HEAD: usize = 1 + 8 + 16 + 1;

pub fn encode_msg1(ts_ms: u64, group_id: u128, mux_prefs: &[MuxId]) -> Vec<u8> {
    let mut out = Vec::with_capacity(MSG1_HEAD + mux_prefs.len());
    out.push(1);
    out.extend_from_slice(&ts_ms.to_be_bytes());
    out.extend_from_slice(&group_id.to_be_bytes());
    out.push(mux_prefs.len() as u8);
    for m in mux_prefs {
        out.push(*m as u8);
    }
    out
}

pub fn decode_msg1(bytes: &[u8]) -> Result<Msg1, HelloError> {
    if bytes.len() < MSG1_HEAD {
        return Err(HelloError::BadLength(MSG1_HEAD, bytes.len()));
    }
    let version = bytes[0];
    if version != 1 {
        return Err(HelloError::BadVersion(version));
    }
    let ts_ms = u64::from_be_bytes(bytes[1..9].try_into().unwrap());
    let group_id = u128::from_be_bytes(bytes[9..25].try_into().unwrap());
    let mux_count = bytes[25] as usize;
    if mux_count == 0 || mux_count > 5 {
        return Err(HelloError::BadMuxCount(bytes[25]));
    }
    if bytes.len() != MSG1_HEAD + mux_count {
        return Err(HelloError::BadLength(MSG1_HEAD + mux_count, bytes.len()));
    }
    let mux_prefs = bytes[MSG1_HEAD..]
        .iter()
        .map(|&b| MuxId::from_u8(b).ok_or(HelloError::BadMuxId(b)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Msg1 {
        version,
        ts_ms,
        group_id,
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
