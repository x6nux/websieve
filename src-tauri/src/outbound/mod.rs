//! 多出站管理（设计文档 §9）。
//!
//! 两级生命周期，是本模块全部设计的出发点：
//!
//! | 层级 | 死亡含义 | 处置 |
//! |---|---|---|
//! | core / WebView | 页面崩了、心跳停摆 | reload 页面 → 全部出站重建 |
//! | 单个出站会话 | 握手失败、下行流断、服务端 GC | 只重建这一个，不碰 core，不 reload |
//!
//! 后者即 §9.4 优化①：省掉恢复路径上最贵的一段（导航到伪装页，1–3 RTT
//! 加整页字节）。判据是 `TransportCore::is_dead()` —— core 活着就说明
//! emitter 还在页面上下文里跑，post / openStream 都无状态，直接重新握手即可。
//!
//! **§6.4 拒绝而非静默回退**：出站不可用时 `dialer()` 返回 `None`，调用方
//! 只能拒绝这条连接。本模块**不提供**任何「换一个出站试试」的入口 ——
//! 那会把「发去日本节点」悄悄改写成「发去随便哪里」，是隐私事故而非
//! 可用性折衷。
//!
//! 本模块自身负责三件总装工作：
//!
//! - **端口分段**：每出站占 `extra_sessions + 1` 个本地转发端口，依次排开，
//!   不重叠（`plan_ports`）
//! - **优化④ 并行拉起**：全部启用的出站同时开始握手，而不是排队等前一个
//! - **优化⑤ 启用即预连**：出站被启用的那一刻就开始连，不等第一个请求

pub mod carrier;
pub mod instance;

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::bridge::TransportCore;
use instance::{OutboundInstance, SessionEnv, Status};

/// 端口分段结果：出站名 → 它独占的那段端口（按会话序）。
pub type PortPlan = BTreeMap<String, Vec<u16>>;

/// 给每个出站切一段互不重叠的本地转发端口。
///
/// `outbounds` 为 `(出站名, extra_sessions)`，顺序即分配顺序。每个出站占
/// `extra_sessions + 1` 个端口（主会话 + 额外会话），依次排开。
///
/// **溢出报错而非回绕**：`u16` 加到头悄悄绕回去，两个出站就会抢同一个端口，
/// 表现为「A 的流量偶尔跑到 B 的服务器上」—— 这是隐私事故，而且极难定位。
pub fn try_plan_ports(base: u16, outbounds: &[(&str, usize)]) -> anyhow::Result<PortPlan> {
    let mut plan = PortPlan::new();
    // 用 u32 走位再收窄：u16 上做 checked_add 时，「刚好用到 65535」与
    // 「越过 65535」很难分清 —— 早先一版为了让前者通过而放宽了判断，
    // 结果后一个出站又从 65535 开始，两段悄悄重叠。宽类型算完再校验，
    // 两种情况自然分开。
    let mut next = u32::from(base);
    for (name, extra) in outbounds {
        let count = extra
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("出站「{name}」的会话数溢出"))?;
        let mut ports = Vec::with_capacity(count);
        for _ in 0..count {
            if next > u32::from(u16::MAX) {
                anyhow::bail!(
                    "端口号溢出：从 {base} 起分不下这些出站（到「{name}」时越过 65535）"
                );
            }
            ports.push(next as u16);
            next += 1;
        }
        if plan.insert(name.to_string(), ports).is_some() {
            // 重名会让后一个覆盖前一个，两者随后共用同一段端口。
            anyhow::bail!("出站名重复：{name}");
        }
    }
    Ok(plan)
}

/// 同 `try_plan_ports`，失败即 panic。
///
/// **只给测试用**（`#[cfg(test)]`）：生产路径必须处理溢出与重名，
/// 而一个会 panic 的便捷版摆在那里，迟早有人图省事在启动路径上调它 ——
/// 那会把一条本该报错退出的配置问题变成崩溃。
#[cfg(test)]
fn plan_ports(base: u16, outbounds: &[(&str, usize)]) -> PortPlan {
    try_plan_ports(base, outbounds).expect("端口分段失败")
}

/// 出站管理器：持全部出站实例，负责拉起、停机与状态查询。
///
/// `core` 由承载 WebView 提供，**全部出站共享同一份**（shared 承载）。
/// core 对应一个 WebView，不对应一个会话。
pub struct OutboundManager {
    outbounds: BTreeMap<String, Arc<OutboundInstance>>,
    core: Arc<TransportCore>,
    env: SessionEnv,
    /// 正在跑的会话循环任务。停机时要 abort，否则循环会一直重连一个
    /// 已经被禁用的出站。
    tasks: Mutex<BTreeMap<String, tokio::task::JoinHandle<()>>>,
}

