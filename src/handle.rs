//! 指针与句柄：私有堆的 [`Gc`] / [`Slot`] / [`Root`] / [`Far`]，公开堆的 [`PublicGc`] / [`PublicSlot`] / [`PublicRef`] / [`Public`] 与 [`Shared`]。
//!
//! 堆指针 [`Gc`] 与原子指针槽 [`Slot`]。
//!
//! 根：[`Root`] 是 mutate 之外也能持有的强引用句柄（可跨线程），回收从全部根出发。
//!
//! 根表是只追加的槽数组：新根在 mutate 里领槽（先用释放过的空槽），句柄析构时把槽清空、下标挂回去。
//! 空槽只在回收时（没有 mutator）整理成只读数组，mutate 期间原子游标领取，所以无锁、没有 ABA。
//!
//! 跨堆引用 [`Far`]：同一进程里，一个堆的对象直接指着另一个堆的对象，零拷贝、不经过间接表。
//!
//! - 建的时候在目标堆的根表（也是它的入口表）领一个槽、记下持有方堆；持有它的对象析构时还槽。
//!   目标堆单独回收时入口就是根，而且入口对象所在的块不疏散（别的堆存的是直接指针，改不到）。
//! - 品牌 `'a` 是持有方堆那次 mutate 的品牌：`Far` 只能放进那个堆的对象里，逃不出 mutate。
//! - 读要带目标堆的 [`Mutator`]：证明此刻在目标堆的 mutate 里（对象不会被清扫或搬走）。
//! - 跨堆的环（A 的对象经 Far 指向 B、B 的又指回 A）单独回收收不掉，用 [`crate::Heap::collect_group`] 一起回收。
//!
//! 公开堆里的类型与指针：对象头、[`Shared`]（能放进公开堆的类型）、[`PublicGc`]、[`PublicSlot`]、[`Public`]。
//!
//! 公开堆的对象可能被别的进程读、被不认识这个类型的进程回收，所以：头里不存代码，只存类型号；回收照类型表里的指针图找边；
//! 对象不许有析构；指针一律存「相对公开地址区起点的偏移」，同一个值在每个进程里都指向同一个对象。
use core::fmt;
use core::cell::{Cell, UnsafeCell};
use core::marker::PhantomData;
#[cfg(target_os = "linux")]
use core::num::{NonZeroI16, NonZeroI32, NonZeroI64, NonZeroI8, NonZeroU16, NonZeroU32, NonZeroU64, NonZeroU8};
use core::ops::Deref;
use core::ptr::{NonNull, null_mut};
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
#[cfg(target_os = "linux")]
use core::sync::atomic::{AtomicBool, AtomicI16, AtomicI32, AtomicI64, AtomicI8, AtomicU16, AtomicU32, AtomicU64, AtomicU8};
use std::sync::Arc;

use crate::{Heap, Mutator, Trace, Tracer};
use crate::heap::{HeapInternal, MutatorInternal};
#[cfg(target_os = "linux")]
use crate::heap::{Inner, PublicHeap, PublicHeapInternal, PublicMutator, PublicMutatorInternal};
use crate::layout::Header;
use crate::mark::TracerInternal;
#[cfg(target_os = "linux")]
use crate::shm::Region;
use crate::sync::{PushList, SegVec};

/// 品牌：`'gc` 不变（invariant），`Gc` 逃不出 [`crate::Heap::mutate`] 的闭包。
pub type Invariant<'gc> = PhantomData<Cell<&'gc ()>>;

/// Gc 的品牌：借着 `&'gc T`，再加上对 `'gc` 不变
type GcBrand<'gc, T> = PhantomData<(&'gc T, fn(&'gc ()) -> &'gc ())>;

/// 指向堆对象的指针，只在一次 `mutate` 里有效（`'gc` 是那次调用的品牌）。对象只在暂停式回收（两次 mutate 之间）时可能被搬走。
///
/// 布局保证就是一个 `NonNull<T>`（`#[repr(transparent)]`），所以 `Option<Gc<'_, T>>` 也是一个指针大小。
#[repr(transparent)]
pub struct Gc<'gc, T> {
    ptr: NonNull<T>,
    _brand: GcBrand<'gc, T>,
}

// SAFETY: Gc 只给出 &T，T: Trace 要求 Send + Sync
unsafe impl<T: Send + Sync> Send for Gc<'_, T> {}
// SAFETY: 同上
unsafe impl<T: Send + Sync> Sync for Gc<'_, T> {}

impl<T> Clone for Gc<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Gc<'_, T> {}

/// [`Gc`] 只给本 crate 用的操作：trait 在私有模块里、不导出，crate 外拿不到。
pub trait GcInternal {
    type Data;

    /// # Safety
    /// `ptr` 是本堆里一个活对象的数据指针，品牌正确。
    unsafe fn from_data(ptr: *mut Self::Data) -> Self;

    fn header(self) -> *const Header;
}

impl<T> GcInternal for Gc<'_, T> {
    type Data = T;

    unsafe fn from_data(ptr: *mut T) -> Self {
        // SAFETY: 见 trait 上的说明
        Self { ptr: unsafe { NonNull::new_unchecked(ptr) }, _brand: PhantomData }
    }

    fn header(self) -> *const Header {
        Header::of(self.ptr.as_ptr().cast())
    }
}

impl<'gc, T> Gc<'gc, T> {
    pub fn as_ptr(self) -> *const T {
        self.ptr.as_ptr()
    }

