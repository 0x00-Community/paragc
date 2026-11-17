//! 公开堆里的类型与指针：对象头、[`Shared`]（能放进公开堆的类型）、[`PublicGc`]、[`PublicSlot`]、[`Public`]。
//!
//! 公开堆的对象可能被别的进程读、被不认识这个类型的进程回收，所以：头里不存代码，只存类型号；回收照类型表里的指针图找边；
//! 对象不许有析构；指针一律存「相对公开地址区起点的偏移」，同一个值在每个进程里都指向同一个对象。
use core::fmt;
use core::marker::PhantomData;
use core::num::{NonZeroI8, NonZeroI16, NonZeroI32, NonZeroI64, NonZeroU8, NonZeroU16, NonZeroU32, NonZeroU64};
use core::ops::Deref;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicI8, AtomicI16, AtomicI32, AtomicI64, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use crate::block::Finalize;
use crate::gc::Invariant;
use crate::header::Core;
use crate::{Trace, Tracer};

use super::heap::{Inner, PublicHeap, PublicMutator};
use super::region::Region;

/// 公开堆的对象头（16 B）：两种堆共用的标记字 / 信息字，加类型号。
#[repr(C)]
pub(crate) struct PHeader {
    pub(crate) core: Core,
    /// 类型表里的序号
    pub(crate) ty: u32,
    _pad: u32,
}

/// 公开堆对象头的字节数；数据紧跟在后面
pub(crate) const PHEADER: usize = size_of::<PHeader>();

const _: () = assert!(PHEADER == 16, "公开堆的对象头是 16 字节");

impl PHeader {
    pub(crate) fn new(bytes: usize, large: bool, ty: u32) -> Self {
        // 新对象的标记 0 不等于任何纪元（纪元从 1 开始）
        Self { core: Core::new(bytes, large, 0), ty, _pad: 0 }
    }

    pub(crate) fn data(this: *const Self) -> *mut u8 {
        this.cast::<u8>().cast_mut().wrapping_add(PHEADER)
    }

    pub(crate) fn of(data: *const u8) -> *const Self {
        data.wrapping_sub(PHEADER).cast()
    }
}

// 公开堆的对象没有析构
impl Finalize for PHeader {
    unsafe fn finalize(_: *const Core) {}
}

/// 能放进公开堆的类型：`#[repr(C)]`、只有纯数据、[`PublicSlot`] 与这样的嵌套类型、定长数组，没有析构。
/// 用 `#[derive(Shared)]` 生成，别手写。
///
/// # Safety
/// `edges` 必须把每个 [`PublicSlot`] 的字节偏移（相对 `at`）报给 `map`，漏了就是悬垂指针；`NO_EDGES` 只能在确实没有时为 true。
pub unsafe trait Shared: Send + Sync {
    /// 类型里没有任何 [`PublicSlot`]
    const NO_EDGES: bool = false;

    /// 把自己里面每个指针槽的偏移（`at` + 槽在自己里面的偏移）报给 `map`。
    fn edges(map: &mut EdgeMap, at: u32);
}

/// 一个类型的指针图：对象数据里每个指针槽的字节偏移。
pub struct EdgeMap(pub(crate) Vec<u32>);

impl EdgeMap {
    pub fn edge(&mut self, at: u32) {
        self.0.push(at);
    }

    /// 字段偏移转成指针图里的偏移（derive 用）。
    pub fn offset(bytes: usize) -> u32 {
        u32::try_from(bytes).expect("公开堆的对象不超过 4 GiB")
    }
}

/// 不含指针槽的类型。
macro_rules! leaf {
    ($($ty:ty),* $(,)?) => {$(
        // SAFETY: 没有指针槽
        unsafe impl Shared for $ty {
            const NO_EDGES: bool = true;
            fn edges(_: &mut EdgeMap, _: u32) {}
        }
    )*};
}

leaf!(
    (), bool, char, u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, f32, f64,
    NonZeroU8, NonZeroU16, NonZeroU32, NonZeroU64, NonZeroI8, NonZeroI16, NonZeroI32, NonZeroI64,
    AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicI8, AtomicI16, AtomicI32, AtomicI64,
);

// SAFETY: 没有值
unsafe impl<T: ?Sized + Send + Sync> Shared for PhantomData<T> {
    const NO_EDGES: bool = true;
    fn edges(_: &mut EdgeMap, _: u32) {}
}