impl OutboundManager {
    pub fn new(
        outbounds: BTreeMap<String, Arc<OutboundInstance>>,
        core: Arc<TransportCore>,
        env: SessionEnv,
    ) -> Arc<Self> {
        Arc::new(Self {
            outbounds,
            core,
            env,
            tasks: Mutex::new(BTreeMap::new()),
        })
    }

    /// 按配置建实例并组装。`cfgs` 已经带好各自的 `session_bases`。
    ///
    /// 生产路径走 `new`：实例表要与 `Router` 共用**同一批** `Arc`
    /// （见 `main.rs::run_stack` 的注释），因此实例在更外层建好再传进来。
    /// 这个便捷构造只在测试里用。
    #[cfg(test)]
    fn from_cfgs(
        cfgs: Vec<instance::OutboundCfg>,
        core: Arc<TransportCore>,
        env: SessionEnv,
    ) -> Arc<Self> {
        let mut m = BTreeMap::new();
        for c in cfgs {
            m.insert(c.name.clone(), OutboundInstance::new(c));
        }
        Self::new(m, core, env)
    }

    /// 取一个出站实例。
    ///
    /// `allow(dead_code)` 逐项标注而非整模块开：这几个是**阶段 4 的 IPC
    /// 命令面**（设计文档 §11.2 的 `outbound_list` / `outbound_toggle`），
    /// 命令本身还没写，因此暂时只有测试在调。逐项标的好处是范围明确 ——
    /// 整模块 allow 会连真正的死代码一起盖住，而这几项是有主的。
    #[allow(dead_code)]
    pub fn get(&self, name: &str) -> Option<&Arc<OutboundInstance>> {
        self.outbounds.get(name)
    }

    /// 某出站的当前状态。未知出站返回 `None` —— 不要伪造一个 `Stopped`，
    /// 「不存在」与「存在但停着」对 UI 是两件事。
    #[allow(dead_code)]
    pub fn status(&self, name: &str) -> Option<Status> {
        self.outbounds.get(name).map(|o| o.status())
    }

    /// 全部出站的状态快照（推给 UI）。
    ///
    /// 这是阶段 4 `outbound_list` 命令的数据源（设计文档 §11.2）。
    pub fn statuses(&self) -> BTreeMap<String, Status> {
        self.outbounds
            .iter()
            .map(|(n, o)| (n.clone(), o.status()))
            .collect()
    }

    /// **优化④**：并行拉起全部出站。
    ///
    /// 串行拉起时总时长是各出站握手耗时之和 —— 四个跨国节点各 300ms 就是
    /// 1.2s 的启动黑屏。它们之间没有任何依赖（各自独立握手，只共享一个
    /// 无状态的 core），排队纯属浪费。
    pub async fn start_all(self: &Arc<Self>) {
        let names: Vec<String> = self.outbounds.keys().cloned().collect();
        // 逐个 spawn 就是并行：每个会话循环各占一个任务，start_all 本身
        // 不等它们连上（等的话就又串回去了）。
        for name in names {
            self.start_one(&name).await;
        }
    }

    /// **优化⑤**：启用即预连。
    ///
    /// 不等第一个请求到来才开始握手 —— 那样用户点开浏览器的第一个页面必然
    /// 要额外等一整个握手往返。启用这个动作本身就是「我要用它」的信号。
    /// （阶段 4 的 `outbound_toggle` 命令会调它，见 `get` 上的说明。）
    #[allow(dead_code)]
    pub async fn set_enabled(self: &Arc<Self>, name: &str, on: bool) -> anyhow::Result<()> {
        if !self.outbounds.contains_key(name) {
            // 不存在就报错，绝不当作「启用了个空出站」放过去：调用方
            // （UI / 配置热更新）拼错名字时必须当场知道。
            anyhow::bail!("出站「{name}」不存在");
        }
        if on {
            self.start_one(name).await;
        } else {
            self.stop_one(name).await;
        }
        Ok(())
    }

    /// 起一个出站的会话循环。已经在跑就什么都不做（幂等）。
    async fn start_one(self: &Arc<Self>, name: &str) {
        let Some(inst) = self.outbounds.get(name) else {
            return;
        };
        let mut tasks = self.tasks.lock().await;
        if let Some(h) = tasks.get(name) {
            if !h.is_finished() {
                return; // 已在跑
            }
        }
        let inst = inst.clone();
        let core = self.core.clone();
        let env = self.env.clone();
        let handle = tokio::spawn(async move {
            inst.run(core, env).await;
        });
        tasks.insert(name.to_string(), handle);
    }

