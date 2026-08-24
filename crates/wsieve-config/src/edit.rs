//! 规则的行级定点改写（设计文档 §5.6）。
//!
//! 本模块的存在理由是一条纪律：**结构化保存必须保留规则注释**。
//! 因此写回**不走** serde 序列化 —— 走序列化就等于把用户手写的
//! 缩进、前导注释、行尾注释全部归一化掉，UI 上点一次开关就全没了。
//!
//! 为什么连 serde-saphyr 的 `Commented<T>` 也不行：它是 `(pub T, pub String)`，
//! 只有**一个** String，分不开「键上方的注释」与「同一行行尾的注释」；
//! 而 `comment_position` 又是序列化器的**全局**选项。也就是说无论怎么配，
//! 第一次保存就会把用户原文重排一遍。读走 serde、写走字节，这个不对称是有意的。
//!
//! 取而代之的是按 `Spanned<String>` 携带的行号定位到规则所在行，
//! 只替换该行 `- ` 之后、行尾注释之前的那一段，文件其余部分一个字节都不动。
//! 「其余部分不变」不是靠小心翼翼地重建，而是靠**原样透传**：
//! 非目标行连看都不看，直接 push 原字符串。
//!
//! 已知限制（都以报错或良性降级收场，不会损坏文件）：
//!
//! 1. **值里带空格又带 `#` 的引号规则**（`- "A #B"`）会被误判成行尾注释。
//!    Clash 语法里字段中不出现空格，故实际写不出这种值。注意「值里含 `#`」
//!    本身是安全的：注释起点按 YAML 真实规则判定（见 `find_comment_start`）
//! 2. **流式序列**（`rules: [A, B]`）所有规则挤在一行，这里返回
//!    `NotASequenceItem` 而非破坏文件。UI 应提示用户改用块式序列
//! 3. **带引号的规则**（`- "DOMAIN,a.com,PROXY"`）被替换后引号会消失。
//!    值仍能正确读回，是良性的，但看到的人可能会意外

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("行号 {0} 超出范围（文件共 {1} 行）")]
    LineOutOfRange(u64, usize),
    #[error("第 {0} 行不是一个规则项（找不到 `- ` 前缀，或该项为空）")]
    NotASequenceItem(u64),
    #[error("规则值不能跨行：{0:?}。写进去会把一行撑成两行，破坏 YAML 结构")]
    ValueSpansLines(String),
    #[error("规则值不能为空。写进去会退化成裸 `-`，读回来就不再是一条规则")]
    EmptyValue,
}

/// 把第 `line` 行（1-based）的规则值换成 `new_value`。
/// 缩进、行尾注释、值与注释之间的对齐空白、行尾换行符（LF / CRLF）全部保留。
pub fn replace_rule_line(src: &str, line: u64, new_value: &str) -> Result<String, EditError> {
    check_new_value(new_value)?;

    let lines: Vec<&str> = split_keep_ends(src);
    let idx = check_index(line, lines.len())?;

    let (body, eol) = split_eol(lines[idx]);
    let item = split_item(body, line)?;

    // 值与注释之间的空白照原样留着，视觉对齐不被破坏
    let gap: String = item
        .value
        .chars()
        .rev()
        .take_while(|c| c.is_whitespace())
        .collect();

    let mut out = String::with_capacity(src.len() + new_value.len());
    for (i, l) in lines.iter().enumerate() {
        if i == idx {
            out.push_str(item.prefix);
            out.push_str(new_value);
            out.push_str(&gap);
            out.push_str(item.comment);
            out.push_str(eol);
        } else {
            // 非目标行原样透传 —— 「其余字节不变」由此在构造上成立，而非靠断言
            out.push_str(l);
        }
    }
    Ok(out)
}

