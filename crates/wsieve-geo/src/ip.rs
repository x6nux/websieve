//! geoip.dat 的解析与 CIDR 匹配。
//!
//! 匹配结构用「合并区间 + 二分」而非前缀树：数十万条 CIDR 下
//! log2(n) ≈ 18 次比较，与前缀树的 32/128 步同一量级，但代码量少一半、
//! 无指针跳转、cache 友好。

use std::collections::HashMap;
use std::net::IpAddr;

use crate::pb::{PbError, Reader};

#[derive(Debug, thiserror::Error)]
pub enum IpDbError {
    #[error(transparent)]
    Pb(#[from] PbError),
    #[error("CIDR 地址长度非法：{0} 字节（只接受 4 或 16）")]
    BadIpLen(usize),
    #[error("CIDR 前缀长度越界：/{prefix}（地址为 {bits} 位）")]
    BadPrefix { prefix: u32, bits: u32 },
}

#[derive(Default)]
struct IpClass {
    v4: Vec<(u32, u32)>,   // 闭区间 [start, end]
    v6: Vec<(u128, u128)>,
}

pub struct IpDb {
    classes: HashMap<String, IpClass>,
}

impl IpDb {
    pub fn parse(buf: &[u8]) -> Result<Self, IpDbError> {
        let mut classes: HashMap<String, IpClass> = HashMap::new();

        let mut r = Reader::new(buf);
        while !r.is_empty() {
            let (field, wire) = r.tag()?;
            if field != 1 || wire != 2 {
                r.skip(wire)?;
                continue;
            }
            let entry = r.bytes()?;
            let (code, class) = parse_geoip(entry)?;
            let slot = classes.entry(code).or_default();
            slot.v4.extend(class.v4);
            slot.v6.extend(class.v6);
        }

        // 合并重叠区间，之后才能二分
        for class in classes.values_mut() {
            class.v4 = merge_ranges(std::mem::take(&mut class.v4));
            class.v6 = merge_ranges(std::mem::take(&mut class.v6));
        }
        Ok(Self { classes })
    }

    pub fn has(&self, code: &str) -> bool {
        self.classes.contains_key(&code.trim().to_ascii_lowercase())
    }

    pub fn matches(&self, code: &str, ip: IpAddr) -> bool {
        let Some(class) = self.classes.get(&code.trim().to_ascii_lowercase()) else {
            return false;
        };
        match ip {
            IpAddr::V4(a) => contains(&class.v4, u32::from(a)),
            IpAddr::V6(a) => contains(&class.v6, u128::from(a)),
        }
    }

    #[doc(hidden)] // 测试用
    pub fn range_count(&self, code: &str) -> usize {
        self.classes
            .get(&code.trim().to_ascii_lowercase())
            .map(|c| c.v4.len() + c.v6.len())
            .unwrap_or(0)
    }
}

fn parse_geoip(buf: &[u8]) -> Result<(String, IpClass), IpDbError> {
    let mut code = String::new();
    let mut class = IpClass::default();

    let mut r = Reader::new(buf);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, 2) => code = r.string()?,
            (2, 2) => {
                let c = r.bytes()?;
                match parse_cidr(c)? {
                    Cidr::V4(s, e) => class.v4.push((s, e)),
                    Cidr::V6(s, e) => class.v6.push((s, e)),
                }
            }
            // field 3 是 reverse_match。我们不支持反向匹配的 geoip 类别
            // （标准 geoip.dat 里不存在），遇到即跳过。
            _ => r.skip(wire)?,
        }
    }
    Ok((code.trim().to_ascii_lowercase(), class))
}

enum Cidr {
    V4(u32, u32),
    V6(u128, u128),
}

