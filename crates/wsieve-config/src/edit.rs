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
//! 这条纪律不止写在这里：workspace 的 `serde-saphyr` 已关掉 `serialize` feature，
//! `serde_saphyr::to_string` 在编译期根本不存在。靠注释去约束一个随手可调用的
//! 公开函数是靠不住的 —— 让它压根编译不过才是。
//!
//! 取而代之的是按 `Spanned<String>` 携带的行号定位到规则所在行，
//! 只替换该行 `- ` 之后、行尾注释之前的那一段，文件其余部分一个字节都不动。
//! 「其余部分不变」不是靠小心翼翼地重建，而是靠**原样透传**：
//! 非目标行连看都不看，直接 push 原字符串。
//!
//! 写出去的结果**由 YAML 自己复核**：拼好之后立刻 `load_str` 读回，确认目标行
//! 恰好等于写入的值，不等就整体回滚成 `Err`（见 `verify_written`）。
//! 这一步不是保险丝，是纪律的落点 —— 「不打扰用户手写的 YAML」这句承诺，
//! 只有在「写进去的东西读得回来」成立时才有意义。
//!
//! 已知限制（都以报错或良性降级收场，不会损坏文件）：
//!
//! 1. **值里带空格又带 `#` 的引号规则**（`- "A #B"`）会被误判成行尾注释。
//!    注意「值里含 `#`」本身是安全的：注释起点按 YAML 真实规则判定
//!    （见 `find_comment_start`）。而写入这类值时，`verify_written` 会发现
//!    读回来的是被截断的值并报错回滚 —— 实测 `MATCH,东京 #1` 正是这条路径。
//!    出站名是用户自由输入的中文，这**不是**假想场景：曾以静默截断收场，
//!    随后 `RuleSet::build` 报「引用了不存在的出站」，错处离现场很远
//! 2. **流式序列**（`rules: [A, B]`）所有规则挤在一行，这里返回
//!    `NotASequenceItem` 而非破坏文件。UI 应提示用户改用块式序列
//! 3. **带引号的规则**（`- "DOMAIN,a.com,PROXY"`）被替换后引号会消失。
//!    多数情况下值仍能正确读回，是良性的；但**引号有时正是值合法的原因** ——
//!    `- "MATCH,节点: 主力"` 去掉引号后 `: ` 会被当成映射，整份配置解析失败。
//!    这类值同样由 `verify_written` 挡下并回滚，不会写出一份起不来的配置

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
    #[error(
        "改写后第 {line} 行读回来是 {got:?}，而不是写入的 {value:?}。已放弃本次改写 —— \
         写出去的文件读回来不是原意，属于静默损坏"
    )]
    ValueNotPreserved { line: u64, value: String, got: String },
    #[error(
        "规则值 {value:?} 写进去会让整份配置无法解析：{message}。已放弃本次改写 —— \
         这样的文件会让代理直接起不来"
    )]
    ValueBreaksFile { value: String, message: String },
    #[error("改写后第 {line} 行不再是一条规则（写入的是 {value:?}）。已放弃本次改写")]
    NotARuleAfterWrite { line: u64, value: String },
    #[error("原文本身就无法解析：{message}。无从校验改写结果，已放弃本次改写")]
    SourceNotParsable { message: String },
}

/// 把第 `line` 行（1-based）的规则值换成 `new_value`。
/// 缩进、行尾注释、值与注释之间的对齐空白、行尾换行符（LF / CRLF）全部保留。
///
/// 成功返回意味着**结果已被 YAML 自己验过**：读回来第 `line` 行恰好是
/// `new_value`。验不过就整体回滚成 `Err`，绝不写出半坏的文件（见 `verify_written`）。
pub fn replace_rule_line(src: &str, line: u64, new_value: &str) -> Result<String, EditError> {
    check_new_value(new_value)?;

    let lines: Vec<&str> = split_keep_ends(src);
    let idx = check_index(line, lines.len())?;

    let (body, eol) = split_eol(lines[idx]);
    let item = split_item(body, line)?;

    // 值与注释之间的对齐空白照原样留着，视觉对齐不被破坏。
    // 这里同样只认 YAML 的 s-white（空格 / 制表符），理由与 find_comment_start 一致：
    // U+3000 之类在 YAML 眼里是**值的一部分**，当成 gap 采走的话，写回时
    // 新值后面会凭空多出一个全角空格（实测 `- MATCH,DIRECT　 # 注释` 每存一次多一个）。
    // 另外按**原顺序**切片而非 `rev().collect()`：后者会把 " \t" 写成 "\t "，
    // 同值替换就不再恒等 —— 全是空格时看不出来，空格与 TAB 混用才现形。
    let gap = &item.value[item.value.trim_end_matches([' ', '\t']).len()..];

    let mut out = String::with_capacity(src.len() + new_value.len());
    for (i, l) in lines.iter().enumerate() {
        if i == idx {
            out.push_str(item.prefix);
            out.push_str(new_value);
            out.push_str(gap);
            out.push_str(item.comment);
            out.push_str(eol);
        } else {
            // 非目标行原样透传 —— 「其余字节不变」由此在构造上成立，而非靠断言
            out.push_str(l);
        }
    }
    verify_written(src, &out, line, new_value)?;
    Ok(out)
}