    /// 由数据指针（[`Gc::as_ptr`] 的结果）取回 `Gc`，比如从 NaN-boxing 的值里解出来。
    ///
    /// # Safety
    /// `ptr` 必须是本堆里一个活对象的数据指针，而且是在当前这次 `mutate`（品牌 `'gc`）里由 [`Gc::as_ptr`] 得到的：
    /// 暂停式回收会搬对象，上一次 mutate 里拿到的地址这一次可能已经失效。
    ///
    /// 把这样的裸地址存进堆对象里时，回收器看不见它：对象的 [`crate::Trace`] 实现要用 `from_raw` 把它变回 `Gc` 报给
    /// [`crate::Tracer::edge`]（追踪），并在 `relocate` 里交给 [`crate::Tracer::relocate`]、把返回的新地址写回去（疏散会搬对象）；
    /// 否则对象会被当垃圾回收或搬走后留下悬垂地址。
    pub unsafe fn from_raw(ptr: *const T) -> Self {
        // SAFETY: 见上；数据指针非空
        unsafe { Self::from_data(ptr.cast_mut()) }
    }

    /// 在整个 `'gc` 里有效的引用。
    pub fn get(self) -> &'gc T {
        // SAFETY: 回收只在 mutate 之外，'gc 期间对象一直活着且不动
        unsafe { self.ptr.as_ref() }
    }

    pub fn ptr_eq(a: Self, b: Self) -> bool {
        a.ptr == b.ptr
    }
}

impl<T> Deref for Gc<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: 同 get
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: fmt::Debug> fmt::Debug for Gc<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// 原子的可空指针槽：对象里要改的边放这里（`Trace` 要求 `Sync`，所以 `Cell<Gc>` 用不了）。
///
/// 写入要带 [`Mutator`]：并发标记期间被覆盖的旧值要经写屏障交给回收器（初始快照）；
/// 平时老对象里的槽每轮第一次被写时登记，回收时按「新值 +1、上次计入的值 −1」结算引用计数。
#[repr(C)]
pub struct Slot<'gc, T> {
    ptr: AtomicPtr<T>,
    /// 上次计入引用计数的值（地址）| [`LOGGED`]；[`UNCOUNTED`] = 不计数（年轻对象里的槽，晋升时才开始计）
    counted: AtomicUsize,
    _brand: PhantomData<Gc<'gc, T>>,
}

/// [`Slot::counted`]：这个槽不在引用计数里
pub const UNCOUNTED: usize = usize::MAX;
/// [`Slot::counted`] 的最低位：本轮已登记（地址按粒度对齐，最低位空着）
pub const LOGGED: usize = 1;

/// 抹掉类型的槽：写屏障登记、回收时结算用（与 [`Slot`] 同布局）。
#[repr(C)]
pub struct RawSlot {
    pub ptr: AtomicPtr<u8>,
    pub counted: AtomicUsize,
}

impl RawSlot {
    fn header(p: *mut u8) -> Option<*const Header> {
        (!p.is_null()).then(|| Header::of(p))
    }

    fn from_counted(c: usize) -> Option<*const Header> {
        (c != UNCOUNTED && c & !LOGGED != 0).then(|| Header::of(core::ptr::with_exposed_provenance::<u8>(c & !LOGGED)))
    }

    /// 本轮第一次被写：打上登记标志，返回 true（调用方把槽地址交给回收器）。不计数的槽返回 false。
    pub fn log(&self) -> bool {
        let c = self.counted.load(Ordering::Relaxed);
        c != UNCOUNTED && c & LOGGED == 0 && self.counted.compare_exchange(c, c | LOGGED, Ordering::AcqRel, Ordering::Relaxed).is_ok()
    }

    /// 清掉登记标志（计数不可信时丢弃登记）。
    pub fn unlog(&self) {
        self.counted.fetch_and(!LOGGED, Ordering::AcqRel);
    }

    /// 改成计入当前值；返回（当前值, 上次计入的值）。
    pub fn recount(&self) -> (Option<*const Header>, Option<*const Header>) {
        let p = self.ptr.load(Ordering::Acquire);
        let prev = self.counted.swap(p.expose_provenance(), Ordering::AcqRel);
        (Self::header(p), Self::from_counted(prev))
    }

    /// 这个槽当前的值。
    pub fn current(&self) -> Option<*const Header> {
        Self::header(self.ptr.load(Ordering::Acquire))
    }

    /// 上次计入的值（释放它所在的对象时 −1）。
    pub fn counted(&self) -> Option<*const Header> {
        Self::from_counted(self.counted.load(Ordering::Acquire))
    }
}

impl<T> Default for Slot<'_, T> {
    fn default() -> Self {
        Self { ptr: AtomicPtr::new(null_mut()), counted: AtomicUsize::new(UNCOUNTED), _brand: PhantomData }
    }
}

impl<'gc, T> From<Option<Gc<'gc, T>>> for Slot<'gc, T> {
    fn from(v: Option<Gc<'gc, T>>) -> Self {
        Self { ptr: AtomicPtr::new(Self::raw(v)), counted: AtomicUsize::new(UNCOUNTED), _brand: PhantomData }
    }
}

impl<'gc, T> From<Gc<'gc, T>> for Slot<'gc, T> {
    fn from(v: Gc<'gc, T>) -> Self {
        Some(v).into()
    }
}

