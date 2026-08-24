//! v2 多流条带化（striping）线格式（纯编解码，无 IO）。
//!
//! 每条 mux 子流的首帧是 16 字节 ConnHeader：
//! ```text
//! u8  ver = 2
//! u64 conn_id (BE)
//! u8  cmd   0x01 OPEN / 0x02 DATA / 0x03 CLOSE
//! u8  dir   0x01 UP / 0x02 DOWN / 0x03 BIDI
//! u16 lane_id (BE)
//! ```
//! OPEN 的首 lane（客户端发起）在 header 后紧跟 TargetAddr（wsieve-proto
//! addr 格式）。后续所有数据都是 DataFrame：
//! ```text
//! u64 offset (BE) — 该方向流内的绝对偏移
//! u32 len    (BE)
//! len 字节 payload
//! ```
//! CLOSE 帧内联在 lane 上：[CLOSE header][u64 final_offset BE][u8 reason]。
//! 非首帧的 CLOSE 识别约定：DataFrame 首字节是 offset 的最高位字节，
//! 任何现实 offset（< 2^56）下恒为 0x00，因此帧边界上出现 0x02 即内联
//! ConnHeader（ver=2），二者无歧义。

pub const VER: u8 = 2;
pub const HEADER_LEN: usize = 16;
/// DataFrame 头（offset + len）长度。
pub const FRAME_HEADER_LEN: usize = 12;
/// CLOSE 载荷长度（u64 final_offset + u8 reason）。
pub const CLOSE_PAYLOAD_LEN: usize = 9;

/// 条带数据分片大小。
pub const CHUNK: usize = 64 * 1024;

/// 单个 DataFrame 载荷上限（防恶意/损坏长度字段；正常分片为 64 KiB，
/// 留 4x 余量容纳非对齐切分）。
pub const MAX_FRAME_PAYLOAD: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    Open,
    Data,
    Close,
}

