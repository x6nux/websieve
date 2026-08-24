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
    ip: OnceLock<Result<IpDb, String>>,
    site: OnceLock<Result<SiteDb, String>>,
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

    pub fn ip_matches(&self, code: &str, addr: IpAddr) -> Result<bool, GeoError> {
        let db = self.ip.get_or_init(|| {
            std::fs::read(&self.ip_path)
                .map_err(|e| format!("读取 {} 失败：{e}", self.ip_path.display()))
                .and_then(|buf| {
                    IpDb::parse(&buf)
                        .map_err(|e| format!("解析 {} 失败：{e}", self.ip_path.display()))
                })
        });
        match db {
            Ok(db) => Ok(db.matches(code, addr)),
            Err(msg) => Err(GeoError::Io {
                path: self.ip_path.display().to_string(),
                source: std::io::Error::other(msg.clone()),
            }),
        }
    }

    pub fn site_matches(&self, code: &str, domain: &str) -> Result<bool, GeoError> {
        let db = self.site.get_or_init(|| {
            std::fs::read(&self.site_path)
                .map_err(|e| format!("读取 {} 失败：{e}", self.site_path.display()))
                .and_then(|buf| {
                    SiteDb::parse(&buf)
                        .map_err(|e| format!("解析 {} 失败：{e}", self.site_path.display()))
                })
        });
        match db {
            Ok(db) => Ok(db.matches(code, domain)),
            Err(msg) => Err(GeoError::Io {
                path: self.site_path.display().to_string(),
                source: std::io::Error::other(msg.clone()),
            }),
        }
    }

    /// 加载阶段校验用：规则引用的类别是否存在。
    /// 返回 Err 表示文件本身读不了；Ok(false) 表示文件正常但没这个类别。
    pub fn has_site_class(&self, code: &str) -> Result<bool, GeoError> {
        self.site_matches(code, "\u{0}invalid\u{0}")?; // 触发加载
        Ok(self
            .site
            .get()
            .and_then(|r| r.as_ref().ok())
            .map(|db| db.has(code))
            .unwrap_or(false))
    }

    pub fn has_ip_class(&self, code: &str) -> Result<bool, GeoError> {
        self.ip_matches(code, IpAddr::from([0, 0, 0, 0]))?;
        Ok(self
            .ip
            .get()
            .and_then(|r| r.as_ref().ok())
            .map(|db| db.has(code))
            .unwrap_or(false))
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

    #[test]
    fn site_file_is_not_read_until_first_query() {
        // geosite 路径不存在，但只查 geoip 不应触发它的加载
        let dir = std::env::temp_dir().join("wsieve-geo-lazy-test");
        std::fs::create_dir_all(&dir).unwrap();
        let ip_path = dir.join("geoip.dat");
        std::fs::write(&ip_path, Vec::<u8>::new()).unwrap();

        let db = GeoDb::new(ip_path, "/nonexistent/geosite.dat".into());
        // 空文件解析出空库，查询返回 false 而非报错
        // （写成 assert!(!…) 而非 assert_eq!(…, false)，后者会被 clippy 拦）
        assert!(!db.ip_matches("cn", "1.2.3.4".parse().unwrap()).unwrap());
    }
}