/// 删除第 `line` 行的规则，连同紧贴它上方的前导注释块一起删。
///
/// 「紧贴」的定义：从目标行往上，连续的、**缩进相同**的纯注释行。
/// 中间一旦出现空行、缩进不同的注释、或别的规则就停 —— 那些注释不属于这一条。
/// 缩进不同的注释（典型如顶格写的段落标题）宁可留成孤儿也不删：
/// 本模块存在的全部意义就是不弄丢用户写的字。
pub fn delete_rule_line(src: &str, line: u64) -> Result<String, EditError> {
    let lines: Vec<&str> = split_keep_ends(src);
    let idx = check_index(line, lines.len())?;
    let (body, _) = split_eol(lines[idx]);
    split_item(body, line)?;

    let want_indent = indent_width(body);
    let mut start = idx;
    while start > 0 {
        let (prev, _) = split_eol(lines[start - 1]);
        if prev.trim_start().starts_with('#') && indent_width(prev) == want_indent {
            start -= 1;
        } else {
            break;
        }
    }

    let mut out = String::with_capacity(src.len());
    for (i, l) in lines.iter().enumerate() {
        if i < start || i > idx {
            out.push_str(l);
        }
    }
    Ok(out)
}

/// 一行序列项拆成三段：`- ` 及其之前的原样前缀、值、行尾注释。
/// 三段首尾相接即原行正文，这是「同值替换是恒等操作」的依据。
struct Item<'a> {
    prefix: &'a str,
    value: &'a str,
    comment: &'a str,
}

/// 把序列项正文拆成 prefix / value / comment；不是规则项就报错。
fn split_item(body: &str, line: u64) -> Result<Item<'_>, EditError> {
    let dash = find_dash(body).ok_or(EditError::NotASequenceItem(line))?;
    // 切片边界安全性：`dash` 由 find_dash 保证正好落在 ASCII `- ` 的右边界上，
    // 其左侧只有 ASCII 空白，故必是 char 边界 —— 规则值里的中文（如「日本节点」）
    // 或 IDN 域名不会被从中间劈开。
    let (prefix, after_dash) = (&body[..dash], &body[dash..]);
    // find_comment_start 返回的下标来自 char_indices，同样是 char 边界，
    // 所以行尾注释里的中文一并安全。
    let (value, comment) = match find_comment_start(after_dash) {
        Some(c) => (&after_dash[..c], &after_dash[c..]),
        None => (after_dash, ""),
    };
    // `- # 只有注释` 与 `- ` 都是 YAML 的空序列项，不是规则。
    // 照写会把新值糊到注释前面，读回来变成「值+注释」一整坨 —— 与裸 `-` 同类，一并拒绝。
    if value.trim().is_empty() {
        return Err(EditError::NotASequenceItem(line));
    }
    Ok(Item {
        prefix,
        value,
        comment,
    })
}

/// 行尾注释的起点。按 YAML 的真实规则：`#` 只有在**前面是空白**
/// （或位于该区段开头）时才开启注释，否则它只是值里的一个普通字符。
///
/// 实测 `- MATCH,DIRECT#兜底` 读回来的值就是 `MATCH,DIRECT#兜底` 整串。
/// 若按「第一个 `#` 即注释起点」去切，同值替换会写出 `MATCH,DIRECT#兜底#兜底`
/// —— 一次静默的文件损坏，正是本模块要防的事。
fn find_comment_start(s: &str) -> Option<usize> {
    // 区段开头等同于「前面是空白」：`- #foo` 里的 `#` 确实开启注释
    let mut prev_is_space = true;
    for (i, c) in s.char_indices() {
        if c == '#' && prev_is_space {
            return Some(i);
        }
        prev_is_space = c.is_whitespace();
    }
    None
}

/// 新值的写入前校验。这两种值写进去都会让文件读回来不再是原意，
/// 属于「静默损坏」，必须当场报错而不是照写。
fn check_new_value(new_value: &str) -> Result<(), EditError> {
    if new_value.contains('\n') || new_value.contains('\r') {
        return Err(EditError::ValueSpansLines(new_value.to_string()));
    }
    if new_value.trim().is_empty() {
        return Err(EditError::EmptyValue);
    }
    Ok(())
}

