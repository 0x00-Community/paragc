//! 疏散时每个回收线程一个复制缓冲：在目标块的空洞里碰撞分配，把被搬的对象整个抄过去，
//! 新副本标成本轮（含行标记与起点位）。目标块先从「可搬入」队列领（清扫后没再用过的非候选块），没有就开新块。
use core::ptr::null_mut;
use core::sync::atomic::{AtomicUsize, Ordering};

use crossbeam_deque::{Injector, Steal};
use crate::lockfree::SegVec;

use crate::block::Block;
use crate::header::Header;

pub struct Copier {
    /// 可搬入的块（只在本轮回收期间有效）
    targets: *const Injector<Block>,
    /// 全部块表：新开的块登记进去，本轮清扫会扫到
    blocks: *const SegVec<Block>,
    /// 上次完成的纪元：行标记早于它的是空行
    last: u32,
    /// 本轮纪元
    epoch: u32,
    block: Option<Block>,
    line: usize,
    cursor: *mut u8,
    limit: *mut u8,
    /// 本线程搬了多少字节；析构时并进堆的计数
    moved: usize,
    total: *const AtomicUsize,
}

impl Drop for Copier {
    fn drop(&mut self) {
        // SAFETY: 计数器在本轮回收期间有效（Copier 在回收结束前析构）
        unsafe { &*self.total }.fetch_add(self.moved, Ordering::Relaxed);
    }
}

// SAFETY: 指针指向回收期间一直有效的堆结构；每个回收线程独占自己的 Copier
unsafe impl Send for Copier {}

impl Copier {
    pub fn new(targets: &Injector<Block>, blocks: &SegVec<Block>, total: &AtomicUsize, last: u32, epoch: u32) -> Self {
        Self { targets, blocks, last, epoch, block: None, line: 0, cursor: null_mut(), limit: null_mut(), moved: 0, total }
    }

    /// 把 `from` 整个抄到新地方，返回新头。SAFETY（调用方）：本线程认领了 `from`，它是块里的小对象。
    pub unsafe fn copy(&mut self, from: *const Header) -> *const Header {
        // SAFETY: from 有效
        let bytes = unsafe { &*from }.bytes();
        let to = self.alloc(bytes);
        self.moved += bytes;
        // SAFETY: to 是 bytes 字节的新内存；from 归本线程（认领后别人只读头里的标记字）
        unsafe { core::ptr::copy_nonoverlapping(from.cast::<u8>(), to, bytes) };
        let block = Block::containing(to);
        block.set_start(to);
        block.mark_lines(to, bytes, self.epoch);
        let to = to.cast::<Header>().cast_const();
        // SAFETY: 新头已抄好
        unsafe { &*to }.set_mark(self.epoch);
        to
    }

    fn alloc(&mut self, bytes: usize) -> *mut u8 {
        loop {
            if self.limit.addr() - self.cursor.addr() >= bytes {
                let p = self.cursor;
                self.cursor = p.wrapping_add(bytes);
                return p;
            }
            self.refill();
        }
    }

    /// 当前空洞放不下：找当前块的下一个空洞，没有了就换块。
    fn refill(&mut self) {
        if let Some(b) = self.block
            && let Some((start, end)) = b.hole(self.line, self.last)
        {
            self.line = end;
            self.cursor = b.line(start);
            self.limit = b.line(end);
            return;
        }
        // SAFETY: 两个指针在本轮回收期间有效
        let (targets, blocks) = unsafe { (&*self.targets, &*self.blocks) };
        let b = loop {
            match targets.steal() {
                Steal::Success(b) => break b,
                Steal::Retry => {}
                Steal::Empty => {
                    let b = Block::new();
                    blocks.push(b);
                    break b;
                }
            }
        };
        self.block = Some(b);
        self.line = Block::FIRST_LINE;
        self.cursor = null_mut();
        self.limit = null_mut();
    }
}
