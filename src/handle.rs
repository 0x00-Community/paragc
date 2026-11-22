//! 指针与句柄：私有堆的 [`Gc`] / [`Slot`] / [`Root`] / [`Weak`] / [`Far`]，公开堆的 [`CrossHeapGc`] / [`CrossHeapSlot`] / [`CrossHeapRef`] / [`CrossHeapRoot`] 与 [`Shared`]。
//!
//! 堆指针 [`Gc`] 与原子指针槽 [`Slot`]。
//!
//! 根：[`Root`] 是 mutate 之外也能持有的强引用句柄（可跨线程），回收从全部根出发。
//! 弱引用 [`Weak`] 同样可以在 mutate 之外持有，但不保活：每次回收标记完清空没标到的目标。
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
//! 公开堆里的类型与指针：对象头、[`Shared`]（能放进公开堆的类型）、[`CrossHeapGc`]、[`CrossHeapSlot`]、[`CrossHeapRoot`]。
//!
//! 公开堆的对象可能被别的进程读、被不认识这个类型的进程回收，所以：头里不存代码，只存类型号；回收照类型表里的指针图找边；
//! 对象不许有析构；指针一律存「相对公开地址区起点的偏移」，同一个值在每个进程里都指向同一个对象。
use core::fmt;
use core::cell::{Cell, UnsafeCell};
use core::marker::PhantomData;
#[cfg(crossheap)]
use core::num::{NonZeroI16, NonZeroI32, NonZeroI64, NonZeroI8, NonZeroU16, NonZeroU32, NonZeroU64, NonZeroU8};
use core::ops::Deref;
use core::ptr::{NonNull, null_mut};
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};
#[cfg(crossheap)]
use core::sync::atomic::{AtomicBool, AtomicI16, AtomicI32, AtomicI64, AtomicI8, AtomicU16, AtomicU32, AtomicU8};
use std::sync::Arc;

use crate::{Heap, Mutator, Trace, Tracer};
use crate::heap::{HeapInternal, MutatorInternal};
#[cfg(crossheap)]
use crate::heap::{Inner, CrossHeap, CrossHeapInternal, CrossHeapMutator, CrossHeapMutatorInternal};
use crate::layout::Header;
#[cfg(crossheap)]
use crate::layout::{GRANULE, PHeader};
use crate::mark::TracerInternal;
#[cfg(crossheap)]
use crate::shm::{REGION, Region};
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
    /// The pointer as a word (see [`word_pointer`])
    word: AtomicU64,
    /// 上次计入引用计数的值（地址）| [`LOGGED`]；[`UNCOUNTED`] = 不计数（年轻对象里的槽，晋升时才开始计）
    counted: AtomicUsize,
    _brand: PhantomData<Gc<'gc, T>>,
}

/// [`Slot::counted`]：这个槽不在引用计数里，所在对象还年轻（写屏障不用记它）
pub const UNCOUNTED: usize = usize::MAX;
/// [`Slot::counted`]：这个槽不在引用计数里，但所在对象已被回收追踪过（老了，写进年轻值要记）。
/// 比它大的值都表示不计数（最低位是 [`LOGGED`]，所以占两个值）
pub const PROMOTED: usize = usize::MAX - 1;
/// [`Slot::counted`] 的最低位：本轮已登记（地址按粒度对齐，最低位空着）
pub const LOGGED: usize = 1;

/// 槽里存的是「字」：高 13 位全是 1（`>= WORD_POINTER_MIN`）的字是 GC 指针，低 48 位是对象数据的地址（用户态地址放得下）；
/// 0 是空；别的字都不是指针，由使用者自己解释（数、布尔……：NaN-boxing 的值可以原样放进 [`WordSlot`]）。
/// [`Slot`] 存指针时打 [`RAW_TAG`]。
pub const WORD_POINTER_MIN: u64 = 0xFFF8 << 48;
/// 字的低 48 位：指针字里的地址
pub const WORD_PAYLOAD: u64 = (1 << 48) - 1;
/// [`Slot`] 给指针打的标签（指针区间里最高的一个；别的标签留给使用者区分类型）
pub const RAW_TAG: u64 = 0xFFFF << 48;

/// 字里的 GC 指针（对象数据地址）；不是指针时 `None`。
#[inline]
pub fn word_pointer(word: u64) -> Option<*mut u8> {
    (word >= WORD_POINTER_MIN).then(|| core::ptr::with_exposed_provenance_mut((word & WORD_PAYLOAD) as usize))
}

/// 换掉字里的指针、保留标签（疏散时改成新地址）。
#[inline]
fn word_with_pointer(word: u64, data: *const u8) -> u64 {
    debug_assert!(word >= WORD_POINTER_MIN, "只改指针字");
    (word & !WORD_PAYLOAD) | data.expose_provenance() as u64
}

/// 抹掉类型的槽：写屏障登记、回收时结算用（与 [`Slot`]、[`WordSlot`] 同布局）。
#[repr(C)]
pub struct RawSlot {
    pub word: AtomicU64,
    pub counted: AtomicUsize,
}

impl RawSlot {
    fn header(word: u64) -> Option<*const Header> {
        word_pointer(word).map(|p| Header::of(p))
    }

    /// 改成指向搬过去的对象（`to` 是新头）；只在停顿里（没有 mutator）。
    pub fn relocate(&self, to: *const Header) {
        let w = self.word.load(Ordering::Acquire);
        self.word.store(word_with_pointer(w, Header::data(to)), Ordering::Release);
    }

