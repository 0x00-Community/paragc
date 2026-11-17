//! 释放死对象：清扫器 [`Sweeper`]（标记后与 mutator 并发、惰性）与引用计数减到 0 的后台释放 [`Releaser`]。
//!
//! 清扫：标记收尾时（独占堆）把全部块与大对象交给 [`Sweeper`]，之后 mutator 照常跑，清扫在两头并发进行：
//!
//! - mutator 要新块时先拿清扫好的；没有就自己领一个没扫的块当场扫（析构死对象），能用就用（惰性清扫）
//! - 回收线程（[`crate::Heap::sweep_concurrently`]）把剩下的块和大对象领走扫完
//!
//! 领用是原子游标 `fetch_add`，每个块恰好被一个线程扫一次；扫完才交出去，所以扫的时候没有 mutator 在用它。
//! 判死用「标记纪元 ≠ 本轮」：下一轮标记开始前必须扫完（否则活对象被标成更新的纪元会被当成死的）。
//!
//! 引用计数的减量在线程池上并发处理（mutator 照常跑）：年轻回收的停顿只把减量放进队列。
//!
//! 安全性：停顿那一刻计数为 0 的对象没有计入的引用，根都 +1 过、年轻对象都已晋升或死掉 ⇒ mutator 碰不到它们，
//! 连锁下去的也一样。释放只析构、清起点位、记下脏块；脏块的行要等下一次停顿开头重算（此刻可能有分配缓冲在往里分配）。
//! 被释放对象的行在重算前一直标着占用，所以不会被提前分配出去。
//!
//! 结束判定与标记相同：工人自己的队列空、也偷不到 ⇒ 空闲计数 +1；全部空闲 ⇒ 结束（只有工人会往队列里放）。
//!
//! 只在调用方线程上析构（[`crate::Heap::set_parallel_drop`] 关掉时）：别的工人照旧并行判死、清起点位、算行，
//! 但死对象不析构，连同扫好的块一起放进待办；调用方（线程池的 0 号工人）析构完才把块交出去 / 还给系统。
//! 析构里释放的内存多是 mutator 线程分配的，跨线程释放会让分配器（glibc 的 arena 锁）争用。
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};

use crossbeam_deque::{Injector, Steal, Worker};
use crossbeam_utils::{Backoff, CachePadded};

#[cfg(all(target_os = "linux", not(miri)))]
use crate::layout::LARGE_MMAP;
use crate::layout::{Block, Core, Finalize, GRANULE, Header, State};
use crate::mark::{Tracer, TracerInternal};
use crate::sync::Pool;

/// 单独分配的大对象（头指针）。
#[derive(Clone, Copy)]
pub struct Large(pub NonNull<Header>);

// SAFETY: 独立的堆内存；对象内容跨线程由 Trace: Send + Sync 担保
unsafe impl Send for Large {}
// SAFETY: 同上
unsafe impl Sync for Large {}

impl Large {
    pub fn layout(bytes: usize) -> Layout {
        Layout::from_size_align(bytes, GRANULE).expect("对象太大")
    }

    /// 分配 `bytes` 字节（至少有头）：不小于 [`LARGE_MMAP`] 的在 Linux 上单独映射（释放即还给系统），其余走全局分配器。
    pub fn alloc(bytes: usize) -> *mut u8 {
        #[cfg(all(target_os = "linux", not(miri)))]
        if bytes >= LARGE_MMAP {
            // SAFETY: 匿名私有映射，参数合法
            let p = unsafe { libc::mmap(core::ptr::null_mut(), bytes, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0) };
            if p == libc::MAP_FAILED {
                handle_alloc_error(Self::layout(bytes));
            }
            return p.cast();
        }
        let layout = Self::layout(bytes);
        // SAFETY: 大小非零（至少有头）
        let p = unsafe { alloc(layout) };
        if p.is_null() {
            handle_alloc_error(layout);
        }
        p
    }

    fn header(&self) -> &Header {
        // SAFETY: 大对象活着时头一直有效
        unsafe { self.0.as_ref() }
    }

