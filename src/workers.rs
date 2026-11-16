//! 回收用的工人：每次回收在 `std::thread::scope` 里开，调用线程自己当 0 号工人。
use core::num::NonZeroUsize;

use crate::layout::{SPINS, UNITS_PER_WORKER};

#[derive(Clone, Copy)]
pub(crate) struct Workers(NonZeroUsize);

impl Workers {
    /// 至多 `max` 个；活少（块 + 大对象不多）时少开。
    pub(crate) fn sized(max: NonZeroUsize, units: usize) -> Self {
        let n = (units / UNITS_PER_WORKER).clamp(1, max.get());
        Self(NonZeroUsize::new(n).unwrap_or(NonZeroUsize::MIN))
    }

    pub(crate) fn count(self) -> usize {
        self.0.get()
    }

    /// 每个工人跑一次 `f(工号)`，按工号收集结果。工人 panic 会在这里重新抛出。
    pub(crate) fn run<R: Send>(self, f: impl Fn(usize) -> R + Sync) -> Vec<R> {
        if self.count() == 1 {
            return vec![f(0)];
        }
        let f = &f;
        std::thread::scope(|s| {
            let others: Vec<_> = (1..self.count()).map(|i| s.spawn(move || f(i))).collect();
            let mut out = vec![f(0)];
            out.extend(others.into_iter().map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e))));
            out
        })
    }
}

/// 等别人时的退避：先自旋，之后让出 CPU。
#[derive(Default)]
pub(crate) struct Backoff(u32);

impl Backoff {
    pub(crate) fn snooze(&mut self) {
        if self.0 < SPINS {
            self.0 += 1;
            core::hint::spin_loop();
        } else {
            std::thread::yield_now();
        }
    }
}
