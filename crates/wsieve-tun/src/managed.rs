//! TUN 路由的托管（设计文档 §10 的第三个实现，hosts 与系统代理是前两个）。
//!
//! 三处共用一套纪律：`apply` / `revert` 幂等、`clear_stale` 在任何 `apply`
//! 之前调用、托管条目**可识别**。
//!
//! # 为什么这里没有再定义一遍 `ManagedSystemState`
//!
//! 那个 trait 已经存在于 `src-tauri/src/custody/mod.rs`，且它的文档注释里
//! 写明「日后 TUN 路由接进来时可以用 `Vec<Box<dyn ManagedSystemState>>`
//! 统一收拢」—— 收拢的前提是**只有一个** trait。本 crate 在依赖图的下游
//! （`src-tauri` 依赖 crates，反向不成立），照抄一份就成了两个同名不同源的
//! trait，`CustodyGuard` 永远收不进来，「复用不重建」当场破功。
//!
//! 于是这里给的是**同名同形的固有方法**（`apply` / `revert` / `clear_stale`），
//! 接线时在 `src-tauri` 侧写一个三行的转发 impl 即可（本地 trait + 外部类型，
//! 孤儿规则允许）。纪律一字不改，trait 定义仍然只有一处。
//!
//! # 崩溃残留在路由表上的形状
//!
//! 比 hosts 更隐蔽：一条指向**已经不存在的 utun 设备**的 `0.0.0.0/1`，
//! 会把半个 IPv4 空间黑洞掉，而用户在任何界面上都看不出这跟本程序有关。
//! 所以 `clear_stale` 必须在所有启动路径上跑到 —— **包括本次根本不打算开
//! TUN 的那些早退分支**（这正是 hosts 那边补过的洞，见 `custody/hosts.rs`
//! 的 `cleanup_only`）。
//!
//! # 托管判据：两类路由**判据不同**，这是本模块最容易出人命的地方
//!
//! | 类别 | 网关 | 能否靠扫路由表认出来 |
//! |---|---|---|
//! | 默认路由 `0/1` `128.0/1` | **TUN 地址** | ✅ 能。TUN 地址是我们自己造的，别人不会用 |
//! | bypass 主机路由 | **物理网关** | ❌ **不能** |
//!
//! bypass 路由指向物理网关 —— 和用户机器上一大票正常路由一模一样。
//! 「网关是物理网关」绝不能当作托管判据，否则 `clear_stale` 会删掉用户
//! 自己加的静态路由，把他的网络搞坏。因此 bypass 的残留**只认本进程记账**，
//! 认不出来的那部分明确记在文档里，不假装能处理。

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use crate::routes::{bypass_routes, tun_default_routes, RouteEntry};

/// 执行路由命令的后端。
///
/// 抽出来是为了让托管逻辑可以在无 root 环境下单测 —— 真实实现是
/// `route`(8) / netlink / Windows API，**都要提权**（`route` 实测即使加
/// `-t` test-only 也返回 77）。
pub trait RouteBackend: Send + Sync {
    fn add(&self, e: &RouteEntry) -> anyhow::Result<()>;
    fn delete(&self, e: &RouteEntry) -> anyhow::Result<()>;
    /// 列出当前由我们托管的路由。
    ///
    /// 判据见模块注释：默认路由靠「网关是 TUN 地址」认，bypass 只认记账。
    /// **实现者必须保证这里只返回自己的东西** —— 返回值会被原样删掉。
    fn list_managed(&self) -> anyhow::Result<Vec<RouteEntry>>;
}

/// TUN 路由的托管者。
pub struct TunRoutes {
    backend: Arc<dyn RouteBackend>,
    tun_gateway: String,
    phys_gateway: String,
    /// 当前需要 bypass 的服务器 IP。用 `BTreeSet` 而非 `Vec`：去重与有序
    /// 让 diff 可复现，也让「重复 sync 同一名单」天然是空操作。
    bypass_ips: Mutex<BTreeSet<IpAddr>>,
}

impl TunRoutes {
    pub fn new(
        backend: Arc<dyn RouteBackend>,
        tun_gateway: impl Into<String>,
        phys_gateway: impl Into<String>,
    ) -> Self {
        Self {
            backend,
            tun_gateway: tun_gateway.into(),
            phys_gateway: phys_gateway.into(),
            bypass_ips: Mutex::new(BTreeSet::new()),
        }
    }

