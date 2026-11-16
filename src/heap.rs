//! 堆：多个线程可以同时 [`Heap::mutate`]（各自碰撞分配）。
//!
//! 回收两种走法：
//! - [`Heap::collect`]（`&mut self`）：借用规则排除 mutator，标记 + 清扫全在这一次里
//! - [`Heap::mark_concurrently`]（`&self`）：开头一次短握手（等 mutate 里的线程走空、照下根），之后 mutator 照常跑、
//!   回收线程并发标记；mutator 那边写 `Slot` 走初始快照（SATB）写屏障、新对象直接标成本轮（黑分配）。
//!   之后的 [`Heap::collect`] 只需清空写屏障攒下的灰对象再清扫
//!
//! 握手字：最高位是「要开始标记」请求，低位是 mutate 里的线程数。请求挂着时新进 mutate 的线程等它清掉。
use core::cell::Cell;
use core::marker::PhantomData;
use core::num::{NonZeroU32, NonZeroUsize};
use core::ptr::{NonNull, null_mut};
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::alloc::{alloc, handle_alloc_error};
use std::sync::{Arc, OnceLock};

use crossbeam_deque::Injector;
use crossbeam_utils::{Backoff, CachePadded};

use j8lockfree::{SegVec, Stock};

use crate::block::Block;
use crate::gc::Invariant;
use crate::header::{HEADER, Header};
use crate::layout::{BLOCK, GRANULE, GROWTH, LARGE, MIN_SPARE_BLOCKS, MIN_TRIGGER, SPARE_DIVISOR};
use crate::mark::Marking;
use crate::roots::RootTable;
use crate::sweep::{Large, Sweep, Swept};
use crate::workers::Pool;
use crate::{Gc, Trace};

pub struct Heap {
    /// 回收最多用几个线程
    threads: NonZeroUsize,
    /// 回收线程池：第一次回收时才起线程
    pool: OnceLock<Pool>,
    /// 最后一次完成的标记的纪元（从 1 开始；行标记早于它的行是空行）
    epoch: u32,
    /// 正在并发标记的纪元；0 = 没有
    marking: AtomicU32,
    /// 握手字：[`REQUEST`] 位 + mutate 里的线程数
    gate: CachePadded<AtomicU64>,
    /// 灰队列：根与写屏障记下的旧值（并发标记跨越多次调用，所以放在堆里）
    grey: Injector<usize>,
    /// 回收了几次（分配缓冲据此判断自己是否过期）
    collections: u64,
    /// 全部块
    blocks: SegVec<Block>,
    /// 上次回收后还有空行的块
    recyclable: Stock<Block>,
    /// 全空的块
    empty: Stock<Block>,
    large: SegVec<Large>,
    roots: Arc<RootTable>,
    /// 上次回收后新领用的字节数（按块 / 大对象计）
    allocated: AtomicUsize,
    /// 上次回收后存活的字节数
    live: usize,
    /// 给 [`Heap::mutate`] 复用的分配缓冲
    spare: AtomicPtr<Tlab>,
}

/// 堆的统计。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// 上次回收后存活的字节数（含头）
    pub live: usize,
    /// 上次回收后新领用的字节数
    pub allocated: usize,
    pub blocks: usize,
    pub large: usize,
    pub collections: u64,
}

impl Default for Heap {
    fn default() -> Self {
        Self::new(std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN))
    }
}

impl Heap {
    /// 回收最多用 `threads` 个线程。
    pub fn new(threads: NonZeroUsize) -> Self {
        Self {
            threads,
            pool: OnceLock::new(),
            epoch: 1,
            marking: AtomicU32::new(0),
            gate: CachePadded::new(AtomicU64::new(0)),
            grey: Injector::new(),
            collections: 0,
            blocks: SegVec::default(),
            recyclable: Stock::default(),
            empty: Stock::default(),
            large: SegVec::default(),
            roots: Arc::default(),
            allocated: AtomicUsize::new(0),
            live: 0,
            spare: AtomicPtr::new(null_mut()),
        }
    }

    pub(crate) fn roots(&self) -> &Arc<RootTable> {
        &self.roots
    }

    /// 堆的身份：根表的地址（堆本身可能被移动）。
    fn id(&self) -> usize {
        Arc::as_ptr(&self.roots).addr()
    }