impl<'gc, T> Slot<'gc, T> {
    fn raw(v: Option<Gc<'gc, T>>) -> *mut T {
        v.map_or(null_mut(), |g| g.ptr.as_ptr())
    }

    fn wrap(p: *mut T) -> Option<Gc<'gc, T>> {
        // SAFETY: 槽里只存过同品牌的 Gc，对象被槽所在的对象引用着
        (!p.is_null()).then(|| unsafe { Gc::from_data(p) })
    }

    pub fn get(&self) -> Option<Gc<'gc, T>> {
        Self::wrap(self.ptr.load(Ordering::Acquire))
    }

    pub fn set(&self, m: &Mutator<'gc>, v: Option<Gc<'gc, T>>) {
        self.swap(m, v);
    }

    pub fn swap(&self, m: &Mutator<'gc>, v: Option<Gc<'gc, T>>) -> Option<Gc<'gc, T>> {
        let new = Self::raw(v);
        let old = self.ptr.swap(new, Ordering::AcqRel);
        self.barrier(m, old, new);
        Self::wrap(old)
    }

    /// 槽里是 `current` 才换成 `new`；失败时返回槽里的值。
    pub fn compare_exchange(&self, m: &Mutator<'gc>, current: Option<Gc<'gc, T>>, new: Option<Gc<'gc, T>>) -> Result<Option<Gc<'gc, T>>, Option<Gc<'gc, T>>> {
        let new = Self::raw(new);
        let done = self.ptr.compare_exchange(Self::raw(current), new, Ordering::AcqRel, Ordering::Acquire);
        if let Ok(old) = done {
            self.barrier(m, old, new);
        }
        done.map(Self::wrap).map_err(Self::wrap)
    }

    /// 写屏障：被覆盖的旧值（并发标记的初始快照）、写进去的新值（年轻对象 ⇒ 记下这个槽，年轻回收从它出发）、
    /// 引用计数登记（老对象里的槽本轮第一次被写）。
    fn barrier(&self, m: &Mutator<'gc>, old: *mut T, new: *mut T) {
        let header = |p: *mut T| (!p.is_null()).then(|| Header::of(p.cast()));
        m.written(self.raw_slot(), header(old), header(new));
    }
}

/// [`Slot`] 只给本 crate 用的操作（同 [`GcInternal`]）。
pub trait SlotInternal<'gc> {
    type Data;

    /// 独占时改写（疏散回收改成新地址）。
    fn store_exclusive(&mut self, v: Option<Gc<'gc, Self::Data>>);

    /// 抹掉类型的视图。
    fn raw_slot(&self) -> &RawSlot;
}

impl<'gc, T> SlotInternal<'gc> for Slot<'gc, T> {
    type Data = T;

    fn store_exclusive(&mut self, v: Option<Gc<'gc, T>>) {
        *self.ptr.get_mut() = Self::raw(v);
    }

    fn raw_slot(&self) -> &RawSlot {
        // SAFETY: Slot 与 RawSlot 都是 repr(C)，前两个字段同类型同顺序（AtomicPtr<T> 与 AtomicPtr<u8> 同布局）
        unsafe { &*(&raw const *self).cast::<RawSlot>() }
    }
}

impl<T: fmt::Debug> fmt::Debug for Slot<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Slot").field(&self.get()).finish()
    }
}

/// 把带 `'gc` 的类型换个品牌：`Root<B>` 不带生命周期，取出来时再套上当次 mutate 的 `'gc`。
/// 用 [`brand!`](crate::brand) 声明。
pub trait Brand {
    type Of<'gc>: Trace + 'gc;
}