    fn uncounted(c: usize) -> bool {
        c >= PROMOTED
    }

    fn from_counted(c: usize) -> Option<*const Header> {
        (!Self::uncounted(c) && c & !LOGGED != 0).then(|| Header::of(core::ptr::with_exposed_provenance::<u8>(c & !LOGGED)))
    }

    /// 本轮第一次被写：打上登记标志，返回 true（调用方把槽地址交给回收器）。不计数的槽返回 false。
    pub fn log(&self) -> bool {
        let c = self.counted.load(Ordering::Relaxed);
        !Self::uncounted(c) && c & LOGGED == 0 && self.counted.compare_exchange(c, c | LOGGED, Ordering::AcqRel, Ordering::Relaxed).is_ok()
    }

    /// 所在对象还年轻（自分配以来没被回收追踪过）：写进年轻值不用进记忆集，年轻回收会从对象本身追到。
    pub fn is_young(&self) -> bool {
        self.counted.load(Ordering::Relaxed) == UNCOUNTED
    }

    /// 回收追踪到了所在对象（不计数时）：从此它是老对象，写屏障要记它。
    pub fn promote(&self) {
        // 失败 ⇒ 已经不是年轻（计数中或已晋升），不用改
        let _ = self.counted.compare_exchange(UNCOUNTED, PROMOTED, Ordering::Relaxed, Ordering::Relaxed);
    }

    /// 清掉登记标志（计数不可信时丢弃登记）。
    pub fn unlog(&self) {
        self.counted.fetch_and(!LOGGED, Ordering::AcqRel);
    }

    /// 改成计入当前值；返回（当前值, 上次计入的值）。
    pub fn recount(&self) -> (Option<*const Header>, Option<*const Header>) {
        let p = word_pointer(self.word.load(Ordering::Acquire));
        let prev = self.counted.swap(p.map_or(0, <*mut u8>::expose_provenance), Ordering::AcqRel);
        (p.map(|p| Header::of(p.cast_const())), Self::from_counted(prev))
    }

    /// 这个槽当前的值。
    pub fn current(&self) -> Option<*const Header> {
        Self::header(self.word.load(Ordering::Acquire))
    }

    /// 上次计入的值（释放它所在的对象时 −1）。
    pub fn counted(&self) -> Option<*const Header> {
        Self::from_counted(self.counted.load(Ordering::Acquire))
    }
}

impl<T> Default for Slot<'_, T> {
    fn default() -> Self {
        Self { word: AtomicU64::new(0), counted: AtomicUsize::new(UNCOUNTED), _brand: PhantomData }
    }
}

impl<'gc, T> From<Option<Gc<'gc, T>>> for Slot<'gc, T> {
    fn from(v: Option<Gc<'gc, T>>) -> Self {
        Self { word: AtomicU64::new(Self::raw(v)), counted: AtomicUsize::new(UNCOUNTED), _brand: PhantomData }
    }
}

impl<'gc, T> From<Gc<'gc, T>> for Slot<'gc, T> {
    fn from(v: Gc<'gc, T>) -> Self {
        Some(v).into()
    }
}

impl<'gc, T> Slot<'gc, T> {
    fn raw(v: Option<Gc<'gc, T>>) -> u64 {
        v.map_or(0, |g| RAW_TAG | g.ptr.as_ptr().expose_provenance() as u64)
    }

    fn wrap(word: u64) -> Option<Gc<'gc, T>> {
        // SAFETY: 槽里只存过同品牌的 Gc，对象被槽所在的对象引用着
        word_pointer(word).map(|p| unsafe { Gc::from_data(p.cast()) })
    }

    pub fn get(&self) -> Option<Gc<'gc, T>> {
        Self::wrap(self.word.load(Ordering::Acquire))
    }

    pub fn set(&self, m: &Mutator<'gc>, v: Option<Gc<'gc, T>>) {
        self.swap(m, v);
    }

    pub fn swap(&self, m: &Mutator<'gc>, v: Option<Gc<'gc, T>>) -> Option<Gc<'gc, T>> {
        let new = Self::raw(v);
        let old = self.word.swap(new, Ordering::AcqRel);
        m.written(self.raw_slot(), RawSlot::header(old), RawSlot::header(new));
        Self::wrap(old)
    }

    /// 槽里是 `current` 才换成 `new`；失败时返回槽里的值。
    pub fn compare_exchange(&self, m: &Mutator<'gc>, current: Option<Gc<'gc, T>>, new: Option<Gc<'gc, T>>) -> Result<Option<Gc<'gc, T>>, Option<Gc<'gc, T>>> {
        let new = Self::raw(new);
        let done = self.word.compare_exchange(Self::raw(current), new, Ordering::AcqRel, Ordering::Acquire);
        // 写屏障：被覆盖的旧值（并发标记的初始快照）、写进去的新值（年轻对象 ⇒ 记下这个槽，年轻回收从它出发）、
        // 引用计数登记（老对象里的槽本轮第一次被写）
        if let Ok(old) = done {
            m.written(self.raw_slot(), RawSlot::header(old), RawSlot::header(new));
        }
        done.map(Self::wrap).map_err(Self::wrap)
    }
}

/// 放任意「字」的槽（见 [`word_pointer`]）：使用者自己的值编码（比如 NaN-boxing）原样放进来，字里的 GC 指针照样追踪、
/// 疏散时改地址、计数。与 [`Slot`] 一样大（字 + 上次计入的值），比「值的字 + 一个 [`Slot`]」少一个字。
///
/// 写只能来自持有它所在对象的那一个 mutator（同一个槽不会被两个线程同时写）：所以写是一读一写、不是原子交换。
#[repr(C)]
pub struct WordSlot<'gc> {
    word: AtomicU64,
    counted: AtomicUsize,
    _brand: PhantomData<Gc<'gc, ()>>,
}

