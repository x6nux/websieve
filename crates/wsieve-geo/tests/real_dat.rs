//! 用真实 geoip.dat / geosite.dat 验收。
//!
//! 文件不存在时自动跳过：CI 不应依赖外部下载。本地验收时先执行
//!   mkdir -p /tmp/wsieve-geo
//!   curl -Lo /tmp/wsieve-geo/geoip.dat   https://github.com/Loyalsoldier/v2ray-rules-dat/releases/latest/download/geoip.dat
//!   curl -Lo /tmp/wsieve-geo/geosite.dat https://github.com/Loyalsoldier/v2ray-rules-dat/releases/latest/download/geosite.dat

use std::path::PathBuf;

fn dir() -> PathBuf {
    PathBuf::from("/tmp/wsieve-geo")
}

#[test]
fn real_geoip_cn_contains_known_chinese_address() {
    let p = dir().join("geoip.dat");
    if !p.exists() {
        eprintln!("跳过：{} 不存在", p.display());
        return;
    }
    let db = wsieve_geo::IpDb::parse(&std::fs::read(&p).unwrap()).unwrap();
    assert!(db.has("cn"), "geoip.dat 应含 cn 类别");
    // 114.114.114.114 是南京信风 DNS，稳定属于 CN
    assert!(db.matches("cn", "114.114.114.114".parse().unwrap()));
    // 8.8.8.8 是 Google DNS，绝不属于 CN
    assert!(!db.matches("cn", "8.8.8.8".parse().unwrap()));
}

#[test]
fn real_geosite_cn_and_ads_behave() {
    let p = dir().join("geosite.dat");
    if !p.exists() {
        eprintln!("跳过：{} 不存在", p.display());
        return;
    }
    let db = wsieve_geo::SiteDb::parse(&std::fs::read(&p).unwrap()).unwrap();
    assert!(db.has("cn"));
    assert!(db.matches("cn", "www.baidu.com"), "baidu 应属 cn");
    assert!(!db.matches("cn", "www.google.com"), "google 不应属 cn");
    eprintln!("跳过的 Regex 条目数：{}", db.skipped_regex());
}
