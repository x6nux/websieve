//! wsmux 线格式。
//!
//! 固定 12 字节帧头，无变长字段——解析器可以在一次比较里判断"缓冲里够不够
//! 一个完整帧头"，不用状态机。
//!
//! ```text
//! [0]      ver      协议版本，恒为 VERSION
//! [1]      cmd      Cmd 枚举
//! [2..4]   rsvd     保留，发送端填 0，接收端不校验（留给未来的 flags）
//! [4..8]   sid      流 ID，big-endian u32
//! [8..12]  arg      big-endian u32；PSH 时是 payload 字节数，WND 时是窗口增量，
//!                   其余命令恒为 0
//! ```
//!
//! WND 把增量塞在 `arg` 里而不是帧体，是因为窗口更新在高吞吐下是最密集的
//! 控制帧——省掉帧体就省掉一次分配和一次拷贝。

/// 帧头字节数。整个协议里唯一的"魔数"，解析和拼装都引用它。
pub const HEADER_LEN: usize = 12;

/// 协议版本。对端版本不匹配直接断会话，不做协商——两端总是同版本二进制。
pub const VERSION: u8 = 1;

/// 单帧 payload 上限。发送端按它切分，接收端用它拒绝畸形长度，
/// 避免一个伪造的 `arg` 让我们去 reserve 4 GiB。
pub const MAX_FRAME_PAYLOAD: u32 = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Cmd {
    /// 开流。sid 由发起方分配，见 `session::Session::alloc_sid`。
    Syn = 0,
    /// 发起方不再发数据（半关闭）。对端仍可继续发。
    Fin = 1,
    /// 数据帧，`arg` = payload 长度。
    Psh = 2,
    /// 窗口更新，`arg` = 接收方新释放出的字节数。
    Wnd = 3,
    /// 保活空帧。只为让读侧超时器看到活动，收到即丢弃。
    Nop = 4,
}

impl Cmd {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Syn,
            1 => Self::Fin,
            2 => Self::Psh,
            3 => Self::Wnd,
            4 => Self::Nop,
            _ => return None,
        })
    }
}

/// 解析出的帧头。payload 本身不在这里——它由调用方按 `arg` 从缓冲里零拷贝切出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub cmd: Cmd,
    pub sid: u32,
    pub arg: u32,
}

impl Header {
    pub fn new(cmd: Cmd, sid: u32, arg: u32) -> Self {
        Self { cmd, sid, arg }
    }

    /// 写进定长数组。返回值直接 `extend_from_slice` 进出站缓冲，
    /// 不经过 `BufMut::put_u32` 那串逐字段调用。
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0] = VERSION;
        b[1] = self.cmd as u8;
        // b[2..4] 保留位，`[0u8; _]` 已经填好 0。
        b[4..8].copy_from_slice(&self.sid.to_be_bytes());
        b[8..12].copy_from_slice(&self.arg.to_be_bytes());
        b
    }

    /// 从至少 `HEADER_LEN` 字节的切片解析。
    ///
    /// `None` 表示对端发来的是垃圾（版本不符 / 未知命令 / 长度越界），
    /// 调用方应当据此杀掉整个会话——单帧错误在复用层没有安全的恢复点，
    /// 因为我们已经无法确定下一个帧头从哪个字节开始。
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < HEADER_LEN || b[0] != VERSION {
            return None;
        }
        let cmd = Cmd::from_u8(b[1])?;
        let sid = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
        let arg = u32::from_be_bytes([b[8], b[9], b[10], b[11]]);
        if matches!(cmd, Cmd::Psh) && arg > MAX_FRAME_PAYLOAD {
            return None;
        }
        Some(Self { cmd, sid, arg })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_survives_a_round_trip_through_the_wire_format() {
        for h in [
            Header::new(Cmd::Syn, 1, 0),
            Header::new(Cmd::Psh, 0xDEAD_BEEF, MAX_FRAME_PAYLOAD),
            Header::new(Cmd::Wnd, 7, 4 * 1024 * 1024),
            Header::new(Cmd::Fin, u32::MAX, 0),
            Header::new(Cmd::Nop, 0, 0),
        ] {
            assert_eq!(Header::decode(&h.encode()), Some(h));
        }
    }

    #[test]
    fn garbage_on_the_wire_is_rejected_rather_than_guessed_at() {
        let good = Header::new(Cmd::Psh, 3, 16).encode();

        let mut bad_ver = good;
        bad_ver[0] = VERSION.wrapping_add(1);
        assert_eq!(Header::decode(&bad_ver), None, "版本不符必须拒");

        let mut bad_cmd = good;
        bad_cmd[1] = 0xFF;
        assert_eq!(Header::decode(&bad_cmd), None, "未知命令必须拒");

        let mut huge = good;
        huge[8..12].copy_from_slice(&(MAX_FRAME_PAYLOAD + 1).to_be_bytes());
        assert_eq!(Header::decode(&huge), None, "超长 PSH 必须拒，否则是一个 reserve 炸弹");

        assert_eq!(Header::decode(&good[..HEADER_LEN - 1]), None, "残缺帧头必须拒");
    }

    #[test]
    fn a_window_update_may_carry_a_large_argument_without_being_mistaken_for_a_payload() {
        // WND 的 arg 是窗口增量，不受 MAX_FRAME_PAYLOAD 约束——
        // 窗口本来就可以比单帧大得多。
        let h = Header::new(Cmd::Wnd, 1, MAX_FRAME_PAYLOAD * 8);
        assert_eq!(Header::decode(&h.encode()), Some(h));
    }
}