fn check_index(line: u64, total: usize) -> Result<usize, EditError> {
    if line == 0 || line as usize > total {
        return Err(EditError::LineOutOfRange(line, total));
    }
    Ok(line as usize - 1)
}

/// 按行切分但**保留**行尾换行符，这样重组时 LF / CRLF 原样还原。
/// 用 `split_inclusive` 而非手写字节游标：它按 char 切，落点不可能不是 char 边界。
fn split_keep_ends(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

/// 拆成 (正文, 行尾换行符)。
fn split_eol(line: &str) -> (&str, &str) {
    if let Some(b) = line.strip_suffix("\r\n") {
        (b, "\r\n")
    } else if let Some(b) = line.strip_suffix('\n') {
        (b, "\n")
    } else {
        (line, "")
    }
}

/// 前导空白的字节宽度。YAML 缩进只能是空格（ASCII），故字节宽度即视觉列数。
fn indent_width(body: &str) -> usize {
    body.len() - body.trim_start().len()
}

/// 返回 `- ` 之后第一个字符的下标。
///
/// 只接受 `- ` 开头的序列项。裸 `-`（YAML 里合法的空序列项）**拒绝**：
/// 它不是一条规则，而且它只有 indent+1 个字节 —— 若按 indent+2 切片会
/// 直接 panic（`byte index 4 is out of bounds of "  -"`）。
/// 注意这里是**拒绝**而不是把下标夹到 len：夹一下确实不 panic 了，
/// 但会把值默默写到错误的位置去，比 panic 更坏 —— 静默损坏文件正是本模块要防的事。
fn find_dash(body: &str) -> Option<usize> {
    let trimmed = body.trim_start();
    if !trimmed.starts_with("- ") {
        return None;
    }
    // trimmed 以 "- " 开头 ⇒ trimmed.len() >= 2 ⇒ indent + 2 <= body.len()，
    // 且该处正好是 ASCII "- " 的右边界，必为 char 边界。
    Some(indent_width(body) + 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "\
mixed-port: 7890
rules:
  # 公司内网走直连，勿删
  - IP-CIDR,10.0.0.0/8,DIRECT,no-resolve   # 内网段
  - GEOSITE,cn,DIRECT
  - MATCH,日本节点
";

    fn rules_of(src: &str) -> Vec<(u64, String)> {
        let c = crate::load_str(src).unwrap();
        c.rules
            .iter()
            .map(|r| (r.defined.line(), r.value.clone()))
            .collect()
    }

    #[test]
    fn replacing_a_rule_keeps_every_comment() {
        let rules = rules_of(SRC);
        let (line, _) = rules[1]; // GEOSITE,cn,DIRECT
        let out = replace_rule_line(SRC, line, "GEOSITE,cn,日本节点").unwrap();

        assert!(out.contains("# 公司内网走直连，勿删"), "前导注释必须还在");
        assert!(out.contains("# 内网段"), "另一行的行尾注释必须还在");
        assert!(out.contains("GEOSITE,cn,日本节点"), "新值要写进去");
        assert!(!out.contains("GEOSITE,cn,DIRECT"), "旧值要被替换");
    }

    #[test]
    fn replacing_keeps_the_rules_own_trailing_comment() {
        let rules = rules_of(SRC);
        let (line, _) = rules[0]; // 带行尾注释的那条
        let out = replace_rule_line(SRC, line, "IP-CIDR,172.16.0.0/12,DIRECT,no-resolve").unwrap();

        assert!(out.contains("# 内网段"), "被改那一行的行尾注释也要保留");
        assert!(out.contains("IP-CIDR,172.16.0.0/12"), "新值要写进去");
    }

    #[test]
    fn alignment_gap_before_the_trailing_comment_is_preserved() {
        // 值变短了，但值与 `#` 之间的三个空格要原样留着，
        // 否则用户精心对齐的一列注释会在第一次保存后错开
        let rules = rules_of(SRC);
        let out = replace_rule_line(SRC, rules[0].0, "IP-CIDR,172.16.0.0/12,DIRECT").unwrap();
        assert!(
            out.contains("  - IP-CIDR,172.16.0.0/12,DIRECT   # 内网段\n"),
            "值与注释间的对齐空白要保留：\n{out}"
        );
    }

    #[test]
    fn indentation_is_preserved() {
        let rules = rules_of(SRC);
        let out = replace_rule_line(SRC, rules[2].0, "MATCH,DIRECT").unwrap();
        assert!(out.contains("\n  - MATCH,DIRECT"), "两空格缩进要保留：\n{out}");
    }

    #[test]
    fn untouched_bytes_are_identical() {
        let rules = rules_of(SRC);
        let out = replace_rule_line(SRC, rules[1].0, "GEOSITE,cn,DIRECT").unwrap();
        // 用同样的值替换 → 应该逐字节等于原文
        assert_eq!(out, SRC, "同值替换必须是恒等操作");
    }

    #[test]
    fn identity_replace_is_byte_exact_on_the_commented_line_too() {
        // 恒等性最容易在「有行尾注释 + 有对齐空白」这一行上破功
        let rules = rules_of(SRC);
        let out = replace_rule_line(SRC, rules[0].0, &rules[0].1).unwrap();
        assert_eq!(out, SRC, "带注释行的同值替换也必须逐字节相等");
    }

    #[test]
    fn non_ascii_value_and_comment_survive_replacement() {
        // 中文出站名 + 中文行尾注释：任何按字节切片的实现只要算错一位就会
        // panic 或吐出乱码。这里用逐字节相等钉死结果。
        let src = "rules:\n  - MATCH,日本节点   # 兜底：全走日本\n";
        let out = replace_rule_line(src, 2, "MATCH,香港节点").unwrap();
        assert_eq!(out, "rules:\n  - MATCH,香港节点   # 兜底：全走日本\n");

        let same = replace_rule_line(src, 2, "MATCH,日本节点").unwrap();
        assert_eq!(same, src, "非 ASCII 行的同值替换也必须是恒等操作");
    }

    #[test]
    fn idn_domain_suffix_survives_replacement() {
        // DOMAIN-SUFFIX 的值可能是 IDN，非 ASCII 出现在值的**中间**而非末尾
        let src = "rules:\n  - DOMAIN-SUFFIX,例子.测试,DIRECT\n  - MATCH,DIRECT\n";
        let out = replace_rule_line(src, 2, "DOMAIN-SUFFIX,示例.中国,日本节点").unwrap();
        assert_eq!(
            out,
            "rules:\n  - DOMAIN-SUFFIX,示例.中国,日本节点\n  - MATCH,DIRECT\n"
        );
    }

    #[test]
    fn a_hash_inside_the_value_is_not_mistaken_for_a_comment() {
        // YAML 只在 `#` **前面是空白**时才开启注释。实测
        // `- MATCH,DIRECT#兜底` 读回来的值就是 `MATCH,DIRECT#兜底` 整串。
        // 若按「第一个 `#` 即注释起点」切，同值替换会写出
        // `MATCH,DIRECT#兜底#兜底` —— 一次静默的文件损坏。
        let src = "rules:\n  - MATCH,DIRECT#兜底\n";
        let value = rules_of(src)[0].1.clone();
        assert_eq!(value, "MATCH,DIRECT#兜底", "先钉住 YAML 的实际取值");

        let same = replace_rule_line(src, 2, &value).unwrap();
        assert_eq!(same, src, "同值替换必须是恒等操作，不能把 `#` 后半段重复一遍");

        let out = replace_rule_line(src, 2, "MATCH,日本节点").unwrap();
        assert_eq!(out, "rules:\n  - MATCH,日本节点\n", "旧值应整串被换掉");
    }

    #[test]
    fn a_comment_only_item_is_rejected() {
        // `- # 注释` 与裸 `-` 一样是 YAML 的空序列项，不是规则。
        // 照写会把新值糊到注释前面，读回来变成「值+注释」一整坨。
        let src = "rules:\n  - # 这条以后再填\n  - MATCH,DIRECT\n";
        assert!(matches!(
            replace_rule_line(src, 2, "MATCH,PROXY"),
            Err(EditError::NotASequenceItem(2))
        ));
        assert!(matches!(
            delete_rule_line(src, 2),
            Err(EditError::NotASequenceItem(2))
        ));
    }

    #[test]
    fn deleting_a_rule_removes_its_leading_comments_too() {
        let rules = rules_of(SRC);
        let out = delete_rule_line(SRC, rules[0].0).unwrap();
        assert!(!out.contains("IP-CIDR,10.0.0.0/8"), "规则要被删掉");
        assert!(!out.contains("# 公司内网走直连"), "它的前导注释要一起删掉");
        assert!(out.contains("GEOSITE,cn,DIRECT"), "别的规则不受影响");
    }

    #[test]
    fn deleting_leaves_the_rest_byte_identical() {
        let rules = rules_of(SRC);
        let out = delete_rule_line(SRC, rules[0].0).unwrap();
        assert_eq!(
            out,
            "mixed-port: 7890\nrules:\n  - GEOSITE,cn,DIRECT\n  - MATCH,日本节点\n",
            "删除只应少掉那两行，别的一个字节都不许动"
        );
    }

    #[test]
    fn delete_stops_at_a_blank_line() {
        // 空行是分隔符：它上面的注释不属于这条规则
        let src = "rules:\n  # 段落标题\n\n  - GEOSITE,cn,DIRECT\n  - MATCH,DIRECT\n";
        let out = delete_rule_line(src, 4).unwrap();
        assert!(out.contains("# 段落标题"), "空行之上的注释不该被牵连：\n{out}");
        assert!(!out.contains("GEOSITE"), "规则本身要删掉：\n{out}");
    }

    #[test]
    fn delete_stops_at_a_differently_indented_comment() {
        // 顶格注释多半是段落标题，属于整个 rules 块而非某一条规则
        let src = "rules:\n# ===== 直连段 =====\n  - GEOSITE,cn,DIRECT\n  - MATCH,DIRECT\n";
        let out = delete_rule_line(src, 3).unwrap();
        assert!(
            out.contains("# ===== 直连段 ====="),
            "缩进不同的注释宁可留成孤儿也不能删：\n{out}"
        );
    }

    #[test]
    fn delete_stops_at_the_previous_rule() {
        let rules = rules_of(SRC);
        // MATCH 上面紧挨着的是另一条规则，不是注释 —— 只删自己这一行
        let out = delete_rule_line(SRC, rules[2].0).unwrap();
        assert!(out.contains("GEOSITE,cn,DIRECT"), "上一条规则不能被牵连：\n{out}");
        assert!(!out.contains("MATCH"), "自己要被删掉：\n{out}");
    }

    #[test]
    fn out_of_range_line_is_an_error_not_a_panic() {
        assert!(replace_rule_line(SRC, 9999, "MATCH,DIRECT").is_err());
        assert!(replace_rule_line(SRC, 0, "MATCH,DIRECT").is_err());
        assert!(delete_rule_line(SRC, 9999).is_err());
        assert!(delete_rule_line(SRC, 0).is_err());
    }

    #[test]
    fn line_without_a_dash_is_rejected() {
        // 指到 "rules:" 那一行 —— 不是规则项，必须报错
        assert!(replace_rule_line(SRC, 2, "MATCH,DIRECT").is_err());
    }

    #[test]
    fn bare_dash_is_rejected_not_panicked_on() {
        // `  -` 是合法 YAML（空序列项）但不是规则。
        // 按 `- ` 的长度去切片会越界 panic，必须当作非规则项拒绝。
        let src = "rules:\n  -\n  - MATCH,DIRECT\n";
        assert!(replace_rule_line(src, 2, "MATCH,PROXY").is_err());
        assert!(delete_rule_line(src, 2).is_err());
    }

    #[test]
    fn bare_dash_without_trailing_newline_is_rejected_too() {
        // 文件末尾没有换行时该行更短，是另一条越界路径
        let src = "rules:\n  -";
        assert!(matches!(
            replace_rule_line(src, 2, "MATCH,DIRECT"),
            Err(EditError::NotASequenceItem(2))
        ));
        assert!(matches!(
            delete_rule_line(src, 2),
            Err(EditError::NotASequenceItem(2))
        ));
    }

    #[test]
    fn flow_sequence_is_rejected_not_mangled() {
        // `rules: [A, B]` 里所有规则挤在一行，定点改写无从下手 ——
        // 要报错而不是把整行糊掉
        let src = "rules: [MATCH,DIRECT]\n";
        assert!(matches!(
            replace_rule_line(src, 1, "MATCH,PROXY"),
            Err(EditError::NotASequenceItem(1))
        ));
    }

    #[test]
    fn last_line_without_a_trailing_newline_is_editable() {
        let src = "rules:\n  - MATCH,DIRECT";
        let out = replace_rule_line(src, 2, "MATCH,日本节点").unwrap();
        assert_eq!(out, "rules:\n  - MATCH,日本节点", "不能凭空补一个换行");
    }

    #[test]
    fn crlf_input_keeps_crlf() {
        let src = SRC.replace('\n', "\r\n");
        let c = crate::load_str(&src).unwrap();
        let line = c.rules[2].defined.line();
        let out = replace_rule_line(&src, line, "MATCH,DIRECT").unwrap();
        assert!(out.contains("\r\n"), "不能把 CRLF 悄悄改成 LF");
        assert!(out.contains("  - MATCH,DIRECT\r\n"), "被改的那行也要是 CRLF");
        assert!(!out.contains("\n\n"), "不该出现裸 LF：{out:?}");
    }

    #[test]
    fn crlf_identity_replace_is_byte_exact() {
        let src = SRC.replace('\n', "\r\n");
        let c = crate::load_str(&src).unwrap();
        let line = c.rules[0].defined.line();
        let out = replace_rule_line(&src, line, &c.rules[0].value).unwrap();
        assert_eq!(out, src, "CRLF 文件的同值替换必须逐字节相等");
    }

    #[test]
    fn multiline_value_is_rejected() {
        // 值里带换行会把一行撑成两行，读回来整个 rules 块都错位
        assert!(matches!(
            replace_rule_line(SRC, 5, "GEOSITE,cn,DIRECT\n  - MATCH,REJECT"),
            Err(EditError::ValueSpansLines(_))
        ));
        assert!(matches!(
            replace_rule_line(SRC, 5, "GEOSITE,cn,DIRECT\r"),
            Err(EditError::ValueSpansLines(_))
        ));
    }

    #[test]
    fn empty_value_is_rejected() {
        // 空值写进去就是裸 `-`，正是上面那条要拒绝的东西
        assert!(matches!(
            replace_rule_line(SRC, 5, ""),
            Err(EditError::EmptyValue)
        ));
        assert!(matches!(
            replace_rule_line(SRC, 5, "   "),
            Err(EditError::EmptyValue)
        ));
    }

    #[test]
    fn edited_file_still_parses_and_reports_the_new_value() {
        // 改写的最终验收：读回来必须是新值，且行号与条数不变
        let rules = rules_of(SRC);
        let out = replace_rule_line(SRC, rules[1].0, "GEOSITE,category-ads,REJECT").unwrap();
        let back = rules_of(&out);
        assert_eq!(back[1].1, "GEOSITE,category-ads,REJECT");
        assert_eq!(back[1].0, rules[1].0, "行号不该移动");
        assert_eq!(back.len(), rules.len(), "规则条数不该变");
    }

    #[test]
    fn error_message_names_the_line() {
        let e = replace_rule_line(SRC, 2, "MATCH,DIRECT").unwrap_err();
        let text = e.to_string();
        assert!(text.contains('2'), "错误要点名是哪一行：{text}");
    }
}
