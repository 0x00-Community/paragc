//! 引用计数的减量在线程池上并发处理（mutator 照常跑）：年轻回收的停顿只把减量放进队列。
//!
//! 安全性：停顿那一刻计数为 0 的对象没有计入的引用，根都 +1 过、年轻对象都已晋升或死掉 ⇒ mutator 碰不到它们，
//! 连锁下去的也一样。释放只析构、清起点位、记下脏块；脏块的行要等下一次停顿开头重算（此刻可能有分配缓冲在往里分配）。
//! 被释放对象的行在重算前一直标着占用，所以不会被提前分配出去。
//!
//! 结束判定与标记相同：工人自己的队列空、也偷不到 ⇒ 空闲计数 +1；全部空闲 ⇒ 结束（只有工人会往队列里放）。
use core::sync::atomic::{AtomicUsize, Ordering};

use crossbeam_deque::{Injector, Steal, Worker};
use crossbeam_utils::{Backoff, CachePadded};

use crate::block::Block;
use crate::header::Header;
use crate::trace::Tracer;
use crate::workers::Pool;

pub(crate) struct Releaser<'h> {
    /// 待减的对象（头地址）
    decs: &'h Injector<usize>,
    /// 释放过对象的块（下一次停顿时重算行）
    dirty: &'h Injector<Block>,
    workers: usize,
    idle: CachePadded<AtomicUsize>,
    /// 判老：标记纪元等于它
    epoch: u32,
    /// 当前计数的奇偶
    parity: bool,
}

impl<'h> Releaser<'h> {
    pub(crate) fn new(decs: &'h Injector<usize>, dirty: &'h Injector<Block>, workers: usize, epoch: u32, parity: bool) -> Self {
        Self { decs, dirty, workers: workers.max(1), idle: CachePadded::new(AtomicUsize::new(0)), epoch, parity }
    }

    pub(crate) fn run(&self, pool: &Pool) {
        pool.run(self.workers, |_| self.work());
    }

    fn work(&self) {
        let local = Worker::new_lifo();
        loop {
            while let Some(addr) = local.pop() {
                self.one(addr, &local);
            }
            if let Some(addr) = self.steal(&local) {
                self.one(addr, &local);
                continue;
            }
            self.idle.fetch_add(1, Ordering::AcqRel);
            let backoff = Backoff::new();
            loop {
                if self.idle.load(Ordering::Acquire) == self.workers {
                    return;
                }
                if let Some(addr) = self.steal(&local) {
                    self.idle.fetch_sub(1, Ordering::AcqRel);
                    self.one(addr, &local);
                    break;
                }
                backoff.snooze();
            }
        }
    }

    fn steal(&self, local: &Worker<usize>) -> Option<usize> {
        loop {
            match self.decs.steal_batch_and_pop(local) {
                Steal::Success(addr) => return Some(addr),
                Steal::Empty => return None,
                Steal::Retry => {}
            }
        }
    }

    /// 对一个对象 −1；减到 0 的块内老对象释放：它每条边的目标再 −1（放进本地队列），析构，清起点位。
    /// 大对象、粘住的、年轻的不在这里释放（留给追踪 / 年轻清扫）。
    fn one(&self, addr: usize, local: &Worker<usize>) {
        let header = core::ptr::with_exposed_provenance::<Header>(addr);
        // SAFETY: 减量来自计入过的边，目标在计数归零前一直活着
        let h = unsafe { &*header };
        if !h.rc_dec(self.parity) || h.is_large() || !h.is_marked(self.epoch) {
            return;
        }
        let mut children = Vec::new();
        let seed = Worker::new_lifo();
        let mut t = Tracer::releasing(&seed, &mut children);
        // SAFETY: 对象还没析构；释放模式只读它的边；计数归零后只有拿到 true 的这一个线程碰它
        unsafe { Header::visit(header, &mut t) };
        for child in children {
            local.push(child);
        }
        // SAFETY: 计数归零 ⇒ 没有计入的引用、mutator 碰不到（见模块说明）
        unsafe { Header::finalize(header) };
        let block = Block::containing(header.cast());
        block.clear_start(header.cast());
        self.dirty.push(block);
    }
}
