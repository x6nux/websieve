//! hosts 劫持：把服务端域名指向本地转发器，为多端口条带铺路（见 `shard`）。
//!
//! 为什么需要：多会话条带的收益全部来自「每会话一条独立 TCP ⇒ 独立拥塞
//! 窗口」，而 HTTP/2 把同一 origin 的所有请求复用到一条 TCP 上（实测
//! `crates/wsieve-server/examples/webkit_tcp_probe.rs`：4 会话全挤 1 条）。
//! origin 是 `(scheme, host, port)`，端口不同即不同 origin——两个引擎实测
//! 都会开独立连接，且证书校验不看端口，一张普通单域名证书就够。
//!
//! 于是：hosts 把域名指向 127.0.0.1，本地转发器在多个高端口监听，各自开
//! 独立 TCP 到真实服务端的 :443。对外只有若干条到 :443 的普通 TLS 连接，
//! SNI 全部相同。
//!
//! 本模块只管 hosts 文件的读写，路径是参数（默认系统 hosts），因此全部
//! 逻辑可用临时文件测试，不需要 root。

use std::path::{Path, PathBuf};

/// 托管条目标记。删改只认这个标记，绝不碰用户自己的行。
const MARKER: &str = "# wsieve-managed";

/// 系统 hosts 路径。
pub fn system_path() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\Windows\System32\drivers\etc\hosts")
    } else {
        PathBuf::from("/etc/hosts")
    }
}

/// 去掉全部托管行，其余内容逐字节保留（这是系统文件，不容许顺手「整理」）。
fn strip_managed(content: &str) -> String {
    let kept: Vec<&str> = content
        .lines()
        .filter(|l| !l.trim_end().ends_with(MARKER))
        .collect();
    let mut out = kept.join("\n");
    // 原文件以换行结尾的话保持结尾换行；空文件仍为空。
    if !out.is_empty() && content.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// 追加托管行（调用方保证已先 strip，故天然幂等）。
fn append_managed(content: &str, ip: &str, hosts: &[String]) -> String {
    if hosts.is_empty() {
        return content.to_string();
    }
    let mut out = content.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for h in hosts {
        out.push_str(&format!("{ip} {h} {MARKER}\n"));
    }
    out
}

/// 列出当前托管的主机名。
fn managed_hosts_in(content: &str) -> Vec<String> {
    content
        .lines()
        .filter(|l| l.trim_end().ends_with(MARKER))
        .filter_map(|l| {
            let body = l.trim_end().strip_suffix(MARKER)?;
            // 格式：`<ip> <host> `
            body.split_whitespace().nth(1).map(|s| s.to_string())
        })
        .collect()
}

pub struct HostsFile {
    path: PathBuf,
}

impl HostsFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 能否写入。不可写时调用方降级到单会话并打警告，而不是拒绝启动
    /// （与 mux 协商失败的处理一致：优先建立连接 + 警告日志）。
    pub fn writable(&self) -> bool {
        // 直接试着以追加方式打开——权限的唯一可靠判据是真去开一次，
        // 光看 metadata 的 readonly 位在 Unix 上不反映属主/组权限。
        std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .is_ok()
    }

    fn read(&self) -> std::io::Result<String> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) => Ok(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e),
        }
    }

    /// 当前托管的主机名（诊断与测试用）。
    #[allow(dead_code)]
    pub fn managed_hosts(&self) -> std::io::Result<Vec<String>> {
        Ok(managed_hosts_in(&self.read()?))
    }

    /// 清掉全部托管行。
    pub fn clear_managed(&self) -> std::io::Result<()> {
        let content = self.read()?;
        let stripped = strip_managed(&content);
        if stripped == content {
            return Ok(()); // 无托管行，不写盘（少一次系统文件改动）
        }
        std::fs::write(&self.path, stripped)
    }

    /// 设为「只有这些托管条目」。先清后写，故重复调用幂等。
    pub fn set_managed(&self, ip: &str, hosts: &[String]) -> std::io::Result<()> {
        let content = self.read()?;
        let next = append_managed(&strip_managed(&content), ip, hosts);
        if next == content {
            return Ok(());
        }
        std::fs::write(&self.path, next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_LINES: &str = "127.0.0.1 localhost\n::1 localhost\n# 用户自己的注释\n10.0.0.5 intranet.corp\n";

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "wsieve-hosts-{}-{}-{name}",
            std::process::id(),
            // 同一进程内多个测试并发，用 name 区分即可
            name.len()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn set_then_clear_restores_file_byte_for_byte() {
        let p = tmp("roundtrip");
        std::fs::write(&p, USER_LINES).unwrap();
        let h = HostsFile::new(&p);

        h.set_managed("127.0.0.1", &["x.com".to_string()]).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("127.0.0.1 x.com # wsieve-managed"));
        assert!(after.contains("10.0.0.5 intranet.corp"));

        h.clear_managed().unwrap();
        // 用户的行必须一字节不差地回来——我们改的是系统文件。
        assert_eq!(std::fs::read_to_string(&p).unwrap(), USER_LINES);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn set_managed_is_idempotent() {
        let p = tmp("idem");
        std::fs::write(&p, USER_LINES).unwrap();
        let h = HostsFile::new(&p);
        let hosts = vec!["x.com".to_string()];
        h.set_managed("127.0.0.1", &hosts).unwrap();
        let once = std::fs::read_to_string(&p).unwrap();
        h.set_managed("127.0.0.1", &hosts).unwrap();
        h.set_managed("127.0.0.1", &hosts).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), once);
        assert_eq!(h.managed_hosts().unwrap(), hosts);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn stale_entries_are_replaced_not_accumulated() {
        // 上次运行残留的条目必须被换掉：否则域名会一直指向没在跑的转发器。
        let p = tmp("stale");
        std::fs::write(&p, USER_LINES).unwrap();
        let h = HostsFile::new(&p);
        h.set_managed("127.0.0.1", &["old.com".to_string()]).unwrap();
        h.set_managed("127.0.0.1", &["new.com".to_string()]).unwrap();
        assert_eq!(h.managed_hosts().unwrap(), vec!["new.com".to_string()]);
        let c = std::fs::read_to_string(&p).unwrap();
        assert!(!c.contains("old.com"));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn user_line_ending_with_similar_text_is_not_touched() {
        // 只认行尾的完整标记，不能误伤内容里恰好含这几个字的用户行。
        let content = "1.2.3.4 example.com # wsieve-managed-by-hand\n";
        assert_eq!(strip_managed(content), content);
    }

    #[test]
    fn missing_file_is_treated_as_empty() {
        let h = HostsFile::new(tmp("missing"));
        assert!(h.managed_hosts().unwrap().is_empty());
        assert!(h.clear_managed().is_ok());
    }

    #[test]
    fn multiple_hosts_all_recorded() {
        let p = tmp("multi");
        std::fs::write(&p, "").unwrap();
        let h = HostsFile::new(&p);
        let hosts: Vec<String> = ["a.com", "b.com"].iter().map(|s| s.to_string()).collect();
        h.set_managed("127.0.0.1", &hosts).unwrap();
        assert_eq!(h.managed_hosts().unwrap(), hosts);
        std::fs::remove_file(&p).ok();
    }
}
