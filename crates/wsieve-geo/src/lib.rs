//! geosite.dat / geoip.dat 的解析与匹配。
//!
//! 这两个文件是 v2ray 系的事实标准格式，protobuf 编码。结构极简
//! （枚举 + 字符串 + bytes + uint32，三层嵌套），因此手写 wire format
//! 解析而不引入 prost —— 与仓库里手写 base64 的取舍一致
//! （见 src-tauri/src/bridge.rs:285 的注释）。
//!
//! 权威定义见 .research/Xray-core/common/geodata/geodat.proto。

pub mod ip;
pub mod pb;
pub mod site;

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::OnceLock;

pub use ip::IpDb;
pub use site::SiteDb;

#[derive(Debug, thiserror::Error)]
pub enum GeoError {
    #[error("读取 {path} 失败：{source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("解析 {path} 失败：{source}")]
    Parse {
        path: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// GEO 数据的门面。两个文件各自**惰性加载**：只用 geoip 的配置
/// 永远不会为 geosite 付出解析代价（geosite.dat 通常是前者的数倍大）。
///
/// 加载失败不 panic，错误上抛给调用方 —— 按设计文档 §12，
/// 涉 GEO 的规则跳过并告警，绝不阻断启动。
pub struct GeoDb {
    ip_path: PathBuf,
    site_path: PathBuf,
    ip: OnceLock<Result<IpDb, (FailKind, String)>>,
    site: OnceLock<Result<SiteDb, (FailKind, String)>>,
}

/// OnceLock 里缓存的失败原因。
///
/// 必须带上分类：读失败与解析失败是两类问题（前者多半是文件缺失或权限，
/// 后者说明下载来的 .dat 本身是坏的），报错时不能混为一谈。
/// 只存分类与消息而不存 GeoError 本身，是因为 GeoError 不是 Clone
/// （io::Error 不是），而 OnceLock 里的值只能借出去。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailKind {
    Read,
    Parse,
}

impl GeoDb {
    pub fn new(ip_path: PathBuf, site_path: PathBuf) -> Self {
        Self {
            ip_path,
            site_path,
            ip: OnceLock::new(),
            site: OnceLock::new(),
        }
    }

    /// 把缓存下来的 (分类, 消息) 还原成对应的 GeoError。
    /// 消息里不再自带路径前缀 —— 前缀由这里的 error 模板统一加，
    /// 否则用户会看到「读取 X 失败：解析 X 失败：…」这种双重前缀。
    fn to_error(path: &std::path::Path, (kind, msg): &(FailKind, String)) -> GeoError {
        let path = path.display().to_string();
        match kind {
            FailKind::Read => GeoError::Io {
                path,
                source: std::io::Error::other(msg.clone()),
            },
            FailKind::Parse => GeoError::Parse {
                path,
                source: msg.clone().into(),
            },
        }
    }

    pub fn ip_matches(&self, code: &str, addr: IpAddr) -> Result<bool, GeoError> {
        let db = self.ensure_ip_loaded()?;
        Ok(db.matches(code, addr))
    }

    /// 触发 geoip 的惰性加载并借出解析结果。与 ensure_site_loaded 同构。
    fn ensure_ip_loaded(&self) -> Result<&IpDb, GeoError> {
        let db = self.ip.get_or_init(|| {
            std::fs::read(&self.ip_path)
                .map_err(|e| (FailKind::Read, e.to_string()))
                .and_then(|buf| {
                    IpDb::parse(&buf).map_err(|e| (FailKind::Parse, e.to_string()))
                })
        });
        db.as_ref().map_err(|fail| Self::to_error(&self.ip_path, fail))
    }

    pub fn site_matches(&self, code: &str, domain: &str) -> Result<bool, GeoError> {
        let db = self.ensure_site_loaded()?;
        Ok(db.matches(code, domain))
    }

    /// 触发 geosite 的惰性加载并借出解析结果。
    ///
    /// 单独抽出来，是为了让 has_site_class 不必再靠
    /// site_matches(code, "\0invalid\0") 这种哨兵串来强制加载 ——
    /// 那会白跑一整轮匹配（含线性 substr 扫描）只为把结果扔掉。
    fn ensure_site_loaded(&self) -> Result<&SiteDb, GeoError> {
        let db = self.site.get_or_init(|| {
            std::fs::read(&self.site_path)
                .map_err(|e| (FailKind::Read, e.to_string()))
                .and_then(|buf| {
                    SiteDb::parse(&buf).map_err(|e| (FailKind::Parse, e.to_string()))
                })
        });
        db.as_ref().map_err(|fail| Self::to_error(&self.site_path, fail))
    }

    /// 加载阶段校验用：规则引用的类别是否存在。
    /// 返回 Err 表示文件本身读不了或解析不了；Ok(false) 表示文件正常但没这个类别。
    pub fn has_site_class(&self, code: &str) -> Result<bool, GeoError> {
        Ok(self.ensure_site_loaded()?.has(code))
    }

