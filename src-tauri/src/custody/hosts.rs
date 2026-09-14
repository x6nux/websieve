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
//!
//! 文件末尾的 `HostsCustody` 把这套读写接进 §10 的 `ManagedSystemState`
//! 统一接口 —— 底下的 `HostsFile` 逻辑一行未改，它本来就是这个模式的范本。

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

/// hosts 条目的托管封装（设计文档 §10）。
///
/// 底下的 `HostsFile` 逻辑一行未改 —— 它本来就是 §10 这个模式的范本，
/// 这里只是把它接进统一接口，好让系统代理与 TUN 路由共用同一套纪律。
pub struct HostsCustody {
    file: std::sync::Arc<HostsFile>,
    ip: String,
    hosts: Vec<String>,
}

impl HostsCustody {
    pub fn new(file: std::sync::Arc<HostsFile>, ip: String, hosts: Vec<String>) -> Self {
        Self { file, ip, hosts }
    }

    /// 只为清残留而建的托管项：不管任何域名，`apply` 是 no-op。
    ///
    /// 用在启动时那些「本次不打算劫持」的分支上 —— 上次崩溃留下的条目
    /// 照样得清。没有它的话，只要本次启动早退（比如条带被关掉），
    /// 残留就会永远留在 hosts 里，域名一直指向一个不在跑的转发器。
    pub fn cleanup_only(file: std::sync::Arc<HostsFile>) -> Self {
        Self {
            file,
            ip: String::new(),
            hosts: Vec::new(),
        }
    }

    /// hosts 文件是否可写。不可写时调用方降级到单会话并打警告，
    /// 而不是拒绝启动（见 `HostsFile::writable`）。
    #[cfg(test)]
    pub fn writable(&self) -> bool {
        self.file.writable()
    }
}

