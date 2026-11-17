//! 公开堆：共享内存文件上的堆。堆的全部状态（超级块、类型表、入口表、块状态表、块）都在窗口里，进程这一侧只有缓存。
//!
//! 第一期（单进程）：多个线程同时 mutate、碰撞分配；回收是暂停式的并行标记 + 并行清扫，不搬对象。握手字在超级块里，
//! 所以同一个堆被映射两次（同一进程里两个地址区，或以后跨进程）也互相排斥。
use core::any::type_name;
use core::cell::Cell;
use core::marker::PhantomData;
use core::num::NonZeroUsize;
use core::ptr::{NonNull, null_mut};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use core::time::Duration;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crossbeam_deque::{Injector, Steal};

use super::ptr::{PublicGcInternal, PublicInternal};
use crate::lockfree::SegVec;

use crate::block::{Block, State};
use crate::gc::Invariant;
use crate::header::{Core, MAX_EPOCH};
use crate::layout::{BLOCK, GRANULE, LARGE};
use crate::workers::Pool;

use super::layout::{ATTACH_CAP, ATTACH_OFF, Attachment, BlockState, Entry, Kind, Layout, MAP_CAP, PAGE, Segment, Super, TYPE_CAP, TypeEntry};
use super::mark::{Marker, Member};
use super::process::{FutexWord, Process};
use super::ptr::{EdgeMap, PHEADER, PHeader, Public, PublicBrand, PublicGc, Shared};
use super::region::{Region, WINDOW, WINDOWS};
use super::space::Space;

/// 等别人（回收方、mutate 里的线程）时每隔多久醒来查一次对方进程还在不在
const POLL: Duration = Duration::from_millis(10);
/// [`Attachment::pid`]：有人正在收拾这个死掉的挂接（防两个回收方重复还它的入口）
const REAPING: u32 = u32::MAX;
/// 回收方抢回收权前，等挡在门口的 mutator 进门最多等这么多轮（每轮 [`POLL`]）
const YIELD_ROUNDS: usize = 10;
/// 回收方等 mutate 走空最多等这么久，等不到就先放开请求、过一会儿重来：一个线程在 A 的 mutate 里又去进 B 时，
/// 两个回收方（或一起回收）分别挡着 A、B 会互相等死，放开请求就解开了
const PATIENCE: Duration = Duration::from_millis(50);
/// [`Entry::holder`] 的最高位：持有方是另一个公开堆（低位是它的堆号），不是某个挂接（槽号 + 1）
pub const HEAP_HOLDER: u32 = 1 << 31;
/// 清扫时每次领多少块
const SWEEP_BATCH: usize = 16;
/// 栈头低 32 位：栈顶序号 + 1（0 = 空）；高 32 位是改动次数（防 ABA）
const STACK_TOP: u64 = u32::MAX as u64;
/// 栈头里改动次数的位移
const STACK_TAG_SHIFT: u32 = u32::BITS;

/// 窗口里的无锁栈（可用块、还回来的入口）：头在超级块里，每个元素的「下一个」在它自己的表项里。
pub struct Stack<'a> {
    head: &'a AtomicU64,
}

impl Stack<'_> {
    pub fn push(&self, i: u32, next: &AtomicU32) {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            next.store((head & STACK_TOP) as u32, Ordering::Relaxed);
            let new = ((head >> STACK_TAG_SHIFT).wrapping_add(1) << STACK_TAG_SHIFT) | u64::from(i + 1);
            match self.head.compare_exchange_weak(head, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(now) => head = now,
            }
        }
    }

    pub fn pop<'n>(&self, next_of: impl Fn(u32) -> &'n AtomicU32) -> Option<u32> {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            let top = (head & STACK_TOP) as u32;
            let i = top.checked_sub(1)?;
            // 读到的可能是别人刚弹走又改过的「下一个」：那样头的改动次数也变了，下面的 CAS 会失败
            let next = next_of(i).load(Ordering::Relaxed);
            let new = ((head >> STACK_TAG_SHIFT).wrapping_add(1) << STACK_TAG_SHIFT) | u64::from(next);
            match self.head.compare_exchange_weak(head, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(i),
                Err(now) => head = now,
            }
        }
    }
}

