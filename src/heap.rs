//! 两种堆：私有堆 [`Heap`] / [`Mutator`]，公开堆 [`PublicHeap`] / [`PublicMutator`]（共享内存文件上、多进程），共用的分配缓冲 [`Tlab`]。
//!
//! 堆：多个线程可以同时 [`Heap::mutate`]（各自碰撞分配）。
//!
//! 回收两种走法：
//! - [`Heap::collect`]（`&mut self`）：借用规则排除 mutator，标记 + 清扫全在这一次里
//! - [`Heap::mark_concurrently`]（`&self`）：开头一次短握手（等 mutate 里的线程走空、照下根），之后 mutator 照常跑、
//!   回收线程并发标记；mutator 那边写 `Slot` 走初始快照（SATB）写屏障、新对象直接标成本轮（黑分配）。
//!   之后的 [`Heap::collect`] 只需清空写屏障攒下的灰对象再清扫
//!
//! 握手字：最高位是「要开始标记」请求，低位是 mutate 里的线程数。请求挂着时新进 mutate 的线程等它清掉。
//!
//! 公开堆：共享内存文件上的堆。堆的全部状态（超级块、类型表、入口表、块状态表、块）都在窗口里，进程这一侧只有缓存。
//!
//! 第一期（单进程）：多个线程同时 mutate、碰撞分配；回收是暂停式的并行标记 + 并行清扫，不搬对象。握手字在超级块里，
//! 所以同一个堆被映射两次（同一进程里两个地址区，或以后跨进程）也互相排斥。
#[cfg(target_os = "linux")]
use core::any::type_name;
use core::cell::Cell;
use core::marker::PhantomData;
use core::num::{NonZeroU32, NonZeroUsize};
use core::ptr::{NonNull, null_mut};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
#[cfg(target_os = "linux")]
use core::time::Duration;
#[cfg(target_os = "linux")]
use std::io;
use std::alloc::{alloc, handle_alloc_error};
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crossbeam_deque::{Injector, Steal};
use crossbeam_utils::{Backoff, CachePadded};

use crate::{Gc, Trace};
use crate::handle::{GcInternal, Invariant, RawSlot, RootTable};
#[cfg(target_os = "linux")]
use crate::handle::{EdgeMap, Public, PublicBrand, PublicGc, PublicGcInternal, PublicInternal, Shared};
use crate::layout::{BLOCK, Block, EVACUATE_LIVE, EVACUATE_MAX, EVACUATE_MIN, GRANULE, GROWTH, HEADER, Header, LARGE, MAX_EPOCH, MIN_SPARE_BLOCKS, MIN_TRIGGER, YOUNG_PER_FULL, YOUNG_PER_FULL_RC};
#[cfg(target_os = "linux")]
use crate::layout::{Core, State};
#[cfg(target_os = "linux")]
use crate::layout::{ATTACH_CAP, ATTACH_OFF, Attachment, BlockState, Entry, Kind, MAP_CAP, PAGE, PHEADER, PHeader, Segment, Super, TYPE_CAP, TypeEntry, WindowLayout};
use crate::mark::Marking;
#[cfg(target_os = "linux")]
use crate::mark::{Marker, Member};
#[cfg(target_os = "linux")]
use crate::shm::{Liveness, Process, Region, Registry, Space, WINDOW, WINDOWS};
use crate::sweep::{Large, Outcome, Releaser, Sink, Sweeper};
use crate::sync::{Pool, SegVec};
#[cfg(target_os = "linux")]
use crate::sync::{FutexWord, Retry, Stack};

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
        tlab.adopt((self.id(), self.collections));
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
    fn remember(&self, old: *const Header, epoch: u32) {
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

/// 只给本 crate 用的操作：trait 在私有模块里、不导出，crate 外拿不到。
pub trait HeapInternal {
    fn roots(&self) -> &Arc<RootTable>;

    /// 堆的身份：根表的地址（堆本身可能被移动）；不会是 [`crate::handle::NO_HOLDER`]。
    fn id(&self) -> usize;
}

impl HeapInternal for Heap {
    fn roots(&self) -> &Arc<RootTable> {
        &self.roots
    }

    fn id(&self) -> usize {
        Arc::as_ptr(&self.roots).addr()
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
    /// 公开堆：上一次分配的（类型名的地址, 类型号）
    last: Cell<Option<(usize, u32)>>,
}

impl Default for Tlab {
    fn default() -> Self {
        Self { owner: Cell::default(), block: Cell::default(), line: Cell::default(), cursor: Cell::new(null_mut()), limit: Cell::new(null_mut()), last: Cell::default() }
    }
}

// SAFETY: 指针指向它独占的块；移到别的线程后由那个线程独占使用
unsafe impl Send for Tlab {}

impl Tlab {
    /// 换了堆或堆回收过（`owner` = (堆, 回收次数)）：之前的块已经被清扫重新分类，丢掉；同一个堆的类型号缓存留着。
    fn adopt(&mut self, owner: (usize, u64)) {
        if self.owner.get() != owner {
            let last = if self.owner.get().0 == owner.0 { self.last.get() } else { None };
            *self = Self { owner: Cell::new(owner), last: Cell::new(last), ..Self::default() };
        }
    }

    /// 碰撞分配 `bytes` 字节：当前空洞放不下就找当前块的下一个空洞（行标记早于 `epoch` 的是空行），没有了就用 `acquire` 领新块。
    fn bump(&self, bytes: usize, epoch: u32, acquire: impl Fn() -> Block) -> *mut u8 {
        loop {
            let cursor = self.cursor.get();
            if self.limit.get().addr() - cursor.addr() >= bytes {
                self.cursor.set(cursor.wrapping_add(bytes));
                return cursor;
            }
            if let Some(b) = self.block.get()
                && let Some((start, end)) = b.hole(self.line.get(), epoch)
            {
                self.line.set(end);
                self.cursor.set(b.line(start));
                self.limit.set(b.line(end));
                continue;
            }
            self.block.set(Some(acquire()));
            self.line.set(Block::FIRST_LINE);
            self.cursor.set(null_mut());
            self.limit.set(null_mut());
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

    /// 这次 mutate 是否落在并发标记期间（写屏障与黑分配生效）。
    pub fn is_marking(&self) -> bool {
        self.marking.is_some()
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
        let p = self.tlab.bump(bytes, self.heap.epoch, || self.heap.acquire());
        Block::containing(p).set_start(p);
        p
    }
}

/// 只给本 crate 用的操作：trait 在私有模块里、不导出，crate 外拿不到。
pub trait MutatorInternal<'gc> {
    fn heap(&self) -> &'gc Heap;

    /// 写屏障。并发标记中：被覆盖的旧值若还没标就标上、进灰队列（初始快照）。
    /// 平时：写进去的是年轻对象（标记纪元不是上次完成的那轮）就记下槽的地址——老对象指向年轻对象的边只能这样产生，
    /// 年轻回收从这些槽出发。（并发标记期间新对象都是黑的、收尾后全是老对象，不用记。）
    ///
    /// 计数中的槽（老对象里的；年轻对象的槽不计数）本轮第一次被写还要登记，回收时结算。
    fn written(&self, slot: &RawSlot, old: Option<*const Header>, new: Option<*const Header>);
}

impl<'gc> MutatorInternal<'gc> for Mutator<'gc> {
    fn heap(&self) -> &'gc Heap {
        self.heap
    }

    fn written(&self, slot: &RawSlot, old: Option<*const Header>, new: Option<*const Header>) {
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
}

/// 等别人（回收方、mutate 里的线程）时每隔多久醒来查一次对方进程还在不在
#[cfg(target_os = "linux")]
const POLL: Duration = Duration::from_millis(10);
/// [`Attachment::pid`]：有人正在收拾这个死掉的挂接（防两个回收方重复还它的入口）
#[cfg(target_os = "linux")]
const REAPING: u32 = u32::MAX;
/// 回收方抢回收权前，等挡在门口的 mutator 进门最多等这么多轮（每轮 [`POLL`]）
#[cfg(target_os = "linux")]
const YIELD_ROUNDS: usize = 10;
/// 回收方等 mutate 走空最多等这么久，等不到就先放开请求、过一会儿重来：一个线程在 A 的 mutate 里又去进 B 时，
/// 两个回收方（或一起回收）分别挡着 A、B 会互相等死，放开请求就解开了
#[cfg(target_os = "linux")]
const PATIENCE: Duration = Duration::from_millis(50);
/// [`Entry::holder`] 的最高位：持有方是另一个公开堆（低位是它的堆号，[`Entry::holder_incarnation`] 是它的身份），不是某个挂接（槽号 + 1）
#[cfg(target_os = "linux")]
pub const HEAP_HOLDER: u32 = 1 << 31;
/// [`Entry::holder`]：持有方堆和这个堆在同一个目录（同一份堆号登记），可以按目录判它死活、死了就还掉它的登记
#[cfg(target_os = "linux")]
pub const SAME_DIR: u32 = 1 << 30;
/// [`Entry::holder`] 里堆号那几位
#[cfg(target_os = "linux")]
const HOLDER_INDEX: u32 = SAME_DIR - 1;
/// `try_mutate` 隔多久才重新查一次主人还在不在（读 /proc）
#[cfg(target_os = "linux")]
const ALIVE_RECHECK: Duration = Duration::from_millis(10);
/// 清扫时每次领多少块
#[cfg(target_os = "linux")]
const SWEEP_BATCH: usize = 16;
/// 窗口背后是什么。
#[cfg(target_os = "linux")]
pub enum Backing {
    /// 共享内存文件系统上的文件；`owner` = 本句柄建的（本进程是主人），析构时堆跟着死、删文件与标记（除非 persist）
    File { file: File, path: PathBuf, owner: bool },
    /// 匿名共享内存（只在本进程）
    Anonymous,
}

#[cfg(target_os = "linux")]
impl Backing {
    /// 让窗口的 `[offset, offset + len)` 有内存可用（文件：延长；匿名：本来就有）。
    fn commit(&self, offset: usize, len: usize) {
        if let Self::File { file, .. } = self {
            Region::commit(file, offset, len).expect("公开堆所在的文件系统满了");
        }
    }
}

/// 类型键用的 64 位 FNV-1a。
#[cfg(target_os = "linux")]
struct Fnv(u64);

#[cfg(target_os = "linux")]
impl Fnv {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    /// 两个不同的起点，合成 128 位
    const SEEDS: [u64; 2] = [0xcbf2_9ce4_8422_2325, 0x6c62_272e_07bb_0142];

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(Self::PRIME);
        }
    }

    /// 类型 `T` 的指针图与类型键。
    fn of<T: Shared>() -> (EdgeMap, [u64; 2]) {
        let mut map = EdgeMap::default();
        T::edges(&mut map, 0);
        let key = Self::key(type_name::<T>(), size_of::<T>(), map.as_slice());
        (map, key)
    }

    /// 类型名 + 大小 + 指针图。
    fn key(name: &str, size: usize, edges: &[u32]) -> [u64; 2] {
        Self::SEEDS.map(|seed| {
            let mut h = Self(seed);
            h.write(name.as_bytes());
            h.write(&size.to_le_bytes());
            for e in edges {
                h.write(&e.to_le_bytes());
            }
            h.0
        })
    }
}