impl Cmd {
    pub fn to_u8(self) -> u8 {
        match self {
            Cmd::Open => 0x01,
            Cmd::Data => 0x02,
            Cmd::Close => 0x03,
        }
    }
    pub fn from_u8(v: u8) -> Result<Self, StripeError> {
        match v {
            0x01 => Ok(Cmd::Open),
            0x02 => Ok(Cmd::Data),
            0x03 => Ok(Cmd::Close),
            other => Err(StripeError::BadCmd(other)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Bidi,
}

impl Dir {
    pub fn to_u8(self) -> u8 {
        match self {
            Dir::Up => 0x01,
            Dir::Down => 0x02,
            Dir::Bidi => 0x03,
        }
    }
    pub fn from_u8(v: u8) -> Result<Self, StripeError> {
        match v {
            0x01 => Ok(Dir::Up),
            0x02 => Ok(Dir::Down),
            0x03 => Ok(Dir::Bidi),
            other => Err(StripeError::BadDir(other)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// 目标正常 EOF。
    TargetEof = 0x01,
    /// 目标侧错误。
    TargetError = 0x02,
    /// 连接重置（立即中止，双向）。
    Reset = 0x03,
}

impl CloseReason {
    pub fn to_u8(self) -> u8 {
        self as u8
    }
    pub fn from_u8(v: u8) -> Result<Self, StripeError> {
        match v {
            0x01 => Ok(CloseReason::TargetEof),
            0x02 => Ok(CloseReason::TargetError),
            0x03 => Ok(CloseReason::Reset),
            other => Err(StripeError::BadCloseReason(other)),
        }
    }
}

/// 16 字节 ConnHeader。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnHeader {
    pub conn_id: u64,
    pub cmd: Cmd,
    pub dir: Dir,
    pub lane_id: u16,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq, Clone)]
pub enum StripeError {
    #[error("buffer too short: need {need}, have {have}")]
    Truncated { need: usize, have: usize },
    #[error("bad version: {0:#04x} (expected {VER:#04x})")]
    BadVer(u8),
    #[error("unknown cmd: {0:#04x}")]
    BadCmd(u8),
    #[error("unknown dir: {0:#04x}")]
    BadDir(u8),
    #[error("unknown close reason: {0:#04x}")]
    BadCloseReason(u8),
    #[error("frame payload too large: {0}")]
    FrameTooLarge(u32),
    #[error("close payload too large: need {need}, have {have}")]
    BadClose { need: usize, have: usize },
}

pub fn encode_header(h: &ConnHeader) -> [u8; HEADER_LEN] {
    let mut out = [0u8; HEADER_LEN];
    out[0] = VER;
    out[1..9].copy_from_slice(&h.conn_id.to_be_bytes());
    out[9] = h.cmd.to_u8();
    out[10] = h.dir.to_u8();
    out[11..13].copy_from_slice(&h.lane_id.to_be_bytes());
    out
}

/// 解析 ConnHeader。不足 16 字节 → Truncated；ver/cmd/dir 非法 → 相应错误。
pub fn decode_header(b: &[u8]) -> Result<ConnHeader, StripeError> {
    if b.len() < HEADER_LEN {
        return Err(StripeError::Truncated {
            need: HEADER_LEN,
            have: b.len(),
        });
    }
    if b[0] != VER {
        return Err(StripeError::BadVer(b[0]));
    }
    Ok(ConnHeader {
        conn_id: u64::from_be_bytes(b[1..9].try_into().unwrap()),
        cmd: Cmd::from_u8(b[9])?,
        dir: Dir::from_u8(b[10])?,
        lane_id: u16::from_be_bytes([b[11], b[12]]),
    })
}

/// 编码一个 DataFrame 头 + payload，追加到 `out`。
pub fn encode_frame(offset: u64, payload: &[u8], out: &mut Vec<u8>) {
    out.reserve(FRAME_HEADER_LEN + payload.len());
    out.extend_from_slice(&offset.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
}

/// 尝试解码一个完整 DataFrame。返回 `(offset, payload, consumed)`。
/// 输入不完整 → `Ok(None)`；长度字段超上限 → `FrameTooLarge`。
/// 注意：调用方需先确认首字节不是 0x02（内联 ConnHeader，见模块注释）。
pub fn decode_frame(b: &[u8]) -> Result<Option<(u64, &[u8], usize)>, StripeError> {
    if b.len() < FRAME_HEADER_LEN {
        return Ok(None);
    }
    let offset = u64::from_be_bytes(b[0..8].try_into().unwrap());
    let len = u32::from_be_bytes(b[8..12].try_into().unwrap()) as usize;
    if len > MAX_FRAME_PAYLOAD {
        return Err(StripeError::FrameTooLarge(len as u32));
    }
    if b.len() < FRAME_HEADER_LEN + len {
        return Ok(None);
    }
    Ok(Some((offset, &b[FRAME_HEADER_LEN..FRAME_HEADER_LEN + len], FRAME_HEADER_LEN + len)))
}

/// CLOSE 载荷：u64 final_offset + u8 reason。
pub fn encode_close_payload(final_offset: u64, reason: CloseReason) -> [u8; CLOSE_PAYLOAD_LEN] {
    let mut out = [0u8; CLOSE_PAYLOAD_LEN];
    out[..8].copy_from_slice(&final_offset.to_be_bytes());
    out[8] = reason.to_u8();
    out
}

pub fn decode_close_payload(b: &[u8]) -> Result<(u64, CloseReason), StripeError> {
    if b.len() < CLOSE_PAYLOAD_LEN {
        return Err(StripeError::BadClose {
            need: CLOSE_PAYLOAD_LEN,
            have: b.len(),
        });
    }
    let final_offset = u64::from_be_bytes(b[..8].try_into().unwrap());
    let reason = CloseReason::from_u8(b[8])?;
    Ok((final_offset, reason))
}

/// DataFrame 首字节是否为内联 ConnHeader 标记（ver=2）。offset 的最高
/// 字节在一切现实 offset 下为 0x00，故 0x02 无歧义。
pub fn is_inline_header(first_byte: u8) -> bool {
    first_byte == VER
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_layout_exact() {
        let h = ConnHeader {
            conn_id: 0x0102_0304_0506_0708,
            cmd: Cmd::Open,
            dir: Dir::Bidi,
            lane_id: 0x0A0B,
        };
        let b = encode_header(&h);
        assert_eq!(b.len(), 16);
        assert_eq!(b[0], 2);
        assert_eq!(&b[1..9], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(b[9], 0x01);
        assert_eq!(b[10], 0x03);
        assert_eq!(&b[11..13], &[0x0A, 0x0B]);
    }

    #[test]
    fn header_roundtrip_all_cmds_dirs() {
        for cmd in [Cmd::Open, Cmd::Data, Cmd::Close] {
            for dir in [Dir::Up, Dir::Down, Dir::Bidi] {
                let h = ConnHeader { conn_id: u64::MAX, cmd, dir, lane_id: u16::MAX };
                assert_eq!(decode_header(&encode_header(&h)).unwrap(), h);
            }
        }
    }

    #[test]
    fn header_partial_reads_rejected() {
        let b = encode_header(&ConnHeader { conn_id: 7, cmd: Cmd::Data, dir: Dir::Up, lane_id: 1 });
        for cut in 0..16 {
            assert_eq!(
                decode_header(&b[..cut]),
                Err(StripeError::Truncated { need: 16, have: cut }),
                "cut={cut}"
            );
        }
    }

    #[test]
    fn header_bad_ver_rejected() {
        let mut b = encode_header(&ConnHeader { conn_id: 1, cmd: Cmd::Open, dir: Dir::Up, lane_id: 0 });
        b[0] = 1; // v1
        assert_eq!(decode_header(&b), Err(StripeError::BadVer(1)));
        b[0] = 3;
        assert_eq!(decode_header(&b), Err(StripeError::BadVer(3)));
    }

    #[test]
    fn header_unknown_cmd_rejected() {
        let mut b = encode_header(&ConnHeader { conn_id: 1, cmd: Cmd::Open, dir: Dir::Up, lane_id: 0 });
        for bad in [0u8, 0x04, 0xFF] {
            b[9] = bad;
            assert_eq!(decode_header(&b), Err(StripeError::BadCmd(bad)));
        }
    }

    #[test]
    fn header_unknown_dir_rejected() {
        let mut b = encode_header(&ConnHeader { conn_id: 1, cmd: Cmd::Open, dir: Dir::Up, lane_id: 0 });
        for bad in [0u8, 0x05, 0x80] {
            b[10] = bad;
            assert_eq!(decode_header(&b), Err(StripeError::BadDir(bad)));
        }
    }

    #[test]
    fn frame_roundtrip_and_offsets() {
        let mut buf = Vec::new();
        encode_frame(0, b"hello", &mut buf);
        encode_frame(5, b" world", &mut buf);
        assert_eq!(&buf[..8], &0u64.to_be_bytes());
        assert_eq!(&buf[8..12], &5u32.to_be_bytes());

        let (off1, p1, used1) = decode_frame(&buf).unwrap().unwrap();
        assert_eq!((off1, p1, used1), (0, &b"hello"[..], 17));
        let (off2, p2, used2) = decode_frame(&buf[used1..]).unwrap().unwrap();
        assert_eq!((off2, p2, used2), (5, &b" world"[..], 18));
        assert_eq!(used1 + used2, buf.len());
    }

    #[test]
    fn frame_partial_input_returns_none() {
        let mut buf = Vec::new();
        encode_frame(12345, &[7u8; 100], &mut buf);
        // 每个前缀（除完整帧外）都必须是 None 而非错误
        for cut in 0..buf.len() {
            assert_eq!(decode_frame(&buf[..cut]).unwrap(), None, "cut={cut}");
        }
        assert!(decode_frame(&buf).unwrap().is_some());
    }

    #[test]
    fn frame_empty_payload_ok() {
        let mut buf = Vec::new();
        encode_frame(42, b"", &mut buf);
        assert_eq!(buf.len(), 12);
        let (off, p, used) = decode_frame(&buf).unwrap().unwrap();
        assert_eq!((off, p, used), (42, &b""[..], 12));
    }

    #[test]
    fn frame_len_over_limit_rejected() {
        let mut b = FRAME_HEADER_LEN.to_le_bytes().to_vec();
        let mut hdr = Vec::new();
        encode_frame(0, b"", &mut hdr);
        b = hdr;
        b[8..12].copy_from_slice(&((MAX_FRAME_PAYLOAD as u32) + 1).to_be_bytes());
        assert_eq!(
            decode_frame(&b),
            Err(StripeError::FrameTooLarge((MAX_FRAME_PAYLOAD as u32) + 1))
        );
    }

    #[test]
    fn close_payload_roundtrip() {
        for (off, r) in [
            (0u64, CloseReason::TargetEof),
            (u64::MAX, CloseReason::TargetError),
            (12345, CloseReason::Reset),
        ] {
            let b = encode_close_payload(off, r);
            assert_eq!(b.len(), 9);
            assert_eq!(decode_close_payload(&b).unwrap(), (off, r));
        }
    }

    #[test]
    fn close_payload_bad_input() {
        assert!(matches!(
            decode_close_payload(&[0u8; 8]),
            Err(StripeError::BadClose { need: 9, have: 8 })
        ));
        let b = encode_close_payload(0, CloseReason::TargetEof);
        let mut bad = b;
        bad[8] = 0x99;
        assert_eq!(decode_close_payload(&bad), Err(StripeError::BadCloseReason(0x99)));
    }

    #[test]
    fn inline_header_sentinel() {
        assert!(is_inline_header(0x02));
        assert!(!is_inline_header(0x00));
        assert!(!is_inline_header(0x03));
    }

    #[test]
    fn offset_arithmetic_be_encoding() {
        // 大 offset 编码：2^40，首字节仍为 0x00（与内联 header 无歧义的前提）
        let mut buf = Vec::new();
        encode_frame(1u64 << 40, b"x", &mut buf);
        assert_eq!(buf[0], 0x00);
        let (off, _, _) = decode_frame(&buf).unwrap().unwrap();
        assert_eq!(off, 1 << 40);
    }
}
