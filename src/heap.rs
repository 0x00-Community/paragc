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
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::alloc::{alloc, handle_alloc_error};
use std::sync::{Arc, OnceLock};

use crossbeam_deque::{Injector, Steal};
use crossbeam_utils::{Backoff, CachePadded};

use crate::lockfree::SegVec;

use crate::block::Block;
use crate::gc::{Invariant, RawSlot};
use crate::header::{HEADER, Header, MAX_EPOCH};
use crate::layout::{BLOCK, EVACUATE_LIVE, EVACUATE_MAX, EVACUATE_MIN, GRANULE, GROWTH, LARGE, MIN_SPARE_BLOCKS, MIN_TRIGGER, YOUNG_PER_FULL, YOUNG_PER_FULL_RC};
use crate::mark::Marking;
use crate::roots::RootTable;
use crate::sweep::{Large, Outcome, Sink, Sweeper};
use crate::workers::Pool;
use crate::release::Releaser;
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
    /// 记忆集：上次回收后被写进年轻对象的槽的地址（年轻回收从这些槽出发；任何回收后清空）
    remembered: Injector<usize>,
    /// 引用计数登记：本轮被写过的、计数中的槽（每个槽每轮一次）
    rc_log: Injector<usize>,
    /// 待处理的减量（年轻回收的停顿放进来，sweep_concurrently 在后台做）
    decs: Injector<usize>,
    /// 后台释放过对象的块（下一次停顿开头重算行）
    dirty: Injector<Block>,
    /// 引用计数可信：停顿式完整回收重新计数后为真；并发标记开始后为假（黑分配的对象没计它们的边），直到下一次重新计数
    rc_valid: AtomicBool,
    /// 当前计数的奇偶（重新计数时翻转，旧计数一次作废；并发标记在握手时翻转）
    rc_parity: AtomicBool,
    /// 并发标记期间黑分配的对象（头地址）：它们不会被标记扫描，收尾时补算它们的边
    black: Injector<usize>,
    /// 上一轮计入的根（延迟到这一轮 −1）
    rc_roots: Vec<usize>,
    /// 上次完整回收后做了几次年轻回收
    young_since_full: usize,
    /// 一共做了几次年轻回收
    young_collections: u64,
    /// 回收了几次（分配缓冲据此判断自己是否过期）
    collections: u64,
    /// 全部块
    blocks: SegVec<Block>,
    /// 清扫好、有空行的块（含全空的），分配缓冲先从这里领
    available: Injector<Block>,
    large: SegVec<Large>,
    /// 上一轮的清扫（与 mutator 并发进行）
    sweeper: Sweeper,
    roots: Arc<RootTable>,
    /// 上次回收后新领用的字节数（按块 / 大对象计）
    allocated: AtomicUsize,
    /// 上次回收疏散的块数 / 字节数
    evacuated: (usize, usize),
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
    /// 其中年轻回收几次
    pub young_collections: u64,
    /// 上次回收疏散了几个块、搬了多少字节
    pub evacuated_blocks: usize,
    pub evacuated_bytes: usize,
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
            remembered: Injector::new(),
            rc_log: Injector::new(),
            decs: Injector::new(),
            dirty: Injector::new(),
            rc_valid: AtomicBool::new(false),
            rc_parity: AtomicBool::new(false),
            black: Injector::new(),
            rc_roots: Vec::new(),
            young_since_full: 0,
            young_collections: 0,
            collections: 0,
            blocks: SegVec::default(),
            available: Injector::new(),
            large: SegVec::default(),
            sweeper: Sweeper::default(),
            roots: Arc::default(),
            allocated: AtomicUsize::new(0),
            evacuated: (0, 0),
            spare: AtomicPtr::new(null_mut()),
        }
    }

    pub(crate) fn roots(&self) -> &Arc<RootTable> {
        &self.roots
    }

    /// 堆的身份：根表的地址（堆本身可能被移动）；不会是 [`crate::roots::NO_HOLDER`]。
    pub(crate) fn id(&self) -> usize {
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
        // 上一轮按「纪元 ≠ 本轮」判死，必须在新一轮标记把活对象标成更新的纪元之前扫完
        self.sweep_concurrently();
        let epoch = self.next_epoch();
        // 请求位兼作互斥：谁先挂上谁来开始；已经在标记就撤掉请求返回
        if self.gate.fetch_or(REQUEST, Ordering::AcqRel) & REQUEST != 0 {
            return false;
        }
        if self.marking.load(Ordering::Acquire) != 0 {
            self.gate.fetch_and(!REQUEST, Ordering::AcqRel);
            return false;
        }
        let backoff = Backoff::new();
        while self.gate.load(Ordering::Acquire) & !REQUEST != 0 {
            backoff.snooze();
        }
        // 纪元必须在 mutate 里走空之后才发布：否则先进门、看见「在标记」的 mutator 会黑分配，
        // 而更早进门、没有写屏障的 mutator 还能往这个黑对象底下挂东西，快照之后黑对象不再追踪 ⇒ 漏标
        self.marking.store(epoch, Ordering::Release);
        // 引用计数顺带重新计数：翻转奇偶（旧计数作废），标记扫描时每条边 +1。到收尾补完（黑对象的边、标记期间
        // 被改写的槽）之前计数不可信、不按计数释放
        let parity = !self.rc_parity.fetch_xor(true, Ordering::AcqRel);
        self.rc_valid.store(false, Ordering::Release);
        // 握手前的登记记的是旧奇偶下的「上次计入」：所在对象活着就会被扫描、改成计入当前值；死了的若留到收尾结算，
        // 会拿旧值去减新一轮里别的边计上的数 ⇒ 丢掉（槽上残留的登记标志在扫描重新计入时清掉）
        drop(Self::drain(&self.rc_log));
        // SAFETY: 请求挂着且 mutate 里没人 ⇒ 没有 mutator（根句柄的析构不受影响）
        let roots = unsafe { self.roots.settle() };
        Marking::seed(&self.grey, &roots, epoch);
        self.gate.fetch_and(!REQUEST, Ordering::AcqRel);
        let pool = self.pool();
        Marking::new(pool.threads(), epoch, &self.grey).counting(Some(parity)).logging(&self.rc_log).run(pool);
        true
    }

    /// 并发标记期间被改写过的槽：新值 +1、上次计入的值 −1（扫描可能在写之前或之后计过，两种情况结算后都对）。
    /// 减量只留给本轮活着的对象（死的马上被清扫，碰不得），放进队列由后台处理。SAFETY 见调用处：独占堆。
    fn settle_concurrent_log(&mut self, epoch: u32) {
        let parity = *self.rc_parity.get_mut();
        for addr in Self::drain(&self.rc_log) {
            // SAFETY: 登记的槽在被写时所在对象可达；独占堆、清扫还没开始，内存都还在
            let slot = unsafe { &*core::ptr::with_exposed_provenance::<RawSlot>(addr) };
            let (cur, prev) = slot.recount();
            if let Some(cur) = cur {
                // SAFETY: 槽当前指向的对象还在（清扫没开始）
                unsafe { &*cur }.rc_inc(parity);
            }
            // SAFETY: 同上
            if let Some(prev) = prev
                && unsafe { &*prev }.is_marked(epoch)
            {
                self.decs.push(prev.expose_provenance());
            }
        }
    }

    /// 下一轮的纪元。
    fn next_epoch(&self) -> u32 {
        self.epoch.checked_add(1).filter(|&e| e <= MAX_EPOCH).expect("纪元用完了")
    }

    /// 从清扫后没再用过的可用块里挑疏散候选：存活不到 [`EVACUATE_LIVE`] 字节（且不是全空）的稀疏块，至多
    /// [`EVACUATE_MAX`] 个；够 [`EVACUATE_MIN`] 个才疏散。其余块放进 `targets` 当搬入的目标。返回疏散的块数（0 = 不疏散）。
    fn pick_evacuation(available: &[Block], targets: &Injector<Block>) -> usize {
        let sparse = |b: &Block| b.live_since_sweep().is_some_and(|live| live > 0 && live < EVACUATE_LIVE);
        let count = available.iter().filter(|b| sparse(b)).count().min(EVACUATE_MAX);
        let evacuate = count >= EVACUATE_MIN;
        let mut picked = 0;
        for &b in available {
            if evacuate && picked < count && sparse(&b) {
                b.set_evacuate();
                picked += 1;
            } else {
                targets.push(b);
            }
        }
        picked
    }

    /// 写屏障：并发标记中覆盖掉的旧值若还没标，就标上（连同它跨的行）并放进灰队列（保住初始快照）。
    /// 灰队列里的对象都已标过，取出来只追踪它的边，所以这里必须先标。
    pub(crate) fn remember(&self, old: *const Header, epoch: u32) {
        // SAFETY: old 来自本堆的槽，初始快照时活着，本轮不会被回收
        let h = unsafe { &*old };
        if h.mark(epoch) {
            if !h.is_large() {
                Block::containing(old.cast()).mark_lines(old.cast(), h.bytes(), epoch);
            }
            self.grey.push(old.expose_provenance());
        }
    }

    /// 领一个块给分配缓冲：先拿清扫好的；没有就自己领一个没扫的当场扫（惰性清扫），能用就用；最后开新的。
    fn acquire(&self) -> Block {
        self.allocated.fetch_add(BLOCK, Ordering::Relaxed);
        loop {
            match self.available.steal() {
                Steal::Success(b) => {
                    b.unlist();
                    b.reused();
                    return b;
                }
                Steal::Retry => continue,
                Steal::Empty => {}
            }
            match self.sweeper.sweep_next_block() {
                Some(Outcome::Usable(b)) => {
                    b.reused();
                    self.blocks.push(b);
                    self.sweeper.finished();
                    return b;
                }
                Some(other) => {
                    self.block(other);
                    self.sweeper.finished();
                }
                None => break,
            }
        }
        let b = Block::new();
        self.blocks.push(b);
        b
    }

    /// 上一轮的清扫还没做完（mutator 要块时会顺手扫，或调 [`Heap::sweep_concurrently`]）。
    pub fn is_sweeping(&self) -> bool {
        !self.sweeper.is_done()
    }

    /// 并发清扫：借回收线程池先把引用计数的减量做完（连锁释放），再把上一轮还没扫的块与大对象扫完
    /// （mutator 可以同时跑，也在顺手扫）；返回时上一轮的死对象都已析构。
    pub fn sweep_concurrently(&self) {
        let pool = self.pool();
        if !self.decs.is_empty() {
            Releaser::new(&self.decs, &self.dirty, pool.threads(), self.epoch, self.rc_parity.load(Ordering::Acquire)).run(pool);
        }
        self.sweeper.finish(pool, self);
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
            live: self.sweeper.live(),
            allocated: self.allocated.load(Ordering::Relaxed),
            blocks: self.blocks.len(),
            large: self.large.len(),
            collections: self.collections,
            young_collections: self.young_collections,
            evacuated_blocks: self.evacuated.0,
            evacuated_bytes: self.evacuated.1,
        }
    }

    /// 上次回收后新领用的够多了就回收：平时做年轻回收（死掉的老对象由引用计数收），追踪式的完整回收只为收环、
    /// 计数粘住的对象而少跑——计数可信时每 [`YOUNG_PER_FULL_RC`] 次年轻回收一次；计数不可信（并发标记之后）时
    /// 每 [`YOUNG_PER_FULL`] 次一次，尽快重新计数。回收了返回 true。
    pub fn collect_if_needed(&mut self) -> bool {
        let due = *self.allocated.get_mut() >= self.sweeper.live().max(MIN_TRIGGER) * GROWTH;
        if due {
            let every = if *self.rc_valid.get_mut() { YOUNG_PER_FULL_RC } else { YOUNG_PER_FULL };
            if self.young_since_full < every {
                self.collect_young();
            } else {
                self.collect();
            }
        }
        due
    }

    /// 年轻回收：只追上次回收后分配的对象（标记纪元不是上次完成的那轮），从根与记忆集里的槽出发；碰到老对象就停。
    /// 追到的年轻对象标成上次完成的纪元（晋升为老对象），然后只扫上次回收后被拿去分配过的块：没追到的年轻对象析构、
    /// 行空出来。老对象一个不碰（死掉的老对象留给下一次完整回收）。返回时死掉的年轻对象都已析构。
    ///
    /// 有并发标记在进行时，改为把它收尾（完整回收）。
    pub fn collect_young(&mut self) {
        self.finish_young();
        self.sweep_concurrently();
    }

    /// 年轻回收的停顿部分：标记、结算引用计数的增量；减量与清扫留给之后的 [`Heap::sweep_concurrently`]
    /// （与 mutator 并发；mutator 要块时也会顺手扫）。
    pub fn finish_young(&mut self) {
        if *self.marking.get_mut() != 0 {
            self.finish_marking();
            return;
        }
        self.sweep_concurrently();
        // 上一轮后台释放过对象的块：按还在的老对象重算行（年轻对象的行由下面的追踪标上，死掉的由清扫收）。
        // 被拿去分配过的块里可能有还没扫的死年轻对象（起点位还在）：只重算行、不放回可用队列，交给年轻清扫分类
        let epoch = self.epoch;
        let mut dirty = Self::drain(&self.dirty);
        dirty.sort_unstable_by_key(|b| b.line(0).addr());
        dirty.dedup();
        for b in dirty {
            // SAFETY: 独占；块里起点位标着的对象都还没析构
            if unsafe { b.recompute_lines(epoch) } && b.list() {
                self.available.push(b);
            }
        }
        // SAFETY: &mut self ⇒ 没有 mutator
        let roots = unsafe { self.roots.settle() };
        let epoch = self.epoch;
        let remembered = Self::drain(&self.remembered);
        // 引用计数（可信时）：当前的根 +1、登记过的槽按「新值 +1、上次计入的值 −1」结算，上一轮的根延迟到现在 −1
        let counting = *self.rc_valid.get_mut();
        let parity = *self.rc_parity.get_mut();
        let mut decs = Vec::new();
        if counting {
            for &root in &roots {
                // SAFETY: 根指向活对象
                unsafe { &*root }.rc_inc(parity);
            }
            decs.append(&mut self.rc_roots);
        } else {
            self.rc_roots.clear();
        }
        for addr in Self::drain(&self.rc_log) {
            // SAFETY: 登记的槽在被写时所在对象可达，到这次回收前不会被释放
            let slot = unsafe { &*core::ptr::with_exposed_provenance::<RawSlot>(addr) };
            if counting {
                let (cur, prev) = slot.recount();
                if let Some(cur) = cur {
                    // SAFETY: 槽当前指向活对象
                    unsafe { &*cur }.rc_inc(parity);
                }
                decs.extend(prev.map(<*const Header>::expose_provenance));
            } else {
                slot.unlog();
            }
        }
        Marking::seed(&self.grey, &roots, epoch);
        Marking::seed_slots(&self.grey, &remembered, epoch);
        let pool = self.pool.get_or_init(|| Pool::new(self.threads));
        let workers = pool.sized(self.blocks.len() + self.large.len());
        // 晋升的对象被扫描时，它的每条边 +1（年轻对象的边从晋升起才计数）
        Marking::new(workers, epoch, &self.grey).counting(counting.then_some(parity)).run(pool);
        if counting {
            self.rc_roots = roots.iter().map(|r| r.expose_provenance()).collect();
            // 增量都已加完；减量交给后台（sweep_concurrently）
            for addr in decs {
                self.decs.push(addr);
            }
        }
        // 只扫上次回收后拿去分配过的块（年轻对象只可能在这些块里）；没动过的老块原样放回
        let (young, old): (Vec<Block>, Vec<Block>) = self.blocks.take().into_iter().partition(|b| b.live_since_sweep().is_none());
        self.blocks.extend(old);
        let large = self.large.take();
        let spare = (*self.allocated.get_mut() / BLOCK).max(MIN_SPARE_BLOCKS);
        self.sweeper.start(epoch, young, large, spare);
        self.evacuated = (0, 0);
        *self.allocated.get_mut() = 0;
        self.collections += 1;
        self.young_collections += 1;
        self.young_since_full += 1;
    }

    /// 取空一个队列。
    fn drain<T>(queue: &Injector<T>) -> Vec<T> {
        let mut out = Vec::new();
        loop {
            match queue.steal() {
                Steal::Success(v) => out.push(v),
                Steal::Retry => {}
                Steal::Empty => return out,
            }
        }
    }

    /// 完整回收：[`Heap::finish_marking`] 后立刻把清扫做完（返回时死对象都已析构）。
    pub fn collect(&mut self) {
        self.finish_marking();
        self.sweep_concurrently();
    }

    /// 标记收尾：有并发标记在进行就清空写屏障攒下的灰对象把它标完，否则从全部 [`crate::Root`] 出发并行标记；
    /// 然后把全部块交给清扫器就返回——清扫在之后与 mutator 并发进行（mutator 要块时顺手扫，
    /// 或者某个线程调 [`Heap::sweep_concurrently`]）。停顿只有标记的收尾。
    pub fn finish_marking(&mut self) {
        self.sweep_concurrently();
        // 引用计数顺带重新计数（旧计数作废，扫描时每条边 +1、槽改成计入当前值）：停顿式在这里翻转奇偶；
        // 并发标记已在握手时翻转，扫描已经计了一部分，下面补黑对象的边、结算标记期间被改写的槽
        let concurrent = *self.marking.get_mut() != 0;
        self.begin_full(concurrent);
        let count = Some(*self.rc_parity.get_mut());
        // SAFETY: &mut self ⇒ 没有 mutator
        let roots = unsafe { self.roots.settle() };
        let marking = *self.marking.get_mut();
        let epoch = if marking == 0 { self.next_epoch() } else { marking };
        let pool = self.pool.get_or_init(|| Pool::new(self.threads));
        let workers = pool.sized(self.blocks.len() + self.large.len());
        // 清扫好、没再用过的可用块：不并发标记时从里面挑稀疏的疏散，其余当搬入的目标
        let available = self.take_available();
        let targets = Injector::new();
        let mut evacuate = if marking == 0 { Self::pick_evacuation(&available, &targets) } else { 0 };
        if evacuate > 0 {
            // 别的堆直接指着的对象不能搬：它们所在的块撤掉疏散、改当搬入目标。必须在标记开始前撤，
            // 否则别的工人可能先经堆内的边追到它、把它搬走
            let pin = |h: *const Header| {
                // SAFETY: 入口指着本堆的活对象
                if !unsafe { &*h }.is_large() && Block::containing(h.cast()).pin() {
                    evacuate -= 1;
                    targets.push(Block::containing(h.cast()));
                }
            };
            // SAFETY: 没有 mutator
            unsafe { self.roots.pinned(pin) };
        }
        // 并发标记的黑对象：已标过、没扫描过，放进灰队列让收尾的标记扫描一遍（只为计数）
        for addr in Self::drain(&self.black) {
            self.grey.push(addr);
        }
        let mark = Marking::new(workers, epoch, &self.grey).counting(count);
        let moved = AtomicUsize::new(0);
        if evacuate > 0 {
            mark.evacuating(&self.roots, &targets, &self.blocks, &moved, self.epoch).run(pool);
        } else {
            if marking == 0 {
                Marking::seed(&self.grey, &roots, epoch);
            }
            mark.run(pool);
        }
        if concurrent {
            self.settle_concurrent_log(epoch);
        }
        self.end_full(epoch, (evacuate, moved.into_inner()));
    }

    /// 完整回收开头的簿记（独占）：完整标记重新标行、找到全部活对象，脏块、记忆集不再需要；
    /// 停顿式的在这里翻转计数奇偶（标记前登记的槽：所在对象活着就会被扫描、计入当前值，死了不用管）。
    fn begin_full(&mut self, concurrent: bool) {
        drop(Self::drain(&self.dirty));
        drop(Self::drain(&self.remembered));
        self.young_since_full = 0;
        if !concurrent {
            drop(Self::drain(&self.rc_log));
            let parity = self.rc_parity.get_mut();
            *parity = !*parity;
        }
        self.rc_roots.clear();
    }

    /// 取出全部可用块（完整回收后清扫器会重新分类、放回）。
    fn take_available(&self) -> Vec<Block> {
        let available = Self::drain(&self.available);
        for b in &available {
            b.unlist();
        }
        available
    }

    /// 完整回收收尾（独占）：计数可信了；全部块交给清扫器（疏散时新开的块也已登记），清扫好的可用块之后再一个个放回
    /// available（它们本来就都在 blocks 里）。`evacuated`：疏散的块数与字节数。
    fn end_full(&mut self, epoch: u32, evacuated: (usize, usize)) {
        *self.rc_valid.get_mut() = true;
        let blocks = self.blocks.take();
        let large = self.large.take();
        // 留作备用的全空块：上一轮领用了多少块，下一轮大概还要这么多；多的还给系统。
        // 留少了每轮都要新开块（清零 + 缺页），比如临时对象多的负载每轮几乎全空
        let spare = (*self.allocated.get_mut() / BLOCK).max(MIN_SPARE_BLOCKS);
        self.sweeper.start(epoch, blocks, large, spare);
        self.evacuated = evacuated;
        self.epoch = epoch;
        *self.marking.get_mut() = 0;
        *self.allocated.get_mut() = 0;
        self.collections += 1;
    }

    /// 几个堆一起完整回收，收掉跨堆的环（A 的对象经 [`crate::Far`] 指向 B、B 的又指回 A，单独回收时彼此的入口都是根）。
    ///
    /// 组内堆持有的 Far 不算根：各堆从自己的 [`crate::Root`]（和组外持有的 Far）出发标记，追踪到 Far 就把目标交给目标堆
    /// 接着标，直到不再有新的；没被标到的入口说明持有它的 Far 已经死了，清空。不疏散。返回时死对象都已析构。
    /// 有并发标记在进行的堆先单独收尾。
    pub fn collect_group(heaps: &mut [&mut Heap]) {
        for h in heaps.iter_mut() {
            if *h.marking.get_mut() != 0 {
                h.finish_marking();
            }
            h.sweep_concurrently();
        }
        let ids: Vec<usize> = heaps.iter().map(|h| h.id()).collect();
        let inside = |holder: usize| ids.contains(&holder);
        let mut epochs = Vec::with_capacity(heaps.len());
        for h in heaps.iter_mut() {
            h.begin_full(false);
            drop(h.take_available());
            let epoch = h.next_epoch();
            // SAFETY: &mut ⇒ 没有 mutator
            let roots = unsafe { h.roots.settle_outside(inside) };
            Marking::seed(&h.grey, &roots, epoch);
            epochs.push(epoch);
        }
        let found = Injector::new();
        loop {
            for (h, &epoch) in heaps.iter().zip(&epochs) {
                if h.grey.is_empty() {
                    continue;
                }
                let pool = h.pool();
                let workers = pool.sized(h.blocks.len() + h.large.len());
                Marking::new(workers, epoch, &h.grey).counting(Some(h.rc_parity.load(Ordering::Acquire))).grouping(&found).run(pool);
            }
            for (table, header) in Self::drain(&found) {
                // 组外的堆不在这次回收里，它的入口是自己的根，不用管
                if let Some(j) = ids.iter().position(|&id| id == table) {
                    Marking::seed(&heaps[j].grey, &[core::ptr::with_exposed_provenance(header)], epochs[j]);
                }
            }
            if heaps.iter().all(|h| h.grey.is_empty()) {
                break;
            }
        }
        for (h, &epoch) in heaps.iter_mut().zip(&epochs) {
            // SAFETY: 没有 mutator；还没开始清扫
            unsafe { h.roots.clear_dead(inside, epoch) };
            h.end_full(epoch, (0, 0));
        }
        for h in heaps.iter() {
            h.sweep_concurrently();
        }
    }
}

