//! 公开堆：共享内存文件上的堆。堆的全部状态（超级块、类型表、入口表、块状态表、块）都在窗口里，进程这一侧只有缓存。
//!
//! 第一期（单进程）：多个线程同时 mutate、碰撞分配；回收是暂停式的并行标记 + 并行清扫，不搬对象。握手字在超级块里，
//! 所以同一个堆被映射两次（同一进程里两个地址区，或以后跨进程）也互相排斥。
use core::any::type_name;
use core::cell::Cell;
use core::marker::PhantomData;
use core::num::NonZeroUsize;
use core::ptr::{NonNull, null_mut};
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crossbeam_utils::Backoff;
use crate::lockfree::SegVec;

use crate::block::{Block, State};
use crate::gc::Invariant;
use crate::header::{Core, MAX_EPOCH};
use crate::layout::{BLOCK, GRANULE, LARGE};
use crate::workers::Pool;

use super::layout::{BlockState, Entry, Kind, Layout, MAP_CAP, PAGE, Segment, Super, TYPE_CAP, TypeEntry};
use super::mark::Marker;
use super::ptr::{EdgeMap, PHEADER, PHeader, Public, PublicBrand, PublicGc, Shared};
use super::region::{Region, WINDOW};
use super::space::Space;

/// 握手字的最高位：要回收，新的 mutate 先等
const REQUEST: u64 = 1 << 63;
/// 清扫时每次领多少块
const SWEEP_BATCH: usize = 16;
/// 栈头低 32 位：栈顶序号 + 1（0 = 空）；高 32 位是改动次数（防 ABA）
const STACK_TOP: u64 = u32::MAX as u64;
/// 栈头里改动次数的位移
const STACK_TAG_SHIFT: u32 = u32::BITS;

/// 窗口里的无锁栈（可用块、还回来的入口）：头在超级块里，每个元素的「下一个」在它自己的表项里。
pub(crate) struct Stack<'a> {
    head: &'a AtomicU64,
}