/// 进程这一侧的公开堆：窗口在哪、怎么延长文件、回收线程池、类型号缓存。
#[cfg(target_os = "linux")]
pub struct Inner {
    /// 地址区：窗口在它里面，Arc 让它比堆活得久
    space: Arc<Space>,
    index: usize,
    window: NonNull<u8>,
    layout: WindowLayout,
    backing: Backing,
    threads: NonZeroUsize,
    pool: OnceLock<Pool>,
    /// 本进程见过的类型：（类型名的地址, 类型号）
    types: SegVec<(usize, u32)>,
    /// 给 mutate 复用的分配缓冲
    spare: AtomicPtr<Tlab>,
    /// 本句柄在挂接表里的槽号
    slot: usize,
    /// 句柄建立的时刻：`checked` 从这里算
    born: std::time::Instant,
    /// 上次确认主人还在的时刻（距 `born` 的纳秒数）
    checked: AtomicU64,
}

// SAFETY: 窗口里的状态都是原子量或由握手保护；其余是线程安全的
#[cfg(target_os = "linux")]
unsafe impl Send for Inner {}
// SAFETY: 同上
#[cfg(target_os = "linux")]
unsafe impl Sync for Inner {}

#[cfg(target_os = "linux")]
impl Inner {
    /// 窗口 `index` 已经映射上 `backing`；`fresh` 时写新的超级块，否则核对。出错时析构负责还窗口。
    pub fn new(space: Arc<Space>, index: usize, backing: Backing, threads: NonZeroUsize, fresh: bool) -> io::Result<Self> {
        let window = NonNull::new(space.region().window(index)).expect("窗口地址非空");
        let mut me = Self { space, index, window, layout: WindowLayout::new(), backing, threads, pool: OnceLock::new(), types: SegVec::default(), spare: AtomicPtr::new(null_mut()), slot: ATTACH_CAP, born: std::time::Instant::now(), checked: AtomicU64::new(0) };
        if fresh {
            me.backing.commit(0, PAGE);
            // SAFETY: 新堆，窗口开头一页刚有内存，还没人读
            let incarnation = std::hash::BuildHasher::hash_one(&std::collections::hash_map::RandomState::new(), (index, Process::current().pid));
            // SAFETY: 同上
            unsafe { me.window.as_ptr().cast::<Super>().write(Super::new(index as u64, incarnation, Process::current())) };
        } else if me.sup().heap.load(Ordering::Acquire) != index as u64 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "超级块里的堆号与窗口不符"));
        }
        me.slot = me.attach().ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "这个公开堆的挂接表满了"))?;
        Ok(me)
    }

    /// 在挂接表里领一个槽登记本进程；满了先收拾死掉的挂接再试一次。
    fn attach(&self) -> Option<usize> {
        let me = Process::current();
        let claim = || (0..ATTACH_CAP).find(|&i| self.attachment(i).pid.compare_exchange(0, me.pid, Ordering::AcqRel, Ordering::Relaxed).is_ok());
        let slot = claim().or_else(|| {
            self.reap_dead();
            claim()
        })?;
        let a = self.attachment(slot);
        a.inside.store(0, Ordering::Relaxed);
        a.waiting.store(0, Ordering::Relaxed);
        a.start.store(me.start, Ordering::Release);
        Some(slot)
    }

    fn attachment(&self, i: usize) -> &Attachment {
        // SAFETY: 挂接表在超级块那一页里（建堆时就有内存），i < ATTACH_CAP；表项全是原子量
        unsafe { &*self.at::<Attachment>(ATTACH_OFF).add(i) }
    }

    /// 挂接槽 `i` 的进程还活着（空槽、正在被收拾的都算不活）。
    fn alive(&self, i: usize) -> bool {
        let a = self.attachment(i);
        let pid = a.pid.load(Ordering::Acquire);
        pid != 0 && pid != REAPING && Process { pid, start: a.start.load(Ordering::Acquire) }.alive()
    }

    /// 收拾所有死掉的挂接。
    ///
    /// 正挂着回收请求的那个不收拾：它的槽要是被新进程领走，大家会以为回收方还活着，一直等下去；它由接手的回收方换下来后再收拾。
    fn reap_dead(&self) {
        let owner = self.sup().request.load(Ordering::SeqCst) as usize;
        for i in 0..ATTACH_CAP {
            let pid = self.attachment(i).pid.load(Ordering::Acquire);
            if pid != 0 && pid != REAPING && i + 1 != owner && !self.alive(i) {
                self.reap(i, pid);
            }
        }
    }

    /// 收拾死掉的挂接 `i`（进程号 `pid`）：还掉它持有的入口、清掉它的 mutate 计数，空出槽。先把进程号换成 [`REAPING`]，只有换成功的那个来做。
    fn reap(&self, i: usize, pid: u32) {
        let a = self.attachment(i);
        if a.pid.compare_exchange(pid, REAPING, Ordering::AcqRel, Ordering::Relaxed).is_err() {
            return;
        }
        let holder = u32::try_from(i + 1).expect("挂接槽号");
        // 持有方是它的入口都还掉（含领到一半、指针还没写的）
        for j in 0..self.sup().entries.load(Ordering::Acquire) {
            if self.entry(j).holder.load(Ordering::Acquire) == holder {
                self.release_entry(j);
            }
        }
        a.inside.store(0, Ordering::Release);
        a.waiting.store(0, Ordering::Release);
        a.start.store(0, Ordering::Relaxed);
        a.pid.store(0, Ordering::Release);
    }

    /// 抢回收权之前：上一次回收挡在门口的 mutator 先进门（醒过来要时间，不让一让就会被接连的回收饿死）。
    /// 死掉的挂接不等；最多等 [`YIELD_ROUNDS`] 轮。
    fn yield_to_waiting(&self) {
        for _ in 0..YIELD_ROUNDS {
            let Some(i) = (0..ATTACH_CAP).find(|&i| self.attachment(i).waiting.load(Ordering::SeqCst) != 0 && self.alive(i)) else {
                return;
            };
            let waiting = &self.attachment(i).waiting;
            waiting.wait(waiting.load(Ordering::SeqCst), POLL);
        }
    }

    /// 拿回收权：没人在回收就占下；别人在回收、它还活着就等；它死了（回收到一半被杀）就接手。
    /// `done_by_other`：等到别人做完时直接返回 false（单独回收用：别人刚回收过就不必再收）；否则一直等到拿到为止。
    fn claim_collection(&self, done_by_other: bool) -> bool {
        let sup = self.sup();
        let me = u32::try_from(self.slot + 1).expect("挂接槽号");
        loop {
            let owner = sup.request.load(Ordering::SeqCst);
            if owner == 0 {
                self.yield_to_waiting();
                if sup.request.compare_exchange(0, me, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                    return true;
                }
            } else if self.alive(owner as usize - 1) {
                sup.request.wait(owner, POLL);
                if done_by_other && sup.request.load(Ordering::SeqCst) != owner {
                    return false;
                }
            } else if sup.request.compare_exchange(owner, me, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                sup.takeovers.fetch_add(1, Ordering::Relaxed);
                return true;
            }
        }
    }

    /// 等每个挂接的 mutate 走空（死了的不等），最多 [`PATIENCE`]；走空了返回 true。
    fn drain(&self) -> bool {
        let start = std::time::Instant::now();
        for i in 0..ATTACH_CAP {
            let inside = &self.attachment(i).inside;
            loop {
                let n = inside.load(Ordering::SeqCst);
                if n == 0 || !self.alive(i) {
                    break;
                }
                if start.elapsed() >= PATIENCE {
                    return false;
                }
                inside.wait(n, POLL);
            }
        }
        true
    }

    /// 放开回收权，叫醒门口等着的。
    fn release(&self) {
        let sup = self.sup();
        sup.request.store(0, Ordering::SeqCst);
        sup.request.wake_all();
    }

    /// 暂停式回收：拿回收权（别人刚回收完就返回）、等 mutate 走空（等不到就放开重来）、收拾死掉的挂接、标记 + 清扫。
    /// 标记只写新纪元、纪元到最后才发布，清扫只在标记做完后才开始，所以被杀了由别人从头重做是安全的。
    fn collect(&self) {
        let mut retry = Retry::new();
        loop {
            if !self.claim_collection(true) {
                return;
            }
            if self.drain() {
                break;
            }
            self.release();
            retry.pause();
        }
        self.reap_dead();
        Self::collect_members(&[self]);
        self.release();
    }

    /// 几个堆一起回收：按堆号从小到大拿每个堆的回收权，全部走空才动手，有一个等不到就全放开重来（不会和别的一起回收互相等死）。
    ///
    /// 一个一个来：拿到第 k 个堆的请求、等它走空，再去拿第 k+1 个。这样 mutator 只要按堆号从小到大嵌套进入
    /// （[`PublicHeap::mutate_with`]），在小号堆里等着进大号堆的线程不会卡住它：那时大号堆还没被挡。逆序嵌套才会互等，
    /// 等不到就全部放开、退让后重来。
    fn collect_group(members: &[&Self]) {
        let mut retry = Retry::new();
        'retry: loop {
            for (k, m) in members.iter().enumerate() {
                m.claim_collection(false);
                if !m.drain() {
                    for held in members[..=k].iter().rev() {
                        held.release();
                    }
                    retry.pause();
                    continue 'retry;
                }
            }
            break;
        }
        for m in members {
            m.reap_dead();
        }
        Self::collect_members(members);
        for m in members.iter().rev() {
            m.release();
        }
    }

    pub fn index(&self) -> usize {
        self.index
    }

    /// 公开偏移落在哪个堆（窗口）里。
    pub fn heap_of(off: u64) -> Option<usize> {
        usize::try_from(off).ok().map(|o| o / WINDOW).filter(|&k| k < WINDOWS)
    }

    /// 堆身份（见 [`Super::incarnation`]）。
    pub fn incarnation(&self) -> u64 {
        self.sup().incarnation.load(Ordering::Acquire)
    }

    /// 堆文件的路径（匿名堆没有）。
    fn path(&self) -> Option<&std::path::Path> {
        match &self.backing {
            Backing::File { path, .. } => Some(path),
            Backing::Anonymous => None,
        }
    }

    /// 本堆作为持有方、在 `target` 里登记时用的持有方字：同一个目录时带 [`SAME_DIR`]。
    pub fn holder_word(&self, target: &Self) -> u32 {
        let same_dir = self.path().zip(target.path()).is_some_and(|(a, b)| Registry::of(a).dir() == Registry::of(b).dir());
        HEAP_HOLDER | if same_dir { SAME_DIR } else { 0 } | u32::try_from(self.index).expect("堆号")
    }

    /// 堆还活着：没标过死，并且主人还在（或 persist、或是匿名堆）。发现主人不在了就标死、删掉堆文件与标记。
    pub fn is_alive(&self) -> bool {
        let sup = self.sup();
        if sup.dead.load(Ordering::Acquire) != 0 {
            return false;
        }
        let Some(path) = self.path() else { return true };
        let owner = Process { pid: sup.owner_pid.load(Ordering::Acquire), start: sup.owner_start.load(Ordering::Acquire) };
        if sup.persist.load(Ordering::Acquire) != 0 || owner.alive() {
            return true;
        }
        sup.dead.store(1, Ordering::Release);
        Registry::of(path).bury(path, self.index, self.incarnation());
        false
    }

    /// 同 [`Inner::is_alive`]，但距上次确认不到 [`ALIVE_RECHECK`] 时只看「已死」标志（读 /proc 不便宜）。
    fn alive_cached(&self) -> bool {
        if self.sup().dead.load(Ordering::Acquire) != 0 {
            return false;
        }
        let now = u64::try_from(self.born.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let last = self.checked.load(Ordering::Relaxed);
        if u128::from(now.saturating_sub(last)) < ALIVE_RECHECK.as_nanos() {
            return true;
        }
        let alive = self.is_alive();
        if alive {
            self.checked.store(now, Ordering::Relaxed);
        }
        alive
    }

    /// 持有方字 + 身份是不是组里的某个堆（是组里的堆持有的登记，一起回收时由追踪决定）。
    fn held_by(members: &[&Self], holder: u32, incarnation: u64) -> bool {
        holder & HEAP_HOLDER != 0 && members.iter().any(|m| m.index == (holder & HOLDER_INDEX) as usize && m.incarnation() == incarnation)
    }

    /// 回收前：持有方是同目录里另一个堆、而那个堆已经死了（或号已经换了主）的登记，还掉。查不准的留着。
    fn release_dead_holders(&self) {
        let Some(path) = self.path() else { return };
        let registry = Registry::of(path);
        let mut verdicts: Vec<((usize, u64), Liveness)> = Vec::new();
        for i in 0..self.sup().entries.load(Ordering::Acquire) {
            let e = self.entry(i);
            let holder = e.holder.load(Ordering::Acquire);
            if e.ptr.load(Ordering::Acquire) == 0 || holder & (HEAP_HOLDER | SAME_DIR) != HEAP_HOLDER | SAME_DIR {
                continue;
            }
            let key = ((holder & HOLDER_INDEX) as usize, e.holder_incarnation.load(Ordering::Acquire));
            let verdict = if let Some(&(_, v)) = verdicts.iter().find(|(k, _)| *k == key) {
                v
            } else {
                let v = registry.liveness(key.0, Some(key.1));
                verdicts.push((key, v));
                v
            };
            if matches!(verdict, Liveness::Dead | Liveness::Gone) {
                self.release_entry(i);
            }
        }
    }

    pub fn sup(&self) -> &Super {
        // SAFETY: 窗口开头一页在建堆时就有了，里面是超级块（全是原子量）
        unsafe { &*self.window.as_ptr().cast() }
    }

    fn at<T>(&self, offset: usize) -> *mut T {
        self.window.as_ptr().wrapping_add(offset).cast()
    }

    /// 公开偏移在本进程里的地址。
    pub fn ptr(&self, off: u64) -> *mut u8 {
        self.space.region().base().as_ptr().wrapping_add(usize::try_from(off).expect("公开偏移"))
    }

    /// 公开偏移落在这个堆的窗口里。
    pub fn owns(&self, off: u64) -> bool {
        usize::try_from(off).is_ok_and(|o| o / WINDOW == self.index)
    }

    fn pool(&self) -> &Pool {
        self.pool.get_or_init(|| Pool::new(self.threads))
    }

    /// 让某一段至少覆盖到窗口偏移 `end`。
    fn commit(&self, segment: Segment, end: usize) {
        let committed = &self.sup().committed[segment.index()];
        let have = usize::try_from(committed.load(Ordering::Acquire)).expect("窗口内偏移");
        if have >= end {
            return;
        }
        let (start, limit) = self.layout.segment(segment);
        let to = end.next_multiple_of(PAGE).min(limit);
        let from = have.max(start);
        // 这一段还一项都没用（比如没有入口时回收）：没有要延长的（fallocate 不收 0 长度）
        if to <= from {
            return;
        }
        self.backing.commit(from, to - from);
        committed.fetch_max(to as u64, Ordering::AcqRel);
    }

    fn type_entry(&self, ty: u32) -> &TypeEntry {
        // SAFETY: 序号来自类型表游标，那一项所在的页已经延长过；表项全是原子量
        unsafe { &*self.at::<TypeEntry>(self.layout.types).add(ty as usize) }
    }

    /// 类型 `ty` 的指针图。
    pub fn edges(&self, ty: u32) -> &[u32] {
        let e = self.type_entry(ty);
        let (map, n) = (e.map.load(Ordering::Relaxed) as usize, e.edges.load(Ordering::Relaxed) as usize);
        // SAFETY: 写完指针图才置 ready，读的一侧（分配过这个类型的对象之后的回收）在它之后
        unsafe { core::slice::from_raw_parts(self.at::<u32>(self.layout.map).add(map), n) }
    }

    pub fn has_edges(&self, ty: u32) -> bool {
        self.type_entry(ty).edges.load(Ordering::Relaxed) != 0
    }

    /// 类型号：先看缓冲里上一次的，再看本进程的缓存，最后去类型表里找或登记。
    fn type_of<T: Shared>(&self, tlab: &Tlab) -> u32 {
        let tag = type_name::<T>().as_ptr().addr();
        if let Some((t, ty)) = tlab.last.get()
            && t == tag
        {
            return ty;
        }
        let ty = self.types.iter().find(|(t, _)| *t == tag).map_or_else(
            || {
                let ty = self.register::<T>();
                self.types.push((tag, ty));
                ty
            },
            |&(_, ty)| ty,
        );
        tlab.last.set(Some((tag, ty)));
        ty
    }

    /// 在类型表里找这个类型（别的线程 / 进程可能登记过），没有就登记：指针图写进指针图区，表项最后置 ready。
    /// 两个线程同时登记同一个类型会得到两个号，指针图一样，无害。
    fn register<T: Shared>(&self) -> u32 {
        let (map, key) = Fnv::of::<T>();
        let sup = self.sup();
        let known = (sup.types.load(Ordering::Acquire) as usize).min(TYPE_CAP);
        if let Some(ty) = (0..u32::try_from(known).expect("类型表容量")).find(|&ty| self.type_entry(ty).has_key(key)) {
            return ty;
        }
        let ty = sup.types.fetch_add(1, Ordering::AcqRel);
        assert!((ty as usize) < TYPE_CAP, "公开堆的类型表满了");
        let edges = map.as_slice();
        let n = u32::try_from(edges.len()).expect("指针槽个数");
        let start = sup.map.fetch_add(n, Ordering::AcqRel);
        assert!((start + n) as usize <= MAP_CAP, "公开堆的指针图满了");
        self.commit(Segment::Types, self.layout.types + (ty as usize + 1) * size_of::<TypeEntry>());
        self.commit(Segment::Map, self.layout.map + (start + n) as usize * size_of::<u32>());
        // SAFETY: [start, start + n) 是本线程刚领的、已经有内存的指针图区
        unsafe { self.at::<u32>(self.layout.map).add(start as usize).copy_from_nonoverlapping(edges.as_ptr(), edges.len()) };
        let e = self.type_entry(ty);
        e.edges.store(n, Ordering::Relaxed);
        e.map.store(start, Ordering::Relaxed);
        e.size.store(u32::try_from(size_of::<T>()).expect("对象不超过 4 GiB"), Ordering::Relaxed);
        for (k, v) in e.key.iter().zip(key) {
            k.store(v, Ordering::Relaxed);
        }
        e.ready.store(1, Ordering::Release);
        ty
    }

    /// 类型表里的 `ty` 是不是 `T`（比类型键：类型名 + 大小 + 指针图）。
    fn same_type<T: Shared>(&self, ty: u32) -> bool {
        let (_, key) = Fnv::of::<T>();
        self.type_entry(ty).has_key(key)
    }

    fn entry(&self, i: u32) -> &Entry {
        // SAFETY: 序号来自入口表游标（或还回来的栈），那一项所在的页已经延长过；表项全是原子量
        unsafe { &*self.at::<Entry>(self.layout.entries).add(i as usize) }
    }

    /// 领一个入口放公开偏移 `off`，持有方是本挂接（只在这个堆的 mutate 里调用）。
    pub fn claim_entry(&self, off: u64) -> u32 {
        self.claim_entry_for(off, u32::try_from(self.slot + 1).expect("挂接槽号"), 0)
    }

    /// 同 [`Inner::claim_entry`]，持有方是 `holder`（另一个公开堆：[`Inner::holder_word`]，`incarnation` 是它的身份）。
    pub fn claim_entry_for(&self, off: u64, holder: u32, incarnation: u64) -> u32 {
        let sup = self.sup();
        let i = Stack::on(&sup.free_entries).pop(|i| &self.entry(i).next).unwrap_or_else(|| {
            let i = sup.entries.fetch_add(1, Ordering::AcqRel);
            assert!((i as usize) < self.layout.entry_cap, "公开堆的入口表满了");
            self.commit(Segment::Entries, self.layout.entries + (i as usize + 1) * size_of::<Entry>());
            i
        });
        let e = self.entry(i);
        e.holder.store(holder, Ordering::Relaxed);
        e.holder_incarnation.store(incarnation, Ordering::Relaxed);
        e.ptr.store(off, Ordering::Release);
        i
    }

    /// 入口改成不归任何人（持有方 0）：收拾死进程、一起回收重建登记都不碰它。
    pub fn disown_entry(&self, i: u32) {
        self.entry(i).holder.store(0, Ordering::Release);
    }

    /// 句柄析构：清空入口、还回栈里（任何时候、任何线程）。
    pub fn release_entry(&self, i: u32) {
        let e = self.entry(i);
        // 先清持有方：收拾死进程时按持有方认它没还的入口，还过的不能再还一次
        e.holder.store(0, Ordering::Release);
        e.ptr.store(0, Ordering::Release);
        Stack::on(&self.sup().free_entries).push(i, &e.next);
    }

    fn state(&self, i: u32) -> &BlockState {
        // SAFETY: 序号来自块游标，那一项所在的页已经延长过；表项全是原子量
        unsafe { &*self.at::<BlockState>(self.layout.states).add(i as usize) }
    }

    fn block(&self, i: u32) -> Block {
        // SAFETY: 块区里第 i 块，开过（有内存），按块大小对齐（窗口按窗口大小对齐、块区起点按块对齐）
        unsafe { Block::at(self.at(self.layout.blocks + i as usize * BLOCK)) }
    }

    /// 从块游标开 `n` 个连续的新块（延长文件），返回第一个的序号。
    fn open_blocks(&self, n: usize) -> u32 {
        let count = u32::try_from(n).expect("块数");
        let i = self.sup().blocks.fetch_add(count, Ordering::AcqRel);
        assert!(i as usize + n <= self.layout.max_blocks, "公开堆的窗口用完了");
        self.commit(Segment::States, self.layout.states + (i as usize + n) * size_of::<BlockState>());
        self.backing.commit(self.layout.blocks + i as usize * BLOCK, n * BLOCK);
        i
    }

    /// 领一个块给分配缓冲：先拿可用栈里清扫好的，没有就开新块。
    fn acquire(&self) -> Block {
        let sup = self.sup();
        sup.allocated.fetch_add(BLOCK as u64, Ordering::Relaxed);
        let i = Stack::on(&sup.available).pop(|i| &self.state(i).next).unwrap_or_else(|| {
            let i = self.open_blocks(1);
            self.state(i).kind.store(Kind::Small as u32, Ordering::Release);
            i
        });
        let b = self.block(i);
        b.reused();
        b
    }

    /// 大对象：开一串连续的新块，对象头在第一块开头。
    /// 大对象：开一串连续的新块，对象头在第一块开头；返回（地址, 第一块序号, 块数）。块的种类等对象写完再登记
    /// （[`Inner::publish_large`]）：进程死在中间，这些块停在「还没开过」，清扫不会去读没写完的头。
    fn alloc_large(&self, bytes: usize) -> (*mut u8, u32, u32) {
        let n = bytes.div_ceil(BLOCK);
        let i = self.open_blocks(n);
        self.sup().allocated.fetch_add((n * BLOCK) as u64, Ordering::Relaxed);
        (self.block(i).line(0), i, u32::try_from(n).expect("块数"))
    }

    /// 大对象写完了：登记块的种类，先尾后头（清扫从头认出大对象）。
    fn publish_large(&self, first: u32, n: u32) {
        for j in first + 1..first + n {
            self.state(j).kind.store(Kind::LargeTail as u32, Ordering::Release);
        }
        self.state(first).kind.store(Kind::LargeHead as u32, Ordering::Release);
    }

    /// 回收（握手已经把 `members` 里每个堆的 mutator 都挡在外面）：从入口表出发并行标记，再逐个堆并行清扫。
    ///
    /// 纪元取「上次完成的」与「上次开始的」里大的那个 + 1：上一次回收做到一半被杀时，它标过的对象不能算作这次标过。
    /// 别的进程可能死在开块 / 领入口的中途（游标加了、文件还没延长）：先把这两张表延长到游标处，扫的时候才不会碰到没有内存的页。
    ///
    /// 几个堆一起时：持有方是组里某个堆的入口不当根（由追踪决定死活），标完后按追踪到的跨堆边重建这些登记——先登记新的、
    /// 再还旧的，中间被杀也只多不少（接手的单独回收照样把它们当根）。
    fn collect_members(members: &[&Self]) {
        let in_group = |e: &Entry| members.len() > 1 && Self::held_by(members, e.holder.load(Ordering::Acquire), e.holder_incarnation.load(Ordering::Acquire));
        let mut epochs = Vec::with_capacity(members.len());
        let mut roots = Vec::new();
        let mut blocks = 0;
        for (m, heap) in members.iter().enumerate() {
            let sup = heap.sup();
            let last = sup.epoch.load(Ordering::Acquire).max(sup.marking.load(Ordering::Acquire));
            let epoch = last.checked_add(1).filter(|&e| e <= MAX_EPOCH).expect("纪元用完了");
            sup.marking.store(epoch, Ordering::Release);
            epochs.push(epoch);
            let n = sup.blocks.load(Ordering::Acquire) as usize;
            blocks += n;
            heap.commit(Segment::States, heap.layout.states + n * size_of::<BlockState>());
            heap.commit(Segment::Entries, heap.layout.entries + sup.entries.load(Ordering::Acquire) as usize * size_of::<Entry>());
            heap.release_dead_holders();
            for i in 0..sup.entries.load(Ordering::Acquire) {
                let e = heap.entry(i);
                let off = e.ptr.load(Ordering::Acquire);
                if off != 0 && !in_group(e) {
                    roots.push((m, PHeader::of(heap.ptr(off)).expose_provenance()));
                }
            }
        }
        let marked: Vec<Member<'_>> = members.iter().zip(&epochs).map(|(&heap, &epoch)| Member { heap, epoch }).collect();
        let found = Injector::new();
        let pool = members[0].pool();
        let grey = Injector::new();
        Marker::new(&marked, (members.len() > 1).then_some(&found), &grey, pool.sized(blocks)).run(pool, &roots);
        if members.len() > 1 {
            Self::reregister(members, &found);
        }
        for (heap, epoch) in members.iter().zip(epochs) {
            heap.sweep_all(epoch);
        }
    }

    /// 一起回收标完之后：组内跨堆登记换成这次追踪到的（每对「来源堆, 目标」一项）。先登记新的、再还旧的。
    fn reregister(members: &[&Self], found: &Injector<(usize, u64)>) {
        let old: Vec<Vec<u32>> = members
            .iter()
            .map(|heap| {
                (0..heap.sup().entries.load(Ordering::Acquire))
                    .filter(|&i| {
                        let e = heap.entry(i);
                        e.ptr.load(Ordering::Acquire) != 0 && Self::held_by(members, e.holder.load(Ordering::Acquire), e.holder_incarnation.load(Ordering::Acquire))
                    })
                    .collect()
            })
            .collect();
        let mut edges = Vec::new();
        loop {
            match found.steal() {
                Steal::Success(e) => edges.push(e),
                Steal::Retry => {}
                Steal::Empty => break,
            }
        }
        edges.sort_unstable();
        edges.dedup();
        for (source, off) in edges {
            let target = members.iter().find(|m| m.owns(off)).expect("找到的边都指向组里的堆");
            target.claim_entry_for(off, members[source].holder_word(target), members[source].incarnation());
        }
        for (heap, old) in members.iter().zip(old) {
            for i in old {
                heap.release_entry(i);
            }
        }
    }

    /// 清扫（标记已经用 `epoch` 做完）：可用栈清空、各块重新分类，发布纪元。
    fn sweep_all(&self, epoch: u32) {
        let sup = self.sup();
        let blocks = sup.blocks.load(Ordering::Acquire) as usize;
        let pool = self.pool();
        let workers = pool.sized(blocks);
        // 可用栈清空，清扫时有空行的块重新压进去
        sup.available.store(0, Ordering::Release);
        let cursor = AtomicUsize::new(0);
        let swept = pool.run(workers, |_| self.sweep_some(&cursor, blocks, epoch));
        let mut live = 0;
        for (bytes, dead) in swept {
            live += bytes;
            // 死掉的大对象等全部块扫完再拆：拆开的块会变成小对象块，正在扫的工人不能把它们当小块再扫一遍
            for head in dead {
                self.free_large(head);
            }
        }
        sup.live.store(live as u64, Ordering::Relaxed);
        sup.allocated.store(0, Ordering::Relaxed);
        sup.collections.fetch_add(1, Ordering::AcqRel);
        sup.epoch.store(epoch, Ordering::Release);
    }

    /// 清扫一批批领到的块；返回存活字节数与死掉的大对象（第一块的序号）。
    fn sweep_some(&self, cursor: &AtomicUsize, blocks: usize, epoch: u32) -> (usize, Vec<u32>) {
        let (mut live, mut dead) = (0, Vec::new());
        loop {
            let start = cursor.fetch_add(SWEEP_BATCH, Ordering::AcqRel);
            if start >= blocks {
                return (live, dead);
            }
            for i in start..(start + SWEEP_BATCH).min(blocks) {
                let i = u32::try_from(i).expect("块序号");
                match Kind::from(self.state(i).kind.load(Ordering::Acquire)) {
                    Kind::Small => {
                        // SAFETY: 标记做完了，没有 mutator；块里是公开堆的对象
                        let (state, bytes) = unsafe { self.block(i).sweep::<PHeader>(epoch) };
                        live += bytes;
                        if !matches!(state, State::Full) {
                            Stack::on(&self.sup().available).push(i, &self.state(i).next);
                        }
                    }
                    Kind::LargeHead => {
                        // SAFETY: 大对象的头在第一块开头
                        let h = unsafe { &*self.block(i).line(0).cast::<Core>() };
                        if h.is_marked(epoch) {
                            live += h.bytes();
                        } else {
                            dead.push(i);
                        }
                    }
                    Kind::LargeTail | Kind::Unused => {}
                }
            }
        }
    }

    /// 拆掉一个死掉的大对象：它占的块元数据清零、改成小对象块，压进可用栈。先拆后面的块、最后拆头：
    /// 拆到一半进程被杀，头还在，下一次回收再拆一遍，已经拆过的（不再是 LargeTail）跳过，不会重复进栈。
    fn free_large(&self, head: u32) {
        // SAFETY: 大对象的头在第一块开头，还没拆
        let n = unsafe { &*self.block(head).line(0).cast::<Core>() }.bytes().div_ceil(BLOCK);
        let tails = head + 1..head + u32::try_from(n).expect("块数");
        for j in tails.filter(|&j| Kind::from(self.state(j).kind.load(Ordering::Acquire)) == Kind::LargeTail).chain([head]) {
            let b = self.block(j);
            // SAFETY: 对象已死（没有析构），块里没有别的对象
            unsafe { b.reset() };
            b.reused();
            self.state(j).kind.store(Kind::Small as u32, Ordering::Release);
            Stack::on(&self.sup().available).push(j, &self.state(j).next);
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for Inner {
    fn drop(&mut self) {
        // 句柄都已析构（入口都还了），没有 mutator：空出挂接槽
        if self.slot < ATTACH_CAP {
            let a = self.attachment(self.slot);
            a.start.store(0, Ordering::Relaxed);
            a.pid.store(0, Ordering::Release);
        }
        // 建堆的句柄（主人）放手、又没有 persist：堆跟着死——标死（还映射着它的进程就知道了），删掉文件与标记。
        // 要在拆映射之前读写超级块
        if let Backing::File { path, owner: true, .. } = &self.backing
            && self.sup().persist.load(Ordering::Acquire) == 0
        {
            self.sup().dead.store(1, Ordering::Release);
            Registry::of(path).bury(path, self.index, self.incarnation());
        }
        // SAFETY: 最后一个引用：没有 mutator、句柄都已析构
        unsafe { self.space.region().unmap(self.index) };
        self.space.release(self.index);
        let spare = *self.spare.get_mut();
        if !spare.is_null() {
            // SAFETY: Box::into_raw 来的
            drop(unsafe { Box::from_raw(spare) });
        }
    }
}

/// 公开堆。多个线程可以同时 [`PublicHeap::mutate`]；同一个堆在别处（另一个地址区、以后另一个进程）也映射着时，握手照样互相排斥。
#[cfg(target_os = "linux")]
pub struct PublicHeap(Arc<Inner>);

#[cfg(target_os = "linux")]
impl From<Inner> for PublicHeap {
    fn from(inner: Inner) -> Self {
        Self(Arc::new(inner))
    }
}

/// 堆已经死了（主人不在了）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(target_os = "linux")]
pub struct HeapDead;

#[cfg(target_os = "linux")]
impl core::fmt::Display for HeapDead {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("公开堆已经死了（建它的进程不在了）")
    }
}

#[cfg(target_os = "linux")]
impl std::error::Error for HeapDead {}

/// 公开堆的统计。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg(target_os = "linux")]
pub struct PublicStats {
    /// 上次回收后存活的字节数（含头）
    pub live: usize,
    /// 上次回收后新领用的字节数
    pub allocated: usize,
    /// 开过的块数
    pub blocks: usize,
    pub collections: u64,
    /// 回收方被杀（回收做到一半）、由别人接手重做的次数
    pub takeovers: u64,
}

#[cfg(target_os = "linux")]
impl PublicHeap {
    /// 堆号（= 窗口号）。
    pub fn id(&self) -> usize {
        self.0.index
    }

    /// 建堆的句柄析构时保留堆文件与堆号标记（主进程退出或重启后，别的进程照样 [`crate::PublicSpace::open`] 回来接着用）。
    pub fn persist(&self) {
        self.0.sup().persist.store(1, Ordering::Release);
    }

    /// 堆还活着：主人（建堆的进程）还在，或调过 [`PublicHeap::persist`]。主人不在了堆就死了：这里发现时标死、删掉堆文件与标记；
    /// 之后本进程该放掉指向它的句柄（放掉最后一个，映射才拆、内存才还）。
    pub fn is_alive(&self) -> bool {
        self.0.is_alive()
    }

    /// 同时在这个堆与 `other` 里做事（两个不同的公开堆）：总是先进堆号小的那个。嵌套进几个公开堆时都照这个顺序，
    /// 一起回收（按堆号逐个挡、逐个等走空）就不会和 mutator 互等；逆序嵌套也不会出错，只是回收要放开重来、慢一些。
    pub fn mutate_with<R>(&self, other: &PublicHeap, f: impl for<'a, 'b> FnOnce(&PublicMutator<'a>, &PublicMutator<'b>) -> R) -> R {
        assert!(self.id() != other.id(), "两个不同的公开堆");
        if self.id() < other.id() { self.mutate(|ma| other.mutate(|mb| f(ma, mb))) } else { other.mutate(|mb| self.mutate(|ma| f(ma, mb))) }
    }

    /// 同 [`PublicHeap::mutate`]，但堆已经死了（主人不在了）时不进去，返回 [`HeapDead`]。别的进程建的堆用这个。
    pub fn try_mutate<R>(&self, f: impl for<'p> FnOnce(&PublicMutator<'p>) -> R) -> Result<R, HeapDead> {
        if !self.0.alive_cached() {
            return Err(HeapDead);
        }
        Ok(self.mutate(f))
    }

    /// 挂接表里占着的槽数（映射着这个堆的地方：进程，或同一进程里的多个地址区；含已死、还没被收拾的）。
    pub fn attached(&self) -> usize {
        (0..ATTACH_CAP).filter(|&i| self.0.attachment(i).pid.load(Ordering::Acquire) != 0).count()
    }

    /// 在堆上做事；可以在多个线程上同时调用。闭包里分配的对象只有挂到 [`Public`] 上（直接或间接）才能活过下一次回收。
    pub fn mutate<R>(&self, f: impl for<'p> FnOnce(&PublicMutator<'p>) -> R) -> R {
        let spare = self.0.spare.swap(null_mut(), Ordering::AcqRel);
        let mut tlab = if spare.is_null() {
            Box::default()
        } else {
            // SAFETY: spare 里只放过 Box::into_raw 的 Tlab，swap 后归我们独有
            unsafe { Box::from_raw(spare) }
        };
        let r = {
            let _gate = Gate::enter(&self.0);
            tlab.adopt((Arc::as_ptr(&self.0).addr(), self.0.sup().collections.load(Ordering::Acquire)));
            // 回收只在没有 mutator 时进行，所以一次 mutate 里纪元不变
            let epoch = self.0.sup().epoch.load(Ordering::Acquire);
            f(&PublicMutator { heap: &self.0, tlab: &tlab, epoch, _brand: PhantomData })
        };
        let tlab = Box::into_raw(tlab);
        if self.0.spare.compare_exchange(null_mut(), tlab, Ordering::AcqRel, Ordering::Relaxed).is_err() {
            // SAFETY: 没放进去，仍归我们独有
            drop(unsafe { Box::from_raw(tlab) });
        }
        r
    }

    /// 暂停式回收：挂上请求、等所有挂着这个堆的地方（含别的进程）的 mutate 走空，并行标记 + 并行清扫。
    /// 别人正在回收就等它做完返回；回收方被杀了就接手。死掉的进程不等，它持有的入口在这里还掉。
    /// 不要在这个堆的 mutate 里调用（会一直等自己走空）。
    pub fn collect(&self) {
        self.0.collect();
    }

    /// 几个公开堆（都映射在本进程里）一起回收，收掉跨堆的环：持有方是组里某个堆的 [`crate::PublicRef`] 登记不当根，
    /// 由追踪决定死活；标完后组内的跨堆登记换成这次追踪到的。同一个堆不能出现两次。
    pub fn collect_group(heaps: &[&PublicHeap]) {
        // 已经死了的堆不参加：它持有的登记由各堆回收时按死活还掉
        let mut members: Vec<&Inner> = heaps.iter().map(|h| &*h.0).filter(|m| m.is_alive()).collect();
        members.sort_by_key(|m| m.index);
        assert!(members.windows(2).all(|w| w[0].index != w[1].index), "同一个公开堆不能在组里出现两次");
        if !members.is_empty() {
            Inner::collect_group(&members);
        }
    }

    /// 堆身份（建堆时随机取的；堆号会被重用，身份不会）。
    pub fn incarnation(&self) -> u64 {
        self.0.incarnation()
    }

    pub fn stats(&self) -> PublicStats {
        let sup = self.0.sup();
        PublicStats {
            live: usize::try_from(sup.live.load(Ordering::Relaxed)).unwrap_or(usize::MAX),
            allocated: usize::try_from(sup.allocated.load(Ordering::Relaxed)).unwrap_or(usize::MAX),
            blocks: sup.blocks.load(Ordering::Relaxed) as usize,
            collections: sup.collections.load(Ordering::Relaxed),
            takeovers: sup.takeovers.load(Ordering::Relaxed),
        }
    }
}