    /// 在堆上做事；可以在多个线程上同时调用。闭包里分配的对象只有挂到 [`crate::Root`] 上（直接或间接）才能活过下一次回收。
    pub fn mutate<R>(&self, f: impl for<'gc> FnOnce(&Mutator<'gc>) -> R) -> R {
        let spare = self.spare.swap(null_mut(), Ordering::AcqRel);
        let mut tlab = if spare.is_null() {
            Box::default()
        } else {
            // SAFETY: spare 里只放过 Box::into_raw 的 Tlab，swap 后归我们独有
            unsafe { Box::from_raw(spare) }
        };
        let r = self.mutate_in(&mut tlab, f);
        let tlab = Box::into_raw(tlab);
        if self.spare.compare_exchange(null_mut(), tlab, Ordering::AcqRel, Ordering::Relaxed).is_err() {
            // SAFETY: 没放进去，仍归我们独有
            drop(unsafe { Box::from_raw(tlab) });
        }
        r
    }

    /// 同 [`Heap::mutate`]，但用调用方自己的分配缓冲（一个线程反复 mutate 时留着它，省得每次领新块）。
    pub fn mutate_in<R>(&self, tlab: &mut Tlab, f: impl for<'gc> FnOnce(&Mutator<'gc>) -> R) -> R {
        let _inside = Inside::enter(self);
        tlab.adopt(self);
        // 并发标记只在没有 mutator 时开始、只在独占堆时结束，所以一次 mutate 里它不变
        let marking = NonZeroU32::new(self.marking.load(Ordering::Acquire));
        f(&Mutator { heap: self, tlab, marking, _brand: PhantomData })
    }

    fn pool(&self) -> &Pool {
        self.pool.get_or_init(|| Pool::new(self.threads))
    }

    /// 并发标记：开头短暂握手（等 mutate 里的线程走空、照下根），之后其他线程照常 mutate，
    /// 本线程借回收线程池把从根可达的对象标完（写屏障新攒下的留给下一次 [`Heap::collect`]）。
    /// 已经有一轮在标记时什么都不做，返回 false。
    ///
    /// 不要在这个堆的 mutate 里调用（会一直等自己走空）。
    pub fn mark_concurrently(&self) -> bool {
        let epoch = self.epoch.checked_add(1).expect("纪元用完了");
        if self.marking.compare_exchange(0, epoch, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return false;
        }
        self.gate.fetch_or(REQUEST, Ordering::AcqRel);
        let backoff = Backoff::new();
        while self.gate.load(Ordering::Acquire) & !REQUEST != 0 {
            backoff.snooze();
        }
        // SAFETY: 请求挂着且 mutate 里没人 ⇒ 没有 mutator（根句柄的析构不受影响）
        let roots = unsafe { self.roots.settle() };
        Marking::seed(&self.grey, &roots, epoch);
        self.gate.fetch_and(!REQUEST, Ordering::AcqRel);
        let pool = self.pool();
        Marking::new(pool.threads(), epoch, &self.grey).run(pool);
        true
    }

    /// 写屏障：并发标记中覆盖掉的旧值若还没标，放进灰队列（保住初始快照）。
    pub(crate) fn remember(&self, old: *const Header, epoch: u32) {
        // SAFETY: old 来自本堆的槽，初始快照时活着，本轮不会被回收
        if !unsafe { &*old }.is_marked(epoch) {
            self.grey.push(old.expose_provenance());
        }
    }

    /// 领一个块给分配缓冲：先用有空行的，再用全空的，最后开新的。
    fn acquire(&self) -> Block {
        self.allocated.fetch_add(BLOCK, Ordering::Relaxed);
        self.recyclable.claim_one().or_else(|| self.empty.claim_one()).unwrap_or_else(|| {
            let b = Block::new();
            self.blocks.push(b);
            b
        })
    }

    fn alloc_large(&self, bytes: usize) -> *mut u8 {
        let layout = Large::layout(bytes);
        // SAFETY: 大小非零（至少有头）
        let p = NonNull::new(unsafe { alloc(layout) }).unwrap_or_else(|| handle_alloc_error(layout));
        self.large.push(Large(p.cast()));
        self.allocated.fetch_add(bytes, Ordering::Relaxed);
        p.as_ptr()
    }

    pub fn stats(&self) -> Stats {
        Stats {
            live: self.live,
            allocated: self.allocated.load(Ordering::Relaxed),
            blocks: self.blocks.len(),
            large: self.large.len(),
            collections: self.collections,
        }
    }

    /// 上次回收后新领用的够多了就回收；回收了返回 true。
    pub fn collect_if_needed(&mut self) -> bool {
        let due = *self.allocated.get_mut() >= self.live.max(MIN_TRIGGER) * GROWTH;
        if due {
            self.collect();
        }
        due
    }

    /// 回收。有并发标记在进行时：清空写屏障攒下的灰对象把它标完；否则从全部 [`crate::Root`] 出发并行标记。
    /// 然后并行清扫（析构死对象、整理块）。
    pub fn collect(&mut self) {
        // SAFETY: &mut self ⇒ 没有 mutator
        let roots = unsafe { self.roots.settle() };
        let marking = *self.marking.get_mut();
        let epoch = if marking == 0 { self.epoch.checked_add(1).expect("纪元用完了") } else { marking };
        if marking == 0 {
            Marking::seed(&self.grey, &roots, epoch);
        }
        let blocks = self.blocks.take();
        let large = self.large.take();
        let pool = self.pool.get_or_init(|| Pool::new(self.threads));
        let workers = pool.sized(blocks.len() + large.len());
        Marking::new(workers, epoch, &self.grey).run(pool);
        // SAFETY: 标记完了，没有 mutator
        let swept = unsafe { Sweep::new(&blocks, &large, epoch).run(pool, workers) };
        self.epoch = epoch;
        *self.marking.get_mut() = 0;
        self.rebuild(swept);
    }

    fn rebuild(&mut self, swept: Swept) {
        let Swept { full, recyclable, mut empty, large, live } = swept;
        let total = full.len() + recyclable.len() + empty.len();
        let keep = (total / SPARE_DIVISOR).max(MIN_SPARE_BLOCKS).min(empty.len());
        for b in empty.drain(keep..) {
            // SAFETY: 全空的块里没有活对象，从此不再使用
            unsafe { b.release() };
        }
        self.blocks.extend(full.iter().chain(&recyclable).chain(&empty).copied());
        self.recyclable.clear();
        self.recyclable.extend(recyclable);
        self.empty.clear();
        self.empty.extend(empty);
        self.large.extend(large);
        self.live = live;
        *self.allocated.get_mut() = 0;
        self.collections += 1;
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        // 不标记直接清扫：全部对象都析构（取一个比谁都新的纪元：并发标记中的也算死）
        let epoch = self.epoch.max(*self.marking.get_mut()).wrapping_add(1);
        let blocks = self.blocks.take();
        let large = self.large.take();
        // 从没回收过就不为析构专门起线程
        let pool = self.pool.take().unwrap_or_else(|| Pool::new(NonZeroUsize::MIN));
        let workers = pool.sized(blocks.len() + large.len());
        // SAFETY: 独占；这一轮谁都没标记，全部视为死
        let swept = unsafe { Sweep::new(&blocks, &large, epoch).run(&pool, workers) };
        for b in blocks {
            // SAFETY: 对象都析构过了
            unsafe { b.release() };
        }
        debug_assert!(swept.large.is_empty());
        let spare = *self.spare.get_mut();
        if !spare.is_null() {
            // SAFETY: Box::into_raw 来的
            drop(unsafe { Box::from_raw(spare) });
        }
    }
}

