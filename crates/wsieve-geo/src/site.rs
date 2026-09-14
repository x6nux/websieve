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
    skipped_entries: usize,
}

impl SiteDb {
    pub fn parse(buf: &[u8]) -> Result<Self, PbError> {
        let mut classes: HashMap<String, SiteClass> = HashMap::new();
        let mut skipped_entries = 0usize;

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
            skipped_entries += skipped;
            // 同一 code 出现多次时合并而非覆盖
            let slot = classes.entry(code).or_default();
            slot.full.extend(class.full);
            slot.substr.extend(class.substr);
            slot.suffix.merge(class.suffix);
        }
        Ok(Self { classes, skipped_entries })
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

    /// 解析时被丢弃的条目总数。三个来源：
    /// 1. Domain.Type = Regex —— 不支持正则（见下）；
    /// 2. 空值条目 —— 空 Substr 会让匹配恒真，必须拦；
    /// 3. 标签数超过 MAX_LABELS 的超深域名 —— 见 DomainTrie::insert。
    ///
    /// 名字不叫 skipped_regex：三个来源里只有第一个与正则有关，
    /// 旧名会让读者把「空值」「超深」两类丢弃误读成正则条目，
    /// 对着一个不存在的正则问题排查。
    ///
    /// ponytail: 不支持 Domain.Type = Regex，解析时跳过并计数。
    /// 上限：带正则的 geosite 条目不会命中，表现为漏匹配（绝不会错匹配）。
    /// 升级路径：若实测漏匹配显著，引入 regex crate 并在此加一个 Vec<Regex>。
    pub fn skipped_entries(&self) -> usize {
        self.skipped_entries
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
                    // 空值一律拦下。致命的是 Substr：匹配侧是 d.contains(s)，
                    // 而 "anything".contains("") 恒为真 —— 一条空 Substr 就能把
                    // GEOSITE,cn,DIRECT 变成「匹配一切」，让自以为在走代理的用户
                    // 全程明文直连。且触发条件不止于字段真的为空：parse_domain 会把
                    // 值 "." 规范化成空串。
                    // full / suffix 的空值实测无害（精确比较不中、rsplit 也不中），
                    // 但一并拦掉，比日后重新推导一遍这个论证便宜。
                    Some((_, v)) if v.is_empty() => skipped += 1,
                    Some((0, v)) => class.substr.push(v),
                    Some((2, v)) => {
                        if !class.suffix.insert(&v) {
                            skipped += 1;
                        }
                    }
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

/// 单条域名允许的最大标签数。
///
/// DNS 本身的上限是 127 个标签（255 字节报文、每标签至少占 2 字节），
/// 128 留一格余量，正常数据不可能触顶。这个上限不是性能考虑而是安全边界：
/// trie 深度等于标签数，而标签数完全由下载来的 .dat 文件说了算。
const MAX_LABELS: usize = 128;

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

/// 手写迭代式 Drop，替代编译器生成的递归 Drop。
///
/// 默认的递归 Drop 每层深度吃一个栈帧，实测约 4000 层标签（≈8KB 输入）
/// 就能把测试线程的栈打爆 —— 而 Rust 的栈溢出是 SIGABRT，catch_unwind
/// 接不住，整个代理进程直接消失。MAX_LABELS 已经堵死了今天的入口，
/// 但结构本身不该留这颗雷：这里把子树摘进显式栈里逐个释放，
/// 深度再大也只吃堆内存。
impl Drop for TrieNode {
    fn drop(&mut self) {
        let mut stack: Vec<TrieNode> = self.children.drain().map(|(_, v)| v).collect();
        while let Some(mut node) = stack.pop() {
            // drain 把孙子摘出来交给显式栈；node 自身随即析构时
            // children 已空，不会再触发递归。
            stack.extend(node.children.drain().map(|(_, v)| v));
        }
    }
}

impl DomainTrie {
    /// 插入成功返回 true；标签数超限被丢弃返回 false，由调用方计入 skipped。
    fn insert(&mut self, domain: &str) -> bool {
        if domain.split('.').count() > MAX_LABELS {
            return false;
        }
        let mut node = &mut self.root;
        for seg in domain.rsplit('.') {
            node = node.children.entry(seg.to_string()).or_default();
        }
        node.terminal = true;
        true
    }

    /// 按已经反转好的标签序列插入（merge 用）。不再做长度检查 ——
    /// 两棵树的深度都已在各自 insert 时受 MAX_LABELS 约束。
    fn insert_labels(&mut self, labels: &[String]) {
        let mut node = &mut self.root;
        for seg in labels {
            node = node.children.entry(seg.clone()).or_default();
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

    /// 迭代式合并。与 Drop 同理：递归版本的深度由输入决定，而输入是网上
    /// 下载来的，所以深度必须转移到堆上。
    ///
    /// 做法是遍历 src 的终结路径再逐条插进 dst，而不是逐节点对齐两棵树 ——
    /// 后者要同时持有 dst 上多个互不重叠的可变位置，安全 Rust 表达不了，
    /// 而裸指针版本在 HashMap 扩容重哈希时会失效（悬垂），并不可靠。
    ///
    /// 路径用一个复用的缓冲区配合 Up 标记回溯，而不是每个孩子克隆一份 ——
    /// 后者在单链深树上是 O(深度²)，实测会把测试挂死。现在总代价与
    /// 输入规模同阶。语义等价：insert 总在路径末端置 terminal，
    /// 所以遍历终结路径不会丢掉任何结构。
    fn merge(&mut self, other: DomainTrie) {
        enum Step {
            /// 进入一个子节点：带上它的标签与子树
            Down(String, TrieNode),
            /// 回溯：弹掉一层标签
            Up,
        }

        let mut root = other.root;
        if root.terminal {
            self.root.terminal = true;
        }
        let mut path: Vec<String> = Vec::new();
        let mut stack: Vec<Step> = root
            .children
            .drain()
            .map(|(k, v)| Step::Down(k, v))
            .collect();

        while let Some(step) = stack.pop() {
            match step {
                Step::Up => {
                    path.pop();
                }
                Step::Down(label, mut node) => {
                    path.push(label);
                    if node.terminal {
                        self.insert_labels(&path);
                    }
                    if node.children.is_empty() {
                        path.pop();
                    } else {
                        // 先压 Up：它会在这棵子树的所有孩子处理完之后才弹出
                        stack.push(Step::Up);
                        stack.extend(node.children.drain().map(|(k, v)| Step::Down(k, v)));
                    }
                }
            }
        }
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
        assert_eq!(db.skipped_entries(), 1, "Regex 条目应被计数");
        assert!(db.matches("t", "keep.com"), "同类别的其他条目不受影响");
    }

    #[test]
    fn unknown_class_never_matches() {
        let buf = encode_geosite_list(&[("t", &[(3, "a.com")])]);
        let db = SiteDb::parse(&buf).unwrap();
        assert!(!db.matches("nonexistent", "a.com"));
    }

    fn deep_domain(labels: usize) -> String {
        let mut s = String::with_capacity(labels * 2);
        for i in 0..labels {
            if i > 0 {
                s.push('.');
            }
            s.push('a');
        }
        s
    }

    /// B1：超深标签的条目必须被丢弃，而不是把进程打崩。
    ///
    /// 修复前：这条输入（约 40KB）会让递归 Drop 吃爆栈，
    /// 输出 `fatal runtime error: stack overflow, aborting` 并 SIGABRT。
    /// 栈溢出不是 panic，catch_unwind 接不住 —— 整个代理进程消失。
    /// 实测阈值约 4000 层标签（≈8KB 输入），单条即可触发。
    #[test]
    fn absurdly_deep_domain_is_rejected_not_fatal() {
        let deep = deep_domain(20_000);
        let buf = encode_geosite_list(&[("t", &[(2, deep.as_str()), (3, "keep.com")])]);
        let db = SiteDb::parse(&buf).unwrap();
        assert_eq!(db.skipped_entries(), 1, "超深条目应被计入 skipped");
        assert!(!db.matches("t", &deep), "被丢弃的条目不应能匹配");
        assert!(db.matches("t", "keep.com"), "同类别的正常条目不受影响");
        // 函数正常返回本身就是断言：db 在此处析构，迭代式 Drop 不能爆栈
    }

    /// B1 边界：恰好卡在 MAX_LABELS 上的条目要留下，超一个就丢。
    #[test]
    fn label_cap_boundary_is_inclusive() {
        let at_cap = deep_domain(MAX_LABELS);
        let over_cap = deep_domain(MAX_LABELS + 1);
        let buf = encode_geosite_list(&[(
            "t",
            &[(2, at_cap.as_str()), (2, over_cap.as_str())],
        )]);
        let db = SiteDb::parse(&buf).unwrap();
        assert!(db.matches("t", &at_cap), "128 层应被接受");
        assert_eq!(db.skipped_entries(), 1, "129 层应被丢弃且计数");
    }

    /// B1：merge 路径同样不能递归 —— 同一 code 出现两次会走 merge。
    #[test]
    fn merging_deep_tries_does_not_recurse() {
        // 两个同名 code，各带一条接近上限的深域名，强制走 merge_node
        let a = deep_domain(MAX_LABELS);
        let mut b = deep_domain(MAX_LABELS - 1);
        b.push_str(".b");
        let buf = encode_geosite_list(&[
            ("t", &[(2, a.as_str())]),
            ("t", &[(2, b.as_str())]),
        ]);
        let db = SiteDb::parse(&buf).unwrap();
        assert!(db.matches("t", &a), "合并后第一棵的条目仍在");
        assert!(db.matches("t", &b), "合并后第二棵的条目仍在");
    }

    /// B2：空 Substr 会让 d.contains("") 恒为真，把 GEOSITE,cn,DIRECT
    /// 变成「匹配一切」—— 用户以为在走代理，实际全程明文。必须拦死。
    #[test]
    fn empty_substr_does_not_match_everything() {
        let buf = encode_geosite_list(&[("cn", &[(0, ""), (3, "baidu.com")])]);
        let db = SiteDb::parse(&buf).unwrap();
        assert!(!db.matches("cn", "mybank.com"), "空 Substr 绝不能匹配任意域名");
        assert!(!db.matches("cn", "login.microsoft.com"));
        assert!(db.matches("cn", "baidu.com"), "同类别的正常条目不受影响");
        assert_eq!(db.skipped_entries(), 1, "空条目应被计数");
    }

    /// B2 的隐蔽入口：值 "." 会被 parse_domain 规范化成空串，
    /// 所以「字段非空」不等于「值非空」。
    #[test]
    fn dot_only_substr_normalizes_to_empty_and_is_rejected() {
        let buf = encode_geosite_list(&[("cn", &[(0, "."), (3, "baidu.com")])]);
        let db = SiteDb::parse(&buf).unwrap();
        assert!(!db.matches("cn", "mybank.com"), "\".\" 规范化后为空，同样必须拦");
        assert!(db.matches("cn", "baidu.com"));
        assert_eq!(db.skipped_entries(), 1);
    }

    /// B2：full / suffix 的空值实测无害，但一并拦掉并计数。
    #[test]
    fn empty_full_and_suffix_are_rejected_too() {
        let buf = encode_geosite_list(&[("cn", &[(3, ""), (2, ""), (3, "baidu.com")])]);
        let db = SiteDb::parse(&buf).unwrap();
        assert!(!db.matches("cn", "mybank.com"));
        assert!(db.matches("cn", "baidu.com"));
        assert_eq!(db.skipped_entries(), 2, "空 full 与空 suffix 各计一次");
    }
}
