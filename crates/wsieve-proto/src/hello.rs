//! 握手载荷 msg1/msg2（spec §6.3 + §7.4），大端序：
//!
//! ```text
//! msg1: u8 version=2 | u64 ts_ms | u128 group_id | u8 mux_count(1..=5)
//!       | mux_count × u8 mux_id | u8 ip_strategy
//! msg2: u8 chosen_mux_id | u8 fallback(0/1)
//! ```
//!
//! `ip_strategy`：服务端解析**域名**目标时用哪个地址族（见 [`IpStrategy`]）。
//! 双栈服务器按 RFC 6724 通常优先 IPv6，客户端据此可以强制走 v4。
//!
//! `group_id`：客户端启动时随机生成一次，其发起的全部 XHTTP 会话（主会话
//! 与全部额外会话）共用同一值。服务端据此把同一客户端的多条会话归为一组，
//! 从而能把下行 lane 铺到组内任意会话上（多 TCP 条带 / aria2 效应）。
//! 单会话客户端即「只有一个成员的组」，行为与改动前一致。

use thiserror::Error;

/// 时间戳窗口（毫秒）：|now - ts| <= 300_000 才接受。
pub const TS_WINDOW_MS: u64 = 300_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MuxId {
    /// 本项目自己的复用实现，见 `wsieve_mux::wsmux`。
    ///
    /// 枚举只剩一个变体，但线上字段保留下来了：msg1 里那个字节仍然照发照校验，
    /// 将来要换实现时不必再动一次线格式。
    Wsmux = 0x01,
}

impl MuxId {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0x01 => Self::Wsmux,
            _ => return None,
        })
    }
}

/// 服务端解析**域名**目标时的地址族偏好，由客户端在握手里声明、整个会话生效。
///
/// 线上取值不可更改——它们直接进 msg1 的字节流。
///
/// 只作用于 `TargetAddr::Domain`：目标本来就是 IP 字面量时无从选择，
/// 照它给的地址连（否则就是把用户明确指定的地址改掉，属于 §6.4 禁止的
/// 「静默改道」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum IpStrategy {
    /// 交给系统按 RFC 6724 选。双栈服务器上通常优先 IPv6，这是改动前的行为。
    #[default]
    Auto = 0,
    /// 只用 A 记录。没有 A 记录就失败，**不回退 v6**——用户要的是确定性。
    V4Only = 1,
    /// 只用 AAAA 记录，同样不回退。
    V6Only = 2,
    /// 先把 IPv4 排在前面挨个试，全不通再试 IPv6。
    PreferV4 = 3,
}

impl IpStrategy {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Auto,
            1 => Self::V4Only,
            2 => Self::V6Only,
            3 => Self::PreferV4,
            _ => return None,
        })
    }

    /// 配置文件里的写法 → 枚举。未知取值返回 `None`，由调用方报错——
    /// 悄悄退回 `Auto` 会让用户以为自己的设置生效了。
    ///
    /// **每个取值只有一种写法**（大小写与两侧空白不计）。曾经还收
    /// `ipv4-only` 之类的别名，但 `wsieve_config::Config::validate` 里那份
    /// `check_enum` 列表得跟着列全，多一个别名就多一处能写漏的地方——
    /// 漏了的表现是「保存时通过、重连时报配置错误」。
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "v4-only" => Self::V4Only,
            "v6-only" => Self::V6Only,
            "prefer-v4" => Self::PreferV4,
            _ => return None,
        })
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum HelloError {
    #[error("bad version: {0}")]
    BadVersion(u8),
    #[error("bad ip strategy: {0}")]
    BadIpStrategy(u8),
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
    /// 服务端解析域名目标时用哪个地址族。v1 的 msg1 没有这个字段，解码成
    /// `Auto`（改动前的行为）。
    pub ip_strategy: IpStrategy,
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

/// msg1 线格式版本。
///
/// **改了字节布局就必须改这个数字**，哪怕不打算兼容旧版本：让旧客户端撞上
/// 明确的 `BadVersion`，而不是靠长度校验碰巧把它拒掉——后者在将来某次
/// 字段增删恰好凑成相同长度时会变成静默错位解析。
const MSG1_VERSION: u8 = 2;

pub fn encode_msg1(
    ts_ms: u64,
    group_id: u128,
    mux_prefs: &[MuxId],
    ip_strategy: IpStrategy,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(MSG1_HEAD + mux_prefs.len() + 1);
    out.push(MSG1_VERSION);
    out.extend_from_slice(&ts_ms.to_be_bytes());
    out.extend_from_slice(&group_id.to_be_bytes());
    out.push(mux_prefs.len() as u8);
    for m in mux_prefs {
        out.push(*m as u8);
    }
    out.push(ip_strategy as u8);
    out
}

pub fn decode_msg1(bytes: &[u8]) -> Result<Msg1, HelloError> {
    if bytes.len() < MSG1_HEAD {
        return Err(HelloError::BadLength(MSG1_HEAD, bytes.len()));
    }
    let version = bytes[0];
    if version != MSG1_VERSION {
        return Err(HelloError::BadVersion(version));
    }
    let ts_ms = u64::from_be_bytes(bytes[1..9].try_into().unwrap());
    let group_id = u128::from_be_bytes(bytes[9..25].try_into().unwrap());
    let mux_count = bytes[25] as usize;
    if mux_count == 0 || mux_count > 5 {
        return Err(HelloError::BadMuxCount(bytes[25]));
    }
    // 尾部固定跟一个策略字节。
    let mux_end = MSG1_HEAD + mux_count;
    if bytes.len() != mux_end + 1 {
        return Err(HelloError::BadLength(mux_end + 1, bytes.len()));
    }
    let mux_prefs = bytes[MSG1_HEAD..mux_end]
        .iter()
        .map(|&b| MuxId::from_u8(b).ok_or(HelloError::BadMuxId(b)))
        .collect::<Result<Vec<_>, _>>()?;
    let raw = bytes[mux_end];
    let ip_strategy = IpStrategy::from_u8(raw).ok_or(HelloError::BadIpStrategy(raw))?;
    Ok(Msg1 {
        version,
        ts_ms,
        group_id,
        mux_prefs,
        ip_strategy,
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
