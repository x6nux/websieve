//! 单个出站的会话循环。
//!
//! 由 `proxy.rs` 的 `run()` 迁入并参数化。与原版的四处差异：
//!
//! 1. **不再无条件 reload 页面**（设计文档 §9.4 优化①）。会话死亡分两种：
//!    core 死了（页面/WebView 问题）才需要 reload，而 reload 影响的是**全部**
//!    出站，因此本循环只负责退出、把处置权交回管理器；仅本出站会话死则复用
//!    现有页面直接重新握手 —— 省掉恢复路径上最贵的一段（导航到伪装页）。
//! 2. **绝不 `core.mark_dead()`**。原版在拆解时无条件标死 core，那在共享承载
//!    下等于「出站 A 断线顺手掐死出站 B」。core 的生死归管理器。
//! 3. transport 的 base URL 来自本出站配置（`session_bases`），而非全局。
//!    非宿主出站连主会话都要用绝对 URL。
//! 4. 状态变化推给 UI（`Status`），不再只写日志。
//!
//! **§6.4 拒绝而非静默回退**：`dialer()` 在未连通时返回 `None`。调用方拿不到
//! 拨号器就只能拒绝这条连接 —— 数据结构层面堵死「换个出站发出去」的可能。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Notify;
use wsieve_mux::stripe_runtime::{StripeCfg, StripeDialer};
use wsieve_mux::{mux_factory, Mux, MuxStream};
use wsieve_proto::hello::MuxId;
use wsieve_xhttp::client::{random_group_id, UpstreamCfg, XhttpConn};

use crate::bridge::{TransportCore, WebViewTransport};

/// 会话「站稳」的门槛：活过这么久才认为本次连接是成功的，退避才归零。
///
/// 只按「握手成功」归零会被抖动打穿：服务端若在握手后立刻掐断，每一轮都算
/// 成功、退避永远停在 100ms，循环就退化成对着一台半死的服务器每秒十次重连
/// —— 自制 DoS。握手成功只证明服务端还应答，不证明这条会话可用。
pub const STABLE_SESSION: Duration = Duration::from_secs(10);

/// 本次会话是否稳到可以把退避归零。
pub fn should_reset_backoff(alive: Duration) -> bool {
    alive >= STABLE_SESSION
}

/// 出站对 UI 可见的状态（设计文档 §9.3「状态推 UI」）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Stopped,
    Connecting,
    Connected {
        sessions: usize,
    },
    Retrying {
        after: Duration,
    },
    Failed {
        reason: String,
    },
}

/// 可共享、可观察的状态格。
#[derive(Debug, Default)]
pub struct OutboundState(Mutex<Status>);

impl OutboundState {
    pub fn get(&self) -> Status {
        self.0.lock().unwrap().clone()
    }

    pub fn set(&self, s: Status) {
        *self.0.lock().unwrap() = s;
    }
}

/// 指数退避：100ms 起，翻倍，30s 封顶，成功即重置。
/// 与原 `proxy.rs` 的行为一致，只是抽成可测的类型。
#[derive(Debug)]
pub struct Backoff {
    cur: Duration,
}

impl Backoff {
    pub fn new() -> Self {
        Self {
            cur: Duration::from_millis(100),
        }
    }

    /// 取本次该等多久，并把下一次翻倍（封顶 30s）。
    pub fn next(&mut self) -> Duration {
        let d = self.cur;
        self.cur = (self.cur * 2).min(Duration::from_secs(30));
        d
    }

