//! 块：按自身大小对齐的一段内存，开头是元数据（行标记 + 对象起点位图），后面按行切给小对象。
//!
//! - 行标记存纪元：标记阶段每个活对象把自己跨的行写成本轮纪元，于是「行标记 ≠ 当前纪元」就是空行
//! - 起点位图每粒度一位：清扫时据此逐个找到对象，析构死掉的
//! - 一个块在两次回收之间只归一个分配缓冲（[`crate::Tlab`]）所有，所以分配时写元数据没有竞争
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};

use crate::header::Header;
use crate::layout::{BITS, BLOCK, GRANULE, GRANULES, LINE, LINES};

#[repr(C)]
struct Meta {
    /// 每行最后一次被活对象占用时的纪元
    lines: [AtomicU32; LINES],
    /// 对象起点：每粒度一位
    starts: [AtomicU64; GRANULES / BITS],
    /// 上次清扫时的存活字节数；交给分配缓冲（或刚开出来）后是 [`REUSED`]（不知道，不当疏散候选）
    live: AtomicU32,
    /// 这一轮要疏散：里面的活对象被追踪到时搬走
    evacuate: AtomicBool,
}

/// [`Meta::live`]：块被重新拿去分配了，上次清扫的存活量不再作数
const REUSED: u32 = u32::MAX;

/// 清扫一个块的结果。
pub(crate) enum State {
    /// 没有活对象
    Empty,
    /// 还有空行
    Recyclable,
    /// 没有空行
    Full,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Block(NonNull<Meta>);

// SAFETY: 块是一段独立的堆内存；元数据都是原子量，对象数据的跨线程访问由 Trace: Send + Sync 担保
unsafe impl Send for Block {}
// SAFETY: 同上
unsafe impl Sync for Block {}

impl Block {
    const LAYOUT: Layout = match Layout::from_size_align(BLOCK, BLOCK) {
        Ok(layout) => layout,
        Err(_) => panic!("块的大小 / 对齐不合法"),
    };
    /// 元数据之后第一个能放对象的行
    pub(crate) const FIRST_LINE: usize = size_of::<Meta>().div_ceil(LINE);

    /// 新块：全零（行标记 0 早于任何纪元，全部是空行）。
    pub(crate) fn new() -> Self {
        // SAFETY: LAYOUT 大小非零
        let p = unsafe { alloc_zeroed(Self::LAYOUT) };
        let b = Self(NonNull::new(p.cast()).unwrap_or_else(|| handle_alloc_error(Self::LAYOUT)));
        b.reused();
        b
    }

    /// 交给分配缓冲：上次清扫的存活量不再作数。
    pub(crate) fn reused(self) {
        self.meta().live.store(REUSED, Ordering::Relaxed);
    }

    /// 上次清扫后没再分配过时的存活字节数。
    pub(crate) fn live_since_sweep(self) -> Option<usize> {
        let live = self.meta().live.load(Ordering::Relaxed);
        (live != REUSED).then_some(live as usize)
    }

    pub(crate) fn set_evacuate(self) {
        self.meta().evacuate.store(true, Ordering::Relaxed);
    }

    pub(crate) fn is_evacuating(self) -> bool {
        self.meta().evacuate.load(Ordering::Relaxed)
    }

    /// 还给系统。SAFETY（调用方）：块里没有活对象（死的已析构），之后不再使用。
    pub(crate) unsafe fn release(self) {
        // SAFETY: 见上；指针来自 new
        unsafe { dealloc(self.0.as_ptr().cast(), Self::LAYOUT) };
    }

    /// 地址所在的块（地址必须在某个块里）。
    pub(crate) fn containing(p: *const u8) -> Self {
        let base = p.map_addr(|a| a & !(BLOCK - 1)).cast_mut().cast::<Meta>();
        // SAFETY: 块起点不会是 0
        Self(unsafe { NonNull::new_unchecked(base) })
    }

    fn meta(&self) -> &Meta {
        // SAFETY: 块活着时元数据一直有效
        unsafe { self.0.as_ref() }
    }

    fn base(self) -> *mut u8 {
        self.0.as_ptr().cast()
    }

    /// 第 `i` 行的起点（`i == LINES` 是块尾）。
    pub(crate) fn line(self, i: usize) -> *mut u8 {
        self.base().wrapping_add(i * LINE)
    }

    fn offset(self, p: *const u8) -> usize {
        p.addr() - self.base().addr()
    }

    /// 标记 `[p, p + bytes)` 跨过的行。
    pub(crate) fn mark_lines(self, p: *const u8, bytes: usize, epoch: u32) {
        let first = self.offset(p) / LINE;
        let last = (self.offset(p) + bytes - 1) / LINE;
        for line in &self.meta().lines[first..=last] {
            line.store(epoch, Ordering::Relaxed);
        }
    }

    /// 记下 `p` 处有一个对象。
    pub(crate) fn set_start(self, p: *const u8) {
        let g = self.offset(p) / GRANULE;
        self.meta().starts[g / BITS].fetch_or(1 << (g % BITS), Ordering::Relaxed);
    }

    /// 从第 `from` 行起的第一个空洞（一段连续空行），返回 `[起, 止)` 行号。
    /// 空行 = 行标记早于上次完成的纪元 `epoch`：并发标记期间标记方会把活行写成更新的纪元，那些也不是空行。
    pub(crate) fn hole(self, from: usize, epoch: u32) -> Option<(usize, usize)> {
        let lines = &self.meta().lines;
        let free = |i: usize| lines[i].load(Ordering::Relaxed) < epoch;
        let start = (from..LINES).find(|&i| free(i))?;
        let end = (start..LINES).find(|&i| !free(i)).unwrap_or(LINES);
        Some((start, end))
    }

    /// 清扫：析构没标上本轮纪元的对象（搬走了的不析构）、清掉它们的起点位，返回块的状态与存活字节数。
    /// SAFETY（调用方）：标记已经做完，没有 mutator 在用这个块。
    pub(crate) unsafe fn sweep(self, epoch: u32) -> (State, usize) {
        let mut live = 0;
        for (w, word) in self.meta().starts.iter().enumerate() {
            let mut bits = word.load(Ordering::Relaxed);
            let mut dead = 0u64;
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let header = self.base().wrapping_add((w * BITS + bit) * GRANULE).cast::<Header>().cast_const();
                // SAFETY: 起点位标着的地方都是已初始化的对象
                let h = unsafe { &*header };
                if h.is_forwarded() {
                    // 活着的是搬过去的新副本；旧的只清起点位
                    dead |= 1 << bit;
                } else if h.is_marked(epoch) {
                    live += h.bytes();
                } else {
                    // SAFETY: 没标记 ⇒ 从根不可达，之后没人再用
                    unsafe { Header::finalize(header) };
                    dead |= 1 << bit;
                }
            }
            if dead != 0 {
                word.fetch_and(!dead, Ordering::Relaxed);
            }
        }
        let lines = &self.meta().lines[Self::FIRST_LINE..];
        let used = lines.iter().filter(|l| l.load(Ordering::Relaxed) == epoch).count();
        let state = match used {
            0 => State::Empty,
            n if n == lines.len() => State::Full,
            _ => State::Recyclable,
        };
        self.meta().live.store(u32::try_from(live).unwrap_or(REUSED - 1), Ordering::Relaxed);
        self.meta().evacuate.store(false, Ordering::Relaxed);
        (state, live)
    }
}