/// 只给本 crate 用的操作：trait 在私有模块里、不导出，crate 外拿不到。
#[cfg(target_os = "linux")]
pub trait PublicHeapInternal {
    fn is(&self, inner: &Arc<Inner>) -> bool;
}

#[cfg(target_os = "linux")]
impl PublicHeapInternal for PublicHeap {
    fn is(&self, inner: &Arc<Inner>) -> bool {
        Arc::ptr_eq(&self.0, inner)
    }
}


/// 在 mutate 里：本挂接的计数加一，出门减一（panic 也减）。
///
/// 进门先加计数、再看回收请求，回收方先挂请求、再看计数（都是 SeqCst）：两边至少有一方看到对方，所以不会有
/// mutator 在回收进行时溜进来。看到请求就退回去等；回收方死了（请求一直挂着）就自己接手把回收做完。
#[cfg(target_os = "linux")]
struct Gate<'h> {
    inside: &'h AtomicU32,
    request: &'h AtomicU32,
}

#[cfg(target_os = "linux")]
impl<'h> Gate<'h> {
    fn enter(heap: &'h Inner) -> Self {
        let request = &heap.sup().request;
        let a = heap.attachment(heap.slot);
        let (inside, waiting) = (&a.inside, &a.waiting);
        // 挡在门口时记一笔，进了门再销掉（见 Attachment::waiting）
        let mut queued = false;
        let leave_queue = |queued: &mut bool| {
            if core::mem::take(queued) && waiting.fetch_sub(1, Ordering::SeqCst) == 1 {
                waiting.wake_all();
            }
        };
        loop {
            let owner = request.load(Ordering::SeqCst);
            if owner != 0 {
                if heap.alive(owner as usize - 1) {
                    if !queued {
                        waiting.fetch_add(1, Ordering::SeqCst);
                        queued = true;
                    }
                    request.wait(owner, POLL);
                } else {
                    // 回收方死了：自己接手（先销掉排队，回收方会等排队的人）
                    leave_queue(&mut queued);
                    heap.collect();
                }
                continue;
            }
            inside.fetch_add(1, Ordering::SeqCst);
            if request.load(Ordering::SeqCst) == 0 {
                leave_queue(&mut queued);
                return Self { inside, request };
            }
            inside.fetch_sub(1, Ordering::SeqCst);
            inside.wake_all();
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for Gate<'_> {
    fn drop(&mut self) {
        self.inside.fetch_sub(1, Ordering::SeqCst);
        if self.request.load(Ordering::SeqCst) != 0 {
            self.inside.wake_all();
        }
    }
}

/// 一次公开堆 mutate 里的分配器与品牌；只在调用线程上用。
#[cfg(target_os = "linux")]
pub struct PublicMutator<'p> {
    heap: &'p Arc<Inner>,
    tlab: &'p Tlab,
    /// 上次完成的纪元（空行判定）
    epoch: u32,
    _brand: Invariant<'p>,
}

#[cfg(target_os = "linux")]
impl<'p> PublicMutator<'p> {

