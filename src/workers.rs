//! 回收用的常驻线程池：堆第一次回收时起 `threads − 1` 个工人线程，活到堆析构；调用线程自己当 0 号工人。
//!
//! - 分派：先写好任务与调用方，再把「代号 + 本轮参与人数」一个原子字加一代，只叫醒参与的工人
//! - 工人空闲时先退避自旋（crossbeam `Backoff`），转完一轮就 `thread::park` 挂起（futex，不是锁），不白烧 CPU
//! - 本轮参与者全部做完后由最后一个叫醒调用方；工人里的 panic 在调用方重新抛出
use core::cell::UnsafeCell;
use core::num::NonZeroUsize;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread::{self, JoinHandle, Thread};

use crossbeam_utils::{Backoff, CachePadded};

use crate::layout::UNITS_PER_WORKER;

/// 任务字的低位：本轮参与人数（含调用方）
const ACTIVE_BITS: u32 = 16;
const ACTIVE_MASK: u64 = (1 << ACTIVE_BITS) - 1;
/// 任务字每加一代加这么多
const GENERATION: u64 = 1 << ACTIVE_BITS;

/// 一轮的任务：类型擦除过的 `&dyn Fn(工号)`，只在 [`Pool::run`] 等待期间有效。
type Job = *const (dyn Fn(usize) + Sync);

struct Shared {
    /// 高位代号、低位参与人数；工人一次读出，二者一致
    task: CachePadded<AtomicU64>,
    /// 本轮还没做完的工人数（不含调用方）
    remaining: CachePadded<AtomicUsize>,
    job: UnsafeCell<Job>,
    /// 本轮的调用方（做完的最后一个工人叫醒它）
    caller: UnsafeCell<Option<Thread>>,
    /// 本轮有工人 panic 了
    panicked: AtomicBool,
    /// 正有一轮在跑（防重入：并发标记经 &Heap 调用）
    busy: AtomicBool,
    stop: AtomicBool,
}

// SAFETY: job / caller 只在没有工人读它们的时候写（上一轮全部做完、下一代还没发出）；其余是原子量
unsafe impl Sync for Shared {}
// SAFETY: 同上
unsafe impl Send for Shared {}

impl Shared {
    fn task(&self) -> (u64, usize) {
        let word = self.task.load(Ordering::Acquire);
        (word & !ACTIVE_MASK, usize::try_from(word & ACTIVE_MASK).unwrap_or(usize::MAX))
    }

    /// 工人 `me` 的主循环。
    fn serve(&self, me: usize) {
        // 从第 0 代起算：线程起得晚、第一轮已经发出时也不会漏掉
        let mut seen = 0;
        loop {
            let (generation, active) = self.wait(seen);
            if self.stop.load(Ordering::Acquire) {
                return;
            }
            seen = generation;
            if me >= active {
                continue;
            }
            // SAFETY: 参与本轮的工人做完之前，调用方一直等着，job 指向的闭包活着、不会被改写
            let job = unsafe { &**self.job.get() };
            if catch_unwind(AssertUnwindSafe(|| job(me))).is_err() {
                self.panicked.store(true, Ordering::Release);
            }
            // 先抄下调用方再减计数：减到 0 的瞬间调用方就可能返回、开始下一轮并改写 caller
            // SAFETY: 本工人还没减计数，本轮没做完，调用方不会改 caller
            let caller = unsafe { (*self.caller.get()).clone() };
            if self.remaining.fetch_sub(1, Ordering::AcqRel) == 1
                && let Some(caller) = caller
            {
                // 调用方可能已经走了，多一次 unpark 无害
                caller.unpark();
            }
        }
    }

    /// 等到代号不是 `seen`（或要停）。
    fn wait(&self, seen: u64) -> (u64, usize) {
        let backoff = Backoff::new();
        loop {
            let task = self.task();
            if task.0 != seen || self.stop.load(Ordering::Acquire) {
                return task;
            }
            if backoff.is_completed() {
                thread::park();
            } else {
                backoff.snooze();
            }
        }
    }
}