impl WordSlot<'_> {
    /// 新槽放 `word`（所在对象还年轻，不计数）。
    pub fn new(word: u64) -> Self {
        Self { word: AtomicU64::new(word), counted: AtomicUsize::new(UNCOUNTED), _brand: PhantomData }
    }

    #[inline]
    pub fn get(&self) -> u64 {
        self.word.load(Ordering::Acquire)
    }

    /// 写进 `word`：旧值或新值里有指针（且不是同一个对象）时过写屏障（见 [`Slot`]）。
    #[inline]
    pub fn set(&self, m: &Mutator<'_>, word: u64) {
        let old = self.word.load(Ordering::Relaxed);
        self.word.store(word, Ordering::Release);
        let (o, n) = (RawSlot::header(old), RawSlot::header(word));
        if o != n {
            m.written(self.raw_slot(), o, n);
        }
    }

    /// 独占时写（还没放进堆，或停顿里）
    pub fn set_mut(&mut self, word: u64) {
        *self.word.get_mut() = word;
    }

    fn raw_slot(&self) -> &RawSlot {
        // SAFETY: WordSlot 与 RawSlot 都是 repr(C)，字段同类型同顺序
        unsafe { &*(&raw const *self).cast::<RawSlot>() }
    }
}

impl fmt::Debug for WordSlot<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WordSlot({:#x})", self.get())
    }
}

/// [`WordSlot`] 只给本 crate 用的操作。
pub trait WordSlotInternal {
    fn raw(&self) -> &RawSlot;
}

impl WordSlotInternal for WordSlot<'_> {
    fn raw(&self) -> &RawSlot {
        self.raw_slot()
    }
}

/// [`Slot`] 只给本 crate 用的操作（同 [`GcInternal`]）。
pub trait SlotInternal<'gc> {
    type Data;

    /// 抹掉类型的视图。
    fn raw_slot(&self) -> &RawSlot;
}

