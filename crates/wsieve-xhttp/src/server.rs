//! 服务端会话仓库：重排/去重/GC。spec §6.4 去重规则 + §9.4 生命周期。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::task::Waker;

use bytes::Bytes;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio::time::{interval, Duration, Instant};

/// 会话 ID（128-bit）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sid(pub [u8; 16]);

impl Sid {
    pub fn random() -> Self {
        let mut bytes = [0u8; 16];
        use rand::RngCore;
        rand::rng().fill_bytes(&mut bytes);
        Self(bytes)
    }
}

/// 会话死亡错误
#[derive(Debug, thiserror::Error)]
#[error("session gone")]
pub struct SessionGone;

/// 下行句柄：drop 时触发 GC
pub struct DownlinkHandle {
    sid: Sid,
    store: Arc<AsyncMutex<SessionStoreInner>>,
}

impl Drop for DownlinkHandle {
    fn drop(&mut self) {
        let store = self.store.clone();
        let sid = self.sid;

        // 异步处理 GC
        tokio::spawn(async move {
            let mut inner = store.lock().await;
            if inner.sessions.remove(&sid).is_some() {
                inner.notify.notify_one();
            }
        });
    }
}

struct Session {
    /// 已连续消费到的下一个 seq
    next_seq: u64,
    /// 重组堆：seq -> body（BTreeMap 天然排序）
    heap: BTreeMap<u64, Bytes>,
    /// 下行是否已挂载
    attached: bool,
    /// 最后上行活跃时间
    last_upstream_at: Instant,
    /// 创建时间（attach 窗口）
    created_at: Instant,

    /// 读 waker
    read_waker: Option<Waker>,
}

struct SessionStoreInner {
    sessions: BTreeMap<Sid, Session>,
    /// 会话增删/数据到达的全局通知（与 read_waker 配对）
    notify: Arc<Notify>,
}

pub struct SessionStore {
    inner: Arc<AsyncMutex<SessionStoreInner>>,
}