impl crate::custody::ManagedSystemState for HostsCustody {
    fn name(&self) -> &'static str {
        "hosts 条目"
    }

    fn apply(&self) -> anyhow::Result<()> {
        self.file.set_managed(&self.ip, &self.hosts)?;
        Ok(())
    }

    fn revert(&self) -> anyhow::Result<()> {
        self.file.clear_managed()?;
        Ok(())
    }

    fn clear_stale(&self) -> anyhow::Result<()> {
        // 与 revert 同一动作：摘掉所有带 marker 的行。
        // 崩溃路径与正常退出路径共用这一条，正是它幂等的价值。
        self.file.clear_managed()?;
        Ok(())
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

#[cfg(test)]
mod custody_tests {
    use super::*;
    use crate::custody::{CustodyGuard, ManagedSystemState};
    use std::sync::Arc;

    /// 每个测试一份独立临时文件 —— 单测在同进程内并发跑，共用一个路径
    /// 会互相踩，「摘干净了」与「被隔壁清掉了」在断言上无法区分。
    ///
    /// **绝不指向真实 /etc/hosts**：这套测试改的是系统文件的内容，
    /// 一旦落到真路径上，一次跑挂就在开发机上留下一条劫持行。
    fn temp_hosts(tag: &str, content: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "wsieve-custody-hosts-{}-{tag}",
            std::process::id()
        ));
        std::fs::write(&p, content).unwrap();
        p
    }

    fn custody(p: &PathBuf, hosts: Vec<String>) -> HostsCustody {
        HostsCustody::new(Arc::new(HostsFile::new(p)), "127.0.0.1".into(), hosts)
    }

    #[test]
    fn guard_writes_all_domains_and_removes_them_on_drop() {
        let p = temp_hosts("roundtrip", "127.0.0.1 localhost\n");
        {
            let _g =
                CustodyGuard::acquire(custody(&p, vec!["a.com".into(), "b.net".into()])).unwrap();
            let s = std::fs::read_to_string(&p).unwrap();
            assert!(s.contains("a.com"), "多域名要一次写入：{s}");
            assert!(s.contains("b.net"));
            assert!(s.contains("localhost"), "用户原有条目不能动");
        }
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(!s.contains("a.com"), "drop 后必须摘干净：{s}");
        assert!(!s.contains("b.net"));
        assert!(s.contains("localhost"), "用户条目仍在");
        std::fs::remove_file(&p).ok();
    }

    /// **崩溃恢复的核心测试**：上次运行写完 hosts 就被 SIGKILL，
    /// `revert` 根本没跑到，条目留在文件里。下次启动必须把它清掉 ——
    /// 否则 ghost.com 会一直指向一个已经不在跑的转发器。
    #[test]
    fn stale_entries_from_a_crash_are_cleared_on_acquire() {
        let p = temp_hosts("stale", "127.0.0.1 localhost\n");

        // 第一次运行：托管生效，条目写进文件。
        let g = CustodyGuard::acquire(custody(&p, vec!["ghost.com".into()])).unwrap();
        assert!(std::fs::read_to_string(&p).unwrap().contains("ghost.com"));
        // 模拟 SIGKILL：跳过 Drop，让残留原样留在盘上。
        // （std::mem::forget 正是「进程没机会 revert」的等价物）
        std::mem::forget(g);
        let crashed = std::fs::read_to_string(&p).unwrap();
        assert!(crashed.contains("ghost.com"), "残留没造出来，测试本身失效");

        // 第二次启动：clear_stale 必须把上次的残留清掉。
        let _g = CustodyGuard::acquire(custody(&p, vec!["a.com".into()])).unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(!s.contains("ghost.com"), "上次的残留必须被清掉：{s}");
        assert!(s.contains("a.com"));
        assert!(s.contains("localhost"), "用户条目全程不动");
        std::fs::remove_file(&p).ok();
    }

    /// 崩溃后**本次不打算 apply** 也要能恢复：用户重启时把条带关了，
    /// 残留仍须被清。这条路径 `CustodyGuard::acquire` 覆盖不到 ——
    /// 空域名列表下 apply 是 no-op，靠的纯粹是 clear_stale。
    #[test]
    fn clear_stale_recovers_even_when_this_run_applies_nothing() {
        let p = temp_hosts("stale-noapply", "127.0.0.1 localhost\n");
        let g = CustodyGuard::acquire(custody(&p, vec!["ghost.com".into()])).unwrap();
        std::mem::forget(g); // 崩溃

        let c = custody(&p, vec![]);
        c.clear_stale().unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(!s.contains("ghost.com"), "本次不 apply 也必须清残留：{s}");
        assert_eq!(s, "127.0.0.1 localhost\n", "用户文件必须字节还原");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn revert_and_clear_stale_are_idempotent() {
        // 幂等是 trait 的硬要求：clear_stale 会在正常退出后再跑一次
        // （drop 已 revert 过），此时不能报错也不能动用户的行。
        let p = temp_hosts("idem", "127.0.0.1 localhost\n");
        let c = custody(&p, vec!["a.com".into()]);
        c.clear_stale().unwrap();
        c.clear_stale().unwrap();
        c.revert().unwrap();
        c.apply().unwrap();
        c.apply().unwrap();
        let once = std::fs::read_to_string(&p).unwrap();
        c.apply().unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), once, "apply 必须幂等");
        c.revert().unwrap();
        c.revert().unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "127.0.0.1 localhost\n"
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn unwritable_path_fails_loudly_instead_of_pretending_to_work() {
        // 静默成功会让上层以为劫持生效，随后按条带端口去连一个没人监听的口。
        let c = custody(
            &PathBuf::from("/proc/definitely-not-writable/hosts"),
            vec!["a.com".into()],
        );
        assert!(!c.writable());
        let e = match CustodyGuard::acquire(c) {
            Ok(_) => panic!("不可写路径必须报错"),
            Err(e) => e,
        };
        let msg = format!("{e:#}");
        assert!(msg.contains("hosts 条目"), "错误缺少托管项名字：{msg}");
    }
}

