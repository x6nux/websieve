//! geosite.dat 的解析与域名匹配。

use std::collections::{HashMap, HashSet};

use crate::pb::{PbError, Reader};

/// 一个类别（如 "cn"、"category-ads"）的域名匹配集。
#[derive(Default)]
struct SiteClass {
    /// Domain.Type = Full：精确匹配
    full: HashSet<String>,
    /// Domain.Type = Domain：后缀匹配，用反转标签 trie
    suffix: DomainTrie,
    /// Domain.Type = Substr：关键词包含。数量少，线性扫即可
    substr: Vec<String>,
}

pub struct SiteDb {
    classes: HashMap<String, SiteClass>,
    skipped_regex: usize,
}

impl SiteDb {
    pub fn parse(buf: &[u8]) -> Result<Self, PbError> {
        let mut classes: HashMap<String, SiteClass> = HashMap::new();
        let mut skipped_regex = 0usize;

        let mut r = Reader::new(buf);
        while !r.is_empty() {
            let (field, wire) = r.tag()?;
            if field != 1 || wire != 2 {
                r.skip(wire)?;
                continue;
            }
            // GeoSiteList.entry
            let entry = r.bytes()?;
            let (code, class, skipped) = parse_geosite(entry)?;
            skipped_regex += skipped;
            // 同一 code 出现多次时合并而非覆盖
            let slot = classes.entry(code).or_default();
            slot.full.extend(class.full);
            slot.substr.extend(class.substr);
            slot.suffix.merge(class.suffix);
        }
        Ok(Self { classes, skipped_regex })
    }

    pub fn has(&self, code: &str) -> bool {
        self.classes.contains_key(&normalize_code(code))
    }

    /// 类别不存在时返回 false —— 由调用方在加载阶段校验并报警，
    /// 判决路径上绝不因为一个缺失的类别而中断整条连接。
    pub fn matches(&self, code: &str, domain: &str) -> bool {
        let Some(class) = self.classes.get(&normalize_code(code)) else {
            return false;
        };
        let d = domain.trim_end_matches('.').to_ascii_lowercase();
        if class.full.contains(&d) {
            return true;
        }
        if class.suffix.matches(&d) {
            return true;
        }
        class.substr.iter().any(|s| d.contains(s.as_str()))
    }

    /// ponytail: 不支持 Domain.Type = Regex，解析时跳过并计数。
    /// 上限：带正则的 geosite 条目不会命中，表现为漏匹配（绝不会错匹配）。
    /// 升级路径：若实测漏匹配显著，引入 regex crate 并在此加一个 Vec<Regex>。
    pub fn skipped_regex(&self) -> usize {
        self.skipped_regex
    }
}

fn normalize_code(code: &str) -> String {
    code.trim().to_ascii_lowercase()
}

fn parse_geosite(buf: &[u8]) -> Result<(String, SiteClass, usize), PbError> {
    let mut code = String::new();
    let mut class = SiteClass::default();
    let mut skipped = 0usize;

    let mut r = Reader::new(buf);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, 2) => code = r.string()?,
            (2, 2) => {
                let dom = r.bytes()?;
                match parse_domain(dom)? {
                    Some((0, v)) => class.substr.push(v),
                    Some((2, v)) => class.suffix.insert(&v),
                    Some((3, v)) => {
                        class.full.insert(v);
                    }
                    Some((1, _)) => skipped += 1, // Regex
                    Some((_, _)) | None => skipped += 1,
                }
            }
            _ => r.skip(wire)?,
        }
    }
    Ok((normalize_code(&code), class, skipped))
}

/// 返回 (type, value)。attribute 字段整体跳过 —— 我们不做属性过滤。
fn parse_domain(buf: &[u8]) -> Result<Option<(u64, String)>, PbError> {
    let mut kind = 0u64;
    let mut value: Option<String> = None;

    let mut r = Reader::new(buf);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, 0) => kind = r.varint()?,
            (2, 2) => value = Some(r.string()?.trim_end_matches('.').to_ascii_lowercase()),
            _ => r.skip(wire)?,
        }
    }
    Ok(value.map(|v| (kind, v)))
}

