//! 判决试算 CLI。
//!
//!   cargo run -p wsieve-route --example route -- \
//!       --rules rules.txt --geo-dir /tmp/wsieve-geo \
//!       --outbounds "日本节点,新加坡" \
//!       example.com:443
//!
//! rules.txt 每行一条 Clash 规则，支持 # 注释与空行。
//! 不做 DNS 解析：遇到 NeedResolve 会如实打印出来，这正是
//! 「哪些规则需要解析」的可视化。

use std::collections::HashSet;
use std::net::IpAddr;
use std::path::PathBuf;

use wsieve_geo::GeoDb;
use wsieve_proto::addr::{AddrPort, TargetAddr};
use wsieve_route::{Mode, RuleSet, Verdict};

fn main() {
    // 必须自己打印 Display。若直接让 main 返回 Result，Rust 打印的是
    // **Debug** 形式 —— thiserror 的 Debug 是 derive 来的，输出会是
    // `Error: MissingMatch`，而不是我们精心写的那段中文解释。
    if let Err(e) = run() {
        eprintln!("错误：{e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut rules_path = PathBuf::from("rules.txt");
    let mut geo_dir = PathBuf::from("/tmp/wsieve-geo");
    let mut outbounds: Vec<String> = Vec::new();
    let mut mode = Mode::Rule;
    let mut target_str: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--rules" => rules_path = args.next().ok_or("--rules 缺少参数")?.into(),
            "--geo-dir" => geo_dir = args.next().ok_or("--geo-dir 缺少参数")?.into(),
            "--outbounds" => {
                outbounds = args
                    .next()
                    .ok_or("--outbounds 缺少参数")?
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            }
            "--mode" => mode = args.next().ok_or("--mode 缺少参数")?.parse()?,
            other => target_str = Some(other.to_string()),
        }
    }

    let target_str = target_str.ok_or("用法：… [选项] <host:port>")?;
    let target = parse_target(&target_str)?;

    let lines: Vec<String> = std::fs::read_to_string(&rules_path)
        .map_err(|e| format!("读取 {} 失败：{e}", rules_path.display()))?
        .lines()
        .map(|s| s.to_string())
        .collect();

    let known: HashSet<String> = outbounds.into_iter().collect();
    let set = RuleSet::build(&lines, mode, "", &known)?;
    let geo = GeoDb::new(geo_dir.join("geoip.dat"), geo_dir.join("geosite.dat"));

    println!("目标：{}", target.display());
    println!("规则：{} 条", set.len());

    // GEO 引用校验：类别写错的规则永远不会命中，而「不命中」与
    // 「没写这条规则」表现完全一致 —— 不在这里说出来就永远无人察觉。
    // 告警不阻断判决（设计文档 §12），照常往下走。
    let warnings = set.check_geo(&geo);
    if !warnings.is_empty() {
        println!("GEO 告警：{} 条", warnings.len());
        for w in &warnings {
            println!("  警告：{w}");
        }
    }

    match set.evaluate(&target, None, &geo) {
        Verdict::Decided(d) => println!("判决：{d:?}（第一轮，未解析 DNS）"),
        Verdict::NeedResolve { domain } => {
            println!("第一轮请求解析：{domain}");
            // 演示第二轮：这里不真解析，用空结果表示「解析失败」
            match set.evaluate(&target, Some(&[] as &[IpAddr]), &geo) {
                Verdict::Decided(d) => {
                    println!("判决：{d:?}（第二轮，按解析失败处理）")
                }
                Verdict::NeedResolve { .. } => {
                    unreachable!("第二轮不该再请求解析 —— 若出现，是引擎 bug")
                }
            }
        }
    }
    Ok(())
}

fn parse_target(s: &str) -> Result<AddrPort, Box<dyn std::error::Error>> {
    let (host, port) = s.rsplit_once(':').ok_or("目标格式应为 host:port")?;
    let port: u16 = port.parse()?;
    // 去掉 IPv6 地址两端的方括号（如 [::1]:443）
    let host = host.trim_matches(|c| c == '[' || c == ']');
    let addr = match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(a)) => TargetAddr::V4(a.octets()),
        Ok(IpAddr::V6(a)) => TargetAddr::V6(a.octets()),
        Err(_) => TargetAddr::Domain(host.to_string()),
    };
    Ok(AddrPort { addr, port })
}