    /// 析构并释放。SAFETY（调用方）：死了，没人会再碰它。
    unsafe fn free(self) {
        let bytes = self.header().bytes();
        // SAFETY: 没标记 ⇒ 不可达
        unsafe { Header::finalize(self.0.as_ptr()) };
        #[cfg(all(target_os = "linux", not(miri)))]
        if bytes >= LARGE_MMAP {
            // SAFETY: Large::alloc 按同样的字节数映射的
            unsafe { libc::munmap(self.0.as_ptr().cast(), bytes) };
            return;
        }
        // SAFETY: 内存来自 Large::alloc 按同一 layout 的分配
        unsafe { dealloc(self.0.as_ptr().cast(), Self::layout(bytes)) };
    }
}

/// 别的工人扫好、等调用方析构的项。
enum Pending {
    /// 扫好的块、它的状态、里面待析构的死对象（头地址）
    Block(Block, State, Vec<usize>),
    /// 死了的大对象
    Large(Large),
}

/// 扫完一个块的去向。
pub enum Outcome {
    /// 有空行（含全空），可以分配
    Usable(Block),
    /// 满了
    Full(Block),
    /// 全空且留够了备用：还给系统（死对象已析构，块里没有活对象）
    Release(Block),
}

/// 清扫完一项后交给谁。
pub trait Sink {
    fn block(&self, outcome: Outcome);
    fn large(&self, survivor: Large);
}

#[derive(Default)]
pub struct Sweeper {
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
    /// 只在调用方线程析构时：别的工人扫好、等调用方析构的项（处理完才算扫完）
    pending: Injector<Pending>,
}