impl<'gc, T> SlotInternal<'gc> for Slot<'gc, T> {
    type Data = T;

    fn raw_slot(&self) -> &RawSlot {
        // SAFETY: Slot 与 RawSlot 都是 repr(C)，字段同类型同顺序
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
        unsafe { self.recycle() };
        self.slots
            .iter()
            .filter_map(|s| {
                let p = s.load();
                (!p.is_null() && !inside(s.holder.load(Ordering::Relaxed))).then_some(p.cast_const())
            })
            .collect()
    }

    /// 还回来的槽号并进可复用的（领走的去掉）。SAFETY（调用方）：没有 mutator。
    unsafe fn recycle(&self) {
        // SAFETY: 见上：ready 只在这时改
        let ready = unsafe { &mut *self.ready.get() };
        let used = self.cursor.swap(0, Ordering::AcqRel).min(ready.len());
        ready.drain(..used);
        ready.extend(self.returned.take());
    }

    /// 弱表（[`Weak`] 的槽）在回收标记完、清扫之前：目标没标到 `epoch` 的清空，搬走了的改成新地址；
    /// 活着的目标计数粘住（`parity` 是当前计数的奇偶）。句柄可能同时在别的线程析构（槽被清空），所以用 CAS。
    /// SAFETY（调用方）：没有 mutator；标记（含疏散）做完了、还没清扫，槽里的对象内存都还在。
    pub unsafe fn clear_unreached(&self, epoch: u32, parity: bool) {
        // SAFETY: 见上
        unsafe { self.recycle() };
        for slot in self.slots.iter() {
            let old = slot.load();
            if old.is_null() {
                continue;
            }
            let new = Header::survivor(old, epoch);
            if let Some(new) = new {
                // SAFETY: 活着的对象
                unsafe { &*new }.rc_stick(parity);
            }
            let new = new.map_or(null_mut(), <*const Header>::cast_mut);
            if !core::ptr::eq(old, new) {
                let _ = slot.ptr.compare_exchange(old, new, Ordering::AcqRel, Ordering::Acquire);
            }
        }
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

/// 弱引用句柄：不保活对象；对象被回收后 [`Mutator::upgrade`] 给 None。像 [`Root`] 一样不带 `'gc`，可以存在任何地方
/// （也可以放进堆对象里）、跨线程传。用 [`Mutator::weak`] 建。
///
/// 每次回收（完整、年轻、并发标记收尾、一起回收）在标记之后、清扫之前清空目标没被标到的弱引用，搬走的改成新地址；
/// 所以在任何一次 mutate 里，弱引用要么是空的，要么指着活对象。
/// 年轻对象没有强引用就在下一次年轻回收死；老对象（计数被弱引用粘住，见 [`crate::Heap::collect_young`]）要等完整回收。
pub struct Weak<B: Brand> {
    /// 堆的弱表
    table: Arc<RootTable>,
    slot: usize,
    _brand: PhantomData<fn() -> B>,
}

impl<B: Brand> Weak<B> {
    /// 目标已经被回收了（之后一直是）。不需要进 mutate；为假时目标也可能在下一次回收死。
    pub fn is_cleared(&self) -> bool {
        self.table.slot(self.slot).load().is_null()
    }
}

impl<B: Brand> Drop for Weak<B> {
    fn drop(&mut self) {
        self.table.release(self.slot);
    }
}

// SAFETY: 句柄本身不含 Gc，对目标没有强边
unsafe impl<B: Brand> Trace for Weak<B> {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut crate::Tracer<'_>) {}
}

impl<'gc> Mutator<'gc> {
    /// 给对象建一个弱引用。
    ///
    /// 下一次回收起目标的引用计数粘住（不再按计数释放，留给追踪回收）：按计数释放在后台、与 mutator 并发，来不及先清空
    /// 弱引用。每次停顿处理弱表时粘住，早于那次停顿算出的减量，所以在那之前它还不会被按计数释放。
    pub fn weak<B: Brand>(&self, gc: Gc<'gc, B::Of<'gc>>) -> Weak<B> {
        let table = self.heap().weaks().clone();
        let slot = table.claim(gc.header(), NO_HOLDER);
        Weak { table, slot, _brand: PhantomData }
    }

    /// 弱引用指着的对象；已经被回收了就是 None。并发标记期间取到的对象当作被引用了（标上），本轮不会死。
    pub fn upgrade<B: Brand>(&self, weak: &Weak<B>) -> Option<Gc<'gc, B::Of<'gc>>> {
        assert!(Arc::ptr_eq(&weak.table, self.heap().weaks()), "这个 Weak 不属于这个堆");
        let header = weak.table.slot(weak.slot).load();
        if header.is_null() {
            return None;
        }
        // 读屏障：初始快照只保证快照时强可达的对象被标上，从弱引用取出、再存进黑对象的会漏
        self.reached(header);
        // SAFETY: 弱表里非空的槽指着活对象（每次回收清扫前清空了死的），类型是建它时的 B
        Some(unsafe { Gc::from_data(Header::data(header).cast()) })
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

/// 能放进公开堆的类型：`#[repr(C)]`、只有纯数据、[`CrossHeapSlot`] 与这样的嵌套类型、定长数组，没有析构。
/// 用 `#[derive(Shared)]` 生成，别手写。
///
/// # Safety
/// `edges` 必须把每个 [`CrossHeapSlot`] 的字节偏移（相对 `at`）报给 `map`，漏了就是悬垂指针；`NO_EDGES` 只能在确实没有时为 true。
#[cfg(crossheap)]
pub unsafe trait Shared: Send + Sync {
    /// 类型里没有任何 [`CrossHeapSlot`]
    const NO_EDGES: bool = false;

    /// 把自己里面每个指针槽的偏移（`at` + 槽在自己里面的偏移）报给 `map`。
    fn edges(map: &mut EdgeMap, at: u32);
}

/// 指针图里的偏移带这一位：那里是一个 [`CrossHeapRef`]（偏移 + 目标堆身份），指向别的堆；不带的是同堆的 [`CrossHeapSlot`]
#[cfg(crossheap)]
pub const FAR_EDGE: u32 = 1 << 31;

/// 一个类型的指针图：对象数据里每个指针槽的字节偏移。
#[derive(Default)]
#[cfg(crossheap)]
pub struct EdgeMap(Vec<u32>);

#[cfg(crossheap)]
impl EdgeMap {
    pub fn edge(&mut self, at: u32) {
        self.0.push(at);
    }

    /// 一个指向别的公开堆的 [`CrossHeapRef`]（偏移后面还有目标堆的身份）。
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
#[cfg(crossheap)]
macro_rules! shared_leaf {
    ($($ty:ty),* $(,)?) => {$(
        // SAFETY: 没有指针槽
        unsafe impl Shared for $ty {
            const NO_EDGES: bool = true;
            fn edges(_: &mut EdgeMap, _: u32) {}
        }
    )*};
}

#[cfg(crossheap)]
shared_leaf!(
    (), bool, char, u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, f32, f64,
    NonZeroU8, NonZeroU16, NonZeroU32, NonZeroU64, NonZeroI8, NonZeroI16, NonZeroI32, NonZeroI64,
    AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicI8, AtomicI16, AtomicI32, AtomicI64,
);

// SAFETY: 没有值
#[cfg(crossheap)]
unsafe impl<T: ?Sized + Send + Sync> Shared for PhantomData<T> {
    const NO_EDGES: bool = true;
    fn edges(_: &mut EdgeMap, _: u32) {}
}

// SAFETY: 逐个元素报
#[cfg(crossheap)]
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

/// 公开堆里的对象，只在一次 [`CrossHeap::mutate`] 里有效（`'p` 是那次调用的品牌）。是本进程里的绝对地址。
#[cfg(crossheap)]
pub struct CrossHeapGc<'p, T> {
    ptr: NonNull<T>,
    _brand: PhantomData<(&'p T, Invariant<'p>)>,
}

// SAFETY: 只给出 &T，T: Shared 要求 Send + Sync
#[cfg(crossheap)]
unsafe impl<T: Send + Sync> Send for CrossHeapGc<'_, T> {}
// SAFETY: 同上
#[cfg(crossheap)]
unsafe impl<T: Send + Sync> Sync for CrossHeapGc<'_, T> {}

#[cfg(crossheap)]
impl<T> Clone for CrossHeapGc<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

#[cfg(crossheap)]
impl<T> Copy for CrossHeapGc<'_, T> {}

#[cfg(crossheap)]
impl<'p, T> CrossHeapGc<'p, T> {

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
#[cfg(crossheap)]
pub trait CrossHeapGcInternal {
    /// SAFETY（调用方）：`ptr` 是这个公开堆里一个活对象的数据指针，品牌 `'p` 正确。
    unsafe fn from_data(ptr: *mut u8) -> Self;

    /// 公开偏移：相对公开地址区起点（每个进程里都一样）。
    fn offset(self) -> u64;
}

#[cfg(crossheap)]
impl<T> CrossHeapGcInternal for CrossHeapGc<'_, T> {
    unsafe fn from_data(ptr: *mut u8) -> Self {
        // SAFETY: 见上
        Self { ptr: unsafe { NonNull::new_unchecked(ptr.cast()) }, _brand: PhantomData }
    }

    fn offset(self) -> u64 {
        let addr = self.ptr.as_ptr().addr();
        (addr - Region::base_of(addr)) as u64
    }
}


#[cfg(crossheap)]
impl<T> Deref for CrossHeapGc<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: 同 get
        unsafe { self.ptr.as_ref() }
    }
}

#[cfg(crossheap)]
impl<T: fmt::Debug> fmt::Debug for CrossHeapGc<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// 公开堆对象里的原子指针槽：存目标的公开偏移（0 = 空），所以同一个值在每个映射这个堆的进程里都对。
/// 读写都要带 [`CrossHeapMutator`]（换算成本进程地址要用它的区起点）。
#[repr(transparent)]
#[cfg(crossheap)]
pub struct CrossHeapSlot<'p, T> {
    off: AtomicU64,
    _brand: PhantomData<CrossHeapGc<'p, T>>,
}

