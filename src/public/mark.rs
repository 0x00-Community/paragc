//! 公开堆的并行标记：照类型表里的指针图找边，不调用对象的代码（回收方不需要认识这些类型）。
//! 工作窃取与结束判定同私有堆（[`crate::mark`]）：每个工人一个双端队列，空了成批偷，全部空闲即结束。
//!
//! 可以几个堆一起标（[`super::PublicHeap::collect_group`]）：队列里每一项是「组里第几个堆 + 对象头地址」，
//! 指向组里别的堆的边交给那个堆（按它的纪元标），并报到 `found` 里（之后重建跨堆登记）；指向组外的不跟。
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crossbeam_deque::{Injector, Steal, Stealer, Worker};
use crossbeam_utils::{Backoff, CachePadded};

use crate::block::Block;
use crate::workers::Pool;

use super::heap::Inner;
use super::ptr::PHeader;
use super::region::WINDOWS;

/// 队列项里「组里第几个堆」放在高位（地址只用到低 48 位）
const MEMBER_SHIFT: u32 = 56;
/// 队列项里地址那部分
const ADDR_MASK: usize = (1 << MEMBER_SHIFT) - 1;

/// 组里的一个堆：它和它这一轮的纪元。
pub struct Member<'h> {
    pub heap: &'h Inner,
    pub epoch: u32,
}

pub struct Marker<'h> {
    members: &'h [Member<'h>],
    /// 堆号 → 组里第几个（不在组里是 None）
    by_heap: [Option<usize>; WINDOWS],
    /// 一起回收时：发现的组内跨堆边（来源是组里第几个, 目标的公开偏移）
    found: Option<&'h Injector<(usize, u64)>>,
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
    pub fn new(members: &'h [Member<'h>], found: Option<&'h Injector<(usize, u64)>>, workers: usize) -> Self {
        let locals: Vec<Worker<usize>> = (0..workers).map(|_| Worker::new_lifo()).collect();
        let stealers = locals.iter().map(Worker::stealer).collect();
        let mut by_heap = [None; WINDOWS];
        for (m, member) in members.iter().enumerate() {
            by_heap[member.heap.index()] = Some(m);
        }
        Self {
            members,
            by_heap,
            found,
            locals: locals.into_iter().map(|w| CachePadded::new(UnsafeCell::new(Some(w)))).collect(),
            stealers,
            grey: Injector::new(),
            idle: CachePadded::new(AtomicUsize::new(0)),
        }
    }

    /// 从 `roots`（组里第几个, 对象头地址）出发标记，直到全部工人空闲。
    pub fn run(&self, pool: &Pool, roots: &[(usize, usize)]) {
        for &(m, header) in roots {
            self.visit(m, header, |item| self.grey.push(item));
        }
        pool.run(self.locals.len(), |me| self.work(me));
    }

    /// 标记组里第 `m` 个堆的对象（和它跨的行）；本轮第一次见到且它有边时把队列项交给 `push`。
    fn visit(&self, m: usize, header: usize, push: impl FnOnce(usize)) {
        let member = &self.members[m];
        let h = core::ptr::with_exposed_provenance::<PHeader>(header);
        // SAFETY: 来自入口或活对象的指针槽，对象活着
        let (core, ty) = unsafe { (&(*h).core, (*h).ty) };
        if !core.mark(member.epoch) {
            return;
        }
        if !core.is_large() {
            Block::containing(h.cast()).mark_lines(h.cast(), core.bytes(), member.epoch);
        }
        if member.heap.has_edges(ty) {
            push(header | m << MEMBER_SHIFT);
        }
    }

    /// 追踪一个对象：指针图里每个槽，指向组里某个堆的就标上（指向别的堆时报到 `found`）；组外的由它们自己的入口管。
    fn scan(&self, item: usize, local: &Worker<usize>) {
        let (m, header) = (item >> MEMBER_SHIFT, item & ADDR_MASK);
        let source = self.members[m].heap;
        let h = core::ptr::with_exposed_provenance::<PHeader>(header);
        // SAFETY: 队列里只有本轮标记过的活对象
        let ty = unsafe { (*h).ty };
        let data = PHeader::data(h);
        for &off in source.edges(ty) {
            // SAFETY: 指针图里的偏移都是这个类型里指针槽（一个 AtomicU64）的位置
            let v = unsafe { &*data.add(off as usize).cast::<AtomicU64>() }.load(Ordering::Acquire);
            if v == 0 {
                continue;
            }
            let Some(t) = Inner::heap_of(v).and_then(|k| self.by_heap[k]) else { continue };
            if t != m
                && let Some(found) = self.found
            {
                found.push((m, v));
            }
            let target = self.members[t].heap;
            self.visit(t, PHeader::of(target.ptr(v)).expose_provenance(), |x| local.push(x));
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