    /// 分配一个对象。类型不能有析构（编译期检查）。
    pub fn alloc<T: Shared>(&self, value: T) -> PublicGc<'p, T> {
        const {
            assert!(!core::mem::needs_drop::<T>(), "公开堆的对象不能有析构");
            assert!(align_of::<T>() <= GRANULE, "对象数据的对齐不能超过 8 字节");
        };
        let bytes = (PHEADER + size_of::<T>()).next_multiple_of(GRANULE);
        let ty = self.heap.type_of::<T>(self.tlab);
        let large = bytes > LARGE;
        let (p, run) = if large {
            let (p, first, n) = self.heap.alloc_large(bytes);
            (p, Some((first, n)))
        } else {
            (self.alloc_small(bytes), None)
        };
        // SAFETY: p 是 bytes 字节、按粒度对齐的新内存，归这次分配独有
        let gc = unsafe {
            p.cast::<PHeader>().write(PHeader::new(bytes, large, ty));
            let data = PHeader::data(p.cast());
            data.cast::<T>().write(value);
            PublicGc::from_data(data)
        };
        // 写完才登记（小对象的起点位、大对象的块种类）：进程死在中间，清扫也不会去读一个没写完的对象
        match run {
            Some((first, n)) => self.heap.publish_large(first, n),
            None => Block::containing(p).set_start(p),
        }
        gc
    }