/// 线程的分配缓冲：当前块、在块里找到哪一行、当前空洞的游标与界。块在两次回收之间只归它。
pub struct Tlab {
    /// (所属堆, 那时的回收次数)；对不上就作废
    owner: Cell<(usize, u64)>,
    block: Cell<Option<Block>>,
    /// 下一次找空洞从哪一行开始
    line: Cell<usize>,
    cursor: Cell<*mut u8>,
    limit: Cell<*mut u8>,
}

impl Default for Tlab {
    fn default() -> Self {
        Self {
            owner: Cell::default(),
            block: Cell::default(),
            line: Cell::default(),
            cursor: Cell::new(null_mut()),
            limit: Cell::new(null_mut()),
        }
    }
}

// SAFETY: 指针指向它独占的块；移到别的线程后由那个线程独占使用
unsafe impl Send for Tlab {}

impl Tlab {
    /// 换了堆或堆回收过：之前的块已经被清扫重新分配，丢掉。
    fn adopt(&mut self, heap: &Heap) {
        let owner = (heap.id(), heap.collections);
        if self.owner.get() != owner {
            *self = Self { owner: Cell::new(owner), ..Self::default() };
        }
    }
}

/// 握手字的最高位：要开始并发标记，新的 mutate 先等
const REQUEST: u64 = 1 << 63;