/// 声明一个品牌类型：`brand!(pub NodeB = for<'gc> Node<'gc>);`
#[macro_export]
macro_rules! brand {
    ($(#[$meta:meta])* $vis:vis $name:ident = for<$l:lifetime> $ty:ty) => {
        $(#[$meta])*
        $vis struct $name;
        impl $crate::Brand for $name {
            type Of<$l> = $ty;
        }
    };
}

/// 根槽：对象头指针（空槽是 null）+ 持有方。
#[derive(Default)]
pub struct Entry {
    ptr: AtomicPtr<Header>,
    /// 持有这个根的堆（[`crate::Far`] 所在的堆的身份）；普通 [`Root`] 是 [`NO_HOLDER`]
    holder: AtomicUsize,
}

/// [`Entry::holder`]：不属于任何堆（普通 [`Root`]）
pub const NO_HOLDER: usize = 0;

impl Entry {
    pub fn load(&self) -> *mut Header {
        self.ptr.load(Ordering::Acquire)
    }

    pub fn store(&self, header: *mut Header) {
        self.ptr.store(header, Ordering::Release);
    }
}

/// 根表，同时也是堆的入口表：别的堆经 [`crate::Far`] 直接指进来的对象在这里各占一个槽，记着持有方。
#[derive(Default)]
pub struct RootTable {
    /// 每个根一个槽
    slots: SegVec<Entry>,
    /// 句柄析构时还回来的槽号（下次回收时并进 ready）
    returned: PushList<usize>,
    /// 可以复用的空槽号：只在回收时改，mutate 期间只读
    ready: UnsafeCell<Vec<usize>>,
    /// ready 里下一个没领走的下标
    cursor: AtomicUsize,
}

// SAFETY: ready 只在回收时（独占堆，没有 mutator）改，mutate 期间只读；其余都是原子量 / 无锁结构
unsafe impl Sync for RootTable {}
// SAFETY: 同上
unsafe impl Send for RootTable {}

impl RootTable {
    /// 领一个槽放 `header`（只在这个堆的 mutate 里调用）。`holder`：持有方堆，普通根是 [`NO_HOLDER`]。
    pub fn claim(&self, header: *const Header, holder: usize) -> usize {
        // SAFETY: mutate 期间 ready 不变
        let ready = unsafe { &*self.ready.get() };
        let i = self.cursor.fetch_add(1, Ordering::AcqRel);
        match ready.get(i).and_then(|&slot| self.slots.get(slot).map(|s| (slot, s))) {
            Some((slot, s)) => {
                // 先写持有方再发布指针：读的一侧先读指针（Acquire）再读持有方
                s.holder.store(holder, Ordering::Relaxed);
                s.store(header.cast_mut());
                slot
            }
            None => self.slots.push(Entry { ptr: AtomicPtr::new(header.cast_mut()), holder: AtomicUsize::new(holder) }),
        }
    }

    pub fn slot(&self, i: usize) -> &Entry {
        self.slots.get(i).expect("根槽号来自 claim")
    }

    /// 句柄析构：清空槽、槽号挂回去（任何时候、任何线程）。
    pub fn release(&self, i: usize) {
        self.slot(i).store(null_mut());
        self.returned.push(i);
    }

    /// 疏散回收：每个根交给 `f`（标记，必要时搬走），改成它返回的新地址。
    /// 句柄可能同时在别的线程析构（槽被清空），所以用 CAS：清空了就不改回去。
    /// 入口（别的堆直接指着的）对象所在的块事先已经撤掉疏散（[`RootTable::pinned`]），不会动。
    pub fn relocate(&self, mut f: impl FnMut(*const Header) -> *const Header) {
        for slot in self.slots.iter() {
            let old = slot.load();
            if old.is_null() {
                continue;
            }
            let new = f(old.cast_const());
            if !core::ptr::eq(old, new) {
                let _ = slot.ptr.compare_exchange(old, new.cast_mut(), Ordering::AcqRel, Ordering::Acquire);
            }
        }
    }

    /// 别的堆直接指着的对象，逐个交给 `f`。SAFETY（调用方）：没有 mutator。
    pub unsafe fn pinned(&self, mut f: impl FnMut(*const Header)) {
        for slot in self.slots.iter() {
            let p = slot.load();
            if !p.is_null() && slot.holder.load(Ordering::Relaxed) != NO_HOLDER {
                f(p.cast_const());
            }
        }
    }

    /// 回收时：整理空槽，返回全部根。SAFETY（调用方）：没有 mutator（独占堆）。
    pub unsafe fn settle(&self) -> Vec<*const Header> {
        // SAFETY: 见上
        unsafe { self.settle_outside(|_| false) }
    }

    /// 同 [`RootTable::settle`]，但持有方满足 `inside` 的入口不算根（一起回收的堆里的 Far，由追踪决定死活）。
    pub unsafe fn settle_outside(&self, inside: impl Fn(usize) -> bool) -> Vec<*const Header> {
        // SAFETY: 见 settle
        let ready = unsafe { &mut *self.ready.get() };
        let used = self.cursor.swap(0, Ordering::AcqRel).min(ready.len());
        ready.drain(..used);
        ready.extend(self.returned.take());
        self.slots
            .iter()
            .filter_map(|s| {
                let p = s.load();
                (!p.is_null() && !inside(s.holder.load(Ordering::Relaxed))).then_some(p.cast_const())
            })
            .collect()
    }

    /// 一起回收之后：持有方满足 `inside`、目标没标到的入口——持有它的 Far 已经死了（活的 Far 都被追踪到、把目标标上了）——
    /// 清空，免得在持有方清扫到那个 Far 之前，下一次回收拿它当根去碰已经回收的对象。SAFETY（调用方）：没有 mutator。
    pub unsafe fn clear_dead(&self, inside: impl Fn(usize) -> bool, epoch: u32) {
        for slot in self.slots.iter() {
            let p = slot.load();
            // SAFETY: 槽里非空的都是本堆的对象，回收还没清扫，内存都在
            if !p.is_null() && inside(slot.holder.load(Ordering::Relaxed)) && !unsafe { &*p }.is_marked(epoch) {
                let _ = slot.ptr.compare_exchange(p, null_mut(), Ordering::AcqRel, Ordering::Relaxed);
            }
        }
    }
}

/// 强引用句柄：只要它活着，对象就不会被回收。不带 `'gc`，可以存在任何地方、跨线程传。
/// 用 [`Mutator::root`] 建、[`Mutator::get`] 取。
pub struct Root<B: Brand> {
    table: Arc<RootTable>,
    slot: usize,
    _brand: PhantomData<fn() -> B>,
}

impl<B: Brand> Drop for Root<B> {
    fn drop(&mut self) {
        self.table.release(self.slot);
    }
}

// SAFETY: 句柄本身不含 Gc，是一个根
unsafe impl<B: Brand> Trace for Root<B> {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut crate::Tracer<'_>) {}
}

impl<'gc> Mutator<'gc> {
    /// 给对象建一个根。
    pub fn root<B: Brand>(&self, gc: Gc<'gc, B::Of<'gc>>) -> Root<B> {
        let table = self.heap().roots().clone();
        let slot = table.claim(gc.header(), NO_HOLDER);
        Root { table, slot, _brand: PhantomData }
    }

    /// 取根指向的对象。
    pub fn get<B: Brand>(&self, root: &Root<B>) -> Gc<'gc, B::Of<'gc>> {
        assert!(Arc::ptr_eq(&root.table, self.heap().roots()), "这个 Root 不属于这个堆");
        let header = root.table.slot(root.slot).load();
        // SAFETY: 根槽里是同一个 B 建的、被这个根保活的对象
        unsafe { Gc::from_data(Header::data(header).cast()) }
    }

    /// 让根改指另一个对象。
    pub fn set<B: Brand>(&self, root: &Root<B>, gc: Gc<'gc, B::Of<'gc>>) {
        assert!(Arc::ptr_eq(&root.table, self.heap().roots()), "这个 Root 不属于这个堆");
        root.table.slot(root.slot).store(gc.header().cast_mut());
    }
}

/// 指向另一个堆里对象的直接指针，放在持有方堆（品牌 `'a`）的对象里。
pub struct Far<'a, B: Brand> {
    /// 目标堆的根表：领的槽在这里；Arc 让目标堆先析构时还槽也安全
    table: Arc<RootTable>,
    slot: usize,
    /// 目标对象的头（入口对象不疏散，不会变）
    ptr: NonNull<Header>,
    _brand: PhantomData<(Invariant<'a>, fn() -> B)>,
}

// SAFETY: 只给出 B::Of 的共享引用，B::Of: Trace 要求 Send + Sync；根表是线程安全的
unsafe impl<B: Brand> Send for Far<'_, B> {}
// SAFETY: 同上
unsafe impl<B: Brand> Sync for Far<'_, B> {}

impl<'a, B: Brand> Far<'a, B> {
    /// `holder`：持有方堆（这个 Far 要放进它的对象里）；`target`：`gc` 所在的堆，不能是同一个（同堆的边用 [`crate::Slot`]）。
    pub fn new<'b>(holder: &Mutator<'a>, target: &Mutator<'b>, gc: Gc<'b, B::Of<'b>>) -> Self {
        assert!(!core::ptr::eq(holder.heap(), target.heap()), "同一个堆里的边用 Slot，不用 Far");
        let table = target.heap().roots().clone();
        let slot = table.claim(gc.header(), holder.heap().id());
        // SAFETY: Gc 的头非空
        let ptr = unsafe { NonNull::new_unchecked(gc.header().cast_mut()) };
        Self { table, slot, ptr, _brand: PhantomData }
    }

    /// 取目标对象；`m` 必须是目标堆的 mutator。
    pub fn get<'b>(&self, m: &Mutator<'b>) -> Gc<'b, B::Of<'b>> {
        assert!(Arc::ptr_eq(&self.table, m.heap().roots()), "这个 Far 不指向这个堆");
        // SAFETY: 槽保活这个对象、入口对象不搬；m 证明在目标堆的 mutate 里；B 与建它时相同
        unsafe { Gc::from_data(Header::data(self.ptr.as_ptr()).cast()) }
    }

    /// 目标对象在不在 `heap` 里。
    pub fn points_into(&self, heap: &Heap) -> bool {
        Arc::ptr_eq(&self.table, heap.roots())
    }

    fn report(&self, t: &mut Tracer<'_>) {
        t.far(Arc::as_ptr(&self.table).addr(), self.ptr.as_ptr());
    }
}

