//! 注释保留往返（设计文档 §13）。
//!
//! 仅断言「语义不变」是不够的 —— serde 序列化也能通过那种测试，
//! 却会把注释全部抹掉。这里逐字检查注释仍在。
//!
//! 与 `src/edit.rs` 里的单测的分工：那边验的是改写函数本身的各个分支，
//! 这边验的是**完整链路** —— 读（serde 反序列化）→ 改（字节级拼接）→
//! 写 → 再读。链路上任何一环偷偷把文件归一化了，都要在这里现形。
//!
//! 本文件里最强的一条断言不是 `contains("# 某注释")`，而是
//! `only_line_that_changed` —— 「除目标行外一个字节都没动」。
//! `contains` 只能证明注释还在，证明不了别处没被重排：键序被换、
//! 引号被剥、空行被吞、缩进被统一，都能骗过 `contains`。

use wsieve_config::{edit, load_str, ConfigError};

const SRC: &str = "\
# websieve 配置
mixed-port: 7890
mode: rule

proxies:
  - name: \"日本节点\"
    type: websieve
    url: https://example.com/
    server-pub: \"aa\"
    client-priv: \"bb\"

rules:
  # 广告一律拦掉
  - GEOSITE,category-ads,REJECT
  # 内网直连，别删这条
  - IP-CIDR,192.168.0.0/16,DIRECT,no-resolve   # 家里的网段
  - GEOSITE,cn,DIRECT
  - MATCH,日本节点
";

/// 两份文本逐行对照，返回**有差异的行号**（1-based）。
///
/// 按 `split_inclusive('\n')` 切分，故行尾换行符也参与比较 ——
/// 悄悄把 CRLF 换成 LF 会被这里抓住，而不是从缝里溜走。
fn changed_lines(before: &str, after: &str) -> Vec<usize> {
    let a: Vec<&str> = before.split_inclusive('\n').collect();
    let b: Vec<&str> = after.split_inclusive('\n').collect();
    assert_eq!(a.len(), b.len(), "行数变了：{} → {}", a.len(), b.len());
    a.iter()
        .zip(&b)
        .enumerate()
        .filter(|(_, (x, y))| x != y)
        .map(|(i, _)| i + 1)
        .collect()
}

/// 断言「只有第 `line` 行变了」，并返回那一行改写后的内容。
///
/// 这是本文件的核心断言：它把「保留注释」从一串零散的 `contains`
/// 提升成一条全称命题 —— 除了目标行，整份文件逐字节不变。
fn only_line_that_changed(before: &str, after: &str, line: u64) -> String {
    let diff = changed_lines(before, after);
    assert_eq!(
        diff,
        vec![line as usize],
        "应当只有第 {line} 行发生变化，实际变化的行：{diff:?}"
    );
    after
        .split_inclusive('\n')
        .nth(line as usize - 1)
        .expect("行号已由 changed_lines 校验过")
        .to_string()
}

#[test]
fn edit_one_rule_and_everything_else_survives() {
    let before = load_str(SRC).unwrap();
    assert_eq!(before.rules.len(), 4);

    // 把第三条 GEOSITE,cn,DIRECT 改成走代理
    let line = before.rules[2].defined.line();
    let after_text = edit::replace_rule_line(SRC, line, "GEOSITE,cn,日本节点").unwrap();

    // 1) 注释逐条还在
    for c in [
        "# websieve 配置",
        "# 广告一律拦掉",
        "# 内网直连，别删这条",
        "# 家里的网段",
    ] {
        assert!(after_text.contains(c), "注释丢失：{c}");
    }

    // 2) 语义正确
    let after = load_str(&after_text).unwrap();
    assert_eq!(after.rules.len(), 4);
    assert_eq!(after.rules[2].value, "GEOSITE,cn,日本节点");
    assert_eq!(after.rules[0].value, "GEOSITE,category-ads,REJECT");
    assert_eq!(after.rules[3].value, "MATCH,日本节点");

    // 3) 其余配置项没被动过
    assert_eq!(after.mixed_port, before.mixed_port);
    assert_eq!(after.proxies[0].name, before.proxies[0].name);
    assert_eq!(after.proxies[0].client_priv, before.proxies[0].client_priv);
}