pub(crate) struct Pool {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

impl Pool {
    /// 起 `threads − 1` 个工人。
    pub(crate) fn new(threads: NonZeroUsize) -> Self {
        let shared = Arc::new(Shared {
            task: CachePadded::new(AtomicU64::new(0)),
            remaining: CachePadded::new(AtomicUsize::new(0)),
            job: UnsafeCell::new(core::ptr::null::<fn(usize)>() as Job),
            caller: UnsafeCell::new(None),
            panicked: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let max = u64::try_from(threads.get()).unwrap_or(ACTIVE_MASK).min(ACTIVE_MASK);
        let workers = (1..usize::try_from(max).unwrap_or(1))
            .map(|me| {
                let shared = shared.clone();
                thread::Builder::new().name(format!("paragc-{me}")).spawn(move || shared.serve(me)).expect("起回收线程")
            })
            .collect();
        Self { shared, workers }
    }

    /// 线程总数（含调用方）。
    pub(crate) fn threads(&self) -> usize {
        self.workers.len() + 1
    }

    /// 这么多活（块 + 大对象）该用几个线程：活少时少用。
    pub(crate) fn sized(&self, units: usize) -> usize {
        (units / UNITS_PER_WORKER).clamp(1, self.threads())
    }

    /// `n` 个线程（含调用方，工号 0..n）各跑一次 `f(工号)`，按工号收集结果。
    pub(crate) fn run<R: Send>(&self, n: usize, f: impl Fn(usize) -> R + Sync) -> Vec<R> {
        let n = n.clamp(1, self.threads());
        if n == 1 {
            return vec![f(0)];
        }
        let _busy = Busy::enter(&self.shared.busy);
        let slots: Vec<UnsafeCell<Option<R>>> = (0..n).map(|_| UnsafeCell::new(None)).collect();
        let slots = Slots(&slots);
        let job = |me: usize| slots.put(me, f(me));
        let job: &(dyn Fn(usize) + Sync) = &job;
        let shared = &*self.shared;
        // SAFETY: 上一轮已全部做完（Busy 保证 run 不重入），此刻没有工人读 job / caller；
        // 擦掉的生命周期由下面「等到全部做完才返回」保证
        #[expect(clippy::transmute_ptr_to_ptr, reason = "要擦掉 dyn 的生命周期，as 转换做不到")]
        unsafe {
            *shared.job.get() = core::mem::transmute::<*const (dyn Fn(usize) + Sync + '_), Job>(job);
            *shared.caller.get() = Some(thread::current());
        }
        shared.panicked.store(false, Ordering::Relaxed);
        shared.remaining.store(n - 1, Ordering::Relaxed);
        let active = u64::try_from(n).unwrap_or(ACTIVE_MASK);
        let word = shared.task.load(Ordering::Relaxed);
        shared.task.store(((word & !ACTIVE_MASK) + GENERATION) | active, Ordering::Release);
        for w in &self.workers[..n - 1] {
            w.thread().unpark();
        }
        let mine = catch_unwind(AssertUnwindSafe(|| job(0)));
        let backoff = Backoff::new();
        while shared.remaining.load(Ordering::Acquire) != 0 {
            if backoff.is_completed() {
                thread::park();
            } else {
                backoff.snooze();
            }
        }
        if let Err(e) = mine {
            std::panic::resume_unwind(e);
        }
        assert!(!shared.panicked.load(Ordering::Acquire), "回收线程 panic 了");
        slots.take()
    }
}

/// 一轮的占用标记：同一时刻只能有一轮。
struct Busy<'a>(&'a AtomicBool);

impl<'a> Busy<'a> {
    fn enter(flag: &'a AtomicBool) -> Self {
        assert!(flag.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok(), "回收线程池同时只能跑一轮");
        Self(flag)
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// 结果槽：每个工号一个，跨线程只写自己的。
struct Slots<'a, R>(&'a [UnsafeCell<Option<R>>]);

// SAFETY: 每个槽只被一个线程写，读在全部写完之后
unsafe impl<R: Send> Sync for Slots<'_, R> {}

impl<R> Slots<'_, R> {
    fn put(&self, me: usize, r: R) {
        // SAFETY: 每个工号只有一个线程写自己的槽，读在全部做完之后
        unsafe { *self.0[me].get() = Some(r) };
    }

    /// 全部做完之后取出结果。
    fn take(&self) -> Vec<R> {
        // SAFETY: 全部工人已经做完，没有并发写
        self.0.iter().map(|s| unsafe { (*s.get()).take() }.expect("每个工号都写了结果")).collect()
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.task.fetch_add(GENERATION, Ordering::AcqRel);
        for w in self.workers.drain(..) {
            w.thread().unpark();
            // 工人里的 panic 已经在 run 里报过
            let _ = w.join();
        }
    }
}