    pub fn reset(&mut self) {
        self.cur = Duration::from_millis(100);
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

/// 单个出站的连接参数。由 `wsieve-config` 的 `Proxy` 加承载计划共同产出。
#[derive(Debug, Clone, PartialEq)]
pub struct OutboundCfg {
    /// 出站名（配置里的 `name`，也是规则里引用的标识）。
    pub name: String,
    pub server_pub: [u8; 32],
    pub client_priv: [u8; 32],
    pub mux_prefs: Vec<MuxId>,
    /// 本出站各会话的请求基址，长度即会话数（至少 1）。
    ///
    /// `None` = 相对路径（与承载页面同源；只有宿主出站或 `isolated` 模式
    /// 才拿得到）；`Some(origin)` = 绝对 URL，跨域名靠服务端 CORS 放宽成立
    /// （见设计文档 §9.1 与 `docs/superpowers/spikes/2026-08-25-*`）。
    pub session_bases: Vec<Option<String>>,
}

/// 把 JS 送进承载 WebView 的闭包。
pub type EvalFn = Arc<dyn Fn(String) + Send + Sync>;
/// 状态变化回调：(出站名, 新状态)。
pub type StatusFn = Arc<dyn Fn(&str, &Status) + Send + Sync>;

/// 会话循环与外界（Tauri）的全部接触面。抽成闭包，循环本身即可脱离 Tauri
/// 单测 —— 这是「WebView 承载方式对出站层透明」（§4.2 纪律③）的落点之一。
#[derive(Clone)]
pub struct SessionEnv {
    pub eval: EvalFn,
    pub on_status: StatusFn,
}

impl SessionEnv {
    fn eval_box(&self) -> Box<dyn Fn(String) + Send + Sync> {
        let f = self.eval.clone();
        Box::new(move |js| f(js))
    }
}

/// 一个出站实例：持配置、持拨号器格、持状态，跑自己的会话循环。
pub struct OutboundInstance {
    cfg: OutboundCfg,
    /// 未连通时为 `None` —— §6.4 的「拒绝而非回退」在数据结构上的保证。
    dialer: RwLock<Option<Arc<StripeDialer>>>,
    state: OutboundState,
    /// 请求立刻重来一轮（UI 的「重连」按钮 / 配置变更）。
    restart: Notify,
    /// 「本出站刚连上了」的广播。分派层在出站处于 `Connecting` 时短暂排队
    /// 等它，免得应用刚启动那几秒里所有请求都被拒。
    ///
    /// 与 `restart` 分成两个 `Notify` 而不是复用一个：一个是「外面要我重来」，
    /// 一个是「我连上了」，方向相反。混用会让等待方被重连请求误唤醒，
    /// 于是它以为出站已就绪，实际拿到的仍是 `None`。
    connected: Notify,
    /// 「停下」的广播。与 `restart` 分开：`notify_one` 只叫醒一个等待者，
    /// 而停机要叫醒全部（退避中的循环、握手中的循环、排队的分派请求）。
    stop: Notify,
    /// 请求彻底停下（出站被禁用 / 应用退出）。
    stopping: AtomicBool,
}

impl OutboundInstance {
    pub fn new(cfg: OutboundCfg) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            dialer: RwLock::new(None),
            state: OutboundState::default(),
            restart: Notify::new(),
            connected: Notify::new(),
            stop: Notify::new(),
            stopping: AtomicBool::new(false),
        })
    }

    pub fn name(&self) -> &str {
        &self.cfg.name
    }

    /// 本出站的连接参数（管理器编排承载计划时要读）。
    ///
    /// 逐项 `allow(dead_code)`：当前的启动路径在建实例**之前**就已经算好了
    /// 承载计划，因此还没有人回头读它；阶段 4 的配置热更新要拿它比对
    /// 「参数变没变、要不要重连」。逐项标而非整模块开 —— 后者会连真正的
    /// 死代码一起盖住。
    #[allow(dead_code)]
    pub fn cfg(&self) -> &OutboundCfg {
        &self.cfg
    }

    /// 当前可用的拨号器。`None` ⇒ 本出站不可用 ⇒ 调用方**必须拒绝**这条连接。
    /// 不存在「返回别人的拨号器」这一分支（§6.4）。
    pub fn dialer(&self) -> Option<Arc<StripeDialer>> {
        self.dialer.read().unwrap().clone()
    }

    pub fn status(&self) -> Status {
        self.state.get()
    }

    /// 请求本出站立刻重来一轮（不影响其他出站，也不碰承载页面）。
    /// UI 的「重连」按钮与配置热更新会调它——那两处都是阶段 4 的内容，
    /// 因此暂时无人调用（见 `cfg` 上关于 allow 的说明）。
    #[allow(dead_code)]
    pub fn request_restart(&self) {
        self.restart.notify_one();
    }

    /// 请求彻底停下（出站被禁用 / 应用退出）。
    ///
    /// 三个唤醒点都要通知到：退避睡眠、已连接的等待、**以及正在进行的握手**。
    /// 少了最后一个的话，被禁用的出站会一直卡在 `Connecting` 直到握手自己
    /// 超时 —— UI 上显示「正在连接」，实际是个已经关掉的节点。
    pub fn request_stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        self.restart.notify_one();
        self.stop.notify_waiters();
    }

    /// 等到被叫停。已经在停的状态下立即返回。
    async fn wait_stop(&self) {
        loop {
            if self.stopping.load(Ordering::Relaxed) {
                return;
            }
            self.stop.notified().await;
        }
    }

    /// 等到本出站连上（或至少走完一轮握手尝试）。
    ///
    /// **已经连上时立即返回** —— 否则「先查状态、再进来等」这个常见序列会
    /// 在两步之间错过唤醒，一路等到调用方的超时。
    ///
    /// 注意它只是唤醒时机，**不是可用性判据**：醒来后调用方仍必须看
    /// `dialer()`。状态好看不等于能用（§6.4）。
    pub async fn wait_connected(&self) {
        if self.dialer().is_some() {
            return;
        }
        self.connected.notified().await;
    }

    fn set_status(&self, env: &SessionEnv, s: Status) {
        (env.on_status)(&self.cfg.name, &s);
        self.state.set(s);
    }

    /// 直接摆一个状态，**仅测试可用**。
    ///
    /// 会话循环需要真的握手才会走到 `Connecting`/`Connected`，而分派层的
    /// 「排队等待」分支恰恰要在这些状态下验证。用 `#[cfg(test)]` 而不是
    /// 公开 API：让生产代码在编译期就够不着它，状态的唯一写入者仍是循环本身。
    #[cfg(test)]
    pub fn force_status_for_test(&self, s: Status) {
        self.state.set(s);
    }

    /// 发一次「已连上」广播，**仅测试可用**。
    #[cfg(test)]
    pub fn notify_connected_for_test(&self) {
        self.connected.notify_waiters();
    }

    /// 会话循环。`core` 由管理器提供，**多个出站共享同一份**（shared 承载）。
    ///
    /// 退出条件只有两个：core 已死（页面问题，交回管理器 reload），或被
    /// `request_stop()` 叫停。除此之外一直重试。
    pub async fn run(self: &Arc<Self>, core: Arc<TransportCore>, env: SessionEnv) {
        let mut backoff = Backoff::new();
        loop {
            if self.stopping.load(Ordering::Relaxed) {
                self.set_status(&env, Status::Stopped);
                return;
            }
            // core 死 = 承载页面失效。reload 会波及全部出站，只能由管理器
            // 统一决策，本循环退出即可（§9.4 优化①的另一半）。
            if core.is_dead() {
                self.set_status(
                    &env,
                    Status::Failed {
                        reason: "承载页面已失效".into(),
                    },
                );
                return;
            }

            self.set_status(&env, Status::Connecting);
            let started = tokio::time::Instant::now();
            // 握手可能挂很久（等心跳、等服务端应答）。停机请求必须能当场
            // 打断它，否则一个被禁用的出站会一路显示「正在连接」直到超时
            // —— 用户看着 UI 以为它还在努力，实际早就该停了。
            let attempt = tokio::select! {
                r = self.handshake(&core, &env) => r,
                _ = self.wait_stop() => {
                    self.set_status(&env, Status::Stopped);
                    return;
                }
            };
            match attempt {
                Ok((dialer, liveness)) => {
                    let sessions = dialer.session_count();
                    *self.dialer.write().unwrap() = Some(dialer);
                    self.set_status(&env, Status::Connected { sessions });
                    // 唤醒在 `wait_connected()` 上排队的分派请求。必须在装好
                    // dialer **之后**发，否则被唤醒的一方查 `dialer()` 仍是
                    // None，白等一场还得到「不可用」。
                    self.connected.notify_waiters();

                    // 等本出站的会话全部死掉（或被叫停/要求重连）。
                    // 注意不等 core —— core 的死亡由管理器感知并广播。
                    tokio::select! {
                        _ = liveness.all_dead() => {}
                        _ = self.restart.notified() => {
                            tracing::info!("出站 {} 收到重连请求", self.cfg.name);
                        }
                    }

                    // 先摘拨号器：从这一刻起新连接一律被拒（§6.4），
                    // 而不是打到一条已经死掉的会话上白等超时。
                    *self.dialer.write().unwrap() = None;
                    if should_reset_backoff(started.elapsed()) {
                        backoff.reset();
                    } else {
                        tracing::warn!(
                            "出站 {} 的会话只活了 {:?}（<{:?}），退避不归零",
                            self.cfg.name,
                            started.elapsed(),
                            STABLE_SESSION
                        );
                    }
                }
                Err(e) => {
                    // 握手失败绝不静默：既写日志也推 UI（§12 错误处理）。
                    tracing::warn!("出站 {} 建会话失败: {e:#}", self.cfg.name);
                    self.set_status(
                        &env,
                        Status::Failed {
                            reason: format!("{e:#}"),
                        },
                    );
                    // 也要唤醒排队的分派请求：这一轮已有定论（失败），
                    // 让它们空等到超时只是把一个已知的坏消息拖慢报出来。
                    // 醒来后它们查 `dialer()` 仍是 None，照常被拒。
                    self.connected.notify_waiters();
                }
            }

            if self.stopping.load(Ordering::Relaxed) {
                self.set_status(&env, Status::Stopped);
                return;
            }
            // 拆解：**不碰 core，不 reload 页面**。emitter 有防重注入且
            // post / openStream 均无状态，页面原地就能承载下一代会话。
            if core.is_dead() {
                self.set_status(
                    &env,
                    Status::Failed {
                        reason: "承载页面已失效".into(),
                    },
                );
                return;
            }

            let wait = backoff.next();
            self.set_status(&env, Status::Retrying { after: wait });
            // 退避期间仍要能被叫停/催重连，否则「点重连」得等最多 30s。
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.restart.notified() => {}
            }
        }
    }

    /// 建一代会话：主会话 + `session_bases.len()-1` 个额外会话（多 TCP 条带）。
    ///
    /// 全部会话共享同一个 `TransportCore`（request_id 由它统一分配），但各自
    /// 一个 `WebViewTransport`（各自的 base ⇒ 各自的 origin ⇒ 各自的 TCP）。
    async fn handshake(
        &self,
        core: &Arc<TransportCore>,
        env: &SessionEnv,
    ) -> anyhow::Result<(Arc<StripeDialer>, Arc<SessionLiveness>)> {
        if self.cfg.session_bases.is_empty() {
            anyhow::bail!("出站 {} 没有任何会话基址", self.cfg.name);
        }
        // 会话组 id：本代的全部会话共用一个，服务端据此把它们归为一组并跨
        // 会话铺下行 lane。每代重新生成 —— 上一代已拆除，复用旧 id 只会让
        // 服务端组表里混进死会话。
        let group_id = random_group_id();
        let liveness = SessionLiveness::new();

        let primary_base = self.cfg.session_bases[0].clone().unwrap_or_default();
        let t0 = WebViewTransport::with_base(env.eval_box(), primary_base);
        t0.set_core(core.clone());
        let (conn, neg) = XhttpConn::connect(
            t0,
            &UpstreamCfg {
                server_pub: self.cfg.server_pub,
                client_priv: self.cfg.client_priv,
                mux_prefs: self.cfg.mux_prefs.clone(),
                group_id,
            },
        )
        .await?;
        if neg.fallback {
            tracing::warn!(
                "出站 {} 的 mux 被服务端回退（未采纳偏好）",
                self.cfg.name
            );
        }
        let io: MuxStream = Box::new(liveness.watch(conn));
        let mux: Arc<dyn Mux> = Arc::from(mux_factory(neg.mux_id, io).await?);
        let dialer = StripeDialer::new(mux, StripeCfg::with_env());

        // 额外会话：每个一个独立 origin（同域名不同端口）—— 共用一个
        // transport 就等于共用一个 origin，h2 会把它们复用回同一条 TCP，
        // 多会话就白做了。
        //
        // 额外会话失败不算整代失败：主会话已经通了，条带宽度小一点也比整个
        // 出站不可用强。但**绝不静默** —— 每一条都要留下告警。
        for base in self.cfg.session_bases.iter().skip(1) {
            let base = base.clone().unwrap_or_default();
            match self
                .extra_session(core, env, &base, group_id, neg.mux_id, &liveness)
                .await
            {
                Ok(mux2) => {
                    dialer.attach_session(mux2);
                }
                Err(e) => {
                    tracing::warn!(
                        "出站 {} 的额外会话（base={}）建立失败，本代条带变窄: {e:#}",
                        self.cfg.name,
                        if base.is_empty() { "<同源>" } else { &base }
                    );
                }
            }
        }
        if dialer.session_count() > 1 {
            tracing::info!(
                "出站 {}：{} 条会话条带就绪",
                self.cfg.name,
                dialer.session_count()
            );
        }
        Ok((dialer, liveness))
    }

    async fn extra_session(
        &self,
        core: &Arc<TransportCore>,
        env: &SessionEnv,
        base: &str,
        group_id: u128,
        mux_id: MuxId,
        liveness: &Arc<SessionLiveness>,
    ) -> anyhow::Result<Arc<dyn Mux>> {
        let t = WebViewTransport::with_base(env.eval_box(), base.to_string());
        t.set_core(core.clone());
        let (conn, neg) = XhttpConn::connect(
            t,
            &UpstreamCfg {
                server_pub: self.cfg.server_pub,
                client_priv: self.cfg.client_priv,
                // 额外会话必须与主会话同一种 mux，不再重新协商。
                mux_prefs: vec![mux_id],
                group_id,
            },
        )
        .await?;
        let io: MuxStream = Box::new(liveness.watch(conn));
        Ok(Arc::from(mux_factory(neg.mux_id, io).await?))
    }
}