    /// 停一个出站。
    ///
    /// 先 `request_stop()` 让循环在下一个可中断点自己退出（它会摘掉 dialer
    /// 并把状态推成 `Stopped`），再 abort 兜底 —— 光 abort 的话，循环可能
    /// 停在「dialer 还挂着」的那一瞬，于是一个已被禁用的出站还能被拨号。
    async fn stop_one(&self, name: &str) {
        let Some(inst) = self.outbounds.get(name) else {
            return;
        };
        inst.request_stop();
        let handle = self.tasks.lock().await.remove(name);
        if let Some(h) = handle {
            // 给循环一点时间自己收尾；超时才硬砍。
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while !h.is_finished() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await;
            h.abort();
        }
    }

    /// 全部停机（应用退出）。
    pub async fn stop_all(&self) {
        let names: Vec<String> = self.outbounds.keys().cloned().collect();
        for n in names {
            self.stop_one(&n).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use instance::OutboundCfg;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use wsieve_proto::hello::MuxId;

    fn cfg(name: &str, sessions: usize) -> OutboundCfg {
        OutboundCfg {
            name: name.into(),
            server_pub: [1u8; 32],
            client_priv: [2u8; 32],
            mux_prefs: vec![MuxId::Yamux],
            session_bases: vec![None; sessions],
        }
    }

    /// 一个不碰 Tauri 的 env：eval 记次数，状态回调记录序列。
    fn test_env(evals: Arc<AtomicUsize>) -> SessionEnv {
        SessionEnv {
            eval: Arc::new(move |_, _| {
                evals.fetch_add(1, Ordering::SeqCst);
            }),
            on_status: Arc::new(|_, _| {}),
        }
    }

    fn test_manager(names: &[&str]) -> Arc<OutboundManager> {
        let cfgs = names.iter().map(|n| cfg(n, 1)).collect();
        OutboundManager::from_cfgs(
            cfgs,
            Arc::new(TransportCore::new()),
            test_env(Arc::new(AtomicUsize::new(0))),
        )
    }

    #[test]
    fn ports_are_segmented_without_overlap() {
        // 每出站占 extra_sessions + 1 个端口，依次排开
        let seg = plan_ports(18443, &[("A", 3), ("B", 1), ("C", 0)]);
        assert_eq!(seg["A"], vec![18443, 18444, 18445, 18446]);
        assert_eq!(seg["B"], vec![18447, 18448]);
        assert_eq!(seg["C"], vec![18449]);
    }

    #[test]
    fn no_two_outbounds_ever_share_a_port() {
        // 上一条断言的是具体数值，这条断言的是**性质**：任意两段不相交。
        // 数值断言在有人改了分配算法时会红，但红了之后很容易被「改期望值」
        // 糊弄过去；性质断言糊弄不了。
        let seg = plan_ports(20000, &[("A", 3), ("B", 0), ("C", 7), ("D", 1)]);
        let mut all: Vec<u16> = seg.values().flatten().copied().collect();
        let total = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), total, "端口出现重复：{seg:?}");
        assert_eq!(total, 4 + 1 + 8 + 2);
    }

    #[test]
    fn port_overflow_is_reported_not_wrapped() {
        // u16 溢出必须报错。悄悄回绕会让两个出站抢同一个端口。
        assert!(try_plan_ports(65530, &[("A", 10)]).is_err());
        let e = try_plan_ports(65530, &[("A", 10)]).unwrap_err().to_string();
        assert!(e.contains("A"), "错误要点名是哪个出站分不下：{e}");
    }

    #[test]
    fn a_segment_ending_exactly_at_the_last_port_is_fine() {
        // 边界：正好用到 65535 不是溢出。若把它也判为错，一份合法配置
        // 会莫名其妙拒绝启动。
        let seg = try_plan_ports(65534, &[("A", 1)]).unwrap();
        assert_eq!(seg["A"], vec![65534, 65535]);
        // 但再多一个就真的分不下了
        assert!(try_plan_ports(65534, &[("A", 1), ("B", 0)]).is_err());
    }

    #[test]
    fn duplicate_outbound_names_are_rejected_in_port_planning() {
        // 重名的话后一个会覆盖前一个，两者随后共用同一段端口 ——
        // 「A 的流量偶尔跑到 B 的服务器上」，最难查的那类事故。
        assert!(try_plan_ports(18443, &[("A", 1), ("A", 1)]).is_err());
    }

