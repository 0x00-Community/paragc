//! 公开堆的并行标记：照类型表里的指针图找边，不调用对象的代码（回收方不需要认识这些类型）。
//! 工作窃取与结束判定同私有堆（[`crate::mark`]）：每个工人一个双端队列，空了成批偷，全部空闲即结束。
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crossbeam_deque::{Injector, Steal, Stealer, Worker};
use crossbeam_utils::{Backoff, CachePadded};

use crate::block::Block;
use crate::workers::Pool;

use super::heap::Inner;
use super::ptr::PHeader;

pub(crate) struct Marker<'h> {
    heap: &'h Inner,
    epoch: u32,
    /// 每个工号一个本地队列，由那个工号的线程取走（只取一次）
    locals: Vec<CachePadded<UnsafeCell<Option<Worker<usize>>>>>,
    stealers: Vec<Stealer<usize>>,
    /// 根
    grey: Injector<usize>,
    idle: CachePadded<AtomicUsize>,
}

// SAFETY: 每个工号的本地队列只被一个线程取走；其余是线程安全的
unsafe impl Sync for Marker<'_> {}

impl<'h> Marker<'h> {
    pub(crate) fn new(heap: &'h Inner, epoch: u32, workers: usize) -> Self {
        let locals: Vec<Worker<usize>> = (0..workers).map(|_| Worker::new_lifo()).collect();
        let stealers = locals.iter().map(Worker::stealer).collect();
        Self {
            heap,
            epoch,
            locals: locals.into_iter().map(|w| CachePadded::new(UnsafeCell::new(Some(w)))).collect(),
            stealers,
            grey: Injector::new(),
            idle: CachePadded::new(AtomicUsize::new(0)),
        }
    }

    /// 从 `roots`（对象头地址）出发标记，直到全部工人空闲。
    pub(crate) fn run(&self, pool: &Pool, roots: &[usize]) {
        for &r in roots {
            self.visit(r, |h| self.grey.push(h));
        }
        pool.run(self.locals.len(), |me| self.work(me));
    }

    /// 标记对象（和它跨的行）；本轮第一次见到且它有边时交给 `push`。
    fn visit(&self, header: usize, push: impl FnOnce(usize)) {
        let h = core::ptr::with_exposed_provenance::<PHeader>(header);
        // SAFETY: 来自入口或活对象的指针槽，对象活着
        let (core, ty) = unsafe { (&(*h).core, (*h).ty) };
        if !core.mark(self.epoch) {
            return;
        }
        if !core.is_large() {
            Block::containing(h.cast()).mark_lines(h.cast(), core.bytes(), self.epoch);
        }
        if self.heap.has_edges(ty) {
            push(header);
        }
    }

    /// 追踪一个对象：指针图里每个槽，指向这个堆的就标上（别的公开堆的由它们自己的入口管）。
    fn scan(&self, header: usize, local: &Worker<usize>) {
        let h = core::ptr::with_exposed_provenance::<PHeader>(header);
        // SAFETY: 队列里只有本轮标记过的活对象
        let ty = unsafe { (*h).ty };
        let data = PHeader::data(h);
        for &off in self.heap.edges(ty) {
            // SAFETY: 指针图里的偏移都是这个类型里 PublicSlot（一个 AtomicU64）的位置
            let v = unsafe { &*data.add(off as usize).cast::<AtomicU64>() }.load(Ordering::Acquire);
            if v != 0 && self.heap.owns(v) {
                self.visit(PHeader::of(self.heap.ptr(v)).expose_provenance(), |x| local.push(x));
            }
        }
    }

    fn work(&self, me: usize) {
        // SAFETY: 工号 me 只在这一个线程上
        let local = unsafe { (*self.locals[me].get()).take() }.expect("每个工号只取一次本地队列");
        loop {
            while let Some(item) = local.pop() {
                self.scan(item, &local);
            }
            if let Some(item) = self.steal(me, &local) {
                self.scan(item, &local);
                continue;
            }
            self.idle.fetch_add(1, Ordering::AcqRel);
            match self.wait(me, &local) {
                Some(item) => self.scan(item, &local),
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

    /// 先偷根队列，再从下一个工人开始轮着偷；成批搬进自己的队列，返回其中一个。
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