    /// 给对象建一个句柄（入口）。
    pub fn root<B: PublicBrand>(&self, gc: PublicGc<'p, B::Of<'p>>) -> Public<B> {
        let entry = self.heap.claim_entry(gc.offset());
        // SAFETY: Gc 的数据指针非空
        Public::new(self.heap.clone(), entry, unsafe { NonNull::new_unchecked(gc.as_ptr().cast_mut().cast()) })
    }

    /// 凭入口号（[`Public::index`]，可能来自另一个映射或进程）拿到那个对象的新句柄。入口空着、
    /// 或对象的类型（类型名 + 大小 + 指针图）和 `B` 对不上时返回 None。
    pub fn find<B: PublicBrand>(&self, index: u32) -> Option<Public<B>> {
        let sup = self.heap.sup();
        if index >= sup.entries.load(Ordering::Acquire) {
            return None;
        }
        let off = self.heap.entry(index).ptr.load(Ordering::Acquire);
        if off == 0 || !self.heap.owns(off) {
            return None;
        }
        // 在 mutate 里：入口非空 ⇒ 对象此刻是根，下一次回收前不会死；下面领了自己的入口再离开
        let data = self.heap.ptr(off);
        // SAFETY: 入口指着本堆一个活对象
        let ty = unsafe { (*PHeader::of(data)).ty };
        if !self.heap.same_type::<B::Of<'p>>(ty) {
            return None;
        }
        let entry = self.heap.claim_entry(off);
        Some(Public::new(self.heap.clone(), entry, NonNull::new(data).expect("对象地址非空")))
    }

    /// 还掉一个不归任何人的入口（[`Public::persist`] 留下的）；入口空着或还归某个进程 / 堆时不动，返回 false。
    pub fn release(&self, index: u32) -> bool {
        let heap = self.heap;
        if index >= heap.sup().entries.load(Ordering::Acquire) {
            return false;
        }
        let e = heap.entry(index);
        if e.ptr.load(Ordering::Acquire) == 0 || e.holder.load(Ordering::Acquire) != 0 {
            return false;
        }
        heap.release_entry(index);
        true
    }

    fn alloc_small(&self, bytes: usize) -> *mut u8 {
        self.tlab.bump(bytes, self.epoch, || self.heap.acquire())
    }
}

