//! 堆指针 [`Gc`] 与原子指针槽 [`Slot`]。
use core::cell::Cell;
use core::fmt;
use core::marker::PhantomData;
use core::ops::Deref;
use core::ptr::{NonNull, null_mut};
use core::sync::atomic::{AtomicPtr, Ordering};

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
/// 写入要带 [`Mutator`]：并发标记期间被覆盖的旧值要经写屏障交给回收器（初始快照）。
pub struct Slot<'gc, T> {
    ptr: AtomicPtr<T>,
    _brand: PhantomData<Gc<'gc, T>>,
}

impl<T> Default for Slot<'_, T> {
    fn default() -> Self {
        Self { ptr: AtomicPtr::new(null_mut()), _brand: PhantomData }
    }
}

impl<'gc, T> From<Option<Gc<'gc, T>>> for Slot<'gc, T> {
    fn from(v: Option<Gc<'gc, T>>) -> Self {
        Self { ptr: AtomicPtr::new(Self::raw(v)), _brand: PhantomData }
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
        let old = self.ptr.swap(Self::raw(v), Ordering::AcqRel);
        Self::barrier(m, old);
        Self::wrap(old)
    }

    /// 槽里是 `current` 才换成 `new`；失败时返回槽里的值。
    pub fn compare_exchange(&self, m: &Mutator<'gc>, current: Option<Gc<'gc, T>>, new: Option<Gc<'gc, T>>) -> Result<Option<Gc<'gc, T>>, Option<Gc<'gc, T>>> {
        let done = self.ptr.compare_exchange(Self::raw(current), Self::raw(new), Ordering::AcqRel, Ordering::Acquire);
        if let Ok(old) = done {
            Self::barrier(m, old);
        }
        done.map(Self::wrap).map_err(Self::wrap)
    }

    /// 被覆盖的旧值交给写屏障。
    fn barrier(m: &Mutator<'gc>, old: *mut T) {
        if !old.is_null() {
            m.overwritten(Header::of(old.cast()));
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for Slot<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Slot").field(&self.get()).finish()
    }
}