fn parse_cidr(buf: &[u8]) -> Result<Cidr, IpDbError> {
    let mut ip: Option<Vec<u8>> = None;
    let mut prefix = 0u32;

    let mut r = Reader::new(buf);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, 2) => ip = Some(r.bytes()?.to_vec()),
            (2, 0) => prefix = r.varint()? as u32,
            _ => r.skip(wire)?,
        }
    }

    let ip = ip.ok_or(IpDbError::BadIpLen(0))?;
    match ip.len() {
        4 => {
            if prefix > 32 {
                return Err(IpDbError::BadPrefix { prefix, bits: 32 });
            }
            let base = u32::from_be_bytes([ip[0], ip[1], ip[2], ip[3]]);
            let (start, end) = range_u32(base, prefix);
            Ok(Cidr::V4(start, end))
        }
        16 => {
            if prefix > 128 {
                return Err(IpDbError::BadPrefix { prefix, bits: 128 });
            }
            let mut b = [0u8; 16];
            b.copy_from_slice(&ip);
            let base = u128::from_be_bytes(b);
            let (start, end) = range_u128(base, prefix);
            Ok(Cidr::V6(start, end))
        }
        n => Err(IpDbError::BadIpLen(n)),
    }
}

/// prefix=0 时 shift 会溢出，必须单独处理。
fn range_u32(base: u32, prefix: u32) -> (u32, u32) {
    if prefix == 0 {
        return (0, u32::MAX);
    }
    let mask = u32::MAX << (32 - prefix);
    (base & mask, (base & mask) | !mask)
}

fn range_u128(base: u128, prefix: u32) -> (u128, u128) {
    if prefix == 0 {
        return (0, u128::MAX);
    }
    let mask = u128::MAX << (128 - prefix);
    (base & mask, (base & mask) | !mask)
}

/// 排序后合并相邻或重叠的区间。相邻（end + 1 == next.start）也要合并，
/// 否则 0.0.0.0/1 与 128.0.0.0/1 会留成两段。
fn merge_ranges<T>(mut v: Vec<(T, T)>) -> Vec<(T, T)>
where
    T: Copy + Ord + num_like::NumLike,
{
    if v.is_empty() {
        return v;
    }
    v.sort_unstable_by_key(|r| r.0);
    let mut out: Vec<(T, T)> = Vec::with_capacity(v.len());
    for (s, e) in v {
        match out.last_mut() {
            Some(last) if s <= last.1.saturating_add_one() => {
                if e > last.1 {
                    last.1 = e;
                }
            }
            _ => out.push((s, e)),
        }
    }
    out
}

fn contains<T: Copy + Ord>(ranges: &[(T, T)], x: T) -> bool {
    // 最后一个 start <= x 的区间，就是唯一可能命中的那个
    let i = ranges.partition_point(|r| r.0 <= x);
    i > 0 && ranges[i - 1].1 >= x
}

/// merge_ranges 需要 "end + 1" 且要防溢出。为 u32/u128 各实现一次，
/// 避免为这一个方法引入 num-traits 依赖。
mod num_like {
    pub trait NumLike {
        fn saturating_add_one(self) -> Self;
    }
    impl NumLike for u32 {
        fn saturating_add_one(self) -> Self {
            self.saturating_add(1)
        }
    }
    impl NumLike for u128 {
        fn saturating_add_one(self) -> Self {
            self.saturating_add(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    // 别名是为了避开 clippy::type_complexity —— 裸写四层嵌套会被拦下
    type CidrSpec<'a> = (&'a [u8], u32);
    type GeoIpSpec<'a> = (&'a str, &'a [CidrSpec<'a>]);