/// 只给本 crate 用的操作：trait 在私有模块里、不导出，crate 外拿不到。
#[cfg(target_os = "linux")]
pub trait PublicMutatorInternal {
    fn is(&self, inner: &Arc<Inner>) -> bool;

    /// 公开偏移在本进程里的地址。
    fn at(&self, off: u64) -> *mut u8;

    /// 堆号。
    fn index(&self) -> usize;

    /// 给 `holder` 堆持有的引用在本堆登记一个入口。
    fn register(&self, off: u64, holder: &PublicMutator<'_>);

    /// 堆身份。
    fn incarnation(&self) -> u64;
}

#[cfg(target_os = "linux")]
impl PublicMutatorInternal for PublicMutator<'_> {
    fn is(&self, inner: &Arc<Inner>) -> bool {
        Arc::ptr_eq(self.heap, inner)
    }

    fn at(&self, off: u64) -> *mut u8 {
        self.heap.ptr(off)
    }

    fn index(&self) -> usize {
        self.heap.index
    }

    fn register(&self, off: u64, holder: &PublicMutator<'_>) {
        self.heap.claim_entry_for(off, holder.heap.holder_word(self.heap), holder.heap.incarnation());
    }

    fn incarnation(&self) -> u64 {
        self.heap.incarnation()
    }
}