    /// 出站上下线时**增量**更新 bypass 路由（§8.3.1：名单是动态的）。
    ///
    /// 只对差集动手。全删全加会在期间留出一个「服务器 IP 无 bypass」的
    /// 时间窗，正在跑的转发器连接会在那一瞬间掉进环路 —— 而且是间歇性的，
    /// 复现不了也查不出来。这是本模块存在增量逻辑的**全部**理由。
    ///
    /// 记账**逐条推进**而不是最后一次性赋值：中途某条写失败时，已经写进
    /// 系统的那些必须留在账上，否则它们再也没人认领，永远删不掉。
    pub fn sync_bypass(&self, want: &[IpAddr]) -> anyhow::Result<()> {
        let want: BTreeSet<IpAddr> = want.iter().copied().collect();
        let mut cur = self.bypass_ips.lock().expect("bypass 路由锁中毒");

        // 先加后删：反过来会在两步之间留出「旧的已删、新的没加」的窗口。
        for ip in want.difference(&*cur).copied().collect::<Vec<_>>() {
            for e in bypass_routes(&[ip], &self.phys_gateway) {
                self.backend
                    .add(&e)
                    .map_err(|err| anyhow::anyhow!("新增 bypass 路由 {ip} 失败：{err:#}"))?;
            }
            cur.insert(ip);
        }
        for ip in cur.difference(&want).copied().collect::<Vec<_>>() {
            for e in bypass_routes(&[ip], &self.phys_gateway) {
                // 删不掉要报出来：残留的 bypass 会让该 IP 永远绕过分流规则，
                // 也就是永远直连 —— 与 §6.4「禁止回退直连」同源的事故。
                self.backend
                    .delete(&e)
                    .map_err(|err| anyhow::anyhow!("删除 bypass 路由 {ip} 失败：{err:#}"))?;
            }
            cur.remove(&ip);
        }
        Ok(())
    }

    /// 当前 bypass 名单快照（诊断与测试用）。
    pub fn bypass_snapshot(&self) -> Vec<IpAddr> {
        self.bypass_ips
            .lock()
            .expect("bypass 路由锁中毒")
            .iter()
            .copied()
            .collect()
    }

    /// 本次托管应当存在的全部路由。
    fn all_entries(&self) -> Vec<RouteEntry> {
        let mut v = tun_default_routes(&self.tun_gateway);
        let ips: Vec<IpAddr> = self
            .bypass_ips
            .lock()
            .expect("bypass 路由锁中毒")
            .iter()
            .copied()
            .collect();
        v.extend(bypass_routes(&ips, &self.phys_gateway));
        v
    }

    /// 写入全部托管路由。
    ///
    /// 先 `clear_stale` 再全量写，因此幂等。
    pub fn apply(&self) -> anyhow::Result<()> {
        // TUN 网关与物理网关相同，意味着 bypass 路由指回 TUN 自己 ——
        // 环路防线在配置层面就已经失效，且事后完全看不出来（路由都写成功了）。
        // 宁可整项托管失败。
        if self.tun_gateway == self.phys_gateway {
            anyhow::bail!(
                "TUN 网关与物理网关同为 {}，bypass 路由会指回 TUN 自身，环路防线失效",
                self.tun_gateway
            );
        }
        self.clear_stale()?;
        for e in self.all_entries() {
            self.backend
                .add(&e)
                .map_err(|err| anyhow::anyhow!("写入路由 {} 失败：{err:#}", e.dest))?;
        }
        Ok(())
    }