// 常量
const ATTACH_WINDOW_MS: u64 = 30_000;
const UPSTREAM_IDLE_MS: u64 = 180_000;
const MAX_BUFFERED_POSTS: usize = 30;

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore {
    pub fn new() -> Self {
        let inner = SessionStoreInner {
            sessions: BTreeMap::new(),
            notify: Arc::new(Notify::new()),
        };

        let store = Self {
            inner: Arc::new(AsyncMutex::new(inner)),
        };

        // 启动 GC 任务
        let inner_clone = store.inner.clone();
        tokio::spawn(async move {
            Self::gc_task_static(inner_clone).await;
        });

        store
    }

    /// 创建会话（握手成功后调用）
    pub async fn create(&self, sid: Sid) {
        let mut inner = self.inner.lock().await;
        inner.sessions.insert(
            sid,
            Session {
                next_seq: 1, // n=0 是握手 POST，数据 seq 从 1 起（spec §6.4）
                heap: BTreeMap::new(),
                attached: false,
                last_upstream_at: Instant::now(),
                created_at: Instant::now(),
                read_waker: None,
            },
        );
        inner.notify.notify_one();
    }

    /// 推送 POST body（seq 已解析）
    pub async fn push_post(&self, sid: &Sid, seq: u64, body: Bytes) -> Result<(), SessionGone> {
        let mut inner = self.inner.lock().await;

        let session = inner.sessions.get_mut(sid).ok_or(SessionGone)?;

        // 去重：seq < next_seq（已消费）或已在 heap → 丢弃，仍 Ok
        if seq < session.next_seq || session.heap.contains_key(&seq) {
            return Ok(());
        }

        // 更新活跃时间
        session.last_upstream_at = Instant::now();

        // 插入堆
        session.heap.insert(seq, body.clone());

        // 检查缓冲上限：有空洞 + 堆大小 ≥ 30 → GC
        let has_hole = session
            .heap
            .keys()
            .next()
            .map(|k| *k != session.next_seq)
            .unwrap_or(false);
        if has_hole && session.heap.len() > MAX_BUFFERED_POSTS {
            inner.sessions.remove(sid);
            inner.notify.notify_one();
            return Err(SessionGone);
        }

        // 唤醒读任务（新数据到达）
        if let Some(waker) = session.read_waker.take() {
            waker.wake();
        }
        inner.notify.notify_one();
        #[cfg(any())]
        eprintln!("");
        Ok(())
    }

    /// 读取数据（阻塞直到有数据或会话死亡）。
    /// 返回 0 表示 body 为空 POST（心跳）；调用方继续读即可。
    pub async fn read(&self, sid: &Sid, out: &mut [u8]) -> Result<usize, SessionGone> {
        // Notify 句柄必须在 block 外持有：`Notified` future 借用 Notify 本体
        let notify = {
            let inner = self.inner.lock().await;
            Arc::clone(&inner.notify)
        };
        loop {
            {
                let mut inner = self.inner.lock().await;
                let session = inner.sessions.get_mut(sid).ok_or(SessionGone)?;

                // 尝试从堆中取 next_seq；body 超过 out 容量时保留剩余部分，
                // 下次 read 继续吐（绝不丢字节）
                if let Some(mut b) = session.heap.remove(&session.next_seq) {
                    if b.len() > out.len() {
                        let n = out.len();
                        out.copy_from_slice(&b[..n]);
                        b = b.slice(n..);
                        session.heap.insert(session.next_seq, b);
                        return Ok(n);
                    }
                    session.next_seq += 1;
                    let n = b.len();
                    out[..n].copy_from_slice(&b);
                    return Ok(n);
                }
            }

            // 无数据：挂起直到数据到达或会话被删除（两者都 notify）
            notify.notified().await;
        }
    }

    /// 杀死会话
    pub async fn kill(&self, sid: &Sid) {
        let mut inner = self.inner.lock().await;
        if let Some(session) = inner.sessions.remove(sid) {
            if let Some(w) = session.read_waker {
                w.wake();
            }
        }
        inner.notify.notify_one();
    }

    /// 存活会话数（测试/监控用）。
    pub async fn session_count(&self) -> usize {
        self.inner.lock().await.sessions.len()
    }

    /// 杀死全部会话（测试/管理用）。
    pub async fn kill_all(&self) {
        let mut inner = self.inner.lock().await;
        let sids: Vec<Sid> = inner.sessions.keys().copied().collect();
        for sid in sids {
            if let Some(session) = inner.sessions.remove(&sid) {
                if let Some(w) = session.read_waker {
                    w.wake();
                }
            }
        }
        inner.notify.notify_one();
    }

    /// 绑定下行（返回句柄）
    pub async fn attach_downlink(&self, sid: &Sid) -> Result<DownlinkHandle, SessionGone> {
        let mut inner = self.inner.lock().await;
        let session = inner.sessions.get_mut(sid).ok_or(SessionGone)?;

        if session.attached {
            return Err(SessionGone);
        }

        session.attached = true;

        Ok(DownlinkHandle {
            sid: *sid,
            store: self.inner.clone(),
        })
    }

    /// GC 任务
    async fn gc_task_static(inner: Arc<AsyncMutex<SessionStoreInner>>) {
        let mut ticker = interval(Duration::from_secs(1));

        loop {
            ticker.tick().await;

            let now = Instant::now();
            let mut inner = inner.lock().await;

            let mut to_remove = Vec::new();

            for (&sid, session) in &inner.sessions {
                // GC 1: attach 窗口
                if !session.attached
                    && now.duration_since(session.created_at)
                        > Duration::from_millis(ATTACH_WINDOW_MS)
                {
                    to_remove.push(sid);
                    continue;
                }

                // GC 2: 上行空闲
                if now.duration_since(session.last_upstream_at)
                    > Duration::from_millis(UPSTREAM_IDLE_MS)
                {
                    to_remove.push(sid);
                }
            }

            let mut woken = false;
            for sid in to_remove {
                if let Some(session) = inner.sessions.remove(&sid) {
                    if let Some(w) = session.read_waker {
                        w.wake();
                        woken = true;
                    }
                }
            }
            if woken {
                inner.notify.notify_one();
            }
        }
    }
}