/// 清扫器扫完一项交回堆：块进全部块表，可用的再放进 available；活着的大对象回到大对象表。
impl Sink for Heap {
    fn block(&self, outcome: Outcome) {
        match outcome {
            Outcome::Usable(b) => {
                self.blocks.push(b);
                if b.list() {
                    self.available.push(b);
                }
            }
            Outcome::Full(b) => {
                self.blocks.push(b);
            }
            Outcome::Released => {}
        }
    }

    fn large(&self, survivor: Large) {
        self.large.push(survivor);
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        // 从没回收过就不为析构专门起线程
        let pool = self.pool.take().unwrap_or_else(|| Pool::new(NonZeroUsize::MIN));
        // 先把上一轮扫完，再用一个比谁都新的纪元（并发标记中的也算死）不标记直接扫一轮：全部析构，全空块不留备用全部还回去
        self.sweeper.finish(&pool, self);
        let epoch = self.epoch.max(*self.marking.get_mut()).wrapping_add(1);
        let blocks = self.blocks.take();
        let large = self.large.take();
        while !matches!(self.available.steal(), Steal::Empty) {}
        self.sweeper.start(epoch, blocks, large, 0);
        self.sweeper.finish(&pool, self);
        debug_assert!(self.blocks.is_empty() && self.large.is_empty());
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

/// 新对象的标记：不等于任何纪元（纪元从 1 开始）⇒ 年轻
const YOUNG: u32 = 0;

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

    /// 这次 mutate 是否落在并发标记期间（写屏障与黑分配生效）。
    pub fn is_marking(&self) -> bool {
        self.marking.is_some()
    }

    /// 写屏障。并发标记中：被覆盖的旧值若还没标就标上、进灰队列（初始快照）。
    /// 平时：写进去的是年轻对象（标记纪元不是上次完成的那轮）就记下槽的地址——老对象指向年轻对象的边只能这样产生，
    /// 年轻回收从这些槽出发。（并发标记期间新对象都是黑的、收尾后全是老对象，不用记。）
    ///
    /// 计数中的槽（老对象里的；年轻对象的槽不计数）本轮第一次被写还要登记，回收时结算。
    pub(crate) fn written(&self, slot: &RawSlot, old: Option<*const Header>, new: Option<*const Header>) {
        let addr = (&raw const *slot).expose_provenance();
        if let Some(epoch) = self.marking {
            if let Some(old) = old {
                self.heap.remember(old, epoch.get());
            }
        } else if let Some(new) = new
            // SAFETY: new 是刚写进槽的、活着的对象
            && !unsafe { &*new }.is_marked(self.heap.epoch)
        {
            self.heap.remembered.push(addr);
        }
        // 引用计数登记：平时与并发标记期间都要（标记期间被改写的槽在收尾时结算）
        if slot.log() {
            self.heap.rc_log.push(addr);
        }
    }

    /// 分配一个对象。
    pub fn alloc<T: Trace + 'gc>(&self, value: T) -> Gc<'gc, T> {
        const { assert!(align_of::<T>() <= GRANULE, "对象数据的对齐不能超过 8 字节") };
        let bytes = (HEADER + size_of::<T>()).next_multiple_of(GRANULE);
        let large = bytes > LARGE;
        let p = if large { self.heap.alloc_large(bytes) } else { self.alloc_small(bytes) };
        // 平时：年轻（标记 0 ≠ 上次完成的纪元；在交出去前就扫过的块或新块里，进行中的清扫碰不到它）。
        // 并发标记中：黑分配（直接标成本轮，连同它占的行）
        let mark = self.marking.map_or(YOUNG, NonZeroU32::get);
        if self.marking.is_some() {
            if !large {
                Block::containing(p).mark_lines(p, bytes, mark);
            }
            // 黑对象不会被标记扫描：记下来，收尾时补算它的边（引用计数）
            self.heap.black.push(p.expose_provenance());
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