// SAFETY: 逐个元素报
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
pub struct PublicGc<'p, T> {
    ptr: NonNull<T>,
    _brand: PhantomData<(&'p T, Invariant<'p>)>,
}

// SAFETY: 只给出 &T，T: Shared 要求 Send + Sync
unsafe impl<T: Send + Sync> Send for PublicGc<'_, T> {}
// SAFETY: 同上
unsafe impl<T: Send + Sync> Sync for PublicGc<'_, T> {}

impl<T> Clone for PublicGc<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for PublicGc<'_, T> {}

impl<'p, T> PublicGc<'p, T> {
    /// SAFETY（调用方）：`ptr` 是这个公开堆里一个活对象的数据指针，品牌 `'p` 正确。
    pub(crate) unsafe fn from_data(ptr: *mut u8) -> Self {
        // SAFETY: 见上
        Self { ptr: unsafe { NonNull::new_unchecked(ptr.cast()) }, _brand: PhantomData }
    }

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

    /// 公开偏移：相对公开地址区起点（每个进程里都一样）。
    pub(crate) fn offset(self) -> u64 {
        let addr = self.ptr.as_ptr().addr();
        (addr - Region::base_of(addr)) as u64
    }
}

impl<T> Deref for PublicGc<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: 同 get
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: fmt::Debug> fmt::Debug for PublicGc<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// 公开堆对象里的原子指针槽：存目标的公开偏移（0 = 空），所以同一个值在每个映射这个堆的进程里都对。
/// 读写都要带 [`PublicMutator`]（换算成本进程地址要用它的区起点）。
#[repr(transparent)]
pub struct PublicSlot<'p, T> {
    off: AtomicU64,
    _brand: PhantomData<PublicGc<'p, T>>,
}

impl<T> Default for PublicSlot<'_, T> {
    fn default() -> Self {
        Self { off: AtomicU64::new(0), _brand: PhantomData }
    }
}

impl<'p, T> From<Option<PublicGc<'p, T>>> for PublicSlot<'p, T> {
    fn from(v: Option<PublicGc<'p, T>>) -> Self {
        Self { off: AtomicU64::new(Self::raw(v)), _brand: PhantomData }
    }
}

impl<'p, T> From<PublicGc<'p, T>> for PublicSlot<'p, T> {
    fn from(v: PublicGc<'p, T>) -> Self {
        Some(v).into()
    }
}

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
unsafe impl<T: Send + Sync> Shared for PublicSlot<'_, T> {
    fn edges(map: &mut EdgeMap, at: u32) {
        map.edge(at);
    }
}

/// 把带 `'p` 的公开堆类型换个品牌：[`Public<B>`] 不带生命周期，取出来时再套上当次 mutate 的 `'p`。用 [`public_brand!`](crate::public_brand) 声明。
pub trait PublicBrand {
    type Of<'p>: Shared + 'p;
}

/// 声明一个公开堆的品牌类型：`public_brand!(pub NodeB = for<'p> Node<'p>);`
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
pub struct Public<B: PublicBrand> {
    /// 进程这一侧的堆：Arc 让窗口在句柄还在时一直映射着
    heap: Arc<Inner>,
    entry: u32,
    /// 目标对象数据（公开堆不搬对象）
    data: NonNull<u8>,
    _brand: PhantomData<fn() -> B>,
}

// SAFETY: 只给出 B::Of 的共享引用（Shared: Send + Sync）；入口表是原子的
unsafe impl<B: PublicBrand> Send for Public<B> {}
// SAFETY: 同上
unsafe impl<B: PublicBrand> Sync for Public<B> {}

impl<B: PublicBrand> Public<B> {
    pub(crate) fn new(heap: Arc<Inner>, entry: u32, data: NonNull<u8>) -> Self {
        Self { heap, entry, data, _brand: PhantomData }
    }

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
}

impl<B: PublicBrand> Drop for Public<B> {
    fn drop(&mut self) {
        self.heap.release_entry(self.entry);
    }
}

// SAFETY: 句柄对私有堆没有边（它本身是公开堆的一个根）
unsafe impl<B: PublicBrand> Trace for Public<B> {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut Tracer<'_>) {}
}
