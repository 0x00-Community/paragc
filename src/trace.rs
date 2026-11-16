//! [`Trace`]：对象告诉回收器自己引用了谁；[`Tracer`]：回收器那一侧，标记并把新对象交给工作队列。
use core::marker::PhantomData;
use core::num::{NonZeroI8, NonZeroI16, NonZeroI32, NonZeroI64, NonZeroIsize, NonZeroU8, NonZeroU16, NonZeroU32, NonZeroU64, NonZeroUsize};
use core::sync::atomic::{AtomicBool, AtomicI8, AtomicI16, AtomicI32, AtomicI64, AtomicIsize, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize};
use core::time::Duration;
use std::collections::{BTreeMap, HashMap, VecDeque};

use crossbeam_deque::Worker;

use crate::block::Block;
use crate::copy::Copier;
use crate::header::{Claim, Header};
use crate::{Gc, Slot};

/// 能放进堆的类型。用 `#[derive(Trace)]` 生成，别手写。
///
/// - `Send + Sync`：对象可能在任何线程上被读、被回收线程追踪和析构
/// - 有 `Gc` 字段的类型不许实现 `Drop`（析构的先后不定，`Drop` 里可能碰到已经析构的对象）；derive 会检查
///
/// # Safety
/// `trace` 必须对每个直接持有的 `Gc` / `Slot` 调一次 [`Tracer::edge`]（或对字段调 `trace`），漏了就是悬垂指针；
/// `relocate` 同样对每条边调一次 [`Tracer::relocate`]（或对字段调 `relocate`），回收器会把搬走的对象改成新地址。
/// `NO_EDGES` 只能在类型里确实没有 `Gc` 时为 true。
pub unsafe trait Trace: Send + Sync {
    /// 类型里没有任何 `Gc`：回收器标记后不必再追踪它
    const NO_EDGES: bool = false;

    fn trace(&self, t: &mut Tracer<'_>);

    /// 疏散回收（独占堆，没有 mutator）时代替 `trace`：可以改写边。没有边的类型用缺省实现。
    fn relocate(&mut self, t: &mut Tracer<'_>) {
        let _ = t;
        assert!(Self::NO_EDGES, "有 Gc 的类型必须实现 Trace::relocate（用 #[derive(Trace)]）");
    }
}

/// 标记阶段的一个工人：标记对象、标记它跨的行，把需要追踪的对象压进自己的工作队列；
/// 疏散回收时还把候选块里的对象搬走，并把边改成新地址。
pub struct Tracer<'t> {
    work: &'t Worker<usize>,
    epoch: u32,
    /// 疏散回收时的复制缓冲（有它 = 重定位模式）
    copier: Option<&'t mut Copier>,
    /// 计数模式（晋升 / 重新计数）：扫描到的每条边给目标 +1，值是当前计数的奇偶
    count: Option<bool>,
    /// 释放模式（引用计数减到 0 的对象）：每条边的目标交给它 −1，不标记
    decs: Option<&'t mut Vec<usize>>,
}

impl<'t> Tracer<'t> {
    pub(crate) fn new(work: &'t Worker<usize>, epoch: u32, copier: Option<&'t mut Copier>) -> Self {
        Self { work, epoch, copier, count: None, decs: None }
    }

    /// 计数模式：扫描时每条边给目标 +1（种子——根与记忆集——直接走 visit，不计）。
    pub(crate) fn counting(mut self, parity: Option<bool>) -> Self {
        self.count = parity;
        self
    }

    /// 释放模式：访问一个死对象时把它每条边的目标（槽取上次计入的值）放进 `decs`。
    pub(crate) fn releasing(work: &'t Worker<usize>, decs: &'t mut Vec<usize>) -> Self {
        Self { work, epoch: 0, copier: None, count: None, decs: Some(decs) }
    }

    fn inc(&self, header: *const Header) {
        if let Some(parity) = self.count {
            // SAFETY: header 是活对象
            unsafe { &*header }.rc_inc(parity);
        }
    }

    /// 重定位模式（疏散回收）：取出来的对象走 `relocate` 而不是 `trace`。
    pub(crate) fn relocating(&self) -> bool {
        self.copier.is_some()
    }

    /// 一条边（只读：并发标记、不疏散的回收）。
    pub fn edge<T: Trace>(&mut self, gc: Gc<'_, T>) {
        if let Some(decs) = self.decs.as_deref_mut() {
            decs.push(gc.header().expose_provenance());
            return;
        }
        self.visit(gc.header(), T::NO_EDGES);
        self.inc(gc.header());
    }

    /// 一条可改写的边（疏散回收）：对象被搬走了就改成新地址。
    pub fn relocate<T: Trace>(&mut self, gc: &mut Gc<'_, T>) {
        *gc = self.forward_gc(*gc);
        self.inc(gc.header());
    }