/// 在 mutate 里：进门计数加一（有开始标记的请求就先等），出门减一（panic 也减）。
struct Inside<'h>(&'h Heap);

impl<'h> Inside<'h> {
    fn enter(heap: &'h Heap) -> Self {
        let backoff = Backoff::new();
        let mut word = heap.gate.load(Ordering::Acquire);
        loop {
            if word & REQUEST != 0 {
                backoff.snooze();
                word = heap.gate.load(Ordering::Acquire);
                continue;
            }
            match heap.gate.compare_exchange_weak(word, word + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Self(heap),
                Err(now) => word = now,
            }
        }
    }
}

impl Drop for Inside<'_> {
    fn drop(&mut self) {
        self.0.gate.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 一次 mutate 里的分配器与品牌；只在调用线程上用。
pub struct Mutator<'gc> {
    heap: &'gc Heap,
    tlab: &'gc Tlab,
    /// 进 mutate 时正在并发标记的纪元：写屏障与黑分配用
    marking: Option<NonZeroU32>,
    _brand: Invariant<'gc>,
}

impl<'gc> Mutator<'gc> {
    pub(crate) fn heap(&self) -> &'gc Heap {
        self.heap
    }

    /// 写屏障：槽里被覆盖掉的旧值（并发标记中才有事）。
    pub(crate) fn overwritten(&self, old: *const Header) {
        if let Some(epoch) = self.marking
            && !old.is_null()
        {
            self.heap.remember(old, epoch.get());
        }
    }

    /// 分配一个对象。
    pub fn alloc<T: Trace + 'gc>(&self, value: T) -> Gc<'gc, T> {
        const { assert!(align_of::<T>() <= GRANULE, "对象数据的对齐不能超过 8 字节") };
        let bytes = (HEADER + size_of::<T>()).next_multiple_of(GRANULE);
        let large = bytes > LARGE;
        let p = if large { self.heap.alloc_large(bytes) } else { self.alloc_small(bytes) };
        // 并发标记中：黑分配（直接标成本轮，连同它占的行）
        let mark = self.marking.map_or(0, NonZeroU32::get);
        if mark != 0 && !large {
            Block::containing(p).mark_lines(p, bytes, mark);
        }
        // SAFETY: p 是 bytes 字节、按粒度对齐的新内存，归这次分配独有
        unsafe {
            p.cast::<Header>().write(Header::new::<T>(bytes, large, mark));
            let data = Header::data(p.cast()).cast::<T>();
            data.write(value);
            Gc::from_data(data)
        }
    }

    fn alloc_small(&self, bytes: usize) -> *mut u8 {
        let t = self.tlab;
        loop {
            let cursor = t.cursor.get();
            if t.limit.get().addr() - cursor.addr() >= bytes {
                t.cursor.set(cursor.wrapping_add(bytes));
                Block::containing(cursor).set_start(cursor);
                return cursor;
            }
            self.refill();
        }
    }

    /// 当前空洞放不下：找当前块的下一个空洞，没有了就领新块。
    fn refill(&self) {
        let t = self.tlab;
        if let Some(b) = t.block.get()
            && let Some((start, end)) = b.hole(t.line.get(), self.heap.epoch)
        {
            t.line.set(end);
            t.cursor.set(b.line(start));
            t.limit.set(b.line(end));
            return;
        }
        t.block.set(Some(self.heap.acquire()));
        t.line.set(Block::FIRST_LINE);
        t.cursor.set(null_mut());
        t.limit.set(null_mut());
    }
}