impl Stack<'_> {
    pub(crate) fn push(&self, i: u32, next: &AtomicU32) {
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

    pub(crate) fn pop<'n>(&self, next_of: impl Fn(u32) -> &'n AtomicU32) -> Option<u32> {
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
pub(crate) enum Backing {
    /// 共享内存文件系统上的文件；`owner` = 本句柄建的，析构时删文件
    File { file: File, path: PathBuf, owner: bool },
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
        let mut map = EdgeMap(Vec::new());
        T::edges(&mut map, 0);
        let key = Self::key(type_name::<T>(), size_of::<T>(), &map.0);
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
pub(crate) struct Inner {
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
}

// SAFETY: 窗口里的状态都是原子量或由握手保护；其余是线程安全的
unsafe impl Send for Inner {}
// SAFETY: 同上
unsafe impl Sync for Inner {}

impl Inner {
    /// 窗口 `index` 已经映射上 `backing`；`fresh` 时写新的超级块，否则核对。出错时析构负责还窗口。
    pub(crate) fn new(space: Arc<Space>, index: usize, backing: Backing, threads: NonZeroUsize, fresh: bool) -> io::Result<Self> {
        let window = NonNull::new(space.region().window(index)).expect("窗口地址非空");
        let me = Self { space, index, window, layout: Layout::new(), backing, threads, pool: OnceLock::new(), types: SegVec::default(), spare: AtomicPtr::new(null_mut()) };
        if fresh {
            me.backing.commit(0, PAGE);
            // SAFETY: 新堆，窗口开头一页刚有内存，还没人读
            unsafe { me.window.as_ptr().cast::<Super>().write(Super::new(index as u64)) };
        } else if me.sup().heap.load(Ordering::Acquire) != index as u64 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "超级块里的堆号与窗口不符"));
        }
        Ok(me)
    }

    pub(crate) fn sup(&self) -> &Super {
        // SAFETY: 窗口开头一页在建堆时就有了，里面是超级块（全是原子量）
        unsafe { &*self.window.as_ptr().cast() }
    }

    fn at<T>(&self, offset: usize) -> *mut T {
        self.window.as_ptr().wrapping_add(offset).cast()
    }

    /// 公开偏移在本进程里的地址。
    pub(crate) fn ptr(&self, off: u64) -> *mut u8 {
        self.space.region().base().as_ptr().wrapping_add(usize::try_from(off).expect("公开偏移"))
    }

    /// 公开偏移落在这个堆的窗口里。
    pub(crate) fn owns(&self, off: u64) -> bool {
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
        self.backing.commit(from, to - from);
        committed.fetch_max(to as u64, Ordering::AcqRel);
    }

    fn type_entry(&self, ty: u32) -> &TypeEntry {
        // SAFETY: 序号来自类型表游标，那一项所在的页已经延长过；表项全是原子量
        unsafe { &*self.at::<TypeEntry>(self.layout.types).add(ty as usize) }
    }

    /// 类型 `ty` 的指针图。
    pub(crate) fn edges(&self, ty: u32) -> &[u32] {
        let e = self.type_entry(ty);
        let (map, n) = (e.map.load(Ordering::Relaxed) as usize, e.edges.load(Ordering::Relaxed) as usize);
        // SAFETY: 写完指针图才置 ready，读的一侧（分配过这个类型的对象之后的回收）在它之后
        unsafe { core::slice::from_raw_parts(self.at::<u32>(self.layout.map).add(map), n) }
    }

    pub(crate) fn has_edges(&self, ty: u32) -> bool {
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
        let n = u32::try_from(map.0.len()).expect("指针槽个数");
        let start = sup.map.fetch_add(n, Ordering::AcqRel);
        assert!((start + n) as usize <= MAP_CAP, "公开堆的指针图满了");
        self.commit(Segment::Types, self.layout.types + (ty as usize + 1) * size_of::<TypeEntry>());
        self.commit(Segment::Map, self.layout.map + (start + n) as usize * size_of::<u32>());
        // SAFETY: [start, start + n) 是本线程刚领的、已经有内存的指针图区
        unsafe { self.at::<u32>(self.layout.map).add(start as usize).copy_from_nonoverlapping(map.0.as_ptr(), map.0.len()) };
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

    /// 领一个入口放公开偏移 `off`（只在这个堆的 mutate 里调用）。
    pub(crate) fn claim_entry(&self, off: u64) -> u32 {
        let sup = self.sup();
        let i = Stack { head: &sup.free_entries }.pop(|i| &self.entry(i).next).unwrap_or_else(|| {
            let i = sup.entries.fetch_add(1, Ordering::AcqRel);
            assert!((i as usize) < self.layout.entry_cap, "公开堆的入口表满了");
            self.commit(Segment::Entries, self.layout.entries + (i as usize + 1) * size_of::<Entry>());
            i
        });
        let e = self.entry(i);
        e.holder.store(0, Ordering::Relaxed);
        e.ptr.store(off, Ordering::Release);
        i
    }

    /// 句柄析构：清空入口、还回栈里（任何时候、任何线程）。
    pub(crate) fn release_entry(&self, i: u32) {
        let e = self.entry(i);
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
    fn alloc_large(&self, bytes: usize) -> *mut u8 {
        let n = bytes.div_ceil(BLOCK);
        let i = self.open_blocks(n);
        self.sup().allocated.fetch_add((n * BLOCK) as u64, Ordering::Relaxed);
        for j in i + 1..i + u32::try_from(n).expect("块数") {
            self.state(j).kind.store(Kind::LargeTail as u32, Ordering::Release);
        }
        self.state(i).kind.store(Kind::LargeHead as u32, Ordering::Release);
        self.block(i).line(0)
    }

    /// 暂停式回收（握手已经把 mutator 都挡在外面）：从入口表出发并行标记，再并行清扫。
    fn collect_exclusive(&self) {
        let sup = self.sup();
        let epoch = sup.epoch.load(Ordering::Acquire).checked_add(1).filter(|&e| e <= MAX_EPOCH).expect("纪元用完了");
        let blocks = sup.blocks.load(Ordering::Acquire) as usize;
        let roots: Vec<usize> = (0..sup.entries.load(Ordering::Acquire))
            .filter_map(|i| {
                let off = self.entry(i).ptr.load(Ordering::Acquire);
                (off != 0).then(|| PHeader::of(self.ptr(off)).expose_provenance())
            })
            .collect();
        let pool = self.pool();
        let workers = pool.sized(blocks);
        Marker::new(self, epoch, workers).run(pool, &roots);
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

    /// 拆掉一个死掉的大对象：它占的块元数据清零、改成小对象块，压进可用栈。
    fn free_large(&self, head: u32) {
        // SAFETY: 大对象的头在第一块开头，还没拆
        let n = unsafe { &*self.block(head).line(0).cast::<Core>() }.bytes().div_ceil(BLOCK);
        for j in head..head + u32::try_from(n).expect("块数") {
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
        // SAFETY: 最后一个引用：没有 mutator、句柄都已析构
        unsafe { self.space.region().unmap(self.index) };
        self.space.release(self.index);
        if let Backing::File { path, owner: true, .. } = &self.backing {
            let _ = std::fs::remove_file(path);
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
}

impl PublicHeap {
    /// 堆号（= 窗口号）。
    pub fn id(&self) -> usize {
        self.0.index
    }

    pub(crate) fn is(&self, inner: &Arc<Inner>) -> bool {
        Arc::ptr_eq(&self.0, inner)
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
            let _gate = Gate::enter(self.0.sup());
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

    /// 暂停式回收：挂上请求、等所有地方的 mutate 走空，并行标记 + 并行清扫。别人正在回收就等它做完返回。
    /// 不要在这个堆的 mutate 里调用（会一直等自己走空）。
    pub fn collect(&self) {
        let sup = self.0.sup();
        let backoff = Backoff::new();
        if sup.gate.fetch_or(REQUEST, Ordering::AcqRel) & REQUEST != 0 {
            while sup.gate.load(Ordering::Acquire) & REQUEST != 0 {
                backoff.snooze();
            }
            return;
        }
        while sup.gate.load(Ordering::Acquire) & !REQUEST != 0 {
            backoff.snooze();
        }
        self.0.collect_exclusive();
        sup.gate.fetch_and(!REQUEST, Ordering::AcqRel);
    }

    pub fn stats(&self) -> PublicStats {
        let sup = self.0.sup();
        PublicStats {
            live: usize::try_from(sup.live.load(Ordering::Relaxed)).unwrap_or(usize::MAX),
            allocated: usize::try_from(sup.allocated.load(Ordering::Relaxed)).unwrap_or(usize::MAX),
            blocks: sup.blocks.load(Ordering::Relaxed) as usize,
            collections: sup.collections.load(Ordering::Relaxed),
        }
    }
}

/// 在 mutate 里：超级块握手字进门计数加一（有回收请求就先等），出门减一（panic 也减）。
struct Gate<'h>(&'h Super);

impl<'h> Gate<'h> {
    fn enter(sup: &'h Super) -> Self {
        let backoff = Backoff::new();
        let mut word = sup.gate.load(Ordering::Acquire);
        loop {
            if word & REQUEST != 0 {
                backoff.snooze();
                word = sup.gate.load(Ordering::Acquire);
                continue;
            }
            match sup.gate.compare_exchange_weak(word, word + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Self(sup),
                Err(now) => word = now,
            }
        }
    }
}

impl Drop for Gate<'_> {
    fn drop(&mut self) {
        self.0.gate.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 线程的分配缓冲：当前块、在块里找到哪一行、当前空洞的游标与界，外加上一次分配的类型号。
pub(crate) struct Tlab {
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
    pub(crate) fn is(&self, inner: &Arc<Inner>) -> bool {
        Arc::ptr_eq(self.heap, inner)
    }

    /// 公开偏移在本进程里的地址。
    pub(crate) fn at(&self, off: u64) -> *mut u8 {
        self.heap.ptr(off)
    }

    /// 分配一个对象。类型不能有析构（编译期检查）。
    pub fn alloc<T: Shared>(&self, value: T) -> PublicGc<'p, T> {
        const {
            assert!(!core::mem::needs_drop::<T>(), "公开堆的对象不能有析构");
            assert!(align_of::<T>() <= GRANULE, "对象数据的对齐不能超过 8 字节");
        };
        let bytes = (PHEADER + size_of::<T>()).next_multiple_of(GRANULE);
        let ty = self.heap.type_of::<T>(self.tlab);
        let large = bytes > LARGE;
        let p = if large { self.heap.alloc_large(bytes) } else { self.alloc_small(bytes) };
        // SAFETY: p 是 bytes 字节、按粒度对齐的新内存，归这次分配独有
        unsafe {
            p.cast::<PHeader>().write(PHeader::new(bytes, large, ty));
            let data = PHeader::data(p.cast());
            data.cast::<T>().write(value);
            PublicGc::from_data(data)
        }
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