    #[tokio::test]
    async fn all_enabled_outbounds_start_in_parallel() {
        // 优化④：并行拉起，不是一个接一个。
        //
        // 用 core 已死来让每个会话循环立刻结束：这样测的是**拉起**这个动作
        // 本身有没有排队，而不用真的去握手（那需要一个服务端）。串行的话，
        // start_all 会在每个出站上依次 await 到它退出。
        let core = Arc::new(TransportCore::new());
        core.mark_dead("测试").await;
        let cfgs = vec![cfg("A", 1), cfg("B", 1), cfg("C", 1)];
        let m = OutboundManager::from_cfgs(cfgs, core, test_env(Arc::new(AtomicUsize::new(0))));

        let t0 = std::time::Instant::now();
        m.start_all().await;
        assert!(
            t0.elapsed() < Duration::from_millis(250),
            "start_all 不该等任何一个出站连上，实际用了 {:?}",
            t0.elapsed()
        );
        // 三个都真的被起了起来（各自一个任务）
        assert_eq!(m.tasks.lock().await.len(), 3);
    }

    #[tokio::test]
    async fn enabling_an_outbound_starts_connecting_immediately() {
        // 优化⑤：启用即预连，不等第一个请求。
        let m = test_manager(&["A"]);
        m.set_enabled("A", true).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            matches!(m.status("A"), Some(Status::Connecting) | Some(Status::Connected { .. })),
            "启用后应立刻开始连接，实际：{:?}",
            m.status("A")
        );
    }

    #[tokio::test]
    async fn enabling_an_unknown_outbound_is_an_error() {
        // 拼错名字时必须当场知道，而不是「启用成功」但什么都没发生。
        let m = test_manager(&["A"]);
        let e = m.set_enabled("幽灵", true).await.unwrap_err().to_string();
        assert!(e.contains("幽灵"), "{e}");
    }

    #[tokio::test]
    async fn disabling_an_outbound_makes_it_undialable() {
        // §6.4 的另一半：被禁用的出站必须拿不到 dialer，否则「关掉的节点」
        // 还在替人发流量。
        let m = test_manager(&["A"]);
        m.set_enabled("A", true).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        m.set_enabled("A", false).await.unwrap();
        assert!(
            m.get("A").unwrap().dialer().is_none(),
            "禁用后绝不能还能拨号"
        );
        assert!(m.tasks.lock().await.get("A").is_none(), "任务表要清干净");
    }

    #[tokio::test]
    async fn starting_twice_does_not_spawn_two_loops() {
        // 幂等：UI 连点两下「启用」不该起两个循环去抢同一个出站 ——
        // 两个循环会互相覆盖 dialer，表现为连接时通时断。
        let m = test_manager(&["A"]);
        m.set_enabled("A", true).await.unwrap();
        m.set_enabled("A", true).await.unwrap();
        assert_eq!(m.tasks.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn one_outbound_failing_does_not_kill_its_neighbour() {
        // shared 承载的核心风险：出站 A 挂掉会不会连累出站 B。
        // 会话循环绝不 mark_dead、绝不 reload，因此 B 的 core 毫发无伤。
        let core = Arc::new(TransportCore::new());
        let m = OutboundManager::from_cfgs(
            vec![cfg("A", 1), cfg("B", 1)],
            core.clone(),
            test_env(Arc::new(AtomicUsize::new(0))),
        );
        m.start_all().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        // A 停机
        m.set_enabled("A", false).await.unwrap();
        assert!(!core.is_dead(), "一个出站停机绝不能标死共享的 core");
        // B 的循环还在跑
        assert!(
            m.tasks.lock().await.contains_key("B"),
            "B 不该被 A 的停机牵连"
        );
        assert_eq!(m.status("A"), Some(Status::Stopped));
    }

    #[tokio::test]
    async fn stop_all_leaves_nothing_dialable() {
        // 退出路径：全停之后，任何出站都不该还能拨号。
        let m = test_manager(&["A", "B", "C"]);
        m.start_all().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        m.stop_all().await;
        for n in ["A", "B", "C"] {
            assert!(m.get(n).unwrap().dialer().is_none(), "{n} 停机后仍可拨号");
        }
        assert!(m.tasks.lock().await.is_empty());
    }

    #[test]
    fn unknown_outbound_status_is_none_not_a_fake_stopped() {
        // 「不存在」与「存在但停着」对 UI 是两件事：前者要提示配置有误，
        // 后者只是提示可以启用。伪造一个 Stopped 会把前者藏起来。
        let m = test_manager(&["A"]);
        assert_eq!(m.status("幽灵"), None);
        assert_eq!(m.status("A"), Some(Status::Stopped));
    }
}
