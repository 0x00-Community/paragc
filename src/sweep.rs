//! 并行清扫：工人按批领块 / 大对象，析构死对象、给块分类；结果各自攒着，最后合并。
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::alloc::{Layout, dealloc};

use crate::block::{Block, State};
use crate::header::Header;
use crate::layout::{GRANULE, SWEEP_CHUNK};
use crate::workers::Workers;

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
    /// SAFETY（调用方）：标记已经做完，没有 mutator。
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

#[derive(Default)]
pub(crate) struct Swept {
    pub(crate) full: Vec<Block>,
    pub(crate) recyclable: Vec<Block>,
    pub(crate) empty: Vec<Block>,
    pub(crate) large: Vec<Large>,
    /// 存活字节数（含头）
    pub(crate) live: usize,
}

impl Swept {
    fn merge(mut self, other: Self) -> Self {
        self.full.extend(other.full);
        self.recyclable.extend(other.recyclable);
        self.empty.extend(other.empty);
        self.large.extend(other.large);
        self.live += other.live;
        self
    }
}

pub(crate) struct Sweep<'a> {
    blocks: &'a [Block],
    large: &'a [Large],
    next_block: AtomicUsize,
    next_large: AtomicUsize,
    epoch: u32,
}

impl<'a> Sweep<'a> {
    pub(crate) fn new(blocks: &'a [Block], large: &'a [Large], epoch: u32) -> Self {
        Self { blocks, large, next_block: AtomicUsize::new(0), next_large: AtomicUsize::new(0), epoch }
    }

    /// SAFETY（调用方）：标记已经做完，没有 mutator。
    pub(crate) unsafe fn run(&self, workers: Workers) -> Swept {
        // SAFETY: 见上
        workers.run(|_| unsafe { self.work() }).into_iter().fold(Swept::default(), Swept::merge)
    }

    /// 领一批下标。
    fn claim<'s, T>(cursor: &AtomicUsize, all: &'s [T]) -> &'s [T] {
        let start = cursor.fetch_add(SWEEP_CHUNK, Ordering::Relaxed).min(all.len());
        &all[start..(start + SWEEP_CHUNK).min(all.len())]
    }

    unsafe fn work(&self) -> Swept {
        let mut out = Swept::default();
        loop {
            let batch = Self::claim(&self.next_block, self.blocks);
            if batch.is_empty() {
                break;
            }
            for &b in batch {
                // SAFETY: 调用方担保
                let (state, live) = unsafe { b.sweep(self.epoch) };
                out.live += live;
                match state {
                    State::Empty => out.empty.push(b),
                    State::Recyclable => out.recyclable.push(b),
                    State::Full => out.full.push(b),
                }
            }
        }
        loop {
            let batch = Self::claim(&self.next_large, self.large);
            if batch.is_empty() {
                break;
            }
            for &l in batch {
                // SAFETY: 调用方担保
                if let Some(l) = unsafe { l.sweep(self.epoch) } {
                    out.live += l.header().bytes();
                    out.large.push(l);
                }
            }
        }
        out
    }
}