/// 拿 YAML 自己当裁判：把拼好的结果读回来，确认第 `line` 行**恰好**是 `new_value`。
///
/// 为什么不改成「枚举危险字符然后拒绝」：那份清单永远列不全。YAML 的标量语法里
/// 能改变含义的前缀与序列有一长串（`*` 别名、`&` 锚点、`#` 注释、`-` 嵌套序列、
/// `: ` 映射、`[`/`{` 流式、`|`/`>` 块标量、前导空白被吞、`null`/`~` 退化成空值……），
/// 而出站名是用户在 GUI 里随手敲的自由文本中文，覆盖不全就等于漏。
/// 直接问 YAML「你读出来是不是我写的那个」，判据与真相同源，不会随清单遗漏而失效。
///
/// 这条校验挡下的是实测过的静默损坏，不是假想：
///
/// | 写入值            | 不校验的话           |
/// |-------------------|----------------------|
/// | `MATCH,东京 #1`   | 读回 `MATCH,东京`，被静默截断 |
/// | `MATCH,节点: 主力`| 整份配置语法错，代理起不来（§12）|
/// | `*anchor`         | 同上，`reference to unknown value` |
/// | `- nested`        | 同上，值变成嵌套序列 |
/// | `   MATCH,PROXY`  | 前导空白被吞 |
///
/// 上游 `wsieve-route` 明确承诺出站名按用户原样保留（大小写与内部空格都不改，
/// 见 `wsieve-route/src/rule.rs` 的 `Target`），所以「带空格的中文出站名」
/// 在那一层是**合法**的。两层对「什么是合法出站名」给出矛盾答案时，
/// 该报错的是写这一层 —— 而不是让用户存一次就把配置存坏。
fn verify_written(
    src: &str,
    out: &str,
    line: u64,
    new_value: &str,
) -> Result<(), EditError> {
    let cfg = match crate::load_str(out) {
        Ok(c) => c,
        Err(e) => {
            // 先分清责任：原文本身就读不回来的话，这个语法错不是本次改写造成的。
            // 混为一谈会让用户对着一个自己没碰过的字段找错。
            if let Err(src_err) = crate::load_str(src) {
                return Err(EditError::SourceNotParsable {
                    message: src_err.to_string(),
                });
            }
            return Err(EditError::ValueBreaksFile {
                value: new_value.to_string(),
                message: e.to_string(),
            });
        }
    };
    match cfg.rules.iter().find(|r| r.defined.line() == line) {
        Some(r) if r.value == new_value => Ok(()),
        Some(r) => Err(EditError::ValueNotPreserved {
            line,
            value: new_value.to_string(),
            got: r.value.clone(),
        }),
        // 读得回整份配置，但第 line 行已经不是一条规则了 —— 例如值被 YAML
        // 当成了别的结构。文件没坏，但这一条规则没了，同样不能写出去。
        None => Err(EditError::NotARuleAfterWrite {
            line,
            value: new_value.to_string(),
        }),
    }
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

/// 在 `anchor_line` 之后插入一条新规则。`anchor_line` 可以是：
///   - 某条已有规则所在行——新规则插在它之后，缩进与它一致
///   - `rules:` 键本身所在行（块式，键后即换行）——新规则成为第一条
///   - `rules: []`（空流式序列）所在行——就地展开成块式，新规则成为第一条
/// 除此之外一律报 `NotASequenceItem`，包括非空流式序列（`rules: [A, B]`）——
/// 与 `replace_rule_line` 对这类文件的已知限制一致，不猜测怎么改。
///
/// 与 `replace_rule_line` 同一套「验不过就整体回滚」的纪律：改完立刻读回，
/// 确认新插入的那一行确实是一条值为 `value` 的规则，不是就整体报错。
pub fn insert_rule_line(src: &str, anchor_line: u64, value: &str) -> Result<String, EditError> {
    check_new_value(value)?;

    let lines: Vec<&str> = split_keep_ends(src);
    let idx = check_index(anchor_line, lines.len())?;
    let (body, orig_eol) = split_eol(lines[idx]);
    // 新行自己的换行符：锚点行若有换行符就沿用，没有（文件不以换行结尾）就补一个。
    let sep = if orig_eol.is_empty() { "\n" } else { orig_eol };

    // 情形一：锚点是一条已有规则——插在它之后，缩进与它一致。
    if split_item(body, anchor_line).is_ok() {
        let indent = &body[..indent_width(body)];
        let new_line = format!("{indent}- {value}{sep}");
        let out = splice_after(&lines, idx, orig_eol, &new_line);
        verify_written(src, &out, anchor_line + 1, value)?;
        return Ok(out);
    }

    let trimmed = body.trim_end();

    // 情形二：块式 rules: 键——新规则成为第一条。
    if trimmed == "rules:" {
        let new_line = format!("  - {value}{sep}");
        let out = splice_after(&lines, idx, orig_eol, &new_line);
        verify_written(src, &out, anchor_line + 1, value)?;
        return Ok(out);
    }

    // 情形三：空流式序列——就地展开成块式。
    if trimmed == "rules: []" {
        let replacement = format!("rules:{sep}  - {value}{sep}");
        let mut out = String::with_capacity(src.len() + replacement.len());
        for (i, l) in lines.iter().enumerate() {
            if i == idx {
                out.push_str(&replacement);
            } else {
                out.push_str(l);
            }
        }
        verify_written(src, &out, anchor_line + 1, value)?;
        return Ok(out);
    }

    // 非空流式序列、或压根不是 rules 相关的行——都不猜测，直接拒绝。
    Err(EditError::NotASequenceItem(anchor_line))
}

/// 在 `proxies:` 列表末尾追加一个新的服务器块。`lines` 是调用方已经按
/// 固定缩进格式化好的若干行（每行是 `- name: ...` 这一级的内容，本函数
/// 统一在每行前面补两格缩进），不接受任意文本——UI 端拼好结构，这里只
/// 负责找到插入点。`proxies: []`（空流式）会被就地展开成块式。
///
/// `proxies:` 键不存在时报错，不猜测——本项目的默认配置与
/// `Config::default()` 都会写这个键，不存在意味着文件被手动删过这个键。
pub fn append_proxy_block(src: &str, lines: &[String]) -> Result<String, EditError> {
    if lines.is_empty() {
        return Err(EditError::EmptyValue);
    }

    let all: Vec<&str> = split_keep_ends(src);
    let key_idx = all
        .iter()
        .position(|l| {
            let (body, _) = split_eol(l);
            body == "proxies:" || body.trim_end() == "proxies: []"
        })
        .ok_or(EditError::NotASequenceItem(0))?;

    let (key_body, key_eol) = split_eol(all[key_idx]);
    let sep = if key_eol.is_empty() { "\n" } else { key_eol };

    let mut block = String::new();
    for l in lines {
        block.push_str("  ");
        block.push_str(l);
        block.push_str(sep);
    }

    if key_body.trim_end() == "proxies: []" {
        let expanded = format!("proxies:{sep}{block}");
        let mut out = String::with_capacity(src.len() + expanded.len());
        for (i, l) in all.iter().enumerate() {
            if i == key_idx {
                out.push_str(&expanded);
            } else {
                out.push_str(l);
            }
        }
        return Ok(out);
    }

    // 块式：找列表结束的位置——遇到缩进为 0 的非空行（下一个顶层键）
    // 或文件结束为止。空行仍算列表内的间隔，不当作结束标志。
    let mut end = all.len();
    for i in (key_idx + 1)..all.len() {
        let (body, _) = split_eol(all[i]);
        if body.trim().is_empty() {
            continue;
        }
        if indent_width(body) == 0 {
            end = i;
            break;
        }
    }

    let mut out = String::with_capacity(src.len() + block.len());
    for l in &all[..end] {
        out.push_str(l);
    }
    if end == all.len() {
        if let Some(last) = all.last() {
            let (_, last_eol) = split_eol(last);
            if last_eol.is_empty() {
                out.push_str(sep);
            }
        }
    }
    out.push_str(&block);
    for l in &all[end..] {
        out.push_str(l);
    }
    Ok(out)
}

/// 按 `name` 定位并删除对应的服务器块，从它的 `- name: ...` 行到下一个
/// 同级 `- name:`（或列表结束）为止，整段删掉，其余字节不动。
///
/// 认 `- name: "日本节点"` 与 `- name: 日本节点` 两种写法（带引号与不带）。
/// 找不到匹配的名字、或 `proxies:` 键本身不存在/是空列表，都报错——
/// 删除一个不存在的东西不该悄悄什么都不做。
pub fn delete_proxy_block(src: &str, name: &str) -> Result<String, EditError> {
    let all: Vec<&str> = split_keep_ends(src);
    let key_idx = all
        .iter()
        .position(|l| {
            let (body, _) = split_eol(l);
            body == "proxies:" || body.trim_end() == "proxies: []"
        })
        .ok_or(EditError::NotASequenceItem(0))?;

    let (key_body, _) = split_eol(all[key_idx]);
    if key_body.trim_end() == "proxies: []" {
        return Err(EditError::NotASequenceItem(key_idx as u64 + 1));
    }

    let mut start: Option<usize> = None;
    // 目标项自己的缩进宽度——只有在这个宽度的 `- ` 才是「下一个同级项」，
    // 缩进更深的 `- `（如 `mux-prefs:` 这类嵌套列表字段的元素）是目标块
    // 自己的内容，不是兄弟项的边界。
    let mut start_indent = 0usize;
    let mut end = all.len();
    let mut i = key_idx + 1;
    while i < all.len() {
        let (body, _) = split_eol(all[i]);
        if body.trim().is_empty() {
            i += 1;
            continue;
        }
        if indent_width(body) == 0 {
            end = i;
            break;
        }
        let trimmed = body.trim_start();
        if let Some(rest) = trimmed.strip_prefix("- ") {
            let this_indent = indent_width(body);
            if start.is_some() {
                if this_indent == start_indent {
                    end = i;
                    break;
                }
                // 缩进比目标项更深——是目标块内部嵌套列表的元素，
                // 不是同级兄弟项，继续往下扫，一并纳入待删范围。
            } else if item_name_matches(rest, name) {
                start = Some(i);
                start_indent = this_indent;
            }
        }
        i += 1;
    }
    let start = start.ok_or(EditError::NotASequenceItem(key_idx as u64 + 1))?;

    let mut out = String::with_capacity(src.len());
    for (idx, l) in all.iter().enumerate() {
        if idx < start || idx >= end {
            out.push_str(l);
        }
    }
    Ok(out)
}

/// 判断 `- ` 之后的这一行内容（形如 `name: "xxx"` 或 `name: xxx`）
/// 是否是 `target` 这个名字——认引号也认不带引号两种写法。
fn item_name_matches(rest: &str, target: &str) -> bool {
    let Some(value) = rest.strip_prefix("name:").map(str::trim) else {
        return false;
    };
    value.trim_matches('"') == target
}

/// 在第 `at`（0-based）行之后插入 `new_line`；若该行原本没有换行符
/// （文件不以换行结尾），先补一个，避免原内容与新行糊成一行。
fn splice_after(lines: &[&str], at: usize, orig_eol: &str, new_line: &str) -> String {
    let mut out = String::with_capacity(
        lines.iter().map(|l| l.len()).sum::<usize>() + new_line.len() + 1,
    );
    for (i, l) in lines.iter().enumerate() {
        out.push_str(l);
        if i == at {
            if orig_eol.is_empty() {
                out.push('\n');
            }
            out.push_str(new_line);
        }
    }
    out
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
///
/// 「空白」按 YAML 规范的 `s-white` 判定：**只有** 空格与制表符两种。
/// 这里刻意不用 `char::is_whitespace` —— 它覆盖整个 Unicode White_Space
/// 属性，会把 U+3000（中文输入法的全角空格，本项目用户的日常产物）、
/// U+00A0、U+2003 也算成空白，于是 `- MATCH,DIRECT　#兜底` 里的 `#`
/// 被误判成开启注释，而 YAML 认为它是值的一部分。后果与上面那条 ASCII
/// 陷阱同构，但更凶：每存一次注释就翻一倍，五次之后是 32 份，且每轮都「成功」。
fn find_comment_start(s: &str) -> Option<usize> {
    // 区段开头等同于「前面是空白」：`- #foo` 里的 `#` 确实开启注释
    let mut prev_is_space = true;
    for (i, c) in s.char_indices() {
        if c == '#' && prev_is_space {
            return Some(i);
        }
        prev_is_space = matches!(c, ' ' | '\t');
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
    fn a_full_width_space_before_the_hash_does_not_open_a_comment() {
        // YAML 的 s-white **只有**空格与制表符。U+3000（中文输入法的全角空格）
        // 在 YAML 眼里是值的普通字符，故 `- MATCH,DIRECT　#兜底` 的值是整串。
        // 若用 `char::is_whitespace` 判定（覆盖整个 Unicode White_Space），
        // 这个 `#` 会被误当成注释起点，同值替换每存一次就把 `　#兜底` 翻一倍：
        // 实测五次之后是 32 份，且每一轮都「成功」，永远不报错。
        let src = "rules:\n  - MATCH,DIRECT\u{3000}#兜底\n";
        let value = rules_of(src)[0].1.clone();
        assert_eq!(value, "MATCH,DIRECT\u{3000}#兜底", "先钉住 YAML 的实际取值");

        let same = replace_rule_line(src, 2, &value).unwrap();
        assert_eq!(same, src, "含全角空格 + `#` 的值同值替换必须逐字节恒等");
    }

    #[test]
    fn full_width_space_does_not_compound_across_repeated_saves() {
        // 这个 bug 的杀伤力在于**指数累积**：单看一轮像是「多了个尾巴」，
        // 五轮之后原值被 32 份注释淹没。用连存五次钉死它不再增长。
        let src = "rules:\n  - MATCH,DIRECT\u{3000}#兜底\n";
        let mut text = src.to_string();
        for i in 1..=5 {
            let value = rules_of(&text)[0].1.clone();
            text = replace_rule_line(&text, 2, &value).unwrap();
            assert_eq!(text, src, "第 {i} 次保存后就该逐字节等于原文");
        }
    }

    #[test]
    fn other_unicode_spaces_before_a_hash_are_part_of_the_value_too() {
        // 同一族的另外两个来源：U+00A0（不换行空格，网页复制粘贴的常客）
        // 与 U+2003（em space）。判定必须是「只认空格与 TAB」，
        // 而不是逐个把已知的 Unicode 空白拉黑 —— 后者永远列不全。
        for sp in ['\u{00a0}', '\u{2003}'] {
            let src = format!("rules:\n  - MATCH,DIRECT{sp}#兜底\n");
            let value = rules_of(&src)[0].1.clone();
            assert_eq!(value, format!("MATCH,DIRECT{sp}#兜底"), "U+{:04X}", sp as u32);
            let same = replace_rule_line(&src, 2, &value).unwrap();
            assert_eq!(same, src, "U+{:04X} 之后的 `#` 不该开启注释", sp as u32);
        }
    }

    #[test]
    fn a_tab_in_the_alignment_gap_keeps_its_position() {
        // 对齐空白按**原顺序**保留。用 `chars().rev().collect()` 采集的实现
        // 会把 " \t" 写回成 "\t "，全是空格时看不出来，空格与 TAB 混用才现形。
        let src = "rules:\n  - MATCH,DIRECT \t# 注释\n";
        let value = rules_of(src)[0].1.clone();
        let same = replace_rule_line(src, 2, &value).unwrap();
        assert_eq!(same, src, "gap 里的空格与 TAB 顺序不能被颠倒");
    }

    #[test]
    fn a_trailing_full_width_space_in_the_value_is_not_taken_as_gap() {
        // gap 只能从**值的尾部**采走 YAML 认得的空白。U+3000 属于值本身，
        // 采走它会让写回的新值后面凭空多一个全角空格 —— 每存一次多一个。
        let src = "rules:\n  - MATCH,DIRECT\u{3000} # 注释\n";
        let value = rules_of(src)[0].1.clone();
        assert_eq!(value, "MATCH,DIRECT\u{3000}", "全角空格是值的一部分");
        let same = replace_rule_line(src, 2, &value).unwrap();
        assert_eq!(same, src, "值尾的全角空格不该被当成对齐空白重复写出");
    }

    #[test]
    fn a_value_that_would_be_silently_truncated_is_rejected() {
        // 出站名是用户在 GUI 里敲的自由文本，`wsieve-route` 明确承诺按原样保留
        // （含内部空格，见其 rule.rs 的 Target）。于是 `MATCH,东京 #1` 在那一层合法，
        // 而写到 YAML 里 ` #` 会开启注释、值被截成 `MATCH,东京` —— 两层对
        // 「什么是合法出站名」给出矛盾答案。这时候该报错的是写这一层。
        //
        // 不拦的话，损坏在很远的地方才显形：随后 RuleSet::build 报
        // 「第 1 行引用了不存在的出站：东京」，用户对着自己没碰过的地方找错。
        let src = "rules:\n  - MATCH,DIRECT\n";
        let e = replace_rule_line(src, 2, "MATCH,东京 #1").unwrap_err();
        assert!(
            matches!(&e, EditError::ValueNotPreserved { got, .. } if got == "MATCH,东京"),
            "应报「读回来不是写进去的值」，实为 {e:?}"
        );
        let text = e.to_string();
        assert!(text.contains("MATCH,东京 #1"), "要点名写入的值：{text}");
    }

    #[test]
    fn a_value_that_would_break_the_whole_file_is_rejected() {
        // 冒号加空格会被 YAML 当成映射，整份配置从此读不回来 ——
        // 按设计文档 §12 那意味着代理根本起不来。用户在 GUI 里给节点
        // 起名叫「节点: 主力」，存一次盘就把自己锁在门外。
        let src = "rules:\n  - MATCH,DIRECT\n";
        let e = replace_rule_line(src, 2, "MATCH,节点: 主力").unwrap_err();
        assert!(
            matches!(e, EditError::ValueBreaksFile { .. }),
            "应报「会让整份配置无法解析」，实为 {e:?}"
        );
    }

    #[test]
    fn yaml_special_prefixes_are_all_rejected_not_enumerated() {
        // 这一组的共同点是「不能靠拉黑字符表覆盖」：YAML 里能改变标量含义的
        // 前缀有一长串，逐个列举永远漏。判据交给 YAML 自己 ——
        // 读回来不等于写进去的，一律回滚。
        let src = "rules:\n  - MATCH,DIRECT\n";
        for bad in [
            "*anchor",      // 别名引用
            "&anchor x",    // 锚点定义
            "- nested",     // 嵌套序列
            "#leading",     // 整行变注释
            "   MATCH,阿", // 前导空白被吞
            "null",         // 退化成空值
            "~",            // 同上
            "[A, B]",       // 流式序列
            "{a: b}",       // 流式映射
        ] {
            assert!(
                replace_rule_line(src, 2, bad).is_err(),
                "{bad:?} 读回来不是原值，必须回滚而不是写出去"
            );
        }
    }

    #[test]
    fn ordinary_rules_still_write_through() {
        // 校验必须只拦真出问题的值。若把正常规则也一并拒了，
        // 这个「保险」就把功能本身废掉了 —— 那比不加还坏。
        let src = "rules:\n  - MATCH,DIRECT\n";
        for good in [
            "MATCH,DIRECT",
            "MATCH,日本节点",
            "GEOSITE,category-ads,REJECT",
            "IP-CIDR,192.168.0.0/16,DIRECT,no-resolve",
            "DOMAIN-SUFFIX,例子.测试,DIRECT",
            "MATCH,DIRECT#兜底", // `#` 前无空白，是值的一部分
            "DST-PORT,443,日本节点",
        ] {
            let out = replace_rule_line(src, 2, good)
                .unwrap_or_else(|e| panic!("正常规则 {good:?} 不该被拒：{e}"));
            let back = crate::load_str(&out).unwrap();
            assert_eq!(back.rules[0].value, good, "写进去的要能原样读回来");
        }
    }

    #[test]
    fn a_quoted_rule_that_needs_its_quotes_is_rejected_not_silently_broken() {
        // `- "MATCH,节点: 主力"` 之所以合法，全靠那对引号。定点改写不重建引号，
        // 于是同值替换会写出一个语法错的文件。这一条在校验落地前是**静默**的：
        // 替换「成功」，下次加载才炸。
        let src = "rules:\n  - \"MATCH,节点: 主力\"\n";
        let c = crate::load_str(src).unwrap();
        assert_eq!(c.rules[0].value, "MATCH,节点: 主力", "先钉住带引号时的取值");

        let e = replace_rule_line(src, 2, &c.rules[0].value).unwrap_err();
        assert!(
            matches!(e, EditError::ValueBreaksFile { .. }),
            "引号是这个值合法的唯一原因，丢掉引号必须报错而非照写：{e:?}"
        );
    }

    #[test]
    fn an_already_broken_source_is_named_as_such() {
        // 原文自己就读不回来时，别把责任算到本次改写头上 ——
        // 让用户对着一个他没碰过的字段找错，比不报错好不了多少。
        let src = "rules:\n  - MATCH,DIRECT\nbogus: [\n";
        let e = replace_rule_line(src, 2, "MATCH,PROXY").unwrap_err();
        assert!(
            matches!(e, EditError::SourceNotParsable { .. }),
            "应点明是原文的问题，实为 {e:?}"
        );
        assert!(e.to_string().contains("原文"), "措辞要让用户知道错在哪：{e}");
    }

    #[test]
    fn rejection_never_writes_a_half_broken_file() {
        // 「回滚」的实质含义：调用方拿到 Err 时手里没有任何新文本，
        // 不存在「写了一半」的中间态。用 Result 的形状把这一点钉死。
        let src = "rules:\n  - MATCH,DIRECT\n  - GEOSITE,cn,DIRECT\n";
        assert!(replace_rule_line(src, 2, "MATCH,节点: 主力").is_err());
        // 原文是入参、不可变，失败后它当然还是原样 —— 这里连同断言一次，
        // 免得将来有人改成「就地修改 &mut String」而悄悄破坏这个性质。
        assert_eq!(src, "rules:\n  - MATCH,DIRECT\n  - GEOSITE,cn,DIRECT\n");
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

    #[test]
    fn insert_after_an_existing_rule() {
        let src = "rules:\n  - MATCH,DIRECT\n";
        let out = insert_rule_line(src, 2, "GEOSITE,cn,DIRECT").unwrap();
        assert_eq!(out, "rules:\n  - MATCH,DIRECT\n  - GEOSITE,cn,DIRECT\n");
    }

    #[test]
    fn insert_as_the_first_item_of_a_block_form_list() {
        let src = "rules:\n  - MATCH,DIRECT\n";
        let out = insert_rule_line(src, 1, "GEOSITE,cn,DIRECT").unwrap();
        assert_eq!(out, "rules:\n  - GEOSITE,cn,DIRECT\n  - MATCH,DIRECT\n");
    }

    #[test]
    fn insert_expands_an_empty_flow_sequence() {
        let src = "mixed-port: 7890\nrules: []\n";
        let out = insert_rule_line(src, 2, "MATCH,DIRECT").unwrap();
        assert_eq!(out, "mixed-port: 7890\nrules:\n  - MATCH,DIRECT\n");
    }

    #[test]
    fn insert_preserves_untouched_lines_and_comments() {
        let src = "rules:\n  # 兜底\n  - MATCH,DIRECT\n";
        let out = insert_rule_line(src, 3, "GEOSITE,cn,日本节点").unwrap();
        assert_eq!(
            out,
            "rules:\n  # 兜底\n  - MATCH,DIRECT\n  - GEOSITE,cn,日本节点\n"
        );
    }

    #[test]
    fn insert_refuses_a_non_empty_flow_sequence() {
        // 与 replace_rule_line 对这类文件的已知限制一致：不猜测怎么改。
        let src = "rules: [MATCH,DIRECT]\n";
        assert!(matches!(
            insert_rule_line(src, 1, "GEOSITE,cn,DIRECT"),
            Err(EditError::NotASequenceItem(1))
        ));
    }

    #[test]
    fn insert_refuses_an_anchor_that_is_neither_a_rule_nor_the_rules_key() {
        let src = "mixed-port: 7890\nrules:\n  - MATCH,DIRECT\n";
        assert!(matches!(
            insert_rule_line(src, 1, "GEOSITE,cn,DIRECT"),
            Err(EditError::NotASequenceItem(1))
        ));
    }

    #[test]
    fn insert_rejects_an_empty_value() {
        let src = "rules:\n  - MATCH,DIRECT\n";
        assert!(matches!(
            insert_rule_line(src, 1, "   "),
            Err(EditError::EmptyValue)
        ));
    }

    #[test]
    fn insert_handles_a_file_with_no_trailing_newline() {
        // 锚点行若恰好是文件最后一行且没有换行符，插入后原行与新行不能糊在一起。
        let src = "rules:\n  - MATCH,DIRECT";
        let out = insert_rule_line(src, 2, "GEOSITE,cn,DIRECT").unwrap();
        assert_eq!(out, "rules:\n  - MATCH,DIRECT\n  - GEOSITE,cn,DIRECT\n");
    }

    #[test]
    fn insert_out_of_range_anchor_is_an_error_not_a_panic() {
        let src = "rules:\n  - MATCH,DIRECT\n";
        assert!(matches!(
            insert_rule_line(src, 99, "GEOSITE,cn,DIRECT"),
            Err(EditError::LineOutOfRange(99, 2))
        ));
    }

    #[test]
    fn append_proxy_expands_an_empty_flow_sequence() {
        let src = "mixed-port: 25500\nproxies: []\nrules: []\n";
        let lines = vec![
            "- name: \"日本节点\"".to_string(),
            "  type: websieve".to_string(),
            "  url: https://example.com/".to_string(),
            "  server-pub: \"aa\"".to_string(),
            "  client-priv: \"bb\"".to_string(),
        ];
        let out = append_proxy_block(src, &lines).unwrap();
        assert_eq!(
            out,
            "mixed-port: 25500\nproxies:\n  - name: \"日本节点\"\n    type: websieve\n    url: https://example.com/\n    server-pub: \"aa\"\n    client-priv: \"bb\"\nrules: []\n"
        );
    }

    #[test]
    fn append_proxy_after_an_existing_block() {
        let src = "proxies:\n  - name: \"日本节点\"\n    type: websieve\nrules: []\n";
        let lines = vec!["- name: \"香港节点\"".to_string(), "  type: websieve".to_string()];
        let out = append_proxy_block(src, &lines).unwrap();
        assert_eq!(
            out,
            "proxies:\n  - name: \"日本节点\"\n    type: websieve\n  - name: \"香港节点\"\n    type: websieve\nrules: []\n"
        );
    }

    #[test]
    fn append_proxy_when_the_list_runs_to_end_of_file() {
        let src = "proxies:\n  - name: \"日本节点\"\n    type: websieve\n";
        let lines = vec!["- name: \"香港节点\"".to_string(), "  type: websieve".to_string()];
        let out = append_proxy_block(src, &lines).unwrap();
        assert_eq!(
            out,
            "proxies:\n  - name: \"日本节点\"\n    type: websieve\n  - name: \"香港节点\"\n    type: websieve\n"
        );
    }

    #[test]
    fn append_proxy_untouched_bytes_stay_untouched() {
        let src = "mixed-port: 25500\nproxies:\n  - name: \"日本节点\"\n    type: websieve\nrules:\n  - MATCH,日本节点\n";
        let lines = vec!["- name: \"香港节点\"".to_string(), "  type: websieve".to_string()];
        let out = append_proxy_block(src, &lines).unwrap();
        assert!(out.starts_with("mixed-port: 25500\nproxies:\n  - name: \"日本节点\"\n    type: websieve\n"));
        assert!(out.ends_with("rules:\n  - MATCH,日本节点\n"));
    }

    #[test]
    fn append_proxy_rejects_empty_lines() {
        let src = "proxies: []\n";
        assert!(matches!(append_proxy_block(src, &[]), Err(EditError::EmptyValue)));
    }

    #[test]
    fn append_proxy_missing_key_is_an_error() {
        let src = "mixed-port: 25500\nrules: []\n";
        let lines = vec!["- name: \"x\"".to_string()];
        assert!(matches!(
            append_proxy_block(src, &lines),
            Err(EditError::NotASequenceItem(0))
        ));
    }

    #[test]
    fn delete_proxy_removes_the_named_block_and_nothing_else() {
        let src = "proxies:\n  - name: \"日本节点\"\n    type: websieve\n  - name: \"香港节点\"\n    type: websieve\nrules: []\n";
        let out = delete_proxy_block(src, "日本节点").unwrap();
        assert_eq!(out, "proxies:\n  - name: \"香港节点\"\n    type: websieve\nrules: []\n");
    }

    #[test]
    fn delete_proxy_that_is_the_last_item_before_eof() {
        let src = "proxies:\n  - name: \"日本节点\"\n    type: websieve\n";
        let out = delete_proxy_block(src, "日本节点").unwrap();
        assert_eq!(out, "proxies:\n");
    }

    #[test]
    fn delete_proxy_matches_an_unquoted_name_too() {
        let src = "proxies:\n  - name: 日本节点\n    type: websieve\nrules: []\n";
        let out = delete_proxy_block(src, "日本节点").unwrap();
        assert_eq!(out, "proxies:\nrules: []\n");
    }

    #[test]
    fn delete_proxy_skips_over_a_nested_list_field_without_being_fooled_by_it() {
        // `mux-prefs: Vec<u8>` 写成块式列表时，其元素 `- 0` / `- 1` 缩进
        // 比 `- name:` 更深，但同样以 `- ` 开头。删除逻辑必须按缩进宽度
        // 区分「同级兄弟项」与「目标块自己的嵌套内容」，否则会把
        // `mux-prefs` 的元素误判成下一个节点的边界，导致它们被孤儿化地
        // 留在 `proxies:` 下、缩进错乱，而真正的下一个节点却没被真正处理。
        let src = "proxies:\n  - name: \"日本节点\"\n    type: websieve\n    mux-prefs:\n      - 0\n      - 1\n  - name: \"香港节点\"\n    type: websieve\nrules: []\n";
        let out = delete_proxy_block(src, "日本节点").unwrap();
        assert_eq!(
            out,
            "proxies:\n  - name: \"香港节点\"\n    type: websieve\nrules: []\n",
            "嵌套列表字段的元素不该被当成同级兄弟项，整个目标块（含其嵌套内容）应一并删除：\n{out}"
        );
    }

    #[test]
    fn delete_proxy_unknown_name_is_an_error_not_a_silent_noop() {
        let src = "proxies:\n  - name: \"日本节点\"\n    type: websieve\nrules: []\n";
        assert!(matches!(
            delete_proxy_block(src, "幽灵节点"),
            Err(EditError::NotASequenceItem(_))
        ));
    }
}