    /// 撤销全部托管路由。幂等：`list_managed` 为空时是个空循环。
    pub fn revert(&self) -> anyhow::Result<()> {
        // 逐条删完再报错，**不中途 return**：第一条失败就放弃，会把后面
        // 本可以删掉的路由丢下不管，用户那半个 IPv4 空间就一直黑着。
        // 与 custody/sysproxy.rs 的 run_all 同一条纪律 —— 尽力恢复，绝不静默。
        let mut errs = Vec::new();
        for e in self.backend.list_managed()? {
            if let Err(err) = self.backend.delete(&e) {
                errs.push(format!("{}：{err:#}", e.dest));
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            anyhow::bail!("{} 条路由未能删除：{}", errs.len(), errs.join("；"))
        }
    }

    /// 清理上一次运行的残留。**必须在任何 `apply` 之前调用**，
    /// 且必须在所有启动路径上跑到 —— 包括本次不打算开 TUN 的早退分支。
    ///
    /// 与 `revert` 同一动作：判据是「托管标记」而非「本次会话记了什么」，
    /// 因此对上次崩溃的残留同样有效。这正是 `custody/hosts.rs` 的
    /// `clear_managed` 做法。
    pub fn clear_stale(&self) -> anyhow::Result<()> {
        self.revert()
    }
}

/// 把 `netstat -rn` 简写的目的地还原成完整 CIDR。
///
/// **必要性是实测出来的**（2026-08-25，本机）：macOS 的 `netstat -rn -f inet`
/// 会按经典写法压缩目的地列 ——
///
/// | 实际路由 | netstat 显示 |
/// |---|---|
/// | `128.0.0.0/1` | `128.0/1` |
/// | `10.0.0.0/16` | `10/16` |
/// | `224.0.0.0/4` | `224.0.0/4` |
/// | `203.0.113.7`（主机路由） | `203.0.113.7`（**不压缩**） |
///
/// 不还原就直接喂给 `route delete`，删的是另一条路由或者干脆失败 ——
/// 而这正是崩溃残留唯一的清理路径。
///
/// 还原后的字符串与我们当初 `add` 时用的**逐字相同**（`0.0.0.0/1` /
/// `128.0.0.0/1`），因此 `routes::argv` 构造出的 delete 必然对得上
/// （由 `routes` 的 `delete_mirrors_add` 保证）。
///
/// 无 `/` 且不足四段的形式（如 `1`，即 `1.0.0.0/8`）依赖有类网推断，
/// 本函数**不猜**：补零后按主机地址返回。我们自己的路由从不长这样
/// （两条 /1 一定带前缀，bypass 一定是完整 IP），所以猜错的风险
/// 只会体现在「认不出别人的路由」，而那本来就不该我们碰。
pub fn canonicalize_netstat_dest(dest: &str) -> String {
    let (net, prefix) = match dest.split_once('/') {
        Some((n, p)) => (n, Some(p)),
        None => (dest, None),
    };
    // IPv6 与 `link#7`、`default` 这类非点分形式原样返回，不做手脚。
    if net.contains(':') || net.contains('#') || !net.chars().all(|c| c.is_ascii_digit() || c == '.')
    {
        return dest.to_string();
    }
    let mut octets: Vec<&str> = net.split('.').collect();
    if octets.len() > 4 || octets.iter().any(|o| o.is_empty()) {
        return dest.to_string();
    }
    while octets.len() < 4 {
        octets.push("0");
    }
    let full = octets.join(".");
    match prefix {
        Some(p) => format!("{full}/{p}"),
        None => full,
    }
}

/// macOS 的真实后端：调用 `route`(8)。
///
/// **需要 root。** 无权限时 `route` 退出码 77 并在 stderr 打印
/// "must be root to alter routing table"（`-q` 不抑制它，已实测）。
/// 本实现把它转成一条**可操作**的错误信息，而不是让用户对着 exit code 猜。
#[cfg(target_os = "macos")]
pub struct MacRouteBackend {
    /// 本进程写过的条目。`netstat -rn` 不标注「谁写的」，因此托管判据
    /// 只能靠我们自己记 + 网关特征双保险（后者仅对默认路由有效，
    /// 见模块注释的表）。
    written: Mutex<Vec<RouteEntry>>,
    tun_gateway: String,
}

#[cfg(target_os = "macos")]
impl MacRouteBackend {
    pub fn new(tun_gateway: impl Into<String>) -> Self {
        Self {
            written: Mutex::new(Vec::new()),
            tun_gateway: tun_gateway.into(),
        }
    }