impl Sweeper {
    /// 开始一轮（独占堆时）。上一轮必须已经扫完。
    pub fn start(&mut self, epoch: u32, blocks: Vec<Block>, large: Vec<Large>, spare: usize) {
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

    pub fn is_done(&self) -> bool {
        self.unfinished.load(Ordering::Acquire) == 0
    }

    /// 本轮到目前为止扫出的存活字节数（扫完才是全部）。
    pub fn live(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    /// 还没扫完的项数（估计回收线程数用）。
    pub fn unfinished(&self) -> usize {
        self.unfinished.load(Ordering::Relaxed)
    }

    /// 领一个没扫的块并扫掉。没有可领的了返回 None。领到的扫完、交出结果后要调 [`Sweeper::finished`]。
    pub fn sweep_next_block(&self) -> Option<Outcome> {
        let (b, state) = self.sweep_claimed(None)?;
        Some(self.settle(b, state))
    }

    /// 领一个没扫的块并扫掉（`deferred` 见 [`Block::sweep`]），返回块与状态。
    fn sweep_claimed(&self, deferred: Option<&mut Vec<usize>>) -> Option<(Block, State)> {
        let i = self.next_block.fetch_add(1, Ordering::AcqRel);
        let b = *self.blocks.get(i)?;
        // SAFETY: 本轮标记已完成；这个块只分给了这一次领用，扫完前没人分配进去
        let (state, live) = unsafe { b.sweep::<Header>(self.epoch, deferred) };
        self.live.fetch_add(live, Ordering::Relaxed);
        Some((b, state))
    }

    /// 扫好（死对象都已析构）的块的去向。
    fn settle(&self, b: Block, state: State) -> Outcome {
        match state {
            State::Full => Outcome::Full(b),
            State::Recyclable => Outcome::Usable(b),
            State::Empty if self.take_spare() => Outcome::Usable(b),
            State::Empty => Outcome::Release(b),
        }
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
    pub fn finished(&self) {
        self.unfinished.fetch_sub(1, Ordering::AcqRel);
    }

    /// 领一个没扫的大对象并扫掉；活着的交给 `sink`。没有可领的了返回 false。`defer`：死了不析构，放进待办。
    fn sweep_next_large(&self, sink: &impl Sink, defer: bool) -> bool {
        let i = self.next_large.fetch_add(1, Ordering::AcqRel);
        let Some(&l) = self.large.get(i) else { return false };
        if l.header().is_marked(self.epoch) {
            self.live.fetch_add(l.header().bytes(), Ordering::Relaxed);
            sink.large(l);
        } else if defer {
            self.pending.push(Pending::Large(l));
            return true;
        } else {
            // SAFETY: 本轮标记已完成、没标上 ⇒ 不可达；这个大对象只分给了这一次领用
            unsafe { l.free() };
        }
        self.finished();
        true
    }

    /// 回收线程：扫到没有可领的为止。`defer`：死对象不析构，连同块放进待办（只在调用方线程析构时的其他工人）；
    /// 调用方自己当场析构，每扫完一块顺手处理一批待办。
    fn work(&self, sink: &impl Sink, defer: bool, caller: bool) {
        let mut dead = Vec::new();
        while let Some((b, state)) = self.sweep_claimed(defer.then_some(&mut dead)) {
            if defer {
                self.pending.push(Pending::Block(b, state, core::mem::take(&mut dead)));
                continue;
            }
            sink.block(self.settle(b, state));
            self.finished();
            if caller {
                self.drain(sink);
            }
        }
        while self.sweep_next_large(sink, defer) {}
    }

    /// 调用方：析构待办里的死对象，再把块交出去。
    fn drain(&self, sink: &impl Sink) {
        loop {
            match self.pending.steal() {
                Steal::Success(Pending::Block(b, state, dead)) => {
                    for addr in dead {
                        // SAFETY: 扫块时判死的对象，还没析构；块交出去之前没人碰它
                        unsafe { <Header as Finalize>::finalize(core::ptr::with_exposed_provenance::<Core>(addr)) };
                    }
                    sink.block(self.settle(b, state));
                }
                // SAFETY: 判死的大对象，只在待办里
                Steal::Success(Pending::Large(l)) => unsafe { l.free() },
                Steal::Retry => continue,
                Steal::Empty => return,
            }
            self.finished();
        }
    }

    /// 用回收线程把本轮扫完，并等别人（惰性清扫的 mutator）手里领走的也扫完。
    /// `parallel_drop` 为假时死对象只在调用方线程上析构（见模块说明）。
    pub fn finish(&self, pool: &Pool, sink: &(impl Sink + Sync), parallel_drop: bool) {
        if self.is_done() {
            return;
        }
        pool.run(pool.sized(self.unfinished()), |me| self.work(sink, !parallel_drop && me != 0, me == 0));
        let backoff = Backoff::new();
        loop {
            self.drain(sink);
            if self.is_done() {
                return;
            }
            backoff.snooze();
        }
    }
}

pub struct Releaser<'h> {
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
    /// 只在调用方线程析构：别的工人归零、还没析构的对象（头地址）
    pending: Option<Injector<usize>>,
}

impl<'h> Releaser<'h> {
    /// `parallel_drop` 为假：归零的对象只在调用方线程上析构。
    pub fn new(decs: &'h Injector<usize>, dirty: &'h Injector<Block>, workers: usize, epoch: u32, parity: bool, parallel_drop: bool) -> Self {
        let pending = (!parallel_drop).then(Injector::new);
        Self { decs, dirty, workers: workers.max(1), idle: CachePadded::new(AtomicUsize::new(0)), epoch, parity, pending }
    }

    pub fn run(&self, pool: &Pool) {
        pool.run(self.workers, |me| self.work(if me == 0 { None } else { self.pending.as_ref() }));
        let Some(pending) = &self.pending else { return };
        loop {
            match pending.steal() {
                Steal::Success(addr) => self.free(core::ptr::with_exposed_provenance::<Header>(addr)),
                Steal::Retry => {}
                Steal::Empty => return,
            }
        }
    }

    fn work(&self, defer: Option<&Injector<usize>>) {
        let local = Worker::new_lifo();
        loop {
            while let Some(addr) = local.pop() {
                self.one(addr, &local, defer);
            }
            if let Some(addr) = self.steal(&local) {
                self.one(addr, &local, defer);
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
                    self.one(addr, &local, defer);
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
    /// 大对象、粘住的、年轻的不在这里释放（留给追踪 / 年轻清扫）。`defer`：边处理完后不析构，放进待办给调用方。
    fn one(&self, addr: usize, local: &Worker<usize>, defer: Option<&Injector<usize>>) {
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
        match defer {
            Some(pending) => pending.push(addr),
            None => self.free(header),
        }
    }

    /// 析构归零的对象、清起点位、记下脏块。
    fn free(&self, header: *const Header) {
        // SAFETY: 计数归零 ⇒ 没有计入的引用、mutator 碰不到（见模块说明）；只有归零的那个线程（或它交给的调用方）碰它
        unsafe { Header::finalize(header) };
        let block = Block::containing(header.cast());
        block.clear_start(header.cast());
        self.dirty.push(block);
    }
}