impl<B: Brand> Drop for Far<'_, B> {
    fn drop(&mut self) {
        self.table.release(self.slot);
    }
}

// SAFETY: 对持有方堆没有边；一起回收时把目标报给回收器
unsafe impl<B: Brand> Trace for Far<'_, B> {
    fn trace(&self, t: &mut Tracer<'_>) {
        self.report(t);
    }

    fn relocate(&mut self, t: &mut Tracer<'_>) {
        self.report(t);
    }
}

/// 能放进公开堆的类型：`#[repr(C)]`、只有纯数据、[`PublicSlot`] 与这样的嵌套类型、定长数组，没有析构。
/// 用 `#[derive(Shared)]` 生成，别手写。
///
/// # Safety
/// `edges` 必须把每个 [`PublicSlot`] 的字节偏移（相对 `at`）报给 `map`，漏了就是悬垂指针；`NO_EDGES` 只能在确实没有时为 true。
#[cfg(target_os = "linux")]
pub unsafe trait Shared: Send + Sync {
    /// 类型里没有任何 [`PublicSlot`]
    const NO_EDGES: bool = false;

    /// 把自己里面每个指针槽的偏移（`at` + 槽在自己里面的偏移）报给 `map`。
    fn edges(map: &mut EdgeMap, at: u32);
}

/// 指针图里的偏移带这一位：那里是一个 [`PublicRef`]（偏移 + 目标堆身份），指向别的堆；不带的是同堆的 [`PublicSlot`]
#[cfg(target_os = "linux")]
pub const FAR_EDGE: u32 = 1 << 31;

/// 一个类型的指针图：对象数据里每个指针槽的字节偏移。
#[derive(Default)]
#[cfg(target_os = "linux")]
pub struct EdgeMap(Vec<u32>);

#[cfg(target_os = "linux")]
impl EdgeMap {
    pub fn edge(&mut self, at: u32) {
        self.0.push(at);
    }

    /// 一个指向别的公开堆的 [`PublicRef`]（偏移后面还有目标堆的身份）。
    pub fn far(&mut self, at: u32) {
        self.0.push(at | FAR_EDGE);
    }