#[cfg(crossheap)]
impl<T> Default for CrossHeapSlot<'_, T> {
    fn default() -> Self {
        Self { off: AtomicU64::new(0), _brand: PhantomData }
    }
}

#[cfg(crossheap)]
impl<'p, T> From<Option<CrossHeapGc<'p, T>>> for CrossHeapSlot<'p, T> {
    fn from(v: Option<CrossHeapGc<'p, T>>) -> Self {
        Self { off: AtomicU64::new(Self::raw(v)), _brand: PhantomData }
    }
}

#[cfg(crossheap)]
impl<'p, T> From<CrossHeapGc<'p, T>> for CrossHeapSlot<'p, T> {
    fn from(v: CrossHeapGc<'p, T>) -> Self {
        Some(v).into()
    }
}

#[cfg(crossheap)]
impl<'p, T> CrossHeapSlot<'p, T> {
    fn raw(v: Option<CrossHeapGc<'p, T>>) -> u64 {
        v.map_or(0, CrossHeapGc::offset)
    }

    fn wrap(m: &CrossHeapMutator<'p>, off: u64) -> Option<CrossHeapGc<'p, T>> {
        // SAFETY: 槽里只存过同品牌的 CrossHeapGc 的偏移，对象被槽所在的对象引用着
        (off != 0).then(|| unsafe { CrossHeapGc::from_data(m.at(off)) })
    }

    pub fn get(&self, m: &CrossHeapMutator<'p>) -> Option<CrossHeapGc<'p, T>> {
        Self::wrap(m, self.off.load(Ordering::Acquire))
    }

    pub fn set(&self, m: &CrossHeapMutator<'p>, v: Option<CrossHeapGc<'p, T>>) {
        self.swap(m, v);
    }

    pub fn swap(&self, m: &CrossHeapMutator<'p>, v: Option<CrossHeapGc<'p, T>>) -> Option<CrossHeapGc<'p, T>> {
        Self::wrap(m, self.off.swap(Self::raw(v), Ordering::AcqRel))
    }

    /// 槽里是 `current` 才换成 `new`；失败时返回槽里的值。
    pub fn compare_exchange(&self, m: &CrossHeapMutator<'p>, current: Option<CrossHeapGc<'p, T>>, new: Option<CrossHeapGc<'p, T>>) -> Result<Option<CrossHeapGc<'p, T>>, Option<CrossHeapGc<'p, T>>> {
        self.off
            .compare_exchange(Self::raw(current), Self::raw(new), Ordering::AcqRel, Ordering::Acquire)
            .map(|o| Self::wrap(m, o))
            .map_err(|o| Self::wrap(m, o))
    }
}

// SAFETY: 槽本身就是一个指针
#[cfg(crossheap)]
unsafe impl<T: Send + Sync> Shared for CrossHeapSlot<'_, T> {
    fn edges(map: &mut EdgeMap, at: u32) {
        map.edge(at);
    }
}

/// 公开堆 A（品牌 `'a`）对象里指向另一个公开堆 B 的对象的引用：存目标的公开偏移与 B 的身份，直接指过去，不复制。
///
/// 写入时在 B 的入口表登记一项、持有方记作「堆 A」，所以 B 单独回收时它是根（回收 B 的进程不需要能打开 A）；
/// A 单独回收不跟进 B。跨堆的环、以及已经不用的登记，由 [`CrossHeap::collect_group`] 收。读要带 B 的 mutator。
/// 品牌 `'a` 保证它只能放进 A 的对象里（登记的持有方才对）。
///
/// B 死了以后它的堆号可能被新堆用上：身份对不上的引用读出来是 None，回收时也不跟，不会被当成新堆里的地址。
#[repr(C)]
#[cfg(crossheap)]
pub struct CrossHeapRef<'a, B: CrossHeapBrand> {
    off: AtomicU64,
    /// 目标堆的身份（[`crate::layout::Super::incarnation`]）
    incarnation: AtomicU64,
    _brand: PhantomData<(HolderBrand<'a>, fn() -> B)>,
}

