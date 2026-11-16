//! 清扫：标记收尾时（独占堆）把全部块与大对象交给 [`Sweeper`]，之后 mutator 照常跑，清扫在两头并发进行：
//!
//! - mutator 要新块时先拿清扫好的；没有就自己领一个没扫的块当场扫（析构死对象），能用就用（惰性清扫）
//! - 回收线程（[`crate::Heap::sweep_concurrently`]）把剩下的块和大对象领走扫完
//!
//! 领用是原子游标 `fetch_add`，每个块恰好被一个线程扫一次；扫完才交出去，所以扫的时候没有 mutator 在用它。
//! 判死用「标记纪元 ≠ 本轮」：下一轮标记开始前必须扫完（否则活对象被标成更新的纪元会被当成死的）。
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::alloc::{Layout, dealloc};

use crossbeam_utils::Backoff;

use crate::block::{Block, State};
use crate::header::Header;
use crate::layout::GRANULE;
use crate::workers::Pool;

/// 单独分配的大对象（头指针）。
#[derive(Clone, Copy)]
pub(crate) struct Large(pub(crate) NonNull<Header>);

// SAFETY: 独立的堆内存；对象内容跨线程由 Trace: Send + Sync 担保
unsafe impl Send for Large {}
// SAFETY: 同上
unsafe impl Sync for Large {}

impl Large {
    pub(crate) fn layout(bytes: usize) -> Layout {
        Layout::from_size_align(bytes, GRANULE).expect("对象太大")
    }

    fn header(&self) -> &Header {
        // SAFETY: 大对象活着时头一直有效
        unsafe { self.0.as_ref() }
    }

    /// 死了就析构并释放，返回 None；活着返回自己。
    /// SAFETY（调用方）：本轮标记已经做完；死对象不可达，没人会再碰它。
    unsafe fn sweep(self, epoch: u32) -> Option<Self> {
        if self.header().is_marked(epoch) {
            return Some(self);
        }
        let layout = Self::layout(self.header().bytes());
        // SAFETY: 没标记 ⇒ 不可达；内存来自 Mutator 按同一 layout 的分配
        unsafe {
            Header::finalize(self.0.as_ptr());
            dealloc(self.0.as_ptr().cast(), layout);
        }
        None
    }
}

/// 扫完一个块的去向。
pub(crate) enum Outcome {
    /// 有空行（含全空），可以分配
    Usable(Block),
    /// 满了
    Full(Block),
    /// 全空且留够了备用，已还给系统
    Released,
}

/// 清扫完一项后交给谁。
pub(crate) trait Sink {
    fn block(&self, outcome: Outcome);
    fn large(&self, survivor: Large);
}

#[derive(Default)]
pub(crate) struct Sweeper {
    /// 本轮判活的纪元
    epoch: u32,
    blocks: Vec<Block>,
    next_block: AtomicUsize,
    large: Vec<Large>,
    next_large: AtomicUsize,
    /// 还没扫完的项（领走没扫完的也算）
    unfinished: AtomicUsize,
    /// 还能留作备用的全空块数
    spare: AtomicUsize,
    /// 本轮存活字节数（含头）
    live: AtomicUsize,
}

impl Sweeper {
    /// 开始一轮（独占堆时）。上一轮必须已经扫完。
    pub(crate) fn start(&mut self, epoch: u32, blocks: Vec<Block>, large: Vec<Large>, spare: usize) {
        debug_assert!(self.is_done());
        *self.unfinished.get_mut() = blocks.len() + large.len();
        *self.next_block.get_mut() = 0;
        *self.next_large.get_mut() = 0;
        *self.spare.get_mut() = spare;
        *self.live.get_mut() = 0;
        self.epoch = epoch;
        self.blocks = blocks;
        self.large = large;
    }

    pub(crate) fn is_done(&self) -> bool {
        self.unfinished.load(Ordering::Acquire) == 0
    }

    /// 本轮到目前为止扫出的存活字节数（扫完才是全部）。
    pub(crate) fn live(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    /// 还没扫完的项数（估计回收线程数用）。
    pub(crate) fn unfinished(&self) -> usize {
        self.unfinished.load(Ordering::Relaxed)
    }

    /// 领一个没扫的块并扫掉。没有可领的了返回 None。领到的扫完、交出结果后要调 [`Sweeper::finished`]。
    pub(crate) fn sweep_next_block(&self) -> Option<Outcome> {
        let i = self.next_block.fetch_add(1, Ordering::AcqRel);
        let b = *self.blocks.get(i)?;
        // SAFETY: 本轮标记已完成；这个块只分给了这一次领用，扫完前没人分配进去
        let (state, live) = unsafe { b.sweep(self.epoch) };
        self.live.fetch_add(live, Ordering::Relaxed);
        let outcome = match state {
            State::Full => Outcome::Full(b),
            State::Recyclable => Outcome::Usable(b),
            State::Empty if self.take_spare() => Outcome::Usable(b),
            State::Empty => {
                // SAFETY: 全空 ⇒ 没有活对象，死的刚析构完；块只在这里，从此不再使用
                unsafe { b.release() };
                Outcome::Released
            }
        };
        Some(outcome)
    }

    /// 还能留一个全空块作备用就占掉一个名额。
    fn take_spare(&self) -> bool {
        let mut left = self.spare.load(Ordering::Acquire);
        while left > 0 {
            match self.spare.compare_exchange_weak(left, left - 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(now) => left = now,
            }
        }
        false
    }

    /// 一项扫完（交出结果之后调用）。
    pub(crate) fn finished(&self) {
        self.unfinished.fetch_sub(1, Ordering::AcqRel);
    }

    /// 领一个没扫的大对象并扫掉；活着的交给 `sink`。没有可领的了返回 false。
    fn sweep_next_large(&self, sink: &impl Sink) -> bool {
        let i = self.next_large.fetch_add(1, Ordering::AcqRel);
        let Some(&l) = self.large.get(i) else { return false };
        // SAFETY: 本轮标记已完成；这个大对象只分给了这一次领用
        if let Some(l) = unsafe { l.sweep(self.epoch) } {
            self.live.fetch_add(l.header().bytes(), Ordering::Relaxed);
            sink.large(l);
        }
        self.finished();
        true
    }

    /// 回收线程：扫到没有可领的为止。
    fn work(&self, sink: &impl Sink) {
        while let Some(outcome) = self.sweep_next_block() {
            sink.block(outcome);
            self.finished();
        }
        while self.sweep_next_large(sink) {}
    }

    /// 用回收线程把本轮扫完，并等别人（惰性清扫的 mutator）手里领走的也扫完。
    pub(crate) fn finish(&self, pool: &Pool, sink: &(impl Sink + Sync)) {
        if self.is_done() {
            return;
        }
        pool.run(pool.sized(self.unfinished()), |_| self.work(sink));
        let backoff = Backoff::new();
        while !self.is_done() {
            backoff.snooze();
        }
    }
}