// ---------------- 会话存活观测 ----------------

/// 一代会话的存活计数。
///
/// 「本出站的会话死了」需要一个**属于本出站**的信号：core 的心跳看板是
/// 全局的（共享承载下所有出站共用），拿它当死亡判据就等于把别人的故障算到
/// 自己头上，反过来也一样。这里在每条 `XhttpConn` 外面套一层观测，读/写
/// 报错或读到 EOF 即判该会话死亡；**全部会话都死**才算本出站的这一代结束
/// —— 与 §9.1「任一会话死其上 lane 断，只要还有会话活着 conn 继续」一致。
pub struct SessionLiveness {
    live: AtomicUsize,
    all_dead: Notify,
}

impl SessionLiveness {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            live: AtomicUsize::new(0),
            all_dead: Notify::new(),
        })
    }

    /// 当前还活着的会话数。
    ///
    /// 只有测试在读：生产路径关心的是「**全部**会话是否都死了」
    /// （`all_dead()`），而不是还剩几条。留着是因为测试要能区分
    /// 「死了一条」与「全死了」—— 那正是本类型存在的理由。
    #[cfg(test)]
    pub fn live_count(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    /// 把一条会话流纳入观测。
    pub fn watch<T>(self: &Arc<Self>, inner: T) -> DeathWatch<T> {
        self.live.fetch_add(1, Ordering::Relaxed);
        DeathWatch {
            inner,
            liveness: self.clone(),
            dead: false,
        }
    }

    /// 等到本代全部会话死亡。一条都没登记过时立即返回（无会话＝已死）。
    pub async fn all_dead(&self) {
        loop {
            if self.live.load(Ordering::Relaxed) == 0 {
                return;
            }
            self.all_dead.notified().await;
        }
    }

    fn mark_one_dead(&self) {
        // fetch_sub 返回旧值；旧值为 1 说明这是最后一条。
        if self.live.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.all_dead.notify_waiters();
        }
    }
}