    /// 标记（必要时搬走）并返回对象现在的位置；不计数。
    fn forward_gc<'gc, T: Trace>(&mut self, gc: Gc<'gc, T>) -> Gc<'gc, T> {
        let old = gc.header();
        let new = self.forward(old, T::NO_EDGES);
        // SAFETY: new 是同一个对象（或搬过去的新头），品牌与类型不变
        if core::ptr::eq(old, new) { gc } else { unsafe { Gc::from_data(Header::data(new).cast()) } }
    }

    /// 一个槽（只读）：平时等于一条边；计数模式下改成计入当前值并 +1；释放模式下把上次计入的值交出去 −1。
    pub(crate) fn slot<T: Trace>(&mut self, slot: &Slot<'_, T>) {
        let raw = slot.raw_slot();
        if let Some(decs) = self.decs.as_deref_mut() {
            if let Some(h) = raw.counted() {
                decs.push(h.expose_provenance());
            }
            return;
        }
        if self.count.is_some() {
            if let (Some(cur), _) = raw.recount() {
                self.visit(cur, T::NO_EDGES);
                self.inc(cur);
            }
        } else if let Some(gc) = slot.get() {
            self.visit(gc.header(), T::NO_EDGES);
        }
    }

    /// 一个可改写的槽（疏散回收）：搬走了就改成新地址；计数模式下计入新值并 +1。
    pub(crate) fn slot_mut<T: Trace>(&mut self, slot: &mut Slot<'_, T>) {
        if let Some(gc) = slot.get() {
            let new = self.forward_gc(gc);
            slot.store_exclusive(Some(new));
        }
        if self.count.is_some()
            && let (Some(cur), _) = slot.raw_slot().recount()
        {
            self.inc(cur);
        }
    }

    /// 标记对象，它在疏散候选块里就搬走；返回对象现在的头。
    pub(crate) fn forward(&mut self, header: *const Header, no_edges: bool) -> *const Header {
        // SAFETY: header 来自本堆的 Gc / 根，对象活着
        let h = unsafe { &*header };
        if let Some(copier) = self.copier.as_deref_mut()
            && !h.is_large()
            && Block::containing(header.cast()).is_evacuating()
        {
            return match h.claim() {
                Claim::Moved(to) => to,
                Claim::Mine => {
                    // SAFETY: 本线程认领了它；它在块里
                    let to = unsafe { copier.copy(header) };
                    // SAFETY: 同上
                    unsafe { Header::forward(header, to) };
                    if !no_edges {
                        self.work.push(to.expose_provenance());
                    }
                    to
                }
            };
        }
        self.visit(header, no_edges);
        header
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

    /// 追踪（或重定位）工作队列里取出来的一个对象。
    pub(crate) fn scan(&mut self, item: usize) {
        let header = core::ptr::with_exposed_provenance::<Header>(item);
        // SAFETY: 队列里只有本轮标记过的活对象；重定位模式时它只被这一个线程取出（独占）
        unsafe { Header::visit(header, self) };
    }
}

// SAFETY: Gc 本身就是一条边
unsafe impl<T: Trace> Trace for Gc<'_, T> {
    fn trace(&self, t: &mut Tracer<'_>) {
        t.edge(*self);
    }

    fn relocate(&mut self, t: &mut Tracer<'_>) {
        t.relocate(self);
    }
}

// SAFETY: 槽里的值就是一条边
unsafe impl<T: Trace> Trace for Slot<'_, T> {
    fn trace(&self, t: &mut Tracer<'_>) {
        t.slot(self);
    }

    fn relocate(&mut self, t: &mut Tracer<'_>) {
        t.slot_mut(self);
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
    ($([$($gen:tt)*] $ty:ty => |$s:ident| $iter:expr, |$m:ident| $iter_mut:expr;)*) => {$(
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
            fn relocate(&mut self, t: &mut Tracer<'_>) {
                if !Self::NO_EDGES {
                    let $m = self;
                    for v in $iter_mut {
                        v.relocate(t);
                    }
                }
            }
        }
    )*};
}

each! {
    [T: Trace] Option<T> => |s| s, |m| m;
    [T: Trace + ?Sized] Box<T> => |s| core::iter::once(&**s), |m| core::iter::once(&mut **m);
    [T: Trace] [T] => |s| s, |m| m;
    [T: Trace, const N: usize] [T; N] => |s| s, |m| m;
    [T: Trace] Vec<T> => |s| s, |m| m;
    [T: Trace] VecDeque<T> => |s| s, |m| m;
    [K: Send + Sync, T: Trace] BTreeMap<K, T> => |s| s.values(), |m| m.values_mut();
    [K: Send + Sync, T: Trace, H: Send + Sync] HashMap<K, T, H> => |s| s.values(), |m| m.values_mut();
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
            #[allow(non_snake_case)]
            fn relocate(&mut self, t: &mut Tracer<'_>) {
                let ($($name,)+) = self;
                $($name.relocate(t);)+
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