    fn encode_geoip_list(entries: &[GeoIpSpec<'_>]) -> Vec<u8> {
        fn varint(v: u64, out: &mut Vec<u8>) {
            let mut v = v;
            loop {
                let b = (v & 0x7f) as u8;
                v >>= 7;
                if v == 0 {
                    out.push(b);
                    return;
                }
                out.push(b | 0x80);
            }
        }
        fn field(num: u32, wire: u8, out: &mut Vec<u8>) {
            varint(((num as u64) << 3) | wire as u64, out);
        }
        fn delimited(num: u32, payload: &[u8], out: &mut Vec<u8>) {
            field(num, 2, out);
            varint(payload.len() as u64, out);
            out.extend_from_slice(payload);
        }

        let mut list = Vec::new();
        for (code, cidrs) in entries {
            let mut geoip = Vec::new();
            delimited(1, code.as_bytes(), &mut geoip);
            for (ip, prefix) in *cidrs {
                let mut c = Vec::new();
                delimited(1, ip, &mut c);
                field(2, 0, &mut c);
                varint(*prefix as u64, &mut c);
                delimited(2, &c, &mut geoip);
            }
            delimited(1, &geoip, &mut list);
        }
        list
    }

    #[test]
    fn matches_v4_inside_and_outside() {
        // 192.168.0.0/16
        let buf = encode_geoip_list(&[("private", &[(&[192, 168, 0, 0], 16)])]);
        let db = IpDb::parse(&buf).unwrap();
        assert!(db.matches("private", Ipv4Addr::new(192, 168, 1, 1).into()));
        assert!(db.matches("private", Ipv4Addr::new(192, 168, 255, 255).into()));
        assert!(!db.matches("private", Ipv4Addr::new(192, 169, 0, 1).into()));
        assert!(!db.matches("private", Ipv4Addr::new(10, 0, 0, 1).into()));
    }

    #[test]
    fn prefix_zero_matches_everything() {
        let buf = encode_geoip_list(&[("all", &[(&[0, 0, 0, 0], 0)])]);
        let db = IpDb::parse(&buf).unwrap();
        assert!(db.matches("all", Ipv4Addr::new(8, 8, 8, 8).into()));
    }

    #[test]
    fn full_prefix_matches_single_address() {
        let buf = encode_geoip_list(&[("one", &[(&[1, 2, 3, 4], 32)])]);
        let db = IpDb::parse(&buf).unwrap();
        assert!(db.matches("one", Ipv4Addr::new(1, 2, 3, 4).into()));
        assert!(!db.matches("one", Ipv4Addr::new(1, 2, 3, 5).into()));
    }

    #[test]
    fn v6_is_kept_separate_from_v4() {
        let v6 = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let buf = encode_geoip_list(&[("doc", &[(&v6, 32)])]);
        let db = IpDb::parse(&buf).unwrap();
        assert!(db.matches("doc", "2001:db8::1".parse::<Ipv6Addr>().unwrap().into()));
        // v4 地址绝不能落进 v6 集合
        assert!(!db.matches("doc", Ipv4Addr::new(32, 1, 13, 184).into()));
    }

    #[test]
    fn overlapping_ranges_are_merged() {
        // 10.0.0.0/8 与 10.1.0.0/16 重叠，合并后应只剩一段
        let buf = encode_geoip_list(&[("cn", &[(&[10, 0, 0, 0], 8), (&[10, 1, 0, 0], 16)])]);
        let db = IpDb::parse(&buf).unwrap();
        assert_eq!(db.range_count("cn"), 1);
        assert!(db.matches("cn", Ipv4Addr::new(10, 1, 2, 3).into()));
    }

    #[test]
    fn malformed_ip_length_is_rejected() {
        // 3 字节既不是 v4 也不是 v6 —— 必须报错，不能猜
        let buf = encode_geoip_list(&[("bad", &[(&[1, 2, 3], 24)])]);
        assert!(IpDb::parse(&buf).is_err());
    }

    #[test]
    fn unknown_class_never_matches() {
        let buf = encode_geoip_list(&[("cn", &[(&[10, 0, 0, 0], 8)])]);
        let db = IpDb::parse(&buf).unwrap();
        assert!(!db.matches("us", Ipv4Addr::new(10, 0, 0, 1).into()));
    }
}
