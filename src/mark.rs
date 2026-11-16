//! 并行标记：每个工人一个 crossbeam 双端队列（自己 LIFO 弹），根与写屏障记下的旧值放堆的公共灰队列；
//! 自己的空了就成批偷（`steal_batch_and_pop`，一次搬走别人队列的一半）。
//!
//! 并发标记时 mutator 还在跑，写屏障会继续往灰队列里放东西：这一轮全部空闲就先停，剩下的由结束回收（独占堆）时再跑一轮清空。
//!
//! 结束判定：自己的队列空、也偷不到时把空闲计数加一，然后边退避边偷；空闲计数等于工人数时
//! 所有队列都空、也没人还在追踪（追踪中的工人没计入空闲），标记结束。偷到了就先减一再干活；
//! 偷到还没来得及减一时别人可能先看到「全空闲」退出，但手里的活仍由偷到的人做完，结果不受影响。
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crossbeam_deque::{Injector, Steal, Stealer, Worker};
use crossbeam_utils::{Backoff, CachePadded};

use j8lockfree::SegVec;

use crate::block::Block;
use crate::copy::Copier;
use crate::header::Header;
use crate::roots::RootTable;
use crate::trace::Tracer;
use crate::workers::Pool;

pub(crate) struct Marking<'g> {
    /// 每个工号一个本地队列，由那个工号的线程取走（只取一次）
    locals: Vec<CachePadded<UnsafeCell<Option<Worker<usize>>>>>,
    stealers: Vec<Stealer<usize>>,
    /// 堆的灰队列：根与写屏障记下的旧值
    grey: &'g Injector<usize>,
    idle: CachePadded<AtomicUsize>,
    epoch: u32,
    /// 疏散回收：每个工号一个复制缓冲（空 = 不疏散）
    copiers: Vec<CachePadded<UnsafeCell<Option<Copier>>>>,
    /// 疏散回收：0 号工人先把根表里的槽改成搬走后的新地址（同时把根标灰）
    roots: Option<&'g RootTable>,
}

// SAFETY: 每个工号的本地队列只被一个线程取走（工号互不相同）；其余是线程安全的
unsafe impl Sync for Marking<'_> {}

impl<'g> Marking<'g> {
    pub(crate) fn new(workers: usize, epoch: u32, grey: &'g Injector<usize>) -> Self {
        let locals: Vec<Worker<usize>> = (0..workers).map(|_| Worker::new_lifo()).collect();
        let stealers = locals.iter().map(Worker::stealer).collect();
        Self {
            locals: locals.into_iter().map(|w| CachePadded::new(UnsafeCell::new(Some(w)))).collect(),
            stealers,
            grey,
            idle: CachePadded::new(AtomicUsize::new(0)),
            epoch,
            copiers: Vec::new(),
            roots: None,
        }
    }

    /// 疏散回收：每个工人带一个复制缓冲，候选块里被追踪到的对象搬进 `targets` 里的块（不够就开新块登记进 `blocks`）；
    /// 根由 0 号工人重定位。`last` 是上次完成的纪元（空行判定）。
    pub(crate) fn evacuating(mut self, roots: &'g RootTable, targets: &Injector<Block>, blocks: &SegVec<Block>, moved: &AtomicUsize, last: u32) -> Self {
        self.copiers = (0..self.locals.len()).map(|_| CachePadded::new(UnsafeCell::new(Some(Copier::new(targets, blocks, moved, last, self.epoch))))).collect();
        self.roots = Some(roots);
        self
    }

    /// 标记根（顺带写行标记），需要追踪的放进灰队列。
    pub(crate) fn seed(grey: &Injector<usize>, roots: &[*const Header], epoch: u32) {
        let seed = Worker::new_lifo();
        let mut t = Tracer::new(&seed, epoch, None);
        for &root in roots {
            t.visit(root, false);
        }
        while let Some(item) = seed.pop() {
            grey.push(item);
        }
    }

    /// 年轻回收：记忆集里的槽（地址）当前指向的对象当作根（年轻的标上、进灰队列；老的碰到就停）。
    pub(crate) fn seed_slots(grey: &Injector<usize>, slots: &[usize], epoch: u32) {
        let seed = Worker::new_lifo();
        let mut t = Tracer::new(&seed, epoch, None);
        for &slot in slots {
            let slot = core::ptr::with_exposed_provenance::<AtomicPtr<u8>>(slot);
            // SAFETY: 槽在写入它的对象里；那个对象被写时可达，到这次回收前不会被释放（见 Mutator::written）
            let p = unsafe { &*slot }.load(Ordering::Acquire);
            if !p.is_null() {
                t.visit(Header::of(p), false);
            }
        }
        while let Some(item) = seed.pop() {
            grey.push(item);
        }
    }

    /// 从灰队列出发标记，直到全部工人空闲。
    pub(crate) fn run(&self, pool: &Pool) {
        pool.run(self.locals.len(), |me| self.work(me));
    }

    fn work(&self, me: usize) {
        // SAFETY: 工号 me 只在这一个线程上
        let local = unsafe { (*self.locals[me].get()).take() }.expect("每个工号只取一次本地队列");
        // SAFETY: 同上，复制缓冲也按工号取一次
        let mut copier = self.copiers.get(me).and_then(|c| unsafe { (*c.get()).take() });
        let mut t = Tracer::new(&local, self.epoch, copier.as_mut());
        if me == 0
            && let Some(roots) = self.roots
        {
            roots.relocate(|h| t.forward(h, false));
        }
        loop {
            while let Some(item) = local.pop() {
                t.scan(item);
            }
            if let Some(item) = self.steal(me, &local) {
                t.scan(item);
                continue;
            }
            self.idle.fetch_add(1, Ordering::AcqRel);
            match self.wait(me, &local) {
                Some(item) => t.scan(item),
                None => return,
            }
        }
    }

    /// 空闲时边退避边偷；全部空闲返回 None。
    fn wait(&self, me: usize, local: &Worker<usize>) -> Option<usize> {
        let backoff = Backoff::new();
        loop {
            if self.idle.load(Ordering::Acquire) == self.locals.len() {
                return None;
            }
            if let Some(item) = self.steal(me, local) {
                self.idle.fetch_sub(1, Ordering::AcqRel);
                return Some(item);
            }
            backoff.snooze();
        }
    }

    /// 先偷注入队列，再从下一个工人开始轮着偷；成批搬进自己的队列，返回其中一个。
    fn steal(&self, me: usize, local: &Worker<usize>) -> Option<usize> {
        let n = self.stealers.len();
        loop {
            let found = core::iter::once(self.grey.steal_batch_and_pop(local))
                .chain((1..n).map(|k| self.stealers[(me + k) % n].steal_batch_and_pop(local)))
                .collect::<Steal<usize>>();
            match found {
                Steal::Success(item) => return Some(item),
                Steal::Empty => return None,
                Steal::Retry => {}
            }
        }
    }
}