/// 观测一条会话流的生死：读/写出错或读到 EOF 即判死，并且只判一次。
/// 字节本身原样透传，不做任何缓冲或改写。
pub struct DeathWatch<T> {
    inner: T,
    liveness: Arc<SessionLiveness>,
    dead: bool,
}

impl<T> DeathWatch<T> {
    fn die(&mut self) {
        if !self.dead {
            self.dead = true;
            self.liveness.mark_one_dead();
        }
    }
}

impl<T> Drop for DeathWatch<T> {
    fn drop(&mut self) {
        // mux 把流丢弃了也是死亡（例如 mux 自身出错退出）。
        self.die();
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for DeathWatch<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        match &r {
            Poll::Ready(Err(_)) => self.die(),
            // 没读进任何字节的 Ready(Ok) 就是 EOF。
            Poll::Ready(Ok(())) if buf.filled().len() == before => self.die(),
            _ => {}
        }
        r
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for DeathWatch<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let r = std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
        if matches!(r, Poll::Ready(Err(_))) {
            self.die();
        }
        r
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let r = std::pin::Pin::new(&mut self.inner).poll_flush(cx);
        if matches!(r, Poll::Ready(Err(_))) {
            self.die();
        }
        r
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let r = std::pin::Pin::new(&mut self.inner).poll_shutdown(cx);
        if matches!(r, Poll::Ready(_)) {
            self.die();
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg(name: &str) -> OutboundCfg {
        OutboundCfg {
            name: name.into(),
            server_pub: [1u8; 32],
            client_priv: [2u8; 32],
            mux_prefs: vec![MuxId::Yamux],
            session_bases: vec![None],
        }
    }

    #[test]
    fn backoff_doubles_and_caps_at_30s() {
        let mut b = Backoff::new();
        assert_eq!(b.next(), Duration::from_millis(100));
        assert_eq!(b.next(), Duration::from_millis(200));
        assert_eq!(b.next(), Duration::from_millis(400));
        for _ in 0..20 {
            b.next();
        }
        assert_eq!(b.next(), Duration::from_secs(30), "必须封顶");
    }

    #[test]
    fn success_resets_backoff() {
        let mut b = Backoff::new();
        b.next();
        b.next();
        b.reset();
        assert_eq!(b.next(), Duration::from_millis(100));
    }

    #[test]
    fn flapping_session_does_not_reset_backoff() {
        // 握手成功但秒断：若照样归零，退避就永远停在 100ms，
        // 对着一台半死的服务器每秒重连十次 —— 自制 DoS。
        assert!(!should_reset_backoff(Duration::from_millis(300)));
        assert!(!should_reset_backoff(STABLE_SESSION - Duration::from_millis(1)));
        assert!(should_reset_backoff(STABLE_SESSION));
        assert!(should_reset_backoff(Duration::from_secs(600)));
    }

    #[test]
    fn state_transitions_are_observable() {
        let s = OutboundState::default();
        assert_eq!(s.get(), Status::Stopped);
        s.set(Status::Connecting);
        assert_eq!(s.get(), Status::Connecting);
        s.set(Status::Connected { sessions: 4 });
        assert!(matches!(s.get(), Status::Connected { sessions: 4 }));
    }

    #[test]
    fn dialer_is_absent_while_not_connected() {
        // 出站不可用时必须拿不到 dialer —— 这是 §6.4「拒绝而非静默回退」
        // 在数据结构层面的保证。
        let inst = OutboundInstance::new(test_cfg("测试节点"));
        assert!(inst.dialer().is_none());
        assert_eq!(inst.status(), Status::Stopped);
        assert_eq!(inst.name(), "测试节点");
    }

    #[tokio::test]
    async fn empty_session_bases_is_an_error_not_a_hang() {
        // 没有基址就没有会话，必须报错。若放过去，主会话用空 base 拼出的
        // 相对路径会打到承载页面自己的 origin —— 悄悄发给了另一台服务器。
        let mut cfg = test_cfg("空基址");
        cfg.session_bases.clear();
        let inst = OutboundInstance::new(cfg);
        let core = Arc::new(TransportCore::new());
        let env = SessionEnv {
            eval: Arc::new(|_| {}),
            on_status: Arc::new(|_, _| {}),
        };
        let e = match inst.handshake(&core, &env).await {
            Ok(_) => panic!("空的 session_bases 必须报错"),
            Err(e) => e.to_string(),
        };
        assert!(e.contains("空基址"), "{e}");
    }

    #[tokio::test]
    async fn liveness_fires_only_after_every_session_dies() {
        let l = SessionLiveness::new();
        let (a, a_peer) = tokio::io::duplex(64);
        let (b, b_peer) = tokio::io::duplex(64);
        let mut wa = l.watch(a);
        let mut wb = l.watch(b);
        assert_eq!(l.live_count(), 2);

        let fired = Arc::new(AtomicBool::new(false));
        let f2 = fired.clone();
        let l2 = l.clone();
        let waiter = tokio::spawn(async move {
            l2.all_dead().await;
            f2.store(true, Ordering::Relaxed);
        });

        // A 死：断掉对端 → wa 读到 EOF。
        drop(a_peer);
        let mut buf = [0u8; 8];
        use tokio::io::AsyncReadExt as _;
        assert_eq!(wa.read(&mut buf).await.unwrap(), 0);
        tokio::task::yield_now().await;
        assert_eq!(l.live_count(), 1, "只死了一条");
        assert!(!fired.load(Ordering::Relaxed), "还有会话活着，不该触发");

        // B 也死。
        drop(b_peer);
        assert_eq!(wb.read(&mut buf).await.unwrap(), 0);
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("all_dead 必须在最后一条死后触发")
            .unwrap();
        assert!(fired.load(Ordering::Relaxed));
        assert_eq!(l.live_count(), 0);
    }

    #[tokio::test]
    async fn liveness_counts_a_session_dead_only_once() {
        // 反复读 EOF 不能把计数减穿（AtomicUsize 减到 0 以下会回绕成天文数字，
        // all_dead 从此永不触发 —— 出站会永久卡在「已连接」而实际不通）。
        let l = SessionLiveness::new();
        let (a, a_peer) = tokio::io::duplex(64);
        let mut wa = l.watch(a);
        drop(a_peer);
        let mut buf = [0u8; 8];
        use tokio::io::AsyncReadExt as _;
        for _ in 0..5 {
            assert_eq!(wa.read(&mut buf).await.unwrap(), 0);
        }
        assert_eq!(l.live_count(), 0);
        drop(wa); // Drop 也走 die()，同样只能算一次
        assert_eq!(l.live_count(), 0);
        tokio::time::timeout(Duration::from_secs(1), l.all_dead())
            .await
            .expect("已经全死，必须立即返回");
    }

    #[tokio::test]
    async fn liveness_with_no_session_is_already_dead() {
        let l = SessionLiveness::new();
        tokio::time::timeout(Duration::from_secs(1), l.all_dead())
            .await
            .expect("一条会话都没有＝已死，不能挂起");
    }

    #[tokio::test]
    async fn death_watch_passes_bytes_through_unchanged() {
        // 观测层若改动字节，Noise 的密文当场作废。
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let l = SessionLiveness::new();
        let (a, mut a_peer) = tokio::io::duplex(1024);
        let mut wa = l.watch(a);
        wa.write_all(b"\x00\x01\xffhello").await.unwrap();
        let mut got = [0u8; 8];
        a_peer.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"\x00\x01\xffhello");

        a_peer.write_all(b"\xde\xad\xbe\xef").await.unwrap();
        let mut back = [0u8; 4];
        wa.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"\xde\xad\xbe\xef");
        assert_eq!(l.live_count(), 1, "正常收发不算死亡");
    }