    /// geoip 侧的同款校验。同样不再靠 ip_matches(code, 0.0.0.0) 这种
    /// 哨兵地址来强制加载 —— 那既白跑一次二分查找，又要在之后
    /// 重新从 OnceLock 里把库捞出来，读起来像是在绕开自己的 API。
    pub fn has_ip_class(&self, code: &str) -> Result<bool, GeoError> {
        Ok(self.ensure_ip_loaded()?.has(code))
    }

    /// 惰性加载的测试探针：geosite 是否已经被加载过。
    /// 对外不是 API 契约的一部分，仅供本 crate 的测试断言「没被提前加载」。
    #[doc(hidden)]
    pub fn __site_loaded_for_test(&self) -> bool {
        self.site.get().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_reports_path_not_panics() {
        let db = GeoDb::new("/nonexistent/geoip.dat".into(), "/nonexistent/geosite.dat".into());
        let err = db.site_matches("cn", "a.com").unwrap_err().to_string();
        assert!(err.contains("geosite.dat"), "错误信息要指明是哪个文件：{err}");
    }

    /// 惰性加载：只查 geoip 时 geosite 绝不能被读。
    ///
    /// 用探针直接断言 OnceLock 还是空的。旧版本只断言 ip_matches 成功，
    /// 那是恒真的 —— new() 不返回 Result，改成预加载也只能把 Err 塞进
    /// OnceLock，ip 查询照样成功，测试照样绿。现在改成预加载会立刻红。
    #[test]
    fn site_file_is_not_read_until_first_query() {
        let dir = std::env::temp_dir().join("wsieve-geo-lazy-test");
        std::fs::create_dir_all(&dir).unwrap();
        let ip_path = dir.join("geoip.dat");
        std::fs::write(&ip_path, Vec::<u8>::new()).unwrap();

        let db = GeoDb::new(ip_path, "/nonexistent/geosite.dat".into());
        assert!(!db.__site_loaded_for_test(), "构造后不应加载 geosite");

        // 空文件解析出空库，查询返回 false 而非报错
        // （写成 assert!(!…) 而非 assert_eq!(…, false)，后者会被 clippy 拦）
        assert!(!db.ip_matches("cn", "1.2.3.4".parse().unwrap()).unwrap());
        assert!(!db.__site_loaded_for_test(), "查 geoip 不应牵连 geosite");

        // 反向守住：真的查 geosite 时它必须被加载（这里注定失败，但 OnceLock 已落值）
        assert!(db.site_matches("cn", "a.com").is_err());
        assert!(db.__site_loaded_for_test(), "查过之后应已加载");
    }

    /// I4：读失败归 Io、解析失败归 Parse，且路径只出现一次。
    #[test]
    fn read_failure_is_io_and_parse_failure_is_parse() {
        let dir = std::env::temp_dir().join("wsieve-geo-errkind-test");
        std::fs::create_dir_all(&dir).unwrap();

        // 文件不存在 → Io
        let missing = GeoDb::new("/nonexistent/geoip.dat".into(), "/nonexistent/geosite.dat".into());
        let err = missing.ip_matches("cn", "1.2.3.4".parse().unwrap()).unwrap_err();
        assert!(matches!(err, GeoError::Io { .. }), "读不到文件应是 Io：{err:?}");

        // 文件存在但内容是坏的 → Parse。CIDR 长度 3 字节（合法只有 4 或 16）
        let bad = dir.join("bad-geoip.dat");
        // GeoIPList.entry{ GeoIP.country_code="cn", GeoIP.cidr{ ip=3 字节, prefix=8 } }
        let cidr = [0x0A, 0x03, 1, 2, 3, 0x10, 0x08];
        let mut geoip = vec![0x0A, 0x02, b'c', b'n'];
        geoip.push(0x12);
        geoip.push(cidr.len() as u8);
        geoip.extend_from_slice(&cidr);
        let mut list = vec![0x0A, geoip.len() as u8];
        list.extend_from_slice(&geoip);
        std::fs::write(&bad, &list).unwrap();

        let db = GeoDb::new(bad.clone(), "/nonexistent/geosite.dat".into());
        let err = db.ip_matches("cn", "1.2.3.4".parse().unwrap()).unwrap_err();
        assert!(matches!(err, GeoError::Parse { .. }), "内容坏了应是 Parse：{err:?}");

        // 路径只出现一次 —— 修复前是「读取 X 失败：解析 X 失败：…」
        let text = err.to_string();
        let path = bad.display().to_string();
        assert_eq!(text.matches(&path).count(), 1, "路径不应重复出现：{text}");
        assert!(!text.contains("读取"), "解析失败不应报成读取失败：{text}");
    }

    /// I4 配套：has_site_class 不再靠哨兵串触发加载，行为仍需正确。
    #[test]
    fn has_site_class_reports_missing_file_as_error() {
        let db = GeoDb::new("/nonexistent/geoip.dat".into(), "/nonexistent/geosite.dat".into());
        assert!(db.has_site_class("cn").is_err(), "文件读不了应上报错误而非 Ok(false)");
    }
}