/// 反转标签 trie：按 `.` 分段、从右向左插入与查询。
///
/// 这样「边界必须落在点上」这件事由结构本身保证，不需要额外的字符串比较 ——
/// notexample.com 与 example.com 在第一层（com）之后就分叉了。
#[derive(Default)]
struct DomainTrie {
    root: TrieNode,
}

#[derive(Default)]
struct TrieNode {
    children: HashMap<String, TrieNode>,
    terminal: bool,
}

impl DomainTrie {
    fn insert(&mut self, domain: &str) {
        let mut node = &mut self.root;
        for seg in domain.rsplit('.') {
            node = node.children.entry(seg.to_string()).or_default();
        }
        node.terminal = true;
    }

    fn matches(&self, domain: &str) -> bool {
        let mut node = &self.root;
        for seg in domain.rsplit('.') {
            match node.children.get(seg) {
                Some(n) => {
                    node = n;
                    // 命中一个终结节点即可 —— 它是查询域名的某级父域
                    if node.terminal {
                        return true;
                    }
                }
                None => return false,
            }
        }
        false
    }

    fn merge(&mut self, other: DomainTrie) {
        merge_node(&mut self.root, other.root);
    }
}

fn merge_node(dst: &mut TrieNode, src: TrieNode) {
    dst.terminal |= src.terminal;
    for (k, v) in src.children {
        merge_node(dst.children.entry(k).or_default(), v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工编码一个最小的 GeoSiteList，避免测试依赖外部 .dat 文件。
    fn encode_geosite_list(entries: &[(&str, &[(u8, &str)])]) -> Vec<u8> {
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
        for (code, domains) in entries {
            let mut site = Vec::new();
            delimited(1, code.as_bytes(), &mut site); // GeoSite.code
            for (kind, value) in *domains {
                let mut dom = Vec::new();
                field(1, 0, &mut dom); // Domain.type
                varint(*kind as u64, &mut dom);
                delimited(2, value.as_bytes(), &mut dom); // Domain.value
                delimited(2, &dom, &mut site); // GeoSite.domain
            }
            delimited(1, &site, &mut list); // GeoSiteList.entry
        }
        list
    }

    #[test]
    fn parses_codes_and_domains() {
        let buf = encode_geosite_list(&[
            ("cn", &[(3, "baidu.com"), (2, "qq.com")]),
            ("category-ads", &[(0, "doubleclick")]),
        ]);
        let db = SiteDb::parse(&buf).unwrap();
        assert!(db.has("cn"));
        assert!(db.has("category-ads"));
        assert!(!db.has("us"));
    }

    #[test]
    fn full_matches_exactly_only() {
        let buf = encode_geosite_list(&[("t", &[(3, "example.com")])]);
        let db = SiteDb::parse(&buf).unwrap();
        assert!(db.matches("t", "example.com"));
        assert!(!db.matches("t", "a.example.com"));
    }

    #[test]
    fn domain_matches_self_and_subdomains_on_label_boundary() {
        let buf = encode_geosite_list(&[("t", &[(2, "example.com")])]);
        let db = SiteDb::parse(&buf).unwrap();
        assert!(db.matches("t", "example.com"), "应匹配自身");
        assert!(db.matches("t", "a.b.example.com"), "应匹配多级子域");
        // 关键边界：不能把 notexample.com 当成 example.com 的子域
        assert!(!db.matches("t", "notexample.com"));
    }

    #[test]
    fn substr_matches_anywhere() {
        let buf = encode_geosite_list(&[("t", &[(0, "google")])]);
        let db = SiteDb::parse(&buf).unwrap();
        assert!(db.matches("t", "www.google.com"));
        assert!(db.matches("t", "googleapis.com"));
        assert!(!db.matches("t", "example.com"));
    }

    #[test]
    fn regex_entries_are_skipped_and_counted() {
        let buf = encode_geosite_list(&[("t", &[(1, ".*\\.example\\.com"), (3, "keep.com")])]);
        let db = SiteDb::parse(&buf).unwrap();
        assert_eq!(db.skipped_regex(), 1, "Regex 条目应被计数");
        assert!(db.matches("t", "keep.com"), "同类别的其他条目不受影响");
    }

    #[test]
    fn unknown_class_never_matches() {
        let buf = encode_geosite_list(&[("t", &[(3, "a.com")])]);
        let db = SiteDb::parse(&buf).unwrap();
        assert!(!db.matches("nonexistent", "a.com"));
    }
}
