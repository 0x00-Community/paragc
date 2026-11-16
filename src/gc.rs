//! 堆指针 [`Gc`] 与原子指针槽 [`Slot`]。
use core::cell::Cell;
use core::fmt;
use core::marker::PhantomData;
use core::ops::Deref;
use core::ptr::{NonNull, null_mut};
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::Mutator;
use crate::header::Header;

/// 品牌：`'gc` 不变（invariant），`Gc` 逃不出 [`crate::Heap::mutate`] 的闭包。
pub(crate) type Invariant<'gc> = PhantomData<Cell<&'gc ()>>;

/// Gc 的品牌：借着 `&'gc T`，再加上对 `'gc` 不变
type Brand<'gc, T> = PhantomData<(&'gc T, fn(&'gc ()) -> &'gc ())>;

/// 指向堆对象的指针，只在一次 `mutate` 里有效（`'gc` 是那次调用的品牌）。对象不会移动。
pub struct Gc<'gc, T> {
    ptr: NonNull<T>,
    _brand: Brand<'gc, T>,
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

impl<'gc, T> Gc<'gc, T> {
    /// SAFETY（调用方）：`ptr` 是本堆里一个活对象的数据指针，品牌 `'gc` 正确。
    pub(crate) unsafe fn from_data(ptr: *mut T) -> Self {
        // SAFETY: 见上
        Self { ptr: unsafe { NonNull::new_unchecked(ptr) }, _brand: PhantomData }
    }

    pub(crate) fn header(self) -> *const Header {
        Header::of(self.ptr.as_ptr().cast())
    }

    pub fn as_ptr(self) -> *const T {
        self.ptr.as_ptr()
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
pub(crate) const UNCOUNTED: usize = usize::MAX;
/// [`Slot::counted`] 的最低位：本轮已登记（地址按粒度对齐，最低位空着）
pub(crate) const LOGGED: usize = 1;

/// 抹掉类型的槽：写屏障登记、回收时结算用（与 [`Slot`] 同布局）。
#[repr(C)]
pub(crate) struct RawSlot {
    pub(crate) ptr: AtomicPtr<u8>,
    pub(crate) counted: AtomicUsize,
}

impl RawSlot {
    fn header(p: *mut u8) -> Option<*const Header> {
        (!p.is_null()).then(|| Header::of(p))
    }

    fn from_counted(c: usize) -> Option<*const Header> {
        (c != UNCOUNTED && c & !LOGGED != 0).then(|| Header::of(core::ptr::with_exposed_provenance::<u8>(c & !LOGGED)))
    }

    /// 本轮第一次被写：打上登记标志，返回 true（调用方把槽地址交给回收器）。不计数的槽返回 false。
    pub(crate) fn log(&self) -> bool {
        let c = self.counted.load(Ordering::Relaxed);
        c != UNCOUNTED && c & LOGGED == 0 && self.counted.compare_exchange(c, c | LOGGED, Ordering::AcqRel, Ordering::Relaxed).is_ok()
    }

    /// 清掉登记标志（计数不可信时丢弃登记）。
    pub(crate) fn unlog(&self) {
        self.counted.fetch_and(!LOGGED, Ordering::AcqRel);
    }

    /// 改成计入当前值；返回（当前值, 上次计入的值）。
    pub(crate) fn recount(&self) -> (Option<*const Header>, Option<*const Header>) {
        let p = self.ptr.load(Ordering::Acquire);
        let prev = self.counted.swap(p.expose_provenance(), Ordering::AcqRel);
        (Self::header(p), Self::from_counted(prev))
    }

    /// 上次计入的值（释放它所在的对象时 −1）。
    pub(crate) fn counted(&self) -> Option<*const Header> {
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

    /// 独占时改写（疏散回收改成新地址）。
    pub(crate) fn store_exclusive(&mut self, v: Option<Gc<'gc, T>>) {
        *self.ptr.get_mut() = Self::raw(v);
    }

    /// 抹掉类型的视图。
    pub(crate) fn raw_slot(&self) -> &RawSlot {
        // SAFETY: Slot 与 RawSlot 都是 repr(C)，前两个字段同类型同顺序（AtomicPtr<T> 与 AtomicPtr<u8> 同布局）
        unsafe { &*(&raw const *self).cast::<RawSlot>() }
    }

    /// 写屏障：被覆盖的旧值（并发标记的初始快照）、写进去的新值（年轻对象 ⇒ 记下这个槽，年轻回收从它出发）、
    /// 引用计数登记（老对象里的槽本轮第一次被写）。
    fn barrier(&self, m: &Mutator<'gc>, old: *mut T, new: *mut T) {
        let header = |p: *mut T| (!p.is_null()).then(|| Header::of(p.cast()));
        m.written(self.raw_slot(), header(old), header(new));
    }
}

impl<T: fmt::Debug> fmt::Debug for Slot<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Slot").field(&self.get()).finish()
    }
}