#[test]
fn nothing_outside_the_edited_line_moves_by_a_single_byte() {
    // 上一条测试用 `contains` 逐条点名注释，那是**存在性**断言：
    // 它挡不住「注释还在但别处被重排」。这条改用全称断言 ——
    // 除目标行外整份文件逐字节相同。序列化写回会让这里一片飘红。
    let before = load_str(SRC).unwrap();
    let line = before.rules[2].defined.line();
    let after_text = edit::replace_rule_line(SRC, line, "GEOSITE,cn,日本节点").unwrap();

    let edited = only_line_that_changed(SRC, &after_text, line);
    assert_eq!(edited, "  - GEOSITE,cn,日本节点\n", "被改的那行也要形状不变");
}

#[test]
fn hand_written_shape_is_not_normalized_away() {
    // 序列化写回最典型的三个副作用：剥掉用户写的引号、吞掉分段空行、
    // 把 kebab-case 键改名。这三样都不会被「注释还在吗」问出来，
    // 但用户下次打开文件一眼就能看见。
    let before = load_str(SRC).unwrap();
    let line = before.rules[3].defined.line();
    let out = edit::replace_rule_line(SRC, line, "MATCH,DIRECT").unwrap();

    assert!(out.contains("  - name: \"日本节点\"\n"), "用户写的引号要留着：\n{out}");
    assert!(out.contains("    server-pub: \"aa\"\n"), "kebab-case 键名不能被改写：\n{out}");
    assert!(out.contains("mode: rule\n\nproxies:"), "分段空行不能被吞掉：\n{out}");
    assert!(out.contains("client-priv: \"bb\"\n\nrules:"), "第二处分段空行同理：\n{out}");
}

#[test]
fn repeated_edits_do_not_accumulate_drift() {
    // 连续改 5 次，每次都能正确读回 —— 排除「每轮多一个空格」这类渐变
    let mut text = SRC.to_string();
    for i in 0..5 {
        let c = load_str(&text).unwrap();
        let line = c.rules[2].defined.line();
        let v = format!("GEOSITE,cn,节点{i}");
        text = edit::replace_rule_line(&text, line, &v).unwrap();
        let back = load_str(&text).unwrap();
        assert_eq!(back.rules[2].value, v);
        assert_eq!(back.rules.len(), 4, "第 {i} 轮规则数变了");
    }
    assert!(text.contains("# 家里的网段"), "多轮之后注释仍在");

    // 五轮之后，与原文的差异**仍然只有那一行**。
    // 「渐变」正是那种每轮只多一个空格、单轮看不出来的漂移：
    // 逐轮读回值都对，攒够五轮才在别的行上显形。
    // 行号从原文现算而非写死常量 —— 写死的话改动 SRC 就会让这条断言悄悄失准。
    let target = load_str(SRC).unwrap().rules[2].defined.line();
    let diff = changed_lines(SRC, &text);
    assert_eq!(diff, vec![target as usize], "五轮之后不该有第二行发生变化：{diff:?}");
}

#[test]
fn identity_edit_is_byte_for_byte_stable() {
    let c = load_str(SRC).unwrap();
    let line = c.rules[1].defined.line();
    let same = c.rules[1].value.clone();
    let out = edit::replace_rule_line(SRC, line, &same).unwrap();
    assert_eq!(out, SRC, "同值替换必须逐字节恒等");
}

#[test]
fn every_rule_survives_an_identity_edit() {
    // 上一条只验了一条规则。恒等性是逐行的性质，就把四条都验一遍 ——
    // 带前导注释的、带行尾注释加对齐空白的、光秃秃的、值里含中文的，
    // 四种形状各有一条踩在不同的代码路径上。
    let c = load_str(SRC).unwrap();
    for (i, r) in c.rules.iter().enumerate() {
        let out = edit::replace_rule_line(SRC, r.defined.line(), &r.value).unwrap();
        assert_eq!(out, SRC, "第 {i} 条规则的同值替换破坏了恒等性");
    }
}

/// 两处 YAML 语法陷阱的现场。`#` 前面没有空白时不开注释，
/// 所以第一条规则的值是 `MATCH,DIRECT#兜底` **整串**；
/// 按「第一个 `#` 即注释」去切会写出 `MATCH,DIRECT#兜底#兜底`。
const TRICKY: &str = "\
rules:
  # 这条的 # 在值里，不是注释
  - GEOSITE,ads,REJECT#先留着
  - DOMAIN-SUFFIX,例子.测试,DIRECT     # IDN，非 ASCII 在值中间
  - MATCH,日本节点
";