/// 窗口背后是什么。
pub enum Backing {
    /// 共享内存文件系统上的文件；`marker` = 本句柄建的（堆号标记文件），析构时连同堆文件一起删（除非 persist）
    File { file: File, path: PathBuf, marker: Option<PathBuf> },
    /// 匿名共享内存（只在本进程）
    Anonymous,
}

impl Backing {
    /// 让窗口的 `[offset, offset + len)` 有内存可用（文件：延长；匿名：本来就有）。
    fn commit(&self, offset: usize, len: usize) {
        if let Self::File { file, .. } = self {
            Region::commit(file, offset, len).expect("公开堆所在的文件系统满了");
        }
    }
}

/// 类型键用的 64 位 FNV-1a。
struct Fnv(u64);

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
pub struct Inner {
    /// 地址区：窗口在它里面，Arc 让它比堆活得久
    space: Arc<Space>,
    index: usize,
    window: NonNull<u8>,
    layout: Layout,
    backing: Backing,
    threads: NonZeroUsize,
    pool: OnceLock<Pool>,
    /// 本进程见过的类型：（类型名的地址, 类型号）
    types: SegVec<(usize, u32)>,
    /// 给 mutate 复用的分配缓冲
    spare: AtomicPtr<Tlab>,
    /// 本句柄在挂接表里的槽号
    slot: usize,
    /// 建堆的句柄析构时保留文件（[`PublicHeap::persist`]）
    keep: AtomicBool,
}

// SAFETY: 窗口里的状态都是原子量或由握手保护；其余是线程安全的
unsafe impl Send for Inner {}
// SAFETY: 同上
unsafe impl Sync for Inner {}

