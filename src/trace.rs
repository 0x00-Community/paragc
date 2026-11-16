//! [`Trace`]：对象告诉回收器自己引用了谁；[`Tracer`]：回收器那一侧，标记并把新对象交给工作队列。
use core::marker::PhantomData;
use core::num::{NonZeroI8, NonZeroI16, NonZeroI32, NonZeroI64, NonZeroIsize, NonZeroU8, NonZeroU16, NonZeroU32, NonZeroU64, NonZeroUsize};
use core::sync::atomic::{AtomicBool, AtomicI8, AtomicI16, AtomicI32, AtomicI64, AtomicIsize, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize};
use core::time::Duration;
use std::collections::{BTreeMap, HashMap, VecDeque};

use crossbeam_deque::Worker;

use crate::block::Block;
use crate::header::Header;
use crate::{Gc, Slot};

/// 能放进堆的类型。用 `#[derive(Trace)]` 生成，别手写。
///
/// - `Send + Sync`：对象可能在任何线程上被读、被回收线程追踪和析构
/// - 有 `Gc` 字段的类型不许实现 `Drop`（析构的先后不定，`Drop` 里可能碰到已经析构的对象）；derive 会检查
///
/// # Safety
/// `trace` 必须对每个直接持有的 `Gc` / `Slot` 调一次 [`Tracer::edge`]（或对字段调 `trace`），漏了就是悬垂指针。
/// `NO_EDGES` 只能在类型里确实没有 `Gc` 时为 true。
pub unsafe trait Trace: Send + Sync {
    /// 类型里没有任何 `Gc`：回收器标记后不必再追踪它
    const NO_EDGES: bool = false;

    fn trace(&self, t: &mut Tracer<'_>);
}

/// 标记阶段的一个工人：标记对象、标记它跨的行，把需要追踪的对象压进自己的工作队列。
pub struct Tracer<'t> {
    work: &'t Worker<usize>,
    epoch: u32,
}

impl<'t> Tracer<'t> {
    pub(crate) fn new(work: &'t Worker<usize>, epoch: u32) -> Self {
        Self { work, epoch }
    }

    /// 一条边。
    pub fn edge<T: Trace>(&mut self, gc: Gc<'_, T>) {
        self.visit(gc.header(), T::NO_EDGES);
    }

    /// 标记对象；本轮第一次见到且它有边时压进工作队列。
    pub(crate) fn visit(&mut self, header: *const Header, no_edges: bool) {
        // SAFETY: header 来自本堆的 Gc / 根，对象活着
        let h = unsafe { &*header };
        if !h.mark(self.epoch) {
            return;
        }
        if !h.is_large() {
            Block::containing(header.cast()).mark_lines(header.cast(), h.bytes(), self.epoch);
        }
        if !no_edges {
            self.work.push(header.expose_provenance());
        }
    }

    /// 追踪工作队列里取出来的一个对象。
    pub(crate) fn scan(&mut self, item: usize) {
        let header = core::ptr::with_exposed_provenance::<Header>(item);
        // SAFETY: 队列里只有本轮标记过的活对象
        unsafe { Header::trace(header, self) };
    }
}

// SAFETY: Gc 本身就是一条边
unsafe impl<T: Trace> Trace for Gc<'_, T> {
    fn trace(&self, t: &mut Tracer<'_>) {
        t.edge(*self);
    }
}

// SAFETY: 槽里的值就是一条边
unsafe impl<T: Trace> Trace for Slot<'_, T> {
    fn trace(&self, t: &mut Tracer<'_>) {
        if let Some(gc) = self.get() {
            t.edge(gc);
        }
    }
}

/// 不含 `Gc` 的类型：什么都不用追踪。
macro_rules! leaf {
    ($($ty:ty),* $(,)?) => {$(
        // SAFETY: 没有 Gc
        unsafe impl Trace for $ty {
            const NO_EDGES: bool = true;
            fn trace(&self, _: &mut Tracer<'_>) {}
        }
    )*};
}

leaf!(
    (), bool, char, u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, f32, f64, String, str, Duration,
    NonZeroU8, NonZeroU16, NonZeroU32, NonZeroU64, NonZeroUsize, NonZeroI8, NonZeroI16, NonZeroI32, NonZeroI64, NonZeroIsize,
    AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, AtomicI8, AtomicI16, AtomicI32, AtomicI64, AtomicIsize,
);

// SAFETY: 没有值
unsafe impl<T: ?Sized + Send + Sync> Trace for PhantomData<T> {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut Tracer<'_>) {}
}

/// 逐个追踪元素的容器：`$ty` 用 `T` 作元素类型，`$iter` 把 `&self` 变成元素迭代器。
macro_rules! each {
    ($([$($gen:tt)*] $ty:ty => |$s:ident| $iter:expr;)*) => {$(
        // SAFETY: 逐个追踪了全部元素
        unsafe impl<$($gen)*> Trace for $ty {
            const NO_EDGES: bool = T::NO_EDGES;
            fn trace(&self, t: &mut Tracer<'_>) {
                if !Self::NO_EDGES {
                    let $s = self;
                    for v in $iter {
                        v.trace(t);
                    }
                }
            }
        }
    )*};
}

each! {
    [T: Trace] Option<T> => |s| s;
    [T: Trace + ?Sized] Box<T> => |s| core::iter::once(&**s);
    [T: Trace] [T] => |s| s;
    [T: Trace, const N: usize] [T; N] => |s| s;
    [T: Trace] Vec<T> => |s| s;
    [T: Trace] VecDeque<T> => |s| s;
    [K: Send + Sync, T: Trace] BTreeMap<K, T> => |s| s.values();
    [K: Send + Sync, T: Trace, H: Send + Sync] HashMap<K, T, H> => |s| s.values();
}

/// 元组：逐个字段追踪。
macro_rules! tuple {
    ($($name:ident)+) => {
        // SAFETY: 逐个追踪了全部字段
        unsafe impl<$($name: Trace),+> Trace for ($($name,)+) {
            const NO_EDGES: bool = $($name::NO_EDGES)&&+;
            #[allow(non_snake_case)]
            fn trace(&self, t: &mut Tracer<'_>) {
                let ($($name,)+) = self;
                $($name.trace(t);)+
            }
        }
    };
}

tuple!(A);
tuple!(A B);
tuple!(A B C);
tuple!(A B C D);
tuple!(A B C D E);
tuple!(A B C D E F);
