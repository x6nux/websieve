//! protobuf wire format 的最小读取器。
//!
//! 只覆盖 geodat.proto 实际用到的部分：varint(0)、length-delimited(2)，
//! 外加 64-bit(1) / 32-bit(5) 的跳过能力。groups（3/4）已在 proto3 废弃，
//! 遇到即报错。

#[derive(Debug, thiserror::Error)]
pub enum PbError {
    #[error("数据被截断")]
    Truncated,
    #[error("varint 超过 64 位")]
    VarintOverflow,
    #[error("不支持的 wire type: {0}")]
    BadWireType(u8),
    #[error("字符串不是合法 UTF-8")]
    BadUtf8,
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    pub fn varint(&mut self) -> Result<u64, PbError> {
        let mut v = 0u64;
        let mut shift = 0u32;
        loop {
            let b = *self.buf.get(self.pos).ok_or(PbError::Truncated)?;
            self.pos += 1;
            // 第 10 个字节只允许贡献 1 位（64 = 9*7 + 1）
            if shift >= 64 {
                return Err(PbError::VarintOverflow);
            }
            // protobuf 的 u64 varint 最多 10 字节：前 9 个各贡献 7 位（63 位），
            // 第 10 个只剩 1 位可用。此时若高位非零，说明这个数超出 u64 ——
            // 必须报错，不能让 << 63 把它们静默移出去。
            if shift == 63 && b & 0x7f > 1 {
                return Err(PbError::VarintOverflow);
            }
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
        }
    }

    /// 返回 (field_number, wire_type)。
    pub fn tag(&mut self) -> Result<(u32, u8), PbError> {
        let t = self.varint()?;
        Ok(((t >> 3) as u32, (t & 7) as u8))
    }

    /// 读一段 length-delimited 数据，返回借用切片（零拷贝）。
    pub fn bytes(&mut self) -> Result<&'a [u8], PbError> {
        let len = self.varint()? as usize;
        let end = self.pos.checked_add(len).ok_or(PbError::Truncated)?;
        let s = self.buf.get(self.pos..end).ok_or(PbError::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    pub fn string(&mut self) -> Result<String, PbError> {
        let b = self.bytes()?;
        String::from_utf8(b.to_vec()).map_err(|_| PbError::BadUtf8)
    }

    /// 跳过一个不认识的字段。geo 文件将来加字段时，我们必须还能读。
    pub fn skip(&mut self, wire: u8) -> Result<(), PbError> {
        match wire {
            0 => {
                self.varint()?;
            }
            1 => self.advance(8)?,
            2 => {
                self.bytes()?;
            }
            5 => self.advance(4)?,
            other => return Err(PbError::BadWireType(other)),
        }
        Ok(())
    }

    fn advance(&mut self, n: usize) -> Result<(), PbError> {
        let end = self.pos.checked_add(n).ok_or(PbError::Truncated)?;
        if end > self.buf.len() {
            return Err(PbError::Truncated);
        }
        self.pos = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_decodes_multibyte() {
        // 300 = 0b10_0101100 → varint 编码 [0xAC, 0x02]
        let mut r = Reader::new(&[0xAC, 0x02]);
        assert_eq!(r.varint().unwrap(), 300);
        assert!(r.is_empty());
    }

    #[test]
    fn tag_splits_field_and_wire() {
        // field 2, wire 2 → (2<<3)|2 = 0x12
        let mut r = Reader::new(&[0x12]);
        assert_eq!(r.tag().unwrap(), (2, 2));
    }

    #[test]
    fn bytes_reads_length_delimited() {
        let mut r = Reader::new(&[0x03, b'a', b'b', b'c']);
        assert_eq!(r.bytes().unwrap(), b"abc");
    }

    #[test]
    fn truncated_input_errors_not_panics() {
        // 声明 5 字节却只给 2 —— 必须报错而不是 panic 或静默截断
        let mut r = Reader::new(&[0x05, b'a', b'b']);
        assert!(matches!(r.bytes(), Err(PbError::Truncated)));
    }

    #[test]
    fn unknown_field_is_skipped_safely() {
        // 未知 field 9 wire 0（varint 300），后跟 field 1 wire 2 ("hi")
        let mut r = Reader::new(&[0x48, 0xAC, 0x02, 0x0A, 0x02, b'h', b'i']);
        let (f, w) = r.tag().unwrap();
        assert_eq!((f, w), (9, 0));
        r.skip(w).unwrap();
        let (f, w) = r.tag().unwrap();
        assert_eq!((f, w), (1, 2));
        assert_eq!(r.bytes().unwrap(), b"hi");
    }

    #[test]
    fn varint_overflow_errors() {
        // 11 个带续位的字节 —— 超过 u64 能表示的范围
        let mut r = Reader::new(&[0xFF; 11]);
        assert!(r.varint().is_err());
    }

    #[test]
    fn overlong_tenth_byte_errors_instead_of_truncating() {
        // 10 字节 varint 的第 10 字节只能是 0 或 1。给 0x02 意味着
        // 这个数超出 u64，必须报错而不是把高位静默移出去。
        let mut buf = vec![0xFF; 9];
        buf.push(0x02);
        let mut r = Reader::new(&buf);
        assert!(matches!(r.varint(), Err(PbError::VarintOverflow)));
    }

    #[test]
    fn maximal_valid_varint_still_decodes() {
        // 反向守住：合法的 u64::MAX（第 10 字节为 0x01）不能被误判为溢出
        let mut buf = vec![0xFF; 9];
        buf.push(0x01);
        let mut r = Reader::new(&buf);
        assert_eq!(r.varint().unwrap(), u64::MAX);
    }
}
