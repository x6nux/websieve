//! 用真实 geoip.dat / geosite.dat 验收。
//!
//! 这些用例标了 #[ignore]：它们依赖外部下载的文件，CI 不应依赖网络。
//! 「文件不存在就 return」的老写法会让测试报成 passed —— 跳过等于绿，
//! 是个安静的覆盖率黑洞。现在默认不跑，要跑就得显式点名，
//! 于是「跑了什么」是诚实的。
//!
//! 本地验收：
//!   mkdir -p /tmp/wsieve-geo
//!   curl -Lo /tmp/wsieve-geo/geoip.dat   https://github.com/Loyalsoldier/v2ray-rules-dat/releases/latest/download/geoip.dat
//!   curl -Lo /tmp/wsieve-geo/geosite.dat https://github.com/Loyalsoldier/v2ray-rules-dat/releases/latest/download/geosite.dat
//!   cargo test -p wsieve-geo --test real_dat -- --ignored --nocapture

use std::path::PathBuf;

fn dir() -> PathBuf {
    PathBuf::from("/tmp/wsieve-geo")
}

/// 文件缺失时直接 panic 并说明怎么补 —— 显式点名要跑却跑不成，
/// 是真失败，不能再静默放过。
fn read_fixture(name: &str) -> Vec<u8> {
    let p = dir().join(name);
    match std::fs::read(&p) {
        Ok(buf) => buf,
        Err(e) => panic!(
            "验收数据 {} 读不到（{e}）。先按本文件顶部的 curl 命令下载。",
            p.display()
        ),
    }
}

#[test]
#[ignore = "依赖外部下载的 .dat，用 --ignored 显式运行"]
fn real_geoip_cn_contains_known_chinese_address() {
    let db = wsieve_geo::IpDb::parse(&read_fixture("geoip.dat")).unwrap();
    assert!(db.has("cn"), "geoip.dat 应含 cn 类别");
    // 114.114.114.114 是南京信风 DNS，稳定属于 CN
    assert!(db.matches("cn", "114.114.114.114".parse().unwrap()));
    // 8.8.8.8 是 Google DNS，绝不属于 CN
    assert!(!db.matches("cn", "8.8.8.8".parse().unwrap()));
}

#[test]
#[ignore = "依赖外部下载的 .dat，用 --ignored 显式运行"]
fn real_geosite_cn_and_ads_behave() {
    let db = wsieve_geo::SiteDb::parse(&read_fixture("geosite.dat")).unwrap();
    assert!(db.has("cn"));
    assert!(db.matches("cn", "www.baidu.com"), "baidu 应属 cn");
    assert!(!db.matches("cn", "www.google.com"), "google 不应属 cn");
    eprintln!("跳过的条目数（Regex/空值/超深）：{}", db.skipped_regex());
}

/// B2 的现实回归：真实 geosite 里不该存在能匹配一切的类别。
///
/// 评审确认当前 Loyalsoldier 的 geosite.dat 里没有空 Substr 条目，
/// 但那是数据的偶然属性而非代码的保证 —— 这里用一个与任何真实条目
/// 都无关的随机域名守住：cn 类别绝不能把它认走。
#[test]
#[ignore = "依赖外部下载的 .dat，用 --ignored 显式运行"]
fn real_geosite_cn_does_not_match_everything() {
    let db = wsieve_geo::SiteDb::parse(&read_fixture("geosite.dat")).unwrap();
    for d in [
        "zzq7x4k9-not-a-real-domain.example",
        "mybank.com",
        "login.microsoft.com",
    ] {
        assert!(!db.matches("cn", d), "cn 不应匹配 {d} —— 疑似空条目导致全匹配");
    }
}