    fn run(&self, op: crate::routes::RouteOp, e: &RouteEntry) -> anyhow::Result<()> {
        let args = crate::routes::argv(op, e);
        let out = std::process::Command::new("/sbin/route")
            .args(&args)
            .output()
            .map_err(|err| anyhow::anyhow!("执行 route {args:?} 失败：{err}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if err.contains("must be root") {
                anyhow::bail!(
                    "写路由 {} 需要管理员权限。请用 sudo 启动，或在设置中关闭 TUN",
                    e.dest
                );
            }
            anyhow::bail!(
                "route {args:?} 退出码 {:?}：{}",
                out.status.code(),
                err.trim()
            );
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl RouteBackend for MacRouteBackend {
    fn add(&self, e: &RouteEntry) -> anyhow::Result<()> {
        self.run(crate::routes::RouteOp::Add, e)?;
        self.written.lock().expect("路由记录锁中毒").push(e.clone());
        Ok(())
    }

    fn delete(&self, e: &RouteEntry) -> anyhow::Result<()> {
        self.run(crate::routes::RouteOp::Delete, e)?;
        self.written
            .lock()
            .expect("路由记录锁中毒")
            .retain(|x| x != e);
        Ok(())
    }

    fn list_managed(&self) -> anyhow::Result<Vec<RouteEntry>> {
        // 本进程记账 + 「网关是 TUN 地址」的特征扫描，二者取并集。
        // 前者覆盖正常路径与 bypass（bypass 扫不出来，见模块注释），
        // 后者覆盖上次崩溃残留的默认路由 —— 也就是危害最大的那两条。
        let mut v = self.written.lock().expect("路由记录锁中毒").clone();
        for e in scan_by_gateway(&self.tun_gateway)? {
            if !v.contains(&e) {
                v.push(e);
            }
        }
        Ok(v)
    }
}

/// 扫描路由表里网关等于 `gw` 的条目 —— 上次崩溃残留的识别依据。
///
/// `netstat -rn -f inet` **不需要 root**（已实测）。
///
/// 只按网关精确匹配。`gw` 是 TUN 自己的地址（如 `198.18.0.1`），
/// 用户机器上不会有别的东西用它，因此不会误伤 —— 反过来，**绝不能**
/// 用物理网关来扫，那会把用户自己的静态路由全部认成我们的。
#[cfg(target_os = "macos")]
fn scan_by_gateway(gw: &str) -> anyhow::Result<Vec<RouteEntry>> {
    let out = std::process::Command::new("/usr/sbin/netstat")
        .args(["-rn", "-f", "inet"])
        .output()
        .map_err(|e| anyhow::anyhow!("执行 netstat -rn 失败：{e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "netstat -rn 退出码 {:?}：{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(parse_netstat_by_gateway(
        &String::from_utf8_lossy(&out.stdout),
        gw,
    ))
}

/// `netstat -rn -f inet` 输出的解析（从 IO 里摘出来才好测）。
///
/// 输出形如：
/// ```text
/// Destination        Gateway            Flags               Netif Expire
/// default            10.0.0.1           UGScg                 en0
/// 128.0/1            172.18.0.1         UGSc               utun49
/// ```
/// 表头与「Internet:」之类的段落行靠「第二列必须等于目标网关」自然滤掉。
pub fn parse_netstat_by_gateway(text: &str, gw: &str) -> Vec<RouteEntry> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let dest = f.next()?;
            let gateway = f.next()?;
            (gateway == gw).then(|| RouteEntry {
                // 简写还原，否则删的是另一条路由（见 canonicalize_netstat_dest）。
                dest: canonicalize_netstat_dest(dest),
                gateway: gateway.to_string(),
            })
        })
        .collect()
}

/// fake-ip 段当前归谁管。返回接口名（如 `utun49`），无路由则 `None`。
///
/// # 为什么需要这个
///
/// **实测事故**（2026-08-25，本机）：机器上跑着另一个 TUN 客户端
/// （v2rayN 的 sing-box/xray + mihomo 特权助手），`utun49`（`172.18.0.1`）
/// 用 `0/1 + 128.0/1` 盖住了默认路由，于是：
///
/// ```text
/// route -n get 198.18.0.207  →  gateway 172.18.0.1, interface utun49
/// ```
///
/// 发往 `8.8.8.8` 的查询根本没离开本机，被 utun49 截下用**它的** fake-ip
/// 池作答。两个 fake-ip 池共用 `198.18.0.0/15`，分配出的假 IP 会互相
/// 撞车，反查时各自认领对方的地址 —— 表现是随机的域名错连，无从排查。
///
/// 因此启动前必须问一句这个段归谁。**判据是接口名**：不是我们的 utun，
/// 该段就已经被别人接管了。
///
/// # 这里只提供事实，不做决策
///
/// 「发现冲突后是拒绝启动还是降级」属于启动顺序纪律（Task 10）的判断，
/// 本模块只负责把路由表的事实取回来 —— 与 `RouteBackend` 同一条缝：
/// 需要特权 / 需要策略的部分都不放在这里。
///
/// 注意 `route get` 报的 `destination` 是**命中的那条路由**（上例里是
/// `128.0.0.0`），不是查询地址本身，所以判断只能看 `interface`。
#[cfg(target_os = "macos")]
pub fn fakeip_range_owner(probe: std::net::Ipv4Addr) -> anyhow::Result<Option<String>> {
    let addr = probe.to_string();
    let out = std::process::Command::new("/sbin/route")
        .args(["-n", "get", &addr])
        .output()
        .map_err(|e| anyhow::anyhow!("执行 route -n get {addr} 失败：{e}"))?;
    if !out.status.success() {
        // 没有可用路由时 route 也会失败（"not in table"）—— 那恰恰说明
        // 该段没人管，是我们想要的状态，不是错误。
        return Ok(None);
    }
    Ok(crate::routes::parse_route_get(&String::from_utf8_lossy(&out.stdout)).interface)
}

/// 物理网关地址 —— 全部 bypass 路由的下一跳。
///
/// `route -n get default` 的 `gateway:` 一行即是；该命令**不需要 root**
/// （已实测，只有 add/delete/change 需要）。
///
/// 取不到就报错，不猜：网关猜错的话每一条 bypass 路由都指向不通的下一跳，
/// 出站全部连不上，而路由表看上去一切正常。
#[cfg(target_os = "macos")]
pub fn default_gateway() -> anyhow::Result<String> {
    let out = std::process::Command::new("/sbin/route")
        .args(["-n", "get", "default"])
        .output()
        .map_err(|e| anyhow::anyhow!("执行 route -n get default 失败：{e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "route -n get default 退出码 {:?}：{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let q = crate::routes::parse_route_get(&String::from_utf8_lossy(&out.stdout));
    q.gateway.ok_or_else(|| {
        anyhow::anyhow!("route -n get default 没有 gateway 行，无法确定物理网关（当前可能没有默认路由）")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一张假的路由表。**它不碰真实路由表** —— 真后端要 root，
    /// 单测里但凡真跑一条 `route add`，跑挂一次就在开发机上留下一条
    /// 黑洞路由（这与 `custody/hosts.rs` 的测试绝不指向真实 /etc/hosts
    /// 是同一条纪律）。
    #[derive(Default)]
    struct FakeBackend {
        table: Mutex<Vec<RouteEntry>>,
        adds: Mutex<usize>,
        deletes: Mutex<usize>,
    }

    impl RouteBackend for FakeBackend {
        fn add(&self, e: &RouteEntry) -> anyhow::Result<()> {
            *self.adds.lock().unwrap() += 1;
            self.table.lock().unwrap().push(e.clone());
            Ok(())
        }
        fn delete(&self, e: &RouteEntry) -> anyhow::Result<()> {
            *self.deletes.lock().unwrap() += 1;
            self.table.lock().unwrap().retain(|x| x != e);
            Ok(())
        }
        fn list_managed(&self) -> anyhow::Result<Vec<RouteEntry>> {
            Ok(self.table.lock().unwrap().clone())
        }
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn setup() -> (Arc<FakeBackend>, TunRoutes) {
        let b = Arc::new(FakeBackend::default());
        let r = TunRoutes::new(b.clone(), "198.18.0.1", "10.0.0.1");
        (b, r)
    }

    #[test]
    fn apply_writes_both_halves_of_default() {
        let (b, r) = setup();
        r.apply().unwrap();
        let t = b.table.lock().unwrap();
        assert!(t.iter().any(|e| e.dest == "0.0.0.0/1"));
        assert!(t.iter().any(|e| e.dest == "128.0.0.0/1"));
        // 绝不能出现改 default 的那条 —— 它会冲掉原网关记录。
        assert!(!t.iter().any(|e| e.dest == "default"));
    }

    #[test]
    fn apply_is_idempotent() {
        let (b, r) = setup();
        r.apply().unwrap();
        let once = b.table.lock().unwrap().clone();
        r.apply().unwrap();
        r.apply().unwrap();
        assert_eq!(
            *b.table.lock().unwrap(),
            once,
            "重复 apply 必须收敛到同一状态"
        );
    }

    #[test]
    fn revert_is_idempotent_and_leaves_nothing() {
        let (b, r) = setup();
        r.apply().unwrap();
        r.revert().unwrap();
        r.revert().unwrap();
        assert!(b.table.lock().unwrap().is_empty(), "revert 后必须一条不剩");
    }

    /// **崩溃恢复的核心测试**：上次运行写完路由就被 SIGKILL，`revert`
    /// 根本没跑到，`0.0.0.0/1` 留在系统路由表里指着一个已经不存在的 utun。
    /// 本次进程对此一无所知（`bypass_ips` 是空的、后端没有任何记账），
    /// 全靠 `clear_stale` 按**托管判据**扫出来。
    ///
    /// 不清的话：半个 IPv4 空间被黑洞，且用户看不出这跟本程序有关，
    /// 重启也不会好 —— 路由表是内核状态，跟着机器活着。
    #[test]
    fn clear_stale_removes_previous_crash_leftovers() {
        // 模拟上次崩溃：表里已经有我们的路由，本次进程对此一无所知。
        let b = Arc::new(FakeBackend::default());
        b.add(&RouteEntry {
            dest: "0.0.0.0/1".into(),
            gateway: "198.18.0.1".into(),
        })
        .unwrap();
        b.add(&RouteEntry {
            dest: "128.0.0.0/1".into(),
            gateway: "198.18.0.1".into(),
        })
        .unwrap();

        // 全新的托管者，什么都没记过 —— 正是「下一次启动」的处境。
        let r = TunRoutes::new(b.clone(), "198.18.0.1", "10.0.0.1");
        assert!(r.bypass_snapshot().is_empty(), "本次进程确实一无所知");

        r.clear_stale().unwrap();
        assert!(b.table.lock().unwrap().is_empty(), "崩溃残留必须被清掉");
    }

    /// 崩溃后**本次不打算 apply** 也要能恢复。
    ///
    /// 这是 hosts 那边补过的洞（`custody/hosts.rs::cleanup_only`）：
    /// 用户上次开着 TUN 被 SIGKILL，重启时把 TUN 关了 —— 早退分支若不先
    /// `clear_stale`，那两条 /1 就永远留在内核里。
    #[test]
    fn clear_stale_works_even_when_this_run_applies_nothing() {
        let b = Arc::new(FakeBackend::default());
        b.add(&RouteEntry {
            dest: "0.0.0.0/1".into(),
            gateway: "198.18.0.1".into(),
        })
        .unwrap();
        let r = TunRoutes::new(b.clone(), "198.18.0.1", "10.0.0.1");
        // 注意：全程没有 apply。
        r.clear_stale().unwrap();
        assert!(
            b.table.lock().unwrap().is_empty(),
            "本次不 apply 也必须清残留"
        );
    }

    #[test]
    fn sync_bypass_only_touches_the_delta() {
        // 全删全加会留出「服务器 IP 无 bypass」的窗口，在途连接当场进环路。
        let (b, r) = setup();
        r.sync_bypass(&[ip("203.0.113.7")]).unwrap();
        assert_eq!(*b.adds.lock().unwrap(), 1);

        // 再同步一次同样的名单：不该有任何动作。
        r.sync_bypass(&[ip("203.0.113.7")]).unwrap();
        assert_eq!(*b.adds.lock().unwrap(), 1, "无变化时不该重复写路由");
        assert_eq!(*b.deletes.lock().unwrap(), 0, "无变化时不该删任何路由");

        // 加一个、留一个：只该新增一条。
        r.sync_bypass(&[ip("203.0.113.7"), ip("198.51.100.9")])
            .unwrap();
        assert_eq!(*b.adds.lock().unwrap(), 2);
        assert_eq!(*b.deletes.lock().unwrap(), 0, "仍在用的 IP 不该被删");
    }

    #[test]
    fn sync_bypass_removes_departed_outbound_ip() {
        let (b, r) = setup();
        r.sync_bypass(&[ip("203.0.113.7"), ip("198.51.100.9")])
            .unwrap();
        r.sync_bypass(&[ip("203.0.113.7")]).unwrap();
        assert_eq!(*b.deletes.lock().unwrap(), 1);
        let t = b.table.lock().unwrap();
        assert!(t.iter().any(|e| e.dest == "203.0.113.7"));
        assert!(!t.iter().any(|e| e.dest == "198.51.100.9"));
    }

    #[test]
    fn sync_bypass_ignores_duplicate_ips_in_the_request() {
        // 两个出站落在同一台机器上时，上游给的名单会含重复 IP。
        // 照单全写会加两条同样的路由，删的时候只删掉一条，剩下的永远留着。
        let (b, r) = setup();
        r.sync_bypass(&[ip("203.0.113.7"), ip("203.0.113.7")])
            .unwrap();
        assert_eq!(*b.adds.lock().unwrap(), 1, "重复 IP 只该写一条路由");
        assert_eq!(r.bypass_snapshot(), vec![ip("203.0.113.7")]);
    }

    #[test]
    fn apply_includes_current_bypass_entries() {
        let (b, r) = setup();
        r.sync_bypass(&[ip("203.0.113.7")]).unwrap();
        r.apply().unwrap();
        let t = b.table.lock().unwrap();
        assert!(
            t.iter()
                .any(|e| e.dest == "203.0.113.7" && e.gateway == "10.0.0.1"),
            "apply 必须把 bypass 一并写回，否则 clear_stale 清完就没了"
        );
    }

    #[test]
    fn bypass_routes_never_go_through_the_tun() {
        // bypass 的**全部意义**就是不走 TUN。指回 TUN 网关等于没写。
        let (b, r) = setup();
        r.sync_bypass(&[ip("203.0.113.7")]).unwrap();
        r.apply().unwrap();
        let t = b.table.lock().unwrap();
        let bypass: Vec<_> = t.iter().filter(|e| e.dest == "203.0.113.7").collect();
        assert_eq!(bypass.len(), 1);
        assert_eq!(bypass[0].gateway, "10.0.0.1", "必须指向物理网关");
        assert_ne!(bypass[0].gateway, "198.18.0.1");
    }

    #[test]
    fn identical_gateways_are_rejected_instead_of_silently_looping() {
        // TUN 网关 == 物理网关时，bypass 路由指回 TUN 自己：路由全部写成功、
        // 日志一片祥和，而每个出站连接都在环路里。必须在 apply 就拦下。
        let b = Arc::new(FakeBackend::default());
        let r = TunRoutes::new(b.clone(), "10.0.0.1", "10.0.0.1");
        let e = r.apply().unwrap_err();
        assert!(e.to_string().contains("环路"), "错误应指出后果：{e}");
        assert!(b.table.lock().unwrap().is_empty(), "拦下后不该写任何路由");
    }

    #[test]
    fn backend_failure_propagates_not_swallowed() {
        // 沿用 shard.rs:214 的纪律：写路由失败必须能被上层看见。
        struct Failing;
        impl RouteBackend for Failing {
            fn add(&self, e: &RouteEntry) -> anyhow::Result<()> {
                anyhow::bail!("写路由 {} 失败：需要 root", e.dest)
            }
            fn delete(&self, _: &RouteEntry) -> anyhow::Result<()> {
                Ok(())
            }
            fn list_managed(&self) -> anyhow::Result<Vec<RouteEntry>> {
                Ok(vec![])
            }
        }
        let r = TunRoutes::new(Arc::new(Failing), "198.18.0.1", "10.0.0.1");
        let e = r.apply().unwrap_err();
        assert!(e.to_string().contains("0.0.0.0/1"), "错误应指明是哪条路由: {e}");
    }

    #[test]
    fn partial_sync_failure_keeps_bookkeeping_honest() {
        // 中途失败时，已经写进系统的那条必须留在账上 —— 否则它再也没人
        // 认领，`revert` 与 `clear_stale` 都不会去删它，路由永远残留。
        struct FailSecond {
            n: Mutex<usize>,
        }
        impl RouteBackend for FailSecond {
            fn add(&self, e: &RouteEntry) -> anyhow::Result<()> {
                let mut n = self.n.lock().unwrap();
                *n += 1;
                if *n >= 2 {
                    anyhow::bail!("第二条起一律失败：{}", e.dest);
                }
                Ok(())
            }
            fn delete(&self, _: &RouteEntry) -> anyhow::Result<()> {
                Ok(())
            }
            fn list_managed(&self) -> anyhow::Result<Vec<RouteEntry>> {
                Ok(vec![])
            }
        }
        let r = TunRoutes::new(
            Arc::new(FailSecond { n: Mutex::new(0) }),
            "198.18.0.1",
            "10.0.0.1",
        );
        // BTreeSet 有序：198.51.100.9 排在 203.0.113.7 之前，故第一条成功。
        let err = r
            .sync_bypass(&[ip("203.0.113.7"), ip("198.51.100.9")])
            .unwrap_err();
        assert!(err.to_string().contains("bypass"), "错误须点明是哪一步：{err}");
        assert_eq!(
            r.bypass_snapshot(),
            vec![ip("198.51.100.9")],
            "成功写入的那条必须记在账上，失败的那条不能记"
        );
    }

    #[test]
    fn revert_reports_every_failure_instead_of_stopping_at_the_first() {
        // 第一条删不掉就 return，会把后面本可以删掉的路由丢下不管 ——
        // 用户那半个 IPv4 空间就一直黑着。
        struct AllDeletesFail;
        impl RouteBackend for AllDeletesFail {
            fn add(&self, _: &RouteEntry) -> anyhow::Result<()> {
                Ok(())
            }
            fn delete(&self, e: &RouteEntry) -> anyhow::Result<()> {
                anyhow::bail!("删不掉 {}", e.dest)
            }
            fn list_managed(&self) -> anyhow::Result<Vec<RouteEntry>> {
                Ok(tun_default_routes("198.18.0.1"))
            }
        }
        let r = TunRoutes::new(Arc::new(AllDeletesFail), "198.18.0.1", "10.0.0.1");
        let e = r.revert().unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("2 条路由未能删除"), "两条都要试过：{msg}");
        assert!(msg.contains("0.0.0.0/1"), "{msg}");
        assert!(msg.contains("128.0.0.0/1"), "{msg}");
    }

    #[test]
    fn clear_stale_failure_is_not_swallowed_by_apply() {
        // 残留没清干净就 apply，等于在一份未知状态上叠加。宁可整项失败 ——
        // 与 custody/mod.rs 的 failed_clear_stale_aborts_before_applying 同源。
        struct StaleFails;
        impl RouteBackend for StaleFails {
            fn add(&self, _: &RouteEntry) -> anyhow::Result<()> {
                panic!("清残留失败后不该继续写路由")
            }
            fn delete(&self, _: &RouteEntry) -> anyhow::Result<()> {
                Ok(())
            }
            fn list_managed(&self) -> anyhow::Result<Vec<RouteEntry>> {
                anyhow::bail!("读路由表失败")
            }
        }
        let r = TunRoutes::new(Arc::new(StaleFails), "198.18.0.1", "10.0.0.1");
        let e = r.apply().unwrap_err();
        assert!(format!("{e:#}").contains("读路由表失败"), "根因必须保留：{e:#}");
    }

    // ---- netstat 简写还原 ----

    #[test]
    fn netstat_abbreviations_are_expanded_back_to_full_cidr() {
        // 全部取自本机 2026-08-25 的真实 `netstat -rn -f inet` 输出。
        // 不还原就删不掉自己加的路由 —— 崩溃残留的唯一清理路径。
        assert_eq!(canonicalize_netstat_dest("128.0/1"), "128.0.0.0/1");
        assert_eq!(canonicalize_netstat_dest("10/16"), "10.0.0.0/16");
        assert_eq!(canonicalize_netstat_dest("224.0.0/4"), "224.0.0.0/4");
        assert_eq!(canonicalize_netstat_dest("2/7"), "2.0.0.0/7");
        assert_eq!(canonicalize_netstat_dest("64/2"), "64.0.0.0/2");
    }

    #[test]
    fn already_full_forms_pass_through_unchanged() {
        // 主机路由不压缩，必须原样留着 —— 改一个字都删不掉。
        assert_eq!(canonicalize_netstat_dest("203.0.113.7"), "203.0.113.7");
        assert_eq!(canonicalize_netstat_dest("10.0.0.1/32"), "10.0.0.1/32");
        assert_eq!(
            canonicalize_netstat_dest("255.255.255.255/32"),
            "255.255.255.255/32"
        );
        assert_eq!(canonicalize_netstat_dest("0.0.0.0/1"), "0.0.0.0/1");
    }

    #[test]
    fn non_dotted_destinations_are_left_alone() {
        // `default`、`link#7`、IPv6 都不是我们的托管形状，别去动它们。
        for s in ["default", "link#7", "fe80::/64", "::1"] {
            assert_eq!(canonicalize_netstat_dest(s), s, "{s} 不该被改写");
        }
    }

    #[test]
    fn canonicalized_dest_round_trips_with_what_we_added() {
        // 这条把两头钉在一起：我们 add 时用的字符串，经 netstat 压缩后
        // 再还原，必须逐字回到原样。否则 clear_stale 扫出来也删不掉。
        for e in tun_default_routes("198.18.0.1") {
            let abbreviated = match e.dest.as_str() {
                "0.0.0.0/1" => "0.0.0.0/1", // netstat 对 0.0.0.0/1 不压缩首段
                "128.0.0.0/1" => "128.0/1", // 实测压缩形式
                other => other,
            };
            assert_eq!(
                canonicalize_netstat_dest(abbreviated),
                e.dest,
                "还原后必须与 add 时逐字一致"
            );
        }
    }

    /// 本机 2026-08-25 的真实 `netstat -rn -f inet` 片段（另一个 TUN 客户端
    /// utun49 正盖着默认路由）。
    const REAL_NETSTAT: &str = "\
Routing tables

Internet:
Destination        Gateway            Flags               Netif Expire
default            10.0.0.1           UGScg                 en0
1                  172.18.0.1         UGSc               utun49
2/7                172.18.0.1         UGSc               utun49
128.0/1            172.18.0.1         UGSc               utun49
10/16              link#7             UCS                   en0      !
10.0.0.1           6:25:8e:8:5a:ec    UHLWIir               en0   1199
172.18.0.1         172.18.0.1         UH                 utun49
";

    #[test]
    fn scan_only_matches_our_own_gateway() {
        // 判据是网关精确等于 TUN 地址。用别人的 TUN 地址扫，一条都不该认领 ——
        // 认领了就会去删另一个代理软件的路由，把用户的网络搞坏。
        let ours = parse_netstat_by_gateway(REAL_NETSTAT, "198.18.0.1");
        assert!(ours.is_empty(), "这张表里没有我们的路由：{ours:?}");

        // 换成 utun49 的网关，才扫得出那几条 —— 证明匹配逻辑本身是通的。
        let theirs = parse_netstat_by_gateway(REAL_NETSTAT, "172.18.0.1");
        assert!(theirs.iter().any(|e| e.dest == "128.0.0.0/1"), "{theirs:?}");
        assert!(theirs.iter().any(|e| e.dest == "2.0.0.0/7"), "{theirs:?}");
    }

    #[test]
    fn scan_never_claims_routes_pointing_at_the_physical_gateway() {
        // 这是本模块最危险的一条：bypass 路由指向物理网关，和用户自己的
        // 静态路由长得一模一样。**绝不能**拿物理网关当托管判据。
        // 这里直接验证：拿物理网关去扫，会把用户的 default 也扫出来 ——
        // 所以生产代码永远只拿 TUN 地址扫（见 MacRouteBackend::list_managed）。
        let by_phys = parse_netstat_by_gateway(REAL_NETSTAT, "10.0.0.1");
        assert!(
            by_phys.iter().any(|e| e.dest == "default"),
            "物理网关判据会误伤用户的 default 路由，故不可用：{by_phys:?}"
        );
    }

    #[test]
    fn netstat_header_lines_are_not_parsed_as_routes() {
        // 表头 "Destination Gateway ..." 第二列恰好是 "Gateway"，
        // 只有当有人拿 "Gateway" 当网关名时才会中招 —— 不会发生，
        // 但确认一下正常网关下表头被滤掉。
        let r = parse_netstat_by_gateway(REAL_NETSTAT, "172.18.0.1");
        assert!(!r.iter().any(|e| e.dest == "Destination"), "{r:?}");
        assert!(!r.iter().any(|e| e.dest == "Routing"), "{r:?}");
    }
}