/// 持有方堆的品牌：对 `'a` 不变；用 fn 指针形式是为了能跨线程（[`Invariant`] 用的 Cell 不行）
#[cfg(crossheap)]
type HolderBrand<'a> = fn(&'a ()) -> &'a ();

#[cfg(crossheap)]
impl<B: CrossHeapBrand> Default for CrossHeapRef<'_, B> {
    fn default() -> Self {
        Self { off: AtomicU64::new(0), incarnation: AtomicU64::new(0), _brand: PhantomData }
    }
}

#[cfg(crossheap)]
impl<'a, B: CrossHeapBrand> CrossHeapRef<'a, B> {
    /// 指向 `gc`（在 `target` 堆里）的新引用，放进 `holder` 堆的对象里。
    pub fn to<'b>(holder: &CrossHeapMutator<'a>, target: &CrossHeapMutator<'b>, gc: CrossHeapGc<'b, B::Of<'b>>) -> Self {
        let r = Self::default();
        r.set(holder, target, Some(gc));
        r
    }

    /// 改指向（`None` 清空）。旧值的登记留到下一次一起回收再还。
    pub fn set<'b>(&self, holder: &CrossHeapMutator<'a>, target: &CrossHeapMutator<'b>, v: Option<CrossHeapGc<'b, B::Of<'b>>>) {
        assert!(holder.index() != target.index(), "同一个公开堆里的边用 CrossHeapSlot");
        let off = v.map_or(0, CrossHeapGc::offset);
        if off != 0 {
            target.register(off, holder);
        }
        // 先写身份再写偏移：读的一方看到新偏移时身份也是新的（两个字被并发改写时，对不上的组合只会读成 None）
        self.incarnation.store(target.incarnation(), Ordering::Release);
        self.off.store(off, Ordering::Release);
    }

    /// 取目标对象；`target` 是它指向的那个堆的 mutator。空的、指向别的堆的、指向一个已经死掉的旧堆（堆号被 `target` 重用了）的，都是 None。
    pub fn get<'b>(&self, target: &CrossHeapMutator<'b>) -> Option<CrossHeapGc<'b, B::Of<'b>>> {
        let off = self.off.load(Ordering::Acquire);
        if off == 0 || Inner::heap_of(off) != Some(target.index()) || self.incarnation.load(Ordering::Acquire) != target.incarnation() {
            return None;
        }
        // SAFETY: 目标堆的入口保活它（登记在写入时、一起回收时重建），公开堆不搬对象；target 证明在目标堆的 mutate 里
        Some(unsafe { CrossHeapGc::from_data(target.at(off)) })
    }

    /// 指向 `heap` 里的对象（同一个身份的堆）。
    pub fn points_into(&self, heap: &CrossHeap) -> bool {
        Inner::heap_of(self.off.load(Ordering::Acquire)) == Some(heap.id()) && self.incarnation.load(Ordering::Acquire) == heap.incarnation()
    }
}

// SAFETY: 它本身就是一个跨堆的指针槽（回收时按身份认目标堆）
#[cfg(crossheap)]
unsafe impl<B: CrossHeapBrand> Shared for CrossHeapRef<'_, B> {
    fn edges(map: &mut EdgeMap, at: u32) {
        map.far(at);
    }
}

/// 跨堆弱引用与签名里的那一个字：高位是对象数据的公开偏移（去掉按粒度对齐的低位），低位是对象的出生戳；0 = 空。
/// 一个字，所以读写都是一次原子操作，不用加锁。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[cfg(crossheap)]
pub struct WeakWord(u64);

/// 偏移按粒度对齐，低这么多位总是 0
#[cfg(crossheap)]
const OFF_SHIFT: u32 = GRANULE.trailing_zeros();
/// 偏移去掉低位后要的位数（整个公开地址区）
#[cfg(crossheap)]
const OFF_BITS: u32 = REGION.trailing_zeros() - OFF_SHIFT;
/// 留给出生戳的位数（对象头里的戳是 32 位）
#[cfg(crossheap)]
const STAMP_BITS: u32 = if u64::BITS - OFF_BITS < u32::BITS { u64::BITS - OFF_BITS } else { u32::BITS };
/// 出生戳在字里留下的那几位
#[cfg(crossheap)]
pub const STAMP_MASK: u32 = u32::MAX >> (u32::BITS - STAMP_BITS);

#[cfg(crossheap)]
impl From<(u64, u32)> for WeakWord {
    /// （对象数据的公开偏移, 出生戳）
    fn from((off, stamp): (u64, u32)) -> Self {
        Self(((off >> OFF_SHIFT) << STAMP_BITS) | u64::from(stamp & STAMP_MASK))
    }
}

#[cfg(crossheap)]
impl From<u64> for WeakWord {
    fn from(raw: u64) -> Self {
        Self(raw)
    }
}

#[cfg(crossheap)]
impl From<WeakWord> for u64 {
    fn from(w: WeakWord) -> Self {
        w.0
    }
}

#[cfg(crossheap)]
impl<T> From<CrossHeapGc<'_, T>> for WeakWord {
    fn from(gc: CrossHeapGc<'_, T>) -> Self {
        // SAFETY: gc 是公开堆里一个活对象，头在数据前面
        let stamp = unsafe { (*PHeader::of(gc.as_ptr().cast())).stamp };
        (gc.offset(), stamp).into()
    }
}

#[cfg(crossheap)]
impl WeakWord {
    /// 对象数据的公开偏移（空的是 0）。
    pub fn offset(self) -> u64 {
        (self.0 >> STAMP_BITS) << OFF_SHIFT
    }