    #[tokio::test]
    async fn stopping_instance_exits_the_loop_without_touching_core() {
        // 出站停机绝不能顺手把 core 标死 —— 共享承载下那会连累其他出站。
        let inst = OutboundInstance::new(test_cfg("要停的节点"));
        let core = Arc::new(TransportCore::new());
        inst.request_stop();
        let env = SessionEnv {
            eval: Arc::new(|_| {}),
            on_status: Arc::new(|_, _| {}),
        };
        tokio::time::timeout(Duration::from_secs(2), inst.run(core.clone(), env))
            .await
            .expect("request_stop 后循环必须退出");
        assert_eq!(inst.status(), Status::Stopped);
        assert!(!core.is_dead(), "core 必须毫发无伤");
        assert!(inst.dialer().is_none());
    }

    #[tokio::test]
    async fn dead_core_ends_the_loop_and_reports_why() {
        // core 死 = 页面问题，波及全部出站，只能由管理器统一 reload。
        // 实例的职责是退出并说清原因，而不是自己去 reload。
        let inst = OutboundInstance::new(test_cfg("承载已挂"));
        let core = Arc::new(TransportCore::new());
        core.mark_dead("测试").await;
        let seen = Arc::new(Mutex::new(Vec::<Status>::new()));
        let s2 = seen.clone();
        let env = SessionEnv {
            eval: Arc::new(|_| {}),
            on_status: Arc::new(move |_, s| s2.lock().unwrap().push(s.clone())),
        };
        tokio::time::timeout(Duration::from_secs(2), inst.run(core, env))
            .await
            .expect("core 已死时循环必须退出");
        let last = seen.lock().unwrap().last().cloned().unwrap();
        assert!(
            matches!(&last, Status::Failed { reason } if reason.contains("承载页面")),
            "{last:?}"
        );
    }
}