    /// 报上来的全部偏移（[`FAR_EDGE`] 那一位标着跨堆的）。
    pub fn as_slice(&self) -> &[u32] {
        &self.0
    }

    /// 字段偏移转成指针图里的偏移（derive 用）。
    pub fn offset(bytes: usize) -> u32 {
        u32::try_from(bytes).expect("公开堆的对象不超过 4 GiB")
    }
}

/// 不含指针槽的公开堆类型。
#[cfg(target_os = "linux")]
macro_rules! shared_leaf {
    ($($ty:ty),* $(,)?) => {$(
        // SAFETY: 没有指针槽
        unsafe impl Shared for $ty {
            const NO_EDGES: bool = true;
            fn edges(_: &mut EdgeMap, _: u32) {}
        }
    )*};
}

#[cfg(target_os = "linux")]
shared_leaf!(
    (), bool, char, u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, f32, f64,
    NonZeroU8, NonZeroU16, NonZeroU32, NonZeroU64, NonZeroI8, NonZeroI16, NonZeroI32, NonZeroI64,
    AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicI8, AtomicI16, AtomicI32, AtomicI64,
);

// SAFETY: 没有值
#[cfg(target_os = "linux")]
unsafe impl<T: ?Sized + Send + Sync> Shared for PhantomData<T> {
    const NO_EDGES: bool = true;
    fn edges(_: &mut EdgeMap, _: u32) {}
}

// SAFETY: 逐个元素报
#[cfg(target_os = "linux")]
unsafe impl<T: Shared, const N: usize> Shared for [T; N] {
    const NO_EDGES: bool = T::NO_EDGES;
    fn edges(map: &mut EdgeMap, at: u32) {
        if !T::NO_EDGES {
            for i in 0..N {
                T::edges(map, at + EdgeMap::offset(i * size_of::<T>()));
            }
        }
    }
}

/// 公开堆里的对象，只在一次 [`PublicHeap::mutate`] 里有效（`'p` 是那次调用的品牌）。是本进程里的绝对地址。
#[cfg(target_os = "linux")]
pub struct PublicGc<'p, T> {
    ptr: NonNull<T>,
    _brand: PhantomData<(&'p T, Invariant<'p>)>,
}

// SAFETY: 只给出 &T，T: Shared 要求 Send + Sync
#[cfg(target_os = "linux")]
unsafe impl<T: Send + Sync> Send for PublicGc<'_, T> {}
// SAFETY: 同上
#[cfg(target_os = "linux")]
unsafe impl<T: Send + Sync> Sync for PublicGc<'_, T> {}

#[cfg(target_os = "linux")]
impl<T> Clone for PublicGc<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

#[cfg(target_os = "linux")]
impl<T> Copy for PublicGc<'_, T> {}

#[cfg(target_os = "linux")]
impl<'p, T> PublicGc<'p, T> {

    pub fn as_ptr(self) -> *const T {
        self.ptr.as_ptr()
    }

    /// 在整个 `'p` 里有效的引用。
    pub fn get(self) -> &'p T {
        // SAFETY: 回收只在没有 mutator 时进行，'p 期间对象一直活着且不动
        unsafe { self.ptr.as_ref() }
    }

    pub fn ptr_eq(a: Self, b: Self) -> bool {
        a.ptr == b.ptr
    }

}

/// 只给本 crate 用的操作：trait 在私有模块里、不导出，crate 外拿不到。
#[cfg(target_os = "linux")]
pub trait PublicGcInternal {
    /// SAFETY（调用方）：`ptr` 是这个公开堆里一个活对象的数据指针，品牌 `'p` 正确。
    unsafe fn from_data(ptr: *mut u8) -> Self;

    /// 公开偏移：相对公开地址区起点（每个进程里都一样）。
    fn offset(self) -> u64;
}

#[cfg(target_os = "linux")]
impl<T> PublicGcInternal for PublicGc<'_, T> {
    unsafe fn from_data(ptr: *mut u8) -> Self {
        // SAFETY: 见上
        Self { ptr: unsafe { NonNull::new_unchecked(ptr.cast()) }, _brand: PhantomData }
    }

    fn offset(self) -> u64 {
        let addr = self.ptr.as_ptr().addr();
        (addr - Region::base_of(addr)) as u64
    }
}


#[cfg(target_os = "linux")]
impl<T> Deref for PublicGc<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: 同 get
        unsafe { self.ptr.as_ref() }
    }
}

#[cfg(target_os = "linux")]
impl<T: fmt::Debug> fmt::Debug for PublicGc<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// 公开堆对象里的原子指针槽：存目标的公开偏移（0 = 空），所以同一个值在每个映射这个堆的进程里都对。
/// 读写都要带 [`PublicMutator`]（换算成本进程地址要用它的区起点）。
#[repr(transparent)]
#[cfg(target_os = "linux")]
pub struct PublicSlot<'p, T> {
    off: AtomicU64,
    _brand: PhantomData<PublicGc<'p, T>>,
}

#[cfg(target_os = "linux")]
impl<T> Default for PublicSlot<'_, T> {
    fn default() -> Self {
        Self { off: AtomicU64::new(0), _brand: PhantomData }
    }
}

#[cfg(target_os = "linux")]
impl<'p, T> From<Option<PublicGc<'p, T>>> for PublicSlot<'p, T> {
    fn from(v: Option<PublicGc<'p, T>>) -> Self {
        Self { off: AtomicU64::new(Self::raw(v)), _brand: PhantomData }
    }
}