    pub fn stamp(self) -> u32 {
        u32::try_from(self.0 & u64::from(STAMP_MASK)).expect("戳只占低 32 位以内")
    }

    /// 指向哪个公开堆（空的、越界的是 None）。
    pub fn heap(self) -> Option<usize> {
        (self.0 != 0).then(|| Inner::heap_of(self.offset())).flatten()
    }
}

/// 公开堆对象里（或任何地方）指向某个公开堆对象的弱引用：一个字（偏移 + 出生戳），不在目标堆登记、不保活目标、
/// 回收谁都不看它，所以跨堆、跨进程也不会拖住任何一方的回收。目标被回收后读出来是 None——位置被新对象占了也一样
/// （出生戳对不上）。读要带目标堆的 mutator（在它的 mutate 里，目标此刻不会被回收）。
///
/// 读写都是一次原子操作，不加锁：多个进程同时改，读到的是某一次写进去的值。
#[repr(transparent)]
#[cfg(crossheap)]
pub struct CrossHeapWeak<B: CrossHeapBrand> {
    word: AtomicU64,
    _brand: PhantomData<fn() -> B>,
}

#[cfg(crossheap)]
impl<B: CrossHeapBrand> Default for CrossHeapWeak<B> {
    fn default() -> Self {
        Self { word: AtomicU64::new(0), _brand: PhantomData }
    }
}

#[cfg(crossheap)]
impl<B: CrossHeapBrand> From<Option<CrossHeapGc<'_, B::Of<'_>>>> for CrossHeapWeak<B> {
    fn from(v: Option<CrossHeapGc<'_, B::Of<'_>>>) -> Self {
        Self { word: AtomicU64::new(v.map_or(0, |gc| WeakWord::from(gc).into())), _brand: PhantomData }
    }
}

#[cfg(crossheap)]
impl<B: CrossHeapBrand> From<CrossHeapGc<'_, B::Of<'_>>> for CrossHeapWeak<B> {
    fn from(gc: CrossHeapGc<'_, B::Of<'_>>) -> Self {
        Some(gc).into()
    }
}

#[cfg(crossheap)]
impl<B: CrossHeapBrand> CrossHeapWeak<B> {
    /// 改指向（`None` 清空）。不用目标堆的 mutator：偏移和出生戳都从对象本身读。
    pub fn set(&self, v: Option<CrossHeapGc<'_, B::Of<'_>>>) {
        self.word.store(v.map_or(0, |gc| WeakWord::from(gc).into()), Ordering::Release);
    }

    /// 取目标对象；`target` 是它所在那个堆的 mutator。空的、指向别的堆的、目标已经被回收的（位置空着或被新对象占了）、
    /// 类型对不上的，都是 None。
    pub fn get<'b>(&self, target: &CrossHeapMutator<'b>) -> Option<CrossHeapGc<'b, B::Of<'b>>> {
        let data = target.locate(self.word.load(Ordering::Acquire).into())?;
        // SAFETY: locate 认过：target 堆里此刻一个活对象的数据；在 target 的 mutate 里不会被回收
        target.is_type::<B::Of<'b>>(data).then(|| unsafe { CrossHeapGc::from_data(data) })
    }

    /// 指向哪个公开堆（要读得先打开它）；空的是 None。
    pub fn heap(&self) -> Option<usize> {
        WeakWord::from(self.word.load(Ordering::Acquire)).heap()
    }

    /// 里面那个字（发给别处、再用 [`CrossHeapWeak::from_word`] 还原）。
    pub fn word(&self) -> u64 {
        self.word.load(Ordering::Acquire)
    }

    pub fn from_word(word: u64) -> Self {
        Self { word: AtomicU64::new(word), _brand: PhantomData }
    }
}

// SAFETY: 弱引用不是边：回收不跟它
#[cfg(crossheap)]
unsafe impl<B: CrossHeapBrand> Shared for CrossHeapWeak<B> {
    const NO_EDGES: bool = true;
    fn edges(_: &mut EdgeMap, _: u32) {}
}

// SAFETY: 对私有堆没有边
#[cfg(crossheap)]
unsafe impl<B: CrossHeapBrand> Trace for CrossHeapWeak<B> {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut Tracer<'_>) {}
}

/// 一个公开堆对象的签名：可以发给别的进程（经套接字、经 dowin 转发），对方打开同一个堆后凭它找到这个对象。
/// 内容是弱引用的那个字（偏移 + 出生戳）、目标堆的身份、对象的类型键（类型名、大小、指针图的哈希），32 字节。
///
/// 签名不保活对象（同 [`CrossHeapWeak`]）：发出的一方不再引用它，它在下一次回收死，对方再凭签名找就是 None。
/// 对方不需要知道 Rust 类型也能核对（[`CrossHeapMutator::verify`]）；按类型取用 [`CrossHeapMutator::resolve`]。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg(crossheap)]
pub struct Signature {
    word: WeakWord,
    incarnation: u64,
    key: [u64; 2],
}

/// 签名序列化后的字节数
#[cfg(crossheap)]
pub const SIGNATURE_BYTES: usize = 32;

#[cfg(crossheap)]
impl Signature {
    /// 对象所在的公开堆号（要读得先打开它）。
    pub fn heap(&self) -> Option<usize> {
        self.word.heap()
    }

    /// 对象所在堆的身份（堆号会被重用，身份不会）。
    pub fn incarnation(&self) -> u64 {
        self.incarnation
    }

    /// 对象的类型键。
    pub fn key(&self) -> [u64; 2] {
        self.key
    }
}