#[test]
fn a_hash_inside_a_value_round_trips_without_duplicating_itself() {
    let c = load_str(TRICKY).unwrap();
    assert_eq!(
        c.rules[0].value, "GEOSITE,ads,REJECT#先留着",
        "先钉住 YAML 的实际取值：`#` 前无空白就不开注释"
    );

    // 同值替换必须恒等。若注释起点判错，这里会多出一截 `#先留着`
    let same = edit::replace_rule_line(TRICKY, c.rules[0].defined.line(), &c.rules[0].value).unwrap();
    assert_eq!(same, TRICKY, "含 `#` 的值同值替换必须逐字节恒等");

    // 换成别的值时，`#` 后半段要跟着旧值一起消失，不能残留
    let line = c.rules[0].defined.line();
    let out = edit::replace_rule_line(TRICKY, line, "GEOSITE,ads,REJECT").unwrap();
    let edited = only_line_that_changed(TRICKY, &out, line);
    assert_eq!(edited, "  - GEOSITE,ads,REJECT\n", "`#先留着` 应随旧值整串消失");
    let back = load_str(&out).unwrap();
    assert_eq!(back.rules[0].value, "GEOSITE,ads,REJECT");
}

#[test]
fn idn_value_and_its_alignment_gap_round_trip() {
    // 非 ASCII 出现在值的**中间**，且后面跟着一段用于对齐的空白和中文注释。
    // 任何按字节而非按 char 定位的实现，在这行上不是 panic 就是吐乱码。
    let c = load_str(TRICKY).unwrap();
    let line = c.rules[1].defined.line();
    assert_eq!(c.rules[1].value, "DOMAIN-SUFFIX,例子.测试,DIRECT");

    let out = edit::replace_rule_line(TRICKY, line, "DOMAIN-SUFFIX,示例.中国,日本节点").unwrap();
    let edited = only_line_that_changed(TRICKY, &out, line);
    assert_eq!(
        edited, "  - DOMAIN-SUFFIX,示例.中国,日本节点     # IDN，非 ASCII 在值中间\n",
        "对齐空白与中文行尾注释都要原样留着"
    );
    assert_eq!(
        load_str(&out).unwrap().rules[1].value,
        "DOMAIN-SUFFIX,示例.中国,日本节点"
    );
}

#[test]
fn a_comment_only_item_is_refused_by_both_layers_not_corrupted() {
    // `- # 这条以后再填` 能骗过朴素的 `"- "` 前缀判断，往里拼值会把
    // 值和注释糊成一坨。两层各自独立地拒绝它，谁都不会写坏文件：
    //
    // 读这一层：`Spanned<String>` 收不下 null，报出**带行号**的语法错，
    //           UI 能把光标定过去；
    // 写这一层：即便调用方绕过读、直接拿行号来改，也会被挡下。
    let src = "rules:\n  - # 这条以后再填\n  - MATCH,DIRECT\n";

    match load_str(src).unwrap_err() {
        ConfigError::Syntax { line, .. } => {
            assert_eq!(line, 2, "空序列项的行号要指向它自己，实为第 {line} 行")
        }
        other => panic!("应是语法错，实为 {other:?}"),
    }

    assert!(
        edit::replace_rule_line(src, 2, "MATCH,PROXY").is_err(),
        "写这一层也必须独立拒绝，不能依赖读那一层已经挡过"
    );
    assert!(edit::delete_rule_line(src, 2).is_err(), "删除同理");
}

#[test]
fn a_value_the_writer_cannot_read_back_is_refused_across_the_whole_chain() {
    // 本文件验的是完整链路，那就把「链路上两层对合法值的定义不一致」也验进来。
    //
    // wsieve-route 承诺出站名按用户原样保留（含内部空格与大小写），所以
    // `MATCH,东京 #1`、`MATCH,节点: 主力` 在解析那一层都是合法的规则。
    // 但写到 YAML 里，前者被 ` #` 截成 `MATCH,东京`，后者的 `: ` 让整份配置
    // 解析失败 —— 按设计文档 §12，后者意味着代理直接起不来。
    //
    // 两层给出矛盾答案时，写这一层必须报错并回滚，不能存一次盘就把配置存坏。
    let before = load_str(SRC).unwrap();
    let line = before.rules[3].defined.line();

    for bad in ["MATCH,东京 #1", "MATCH,节点: 主力", "*anchor", "   MATCH,PROXY"] {
        let e = edit::replace_rule_line(SRC, line, bad)
            .expect_err(&format!("{bad:?} 读回来不是原值，必须被拒"));
        assert!(e.to_string().contains("放弃本次改写"), "错误要说明已回滚：{e}");
    }

    // 而正常的值照写，且读回来分毫不差 —— 校验不能把功能本身废掉
    let out = edit::replace_rule_line(SRC, line, "MATCH,香港节点").unwrap();
    let edited = only_line_that_changed(SRC, &out, line);
    assert_eq!(edited, "  - MATCH,香港节点\n");
    assert_eq!(load_str(&out).unwrap().rules[3].value, "MATCH,香港节点");
}