#[cfg(target_os = "linux")]
impl<'p, T> From<PublicGc<'p, T>> for PublicSlot<'p, T> {
    fn from(v: PublicGc<'p, T>) -> Self {
        Some(v).into()
    }
}

#[cfg(target_os = "linux")]
impl<'p, T> PublicSlot<'p, T> {
    fn raw(v: Option<PublicGc<'p, T>>) -> u64 {
        v.map_or(0, PublicGc::offset)
    }

    fn wrap(m: &PublicMutator<'p>, off: u64) -> Option<PublicGc<'p, T>> {
        // SAFETY: 槽里只存过同品牌的 PublicGc 的偏移，对象被槽所在的对象引用着
        (off != 0).then(|| unsafe { PublicGc::from_data(m.at(off)) })
    }

    pub fn get(&self, m: &PublicMutator<'p>) -> Option<PublicGc<'p, T>> {
        Self::wrap(m, self.off.load(Ordering::Acquire))
    }

    pub fn set(&self, m: &PublicMutator<'p>, v: Option<PublicGc<'p, T>>) {
        self.swap(m, v);
    }

    pub fn swap(&self, m: &PublicMutator<'p>, v: Option<PublicGc<'p, T>>) -> Option<PublicGc<'p, T>> {
        Self::wrap(m, self.off.swap(Self::raw(v), Ordering::AcqRel))
    }

    /// 槽里是 `current` 才换成 `new`；失败时返回槽里的值。
    pub fn compare_exchange(&self, m: &PublicMutator<'p>, current: Option<PublicGc<'p, T>>, new: Option<PublicGc<'p, T>>) -> Result<Option<PublicGc<'p, T>>, Option<PublicGc<'p, T>>> {
        self.off
            .compare_exchange(Self::raw(current), Self::raw(new), Ordering::AcqRel, Ordering::Acquire)
            .map(|o| Self::wrap(m, o))
            .map_err(|o| Self::wrap(m, o))
    }
}

// SAFETY: 槽本身就是一个指针
#[cfg(target_os = "linux")]
unsafe impl<T: Send + Sync> Shared for PublicSlot<'_, T> {
    fn edges(map: &mut EdgeMap, at: u32) {
        map.edge(at);
    }
}

/// 公开堆 A（品牌 `'a`）对象里指向另一个公开堆 B 的对象的引用：存目标的公开偏移与 B 的身份，直接指过去，不复制。
///
/// 写入时在 B 的入口表登记一项、持有方记作「堆 A」，所以 B 单独回收时它是根（回收 B 的进程不需要能打开 A）；
/// A 单独回收不跟进 B。跨堆的环、以及已经不用的登记，由 [`PublicHeap::collect_group`] 收。读要带 B 的 mutator。
/// 品牌 `'a` 保证它只能放进 A 的对象里（登记的持有方才对）。
///
/// B 死了以后它的堆号可能被新堆用上：身份对不上的引用读出来是 None，回收时也不跟，不会被当成新堆里的地址。
#[repr(C)]
#[cfg(target_os = "linux")]
pub struct PublicRef<'a, B: PublicBrand> {
    off: AtomicU64,
    /// 目标堆的身份（[`crate::layout::Super::incarnation`]）
    incarnation: AtomicU64,
    _brand: PhantomData<(HolderBrand<'a>, fn() -> B)>,
}

/// 持有方堆的品牌：对 `'a` 不变；用 fn 指针形式是为了能跨线程（[`Invariant`] 用的 Cell 不行）
#[cfg(target_os = "linux")]
type HolderBrand<'a> = fn(&'a ()) -> &'a ();

#[cfg(target_os = "linux")]
impl<B: PublicBrand> Default for PublicRef<'_, B> {
    fn default() -> Self {
        Self { off: AtomicU64::new(0), incarnation: AtomicU64::new(0), _brand: PhantomData }
    }
}

#[cfg(target_os = "linux")]
impl<'a, B: PublicBrand> PublicRef<'a, B> {
    /// 指向 `gc`（在 `target` 堆里）的新引用，放进 `holder` 堆的对象里。
    pub fn to<'b>(holder: &PublicMutator<'a>, target: &PublicMutator<'b>, gc: PublicGc<'b, B::Of<'b>>) -> Self {
        let r = Self::default();
        r.set(holder, target, Some(gc));
        r
    }

    /// 改指向（`None` 清空）。旧值的登记留到下一次一起回收再还。
    pub fn set<'b>(&self, holder: &PublicMutator<'a>, target: &PublicMutator<'b>, v: Option<PublicGc<'b, B::Of<'b>>>) {
        assert!(holder.index() != target.index(), "同一个公开堆里的边用 PublicSlot");
        let off = v.map_or(0, PublicGc::offset);
        if off != 0 {
            target.register(off, holder);
        }
        // 先写身份再写偏移：读的一方看到新偏移时身份也是新的（两个字被并发改写时，对不上的组合只会读成 None）
        self.incarnation.store(target.incarnation(), Ordering::Release);
        self.off.store(off, Ordering::Release);
    }

    /// 取目标对象；`target` 是它指向的那个堆的 mutator。空的、指向别的堆的、指向一个已经死掉的旧堆（堆号被 `target` 重用了）的，都是 None。
    pub fn get<'b>(&self, target: &PublicMutator<'b>) -> Option<PublicGc<'b, B::Of<'b>>> {
        let off = self.off.load(Ordering::Acquire);
        if off == 0 || Inner::heap_of(off) != Some(target.index()) || self.incarnation.load(Ordering::Acquire) != target.incarnation() {
            return None;
        }
        // SAFETY: 目标堆的入口保活它（登记在写入时、一起回收时重建），公开堆不搬对象；target 证明在目标堆的 mutate 里
        Some(unsafe { PublicGc::from_data(target.at(off)) })
    }

    /// 指向 `heap` 里的对象（同一个身份的堆）。
    pub fn points_into(&self, heap: &PublicHeap) -> bool {
        Inner::heap_of(self.off.load(Ordering::Acquire)) == Some(heap.id()) && self.incarnation.load(Ordering::Acquire) == heap.incarnation()
    }
}