#[cfg(crossheap)]
impl From<(WeakWord, u64, [u64; 2])> for Signature {
    fn from((word, incarnation, key): (WeakWord, u64, [u64; 2])) -> Self {
        Self { word, incarnation, key }
    }
}

#[cfg(crossheap)]
impl From<&Signature> for WeakWord {
    fn from(s: &Signature) -> Self {
        s.word
    }
}

#[cfg(crossheap)]
impl From<Signature> for [u8; SIGNATURE_BYTES] {
    fn from(s: Signature) -> Self {
        let mut out = [0; SIGNATURE_BYTES];
        for (chunk, v) in out.as_chunks_mut::<{ size_of::<u64>() }>().0.iter_mut().zip([s.word.into(), s.incarnation, s.key[0], s.key[1]]) {
            chunk.copy_from_slice(&u64::to_le_bytes(v));
        }
        out
    }
}

#[cfg(crossheap)]
impl From<&[u8; SIGNATURE_BYTES]> for Signature {
    fn from(bytes: &[u8; SIGNATURE_BYTES]) -> Self {
        let mut words = bytes.as_chunks::<{ size_of::<u64>() }>().0.iter().map(|c| u64::from_le_bytes(*c));
        let mut next = || words.next().expect("4 段");
        Self { word: next().into(), incarnation: next(), key: [next(), next()] }
    }
}

/// 把带 `'p` 的公开堆类型换个品牌：[`CrossHeapRoot<B>`] 不带生命周期，取出来时再套上当次 mutate 的 `'p`。用 [`cross_heap_brand!`](crate::cross_heap_brand) 声明。
#[cfg(crossheap)]
pub trait CrossHeapBrand {
    type Of<'p>: Shared + 'p;
}

/// 声明一个公开堆的品牌类型：`cross_heap_brand!(pub NodeB = for<'p> Node<'p>);`
#[cfg(crossheap)]
#[macro_export]
macro_rules! cross_heap_brand {
    ($(#[$meta:meta])* $vis:vis $name:ident = for<$l:lifetime> $ty:ty) => {
        $(#[$meta])*
        $vis struct $name;
        impl $crate::CrossHeapBrand for $name {
            type Of<$l> = $ty;
        }
    };
}

/// 指向公开堆对象的句柄：在入口表里占一项，只要它活着对象就不会被回收。不带生命周期，可以放进私有堆的对象里、跨线程传。
/// 用 [`CrossHeapMutator::root`] 建、[`CrossHeapRoot::get`] 取。
#[cfg(crossheap)]
pub struct CrossHeapRoot<B: CrossHeapBrand> {
    /// 进程这一侧的堆：Arc 让窗口在句柄还在时一直映射着
    heap: Arc<Inner>,
    entry: u32,
    /// 目标对象数据（公开堆不搬对象）
    data: NonNull<u8>,
    _brand: PhantomData<fn() -> B>,
}

// SAFETY: 只给出 B::Of 的共享引用（Shared: Send + Sync）；入口表是原子的
#[cfg(crossheap)]
unsafe impl<B: CrossHeapBrand> Send for CrossHeapRoot<B> {}
// SAFETY: 同上
#[cfg(crossheap)]
unsafe impl<B: CrossHeapBrand> Sync for CrossHeapRoot<B> {}

#[cfg(crossheap)]
impl<B: CrossHeapBrand> CrossHeapRoot<B> {

    /// 取目标对象；`m` 必须是目标堆的 mutator。
    pub fn get<'p>(&self, m: &CrossHeapMutator<'p>) -> CrossHeapGc<'p, B::Of<'p>> {
        assert!(m.is(&self.heap), "这个 CrossHeapRoot 不指向这个堆");
        // SAFETY: 入口保活这个对象、公开堆不搬对象；m 证明在这个堆的 mutate 里
        unsafe { CrossHeapGc::from_data(self.data.as_ptr()) }
    }

    /// 目标对象在不在 `heap` 里。
    pub fn points_into(&self, heap: &CrossHeap) -> bool {
        heap.is(&self.heap)
    }

    /// 入口号：在每个映射这个堆的地方都一样，别处可以用 [`CrossHeapMutator::find`] 凭它拿到同一个对象。
    pub fn index(&self) -> u32 {
        self.entry
    }

    /// 把这个入口变成不归任何进程的根，返回入口号：本进程退出（或被杀）也不还，主进程重启后凭入口号
    /// （[`CrossHeapMutator::find`]）接着用；不要了用 [`CrossHeapMutator::release`] 还掉。
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
#[cfg(crossheap)]
pub trait CrossHeapRootInternal {
    fn new(heap: Arc<Inner>, entry: u32, data: NonNull<u8>) -> Self;
}

#[cfg(crossheap)]
impl<B: CrossHeapBrand> CrossHeapRootInternal for CrossHeapRoot<B> {
    fn new(heap: Arc<Inner>, entry: u32, data: NonNull<u8>) -> Self {
        Self { heap, entry, data, _brand: PhantomData }
    }
}


#[cfg(crossheap)]
impl<B: CrossHeapBrand> Drop for CrossHeapRoot<B> {
    fn drop(&mut self) {
        self.heap.release_entry(self.entry);
    }
}

// SAFETY: 句柄对私有堆没有边（它本身是公开堆的一个根）
#[cfg(crossheap)]
unsafe impl<B: CrossHeapBrand> Trace for CrossHeapRoot<B> {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut Tracer<'_>) {}
}