#[test]
fn a_full_width_space_before_a_hash_survives_five_identity_saves() {
    // U+3000 是中文输入法的日常产物。曾经它前面的 `#` 被误判成注释起点，
    // 于是每存一次盘注释就翻一倍：五次之后 32 份，且每一轮都「成功」。
    // 这里跑满五轮并逐轮要求逐字节等于原文 —— 指数累积的 bug 单轮不易看出。
    let src = "\
rules:
  # 全角空格在值里，它后面的 # 不是注释
  - MATCH,DIRECT\u{3000}#兜底
  - GEOSITE,cn,DIRECT
";
    let c = load_str(src).unwrap();
    assert_eq!(
        c.rules[0].value, "MATCH,DIRECT\u{3000}#兜底",
        "先钉住 YAML 的实际取值：U+3000 不是 s-white，`#` 不开注释"
    );

    let mut text = src.to_string();
    for i in 1..=5 {
        let cur = load_str(&text).unwrap();
        let line = cur.rules[0].defined.line();
        let v = cur.rules[0].value.clone();
        text = edit::replace_rule_line(&text, line, &v).unwrap();
        assert_eq!(text, src, "第 {i} 次保存后就该逐字节等于原文");
    }
}

#[test]
fn deleting_a_rule_takes_its_own_comment_and_leaves_the_rest_untouched() {
    // 删除是本阶段另一个写操作，同样要过往返验收：
    // 规则连同它的前导注释一起走，别人的注释一个字都不许少。
    let before = load_str(SRC).unwrap();
    let line = before.rules[0].defined.line();
    let out = edit::delete_rule_line(SRC, line).unwrap();

    let after = load_str(&out).unwrap();
    assert_eq!(after.rules.len(), 3, "只该少一条规则");
    assert_eq!(after.rules[0].value, "IP-CIDR,192.168.0.0/16,DIRECT,no-resolve");
    assert_eq!(after.rules[2].value, "MATCH,日本节点");

    assert!(!out.contains("# 广告一律拦掉"), "被删规则的前导注释要一起走");
    for c in ["# websieve 配置", "# 内网直连，别删这条", "# 家里的网段"] {
        assert!(out.contains(c), "别人的注释不能被牵连：{c}");
    }

    // 与替换一样，用全称断言收口：删除的效果应当**恰好**是去掉那两行，
    // 其余每一行逐字节原样。这里直接拼出期望全文。
    let expected: String = SRC
        .split_inclusive('\n')
        .enumerate()
        .filter(|(i, _)| *i + 1 != line as usize && *i + 1 != line as usize - 1)
        .map(|(_, l)| l)
        .collect();
    assert_eq!(out, expected, "删除只应少掉那两行，别的一个字节都不许动");
}

#[test]
fn a_crlf_file_round_trips_without_line_ending_conversion() {
    // 用户在 Windows 上手写的配置是 CRLF。把它悄悄转成 LF，
    // 会让整份文件在 git diff 里全红 —— 用户的每一行都「被改过」了。
    let src = SRC.replace('\n', "\r\n");
    let before = load_str(&src).unwrap();
    let line = before.rules[2].defined.line();

    let out = edit::replace_rule_line(&src, line, "GEOSITE,cn,日本节点").unwrap();
    let edited = only_line_that_changed(&src, &out, line);
    assert_eq!(edited, "  - GEOSITE,cn,日本节点\r\n", "被改的那行也得是 CRLF");
    assert!(!out.contains("\n\n"), "不该出现裸 LF：{out:?}");

    let after = load_str(&out).unwrap();
    assert_eq!(after.rules.len(), 4);
    assert_eq!(after.rules[2].value, "GEOSITE,cn,日本节点");

    let same = edit::replace_rule_line(&src, line, &before.rules[2].value).unwrap();
    assert_eq!(same, src, "CRLF 文件的同值替换也必须逐字节恒等");
}