impl Inner {
    /// 窗口 `index` 已经映射上 `backing`；`fresh` 时写新的超级块，否则核对。出错时析构负责还窗口。
    pub fn new(space: Arc<Space>, index: usize, backing: Backing, threads: NonZeroUsize, fresh: bool) -> io::Result<Self> {
        let window = NonNull::new(space.region().window(index)).expect("窗口地址非空");
        let mut me = Self { space, index, window, layout: Layout::new(), backing, threads, pool: OnceLock::new(), types: SegVec::default(), spare: AtomicPtr::new(null_mut()), slot: ATTACH_CAP, keep: AtomicBool::new(false) };
        if fresh {
            me.backing.commit(0, PAGE);
            // SAFETY: 新堆，窗口开头一页刚有内存，还没人读
            unsafe { me.window.as_ptr().cast::<Super>().write(Super::new(index as u64)) };
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
        loop {
            if !self.claim_collection(true) {
                return;
            }
            if self.drain() {
                break;
            }
            self.release();
        }
        self.reap_dead();
        Self::collect_members(&[self]);
        self.release();
    }

    /// 几个堆一起回收：按堆号从小到大拿每个堆的回收权，全部走空才动手，有一个等不到就全放开重来（不会和别的一起回收互相等死）。
    fn collect_group(members: &[&Self]) {
        loop {
            for m in members {
                m.claim_collection(false);
            }
            if members.iter().all(|m| m.drain()) {
                break;
            }
            for m in members.iter().rev() {
                m.release();
            }
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
        self.claim_entry_for(off, u32::try_from(self.slot + 1).expect("挂接槽号"))
    }

    /// 同 [`Inner::claim_entry`]，持有方是 `holder`（另一个公开堆：[`HEAP_HOLDER`] | 堆号）。
    pub fn claim_entry_for(&self, off: u64, holder: u32) -> u32 {
        let sup = self.sup();
        let i = Stack { head: &sup.free_entries }.pop(|i| &self.entry(i).next).unwrap_or_else(|| {
            let i = sup.entries.fetch_add(1, Ordering::AcqRel);
            assert!((i as usize) < self.layout.entry_cap, "公开堆的入口表满了");
            self.commit(Segment::Entries, self.layout.entries + (i as usize + 1) * size_of::<Entry>());
            i
        });
        let e = self.entry(i);
        e.holder.store(holder, Ordering::Relaxed);
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
        Stack { head: &self.sup().free_entries }.push(i, &e.next);
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
        let i = Stack { head: &sup.available }.pop(|i| &self.state(i).next).unwrap_or_else(|| {
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
        let group: Vec<u32> = members.iter().map(|m| HEAP_HOLDER | u32::try_from(m.index).expect("堆号")).collect();
        let in_group = |holder: u32| members.len() > 1 && group.contains(&holder);
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
            for i in 0..sup.entries.load(Ordering::Acquire) {
                let e = heap.entry(i);
                let off = e.ptr.load(Ordering::Acquire);
                if off != 0 && !in_group(e.holder.load(Ordering::Acquire)) {
                    roots.push((m, PHeader::of(heap.ptr(off)).expose_provenance()));
                }
            }
        }
        let marked: Vec<Member<'_>> = members.iter().zip(&epochs).map(|(&heap, &epoch)| Member { heap, epoch }).collect();
        let found = Injector::new();
        let pool = members[0].pool();
        Marker::new(&marked, (members.len() > 1).then_some(&found), pool.sized(blocks)).run(pool, &roots);
        if members.len() > 1 {
            Self::reregister(members, &group, &found);
        }
        for (heap, epoch) in members.iter().zip(epochs) {
            heap.sweep_all(epoch);
        }
    }

    /// 一起回收标完之后：组内跨堆登记换成这次追踪到的（每对「来源堆, 目标」一项）。先登记新的、再还旧的。
    fn reregister(members: &[&Self], group: &[u32], found: &Injector<(usize, u64)>) {
        let old: Vec<Vec<u32>> = members
            .iter()
            .map(|heap| (0..heap.sup().entries.load(Ordering::Acquire)).filter(|&i| group.contains(&heap.entry(i).holder.load(Ordering::Acquire))).collect())
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
            target.claim_entry_for(off, group[source]);
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
                            Stack { head: &self.sup().available }.push(i, &self.state(i).next);
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
            Stack { head: &self.sup().available }.push(j, &self.state(j).next);
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // 句柄都已析构（入口都还了），没有 mutator：空出挂接槽
        if self.slot < ATTACH_CAP {
            let a = self.attachment(self.slot);
            a.start.store(0, Ordering::Relaxed);
            a.pid.store(0, Ordering::Release);
        }
        // SAFETY: 最后一个引用：没有 mutator、句柄都已析构
        unsafe { self.space.region().unmap(self.index) };
        self.space.release(self.index);
        if let Backing::File { path, marker: Some(marker), .. } = &self.backing
            && !self.keep.load(Ordering::Acquire)
        {
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(marker);
        }
        let spare = *self.spare.get_mut();
        if !spare.is_null() {
            // SAFETY: Box::into_raw 来的
            drop(unsafe { Box::from_raw(spare) });
        }
    }
}

/// 公开堆。多个线程可以同时 [`PublicHeap::mutate`]；同一个堆在别处（另一个地址区、以后另一个进程）也映射着时，握手照样互相排斥。
pub struct PublicHeap(Arc<Inner>);

impl From<Inner> for PublicHeap {
    fn from(inner: Inner) -> Self {
        Self(Arc::new(inner))
    }
}

/// 公开堆的统计。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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

impl PublicHeap {
    /// 堆号（= 窗口号）。
    pub fn id(&self) -> usize {
        self.0.index
    }

    /// 建堆的句柄析构时保留堆文件与堆号标记（主进程退出或重启后，别的进程照样 [`crate::PublicSpace::open`] 回来接着用）。
    pub fn persist(&self) {
        self.0.keep.store(true, Ordering::Release);
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
            tlab.adopt(&self.0);
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
        let mut members: Vec<&Inner> = heaps.iter().map(|h| &*h.0).collect();
        members.sort_by_key(|m| m.index);
        assert!(members.windows(2).all(|w| w[0].index != w[1].index), "同一个公开堆不能在组里出现两次");
        if !members.is_empty() {
            Inner::collect_group(&members);
        }
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
pub trait PublicHeapInternal {
    fn is(&self, inner: &Arc<Inner>) -> bool;
}

impl PublicHeapInternal for PublicHeap {
    fn is(&self, inner: &Arc<Inner>) -> bool {
        Arc::ptr_eq(&self.0, inner)
    }
}


/// 在 mutate 里：本挂接的计数加一，出门减一（panic 也减）。
///
/// 进门先加计数、再看回收请求，回收方先挂请求、再看计数（都是 SeqCst）：两边至少有一方看到对方，所以不会有
/// mutator 在回收进行时溜进来。看到请求就退回去等；回收方死了（请求一直挂着）就自己接手把回收做完。
struct Gate<'h> {
    inside: &'h AtomicU32,
    request: &'h AtomicU32,
}

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

impl Drop for Gate<'_> {
    fn drop(&mut self) {
        self.inside.fetch_sub(1, Ordering::SeqCst);
        if self.request.load(Ordering::SeqCst) != 0 {
            self.inside.wake_all();
        }
    }
}

/// 线程的分配缓冲：当前块、在块里找到哪一行、当前空洞的游标与界，外加上一次分配的类型号。
pub struct Tlab {
    /// (所属堆, 那时的回收次数)；对不上就作废
    owner: Cell<(usize, u64)>,
    block: Cell<Option<Block>>,
    line: Cell<usize>,
    cursor: Cell<*mut u8>,
    limit: Cell<*mut u8>,
    /// （类型名的地址, 类型号）
    last: Cell<Option<(usize, u32)>>,
}

impl Default for Tlab {
    fn default() -> Self {
        Self { owner: Cell::default(), block: Cell::default(), line: Cell::default(), cursor: Cell::new(null_mut()), limit: Cell::new(null_mut()), last: Cell::default() }
    }
}

impl Tlab {
    /// 换了堆或堆回收过：之前的块已经被清扫重新分类，丢掉。
    fn adopt(&mut self, heap: &Arc<Inner>) {
        let owner = (Arc::as_ptr(heap).addr(), heap.sup().collections.load(Ordering::Acquire));
        if self.owner.get() != owner {
            let last = if self.owner.get().0 == owner.0 { self.last.get() } else { None };
            *self = Self { owner: Cell::new(owner), last: Cell::new(last), ..Self::default() };
        }
    }
}

/// 一次公开堆 mutate 里的分配器与品牌；只在调用线程上用。
pub struct PublicMutator<'p> {
    heap: &'p Arc<Inner>,
    tlab: &'p Tlab,
    /// 上次完成的纪元（空行判定）
    epoch: u32,
    _brand: Invariant<'p>,
}

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
        let t = self.tlab;
        loop {
            let cursor = t.cursor.get();
            if t.limit.get().addr() - cursor.addr() >= bytes {
                t.cursor.set(cursor.wrapping_add(bytes));
                return cursor;
            }
            self.refill();
        }
    }

    /// 当前空洞放不下：找当前块的下一个空洞，没有了就领新块。
    fn refill(&self) {
        let t = self.tlab;
        if let Some(b) = t.block.get()
            && let Some((start, end)) = b.hole(t.line.get(), self.epoch)
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

/// 只给本 crate 用的操作：trait 在私有模块里、不导出，crate 外拿不到。
pub trait PublicMutatorInternal {
    fn is(&self, inner: &Arc<Inner>) -> bool;

    /// 公开偏移在本进程里的地址。
    fn at(&self, off: u64) -> *mut u8;

    /// 堆号。
    fn index(&self) -> usize;

    /// 给别的堆持有的引用登记一个入口（持有方 `holder`）。
    fn register(&self, off: u64, holder: u32);
}

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

    fn register(&self, off: u64, holder: u32) {
        self.heap.claim_entry_for(off, holder);
    }
}

