//! 外部系统状态的托管（设计文档 §10）。
//!
//! hosts、系统代理、TUN 路由三者共性明确：**改了系统全局状态，
//! 进程崩溃后必须能恢复**。三处共用这一套纪律与一套测试。
//!
//! 崩溃路径靠 `clear_stale` 兜底 —— 它在任何 `apply` 之前调用，
//! 负责清掉上一次运行留下的残留。这是 hosts.rs 里已经验证过的做法
//! （不清残留的话，域名会一直指向一个已经不在跑的转发器，
//! 本机之后访问该域名全部失败）。
//!
//! 纪律的重点在 `clear_stale`，不在 `revert`：`revert` 只在正常退出时
//! 跑得到，而 SIGKILL / 断电 / panic-abort 全都跳过它。**能不能恢复，
//! 取决于下一次启动清不清残留**，所以 `clear_stale` 必须在所有启动路径上
//! 都被执行 —— 包括本次根本不打算 `apply` 的那些早退分支。

pub mod hosts;

/// 一项被托管的系统状态。
///
/// 实现者必须保证 `apply` 与 `revert` **幂等** —— 重复调用不产生额外效果。
/// 崩溃、SIGKILL、拔电源都可能让 `revert` 根本没机会跑，所以正确性
/// 不能依赖它一定被调用；`clear_stale` 才是最后一道防线。
///
/// 方法全部取 `&self` 且无泛型参数，故 trait 对象安全：日后 TUN 路由接进来
/// 时可以用 `Vec<Box<dyn ManagedSystemState>>` 统一收拢。
pub trait ManagedSystemState {
    /// 用于日志与错误信息的人类可读名字。
    fn name(&self) -> &'static str;

    /// 写入系统状态。
    fn apply(&self) -> anyhow::Result<()>;

    /// 恢复系统状态。必须幂等：没 apply 过也能安全调用。
    fn revert(&self) -> anyhow::Result<()>;

    /// 清理上一次运行的残留。在任何 `apply` 之前调用。
    fn clear_stale(&self) -> anyhow::Result<()>;
}

/// 持有即生效，drop 即恢复。
///
/// 注意 `acquire` 的顺序：**先 clear_stale，再 apply**。反过来的话，
/// 刚写好的条目会被紧接着的清理抹掉 —— 而且是静默的，托管看上去成功了，
/// 系统状态却什么都没改。
pub struct CustodyGuard<T: ManagedSystemState> {
    inner: T,
}

impl<T: ManagedSystemState> CustodyGuard<T> {
    pub fn acquire(inner: T) -> anyhow::Result<Self> {
        use anyhow::Context;
        let name = inner.name();
        inner
            .clear_stale()
            .with_context(|| format!("{name}：清理残留失败"))?;
        inner.apply().with_context(|| format!("{name}：写入失败"))?;
        Ok(Self { inner })
    }

    /// 借出被托管对象本身（诊断与后续 wiring 用）。
    #[allow(dead_code)]
    pub fn get(&self) -> &T {
        &self.inner
    }
}

impl<T: ManagedSystemState> Drop for CustodyGuard<T> {
    fn drop(&mut self) {
        // drop 里不能 ? —— 失败只能记日志。这也是 clear_stale 必须存在的原因。
        if let Err(e) = self.inner.revert() {
            tracing::warn!("恢复 {} 失败：{e:#}", self.inner.name());
        } else {
            tracing::info!("已恢复 {}", self.inner.name());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Default)]
    struct Spy {
        applied: AtomicUsize,
        reverted: AtomicUsize,
    }

    impl ManagedSystemState for Arc<Spy> {
        fn name(&self) -> &'static str {
            "spy"
        }
        fn apply(&self) -> anyhow::Result<()> {
            self.applied.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn revert(&self) -> anyhow::Result<()> {
            self.reverted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn clear_stale(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn guard_reverts_on_drop() {
        let spy = Arc::new(Spy::default());
        {
            let _g = CustodyGuard::acquire(spy.clone()).unwrap();
            assert_eq!(spy.applied.load(Ordering::SeqCst), 1);
            assert_eq!(spy.reverted.load(Ordering::SeqCst), 0);
        }
        assert_eq!(spy.reverted.load(Ordering::SeqCst), 1, "drop 必须恢复");
    }

    #[derive(Default)]
    struct OrderSpy {
        log: std::sync::Mutex<Vec<&'static str>>,
    }

    impl ManagedSystemState for Arc<OrderSpy> {
        fn name(&self) -> &'static str {
            "order"
        }
        fn apply(&self) -> anyhow::Result<()> {
            self.log.lock().unwrap().push("apply");
            Ok(())
        }
        fn revert(&self) -> anyhow::Result<()> {
            self.log.lock().unwrap().push("revert");
            Ok(())
        }
        fn clear_stale(&self) -> anyhow::Result<()> {
            self.log.lock().unwrap().push("clear_stale");
            Ok(())
        }
    }

    #[test]
    fn acquire_clears_stale_before_applying() {
        // 顺序错了就会：先写新条目，再被 clear_stale 抹掉
        let spy = Arc::new(OrderSpy::default());
        drop(CustodyGuard::acquire(spy.clone()).unwrap());
        assert_eq!(
            *spy.log.lock().unwrap(),
            vec!["clear_stale", "apply", "revert"]
        );
    }

    #[test]
    fn failed_apply_does_not_leave_a_guard() {
        struct Failing;
        impl ManagedSystemState for Failing {
            fn name(&self) -> &'static str {
                "failing"
            }
            fn apply(&self) -> anyhow::Result<()> {
                anyhow::bail!("故意失败")
            }
            fn revert(&self) -> anyhow::Result<()> {
                panic!("apply 失败后不该调用 revert")
            }
            fn clear_stale(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }
        assert!(CustodyGuard::acquire(Failing).is_err());
    }

    #[test]
    fn failed_clear_stale_aborts_before_applying() {
        // 残留没清干净就 apply，等于在一份未知状态上叠加 —— 宁可整项托管失败。
        let applied = Arc::new(AtomicUsize::new(0));
        struct StaleFails(Arc<AtomicUsize>);
        impl ManagedSystemState for StaleFails {
            fn name(&self) -> &'static str {
                "stale-fails"
            }
            fn apply(&self) -> anyhow::Result<()> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn revert(&self) -> anyhow::Result<()> {
                Ok(())
            }
            fn clear_stale(&self) -> anyhow::Result<()> {
                anyhow::bail!("清不掉")
            }
        }
        let e = match CustodyGuard::acquire(StaleFails(applied.clone())) {
            Ok(_) => panic!("清残留失败时必须整项失败"),
            Err(e) => e,
        };
        assert_eq!(applied.load(Ordering::SeqCst), 0, "清残留失败后不该 apply");
        // 错误必须带上「哪一项托管、哪个环节」，否则日志里只有一句「清不掉」。
        let msg = format!("{e:#}");
        assert!(msg.contains("stale-fails"), "错误缺少托管项名字：{msg}");
        assert!(msg.contains("清理残留"), "错误缺少环节：{msg}");
        assert!(msg.contains("清不掉"), "错误丢了根因：{msg}");
    }

    /// trait 对象安全 —— TUN 路由接进来时要靠它统一收拢。
    #[test]
    fn trait_is_object_safe() {
        let spy = Arc::new(Spy::default());
        let boxed: Box<dyn ManagedSystemState> = Box::new(spy.clone());
        boxed.apply().unwrap();
        assert_eq!(spy.applied.load(Ordering::SeqCst), 1);
    }
}