// SAFETY: 它本身就是一个跨堆的指针槽（回收时按身份认目标堆）
#[cfg(target_os = "linux")]
unsafe impl<B: PublicBrand> Shared for PublicRef<'_, B> {
    fn edges(map: &mut EdgeMap, at: u32) {
        map.far(at);
    }
}

/// 把带 `'p` 的公开堆类型换个品牌：[`Public<B>`] 不带生命周期，取出来时再套上当次 mutate 的 `'p`。用 [`public_brand!`](crate::public_brand) 声明。
#[cfg(target_os = "linux")]
pub trait PublicBrand {
    type Of<'p>: Shared + 'p;
}

/// 声明一个公开堆的品牌类型：`public_brand!(pub NodeB = for<'p> Node<'p>);`
#[cfg(target_os = "linux")]
#[macro_export]
macro_rules! public_brand {
    ($(#[$meta:meta])* $vis:vis $name:ident = for<$l:lifetime> $ty:ty) => {
        $(#[$meta])*
        $vis struct $name;
        impl $crate::PublicBrand for $name {
            type Of<$l> = $ty;
        }
    };
}

/// 指向公开堆对象的句柄：在入口表里占一项，只要它活着对象就不会被回收。不带生命周期，可以放进私有堆的对象里、跨线程传。
/// 用 [`PublicMutator::root`] 建、[`Public::get`] 取。
#[cfg(target_os = "linux")]
pub struct Public<B: PublicBrand> {
    /// 进程这一侧的堆：Arc 让窗口在句柄还在时一直映射着
    heap: Arc<Inner>,
    entry: u32,
    /// 目标对象数据（公开堆不搬对象）
    data: NonNull<u8>,
    _brand: PhantomData<fn() -> B>,
}

// SAFETY: 只给出 B::Of 的共享引用（Shared: Send + Sync）；入口表是原子的
#[cfg(target_os = "linux")]
unsafe impl<B: PublicBrand> Send for Public<B> {}
// SAFETY: 同上
#[cfg(target_os = "linux")]
unsafe impl<B: PublicBrand> Sync for Public<B> {}

#[cfg(target_os = "linux")]
impl<B: PublicBrand> Public<B> {

    /// 取目标对象；`m` 必须是目标堆的 mutator。
    pub fn get<'p>(&self, m: &PublicMutator<'p>) -> PublicGc<'p, B::Of<'p>> {
        assert!(m.is(&self.heap), "这个 Public 不指向这个堆");
        // SAFETY: 入口保活这个对象、公开堆不搬对象；m 证明在这个堆的 mutate 里
        unsafe { PublicGc::from_data(self.data.as_ptr()) }
    }

    /// 目标对象在不在 `heap` 里。
    pub fn points_into(&self, heap: &PublicHeap) -> bool {
        heap.is(&self.heap)
    }

    /// 入口号：在每个映射这个堆的地方都一样，别处可以用 [`PublicMutator::find`] 凭它拿到同一个对象。
    pub fn index(&self) -> u32 {
        self.entry
    }

    /// 把这个入口变成不归任何进程的根，返回入口号：本进程退出（或被杀）也不还，主进程重启后凭入口号
    /// （[`PublicMutator::find`]）接着用；不要了用 [`PublicMutator::release`] 还掉。
    pub fn persist(self) -> u32 {
        let me = core::mem::ManuallyDrop::new(self);
        me.heap.disown_entry(me.entry);
        let entry = me.entry;
        // SAFETY: me 不会再析构，Arc 只在这里取出一次、随即放掉
        drop(unsafe { core::ptr::read(&raw const me.heap) });
        entry
    }
}

/// 只给本 crate 用的操作：trait 在私有模块里、不导出，crate 外拿不到。
#[cfg(target_os = "linux")]
pub trait PublicInternal {
    fn new(heap: Arc<Inner>, entry: u32, data: NonNull<u8>) -> Self;
}

#[cfg(target_os = "linux")]
impl<B: PublicBrand> PublicInternal for Public<B> {
    fn new(heap: Arc<Inner>, entry: u32, data: NonNull<u8>) -> Self {
        Self { heap, entry, data, _brand: PhantomData }
    }
}


#[cfg(target_os = "linux")]
impl<B: PublicBrand> Drop for Public<B> {
    fn drop(&mut self) {
        self.heap.release_entry(self.entry);
    }
}

// SAFETY: 句柄对私有堆没有边（它本身是公开堆的一个根）
#[cfg(target_os = "linux")]
unsafe impl<B: PublicBrand> Trace for Public<B> {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut Tracer<'_>) {}
}
