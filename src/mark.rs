//! 标记与追踪：[`Trace`] / [`Tracer`]、工作窃取的标记工人 [`Crew`]、私有堆的标记 [`Marking`] 与疏散复制 [`Copier`]、公开堆按指针图的标记 [`Marker`]。
//!
//! [`Trace`]：对象告诉回收器自己引用了谁；[`Tracer`]：回收器那一侧，标记并把新对象交给工作队列。
//!
//! 并行标记：每个工人一个 crossbeam 双端队列（自己 LIFO 弹），根与写屏障记下的旧值放堆的公共灰队列；
//! 自己的空了就成批偷（`steal_batch_and_pop`，一次搬走别人队列的一半）。
//!
//! 并发标记时 mutator 还在跑，写屏障会继续往灰队列里放东西：这一轮全部空闲就先停，剩下的由结束回收（独占堆）时再跑一轮清空。
//!
//! 结束判定：自己的队列空、也偷不到时把空闲计数加一，然后边退避边偷；空闲计数等于工人数时
//! 所有队列都空、也没人还在追踪（追踪中的工人没计入空闲），标记结束。偷到了就先减一再干活；
//! 偷到还没来得及减一时别人可能先看到「全空闲」退出，但手里的活仍由偷到的人做完，结果不受影响。
//!
//! 疏散时每个回收线程一个复制缓冲：在目标块的空洞里碰撞分配，把被搬的对象整个抄过去，
//! 新副本标成本轮（含行标记与起点位）。目标块先从「可搬入」队列领（清扫后没再用过的非候选块），没有就开新块。
//!
//! 公开堆的并行标记：照类型表里的指针图找边，不调用对象的代码（回收方不需要认识这些类型）。
//! 工作窃取与结束判定同私有堆（[`crate::mark`]）：每个工人一个双端队列，空了成批偷，全部空闲即结束。
//!
//! 可以几个堆一起标（[`super::CrossHeap::collect_group`]）：队列里每一项是「组里第几个堆 + 对象头地址」，
//! 指向组里别的堆的边交给那个堆（按它的纪元标），并报到 `found` 里（之后重建跨堆登记）；指向组外的不跟。
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::num::{NonZeroI16, NonZeroI32, NonZeroI64, NonZeroI8, NonZeroIsize, NonZeroU16, NonZeroU32, NonZeroU64, NonZeroU8, NonZeroUsize};
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicI16, AtomicI32, AtomicI64, AtomicI8, AtomicIsize, AtomicU16, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use core::time::Duration;
use std::collections::{BTreeMap, HashMap, VecDeque};

use crossbeam_deque::{Injector, Steal, Stealer, Worker};
use crossbeam_utils::{Backoff, CachePadded};

use crate::{Gc, Slot, WordSlot};
use crate::handle::{GcInternal, RawSlot, RootTable, SlotInternal, WordSlotInternal};
#[cfg(crossheap)]
#[cfg(crossheap)]
use crate::handle::FAR_EDGE;
#[cfg(crossheap)]
use crate::heap::Inner;
use crate::layout::{Block, Claim, Header, Pages};
#[cfg(crossheap)]
#[cfg(crossheap)]
use crate::layout::PHeader;
#[cfg(crossheap)]
use crate::shm::WINDOWS;
use crate::sync::{Pool, SegVec};

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
    /// 只有这一个线程在改计数（单线程标记）：+1 用普通读写
    exclusive: bool,
    /// 并发标记时的引用计数登记：重新计数的槽在读与改之间被 mutator 改写了，由这里补登记（见 [`Tracer::slot`]）
    log: Option<&'t Injector<usize>>,
    /// 一起回收几个堆（[`crate::Heap::collect_group`]）：追踪到的 Far 把（目标堆的根表, 目标对象的头）报到这里
    group: Option<&'t Injector<(usize, usize)>>,
}

impl Tracer<'_> {

    fn inc(&self, header: *const Header) {
        if let Some(parity) = self.count {
            // SAFETY: header 是活对象
            let h = unsafe { &*header };
            if self.exclusive {
                h.rc_inc_exclusive(parity);
            } else {
                h.rc_inc(parity);
            }
        }
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

}

/// 只给本 crate 用的操作：trait 在私有模块里、不导出，crate 外拿不到。
pub trait TracerInternal<'t> {
    fn new(work: &'t Worker<usize>, epoch: u32, copier: Option<&'t mut Copier>) -> Self;

    /// 计数模式：扫描时每条边给目标 +1（种子——根与记忆集——直接走 visit，不计）。
    /// `exclusive`：只有这一个线程在改计数。
    fn counting(self, parity: Option<bool>, exclusive: bool) -> Self;

    /// 释放模式：访问一个死对象时把它每条边的目标（槽取上次计入的值）放进 `decs`。
    fn releasing(work: &'t Worker<usize>, decs: &'t mut Vec<usize>) -> Self;

    /// 并发标记：给登记队列（见 [`Tracer::log`]）。
    fn logging(self, log: Option<&'t Injector<usize>>) -> Self;

    /// 一起回收：给报告 Far 目标的队列。
    fn grouping(self, group: Option<&'t Injector<(usize, usize)>>) -> Self;

    /// 追踪到一个 [`crate::Far`]：一起回收时报上去（由目标堆接着标），平时什么都不做（目标堆的入口就是根）。
    fn far(&mut self, table: usize, header: *const Header);

    /// 重定位模式（疏散回收）：取出来的对象走 `relocate` 而不是 `trace`。
    fn relocating(&self) -> bool;

    /// 一个槽（只读）：平时等于一条边；计数模式下改成计入当前值并 +1；释放模式下把上次计入的值交出去 −1。
    fn slot<T: Trace>(&mut self, slot: &Slot<'_, T>);

    /// 一个可改写的槽（疏散回收）：搬走了就改成新地址；计数模式下计入新值并 +1。
    fn slot_mut<T: Trace>(&mut self, slot: &mut Slot<'_, T>);

    /// 抹掉类型的槽（[`Slot`]、[`crate::WordSlot`]），只读；`no_edges`：目标没有边
    fn raw(&mut self, raw: &RawSlot, no_edges: bool);

    /// 抹掉类型的槽，可改写（疏散回收）
    fn raw_mut(&mut self, raw: &RawSlot, no_edges: bool);

    /// 标记对象，它在疏散候选块里就搬走；返回对象现在的头。
    fn forward(&mut self, header: *const Header, no_edges: bool) -> *const Header;

    /// 标记对象；本轮第一次见到且它有边时压进工作队列。
    fn visit(&mut self, header: *const Header, no_edges: bool);

    /// 追踪（或重定位）工作队列里取出来的一个对象。
    fn scan(&mut self, item: usize);
}

impl<'t> TracerInternal<'t> for Tracer<'t> {
    fn new(work: &'t Worker<usize>, epoch: u32, copier: Option<&'t mut Copier>) -> Self {
        Self { work, epoch, copier, count: None, decs: None, exclusive: false, log: None, group: None }
    }

    fn counting(mut self, parity: Option<bool>, exclusive: bool) -> Self {
        self.count = parity;
        self.exclusive = exclusive;
        self
    }

    fn releasing(work: &'t Worker<usize>, decs: &'t mut Vec<usize>) -> Self {
        Self { work, epoch: 0, copier: None, count: None, decs: Some(decs), exclusive: false, log: None, group: None }
    }

    fn logging(mut self, log: Option<&'t Injector<usize>>) -> Self {
        self.log = log;
        self
    }

    fn grouping(mut self, group: Option<&'t Injector<(usize, usize)>>) -> Self {
        self.group = group;
        self
    }

    fn far(&mut self, table: usize, header: *const Header) {
        if let Some(group) = self.group {
            group.push((table, header.expose_provenance()));
        }
    }

    fn relocating(&self) -> bool {
        self.copier.is_some()
    }

    fn slot<T: Trace>(&mut self, slot: &Slot<'_, T>) {
        self.raw(slot.raw_slot(), T::NO_EDGES);
    }

    fn slot_mut<T: Trace>(&mut self, slot: &mut Slot<'_, T>) {
        self.raw_mut(slot.raw_slot(), T::NO_EDGES);
    }

    fn raw(&mut self, raw: &RawSlot, no_edges: bool) {
        if let Some(decs) = self.decs.as_deref_mut() {
            if let Some(h) = raw.counted() {
                decs.push(h.expose_provenance());
            }
            return;
        }
        if self.count.is_some() {
            let (cur, _) = raw.recount();
            // 并发标记：读出当前值与改「上次计入」之间 mutator 可能改写了槽；那时槽若还不计数（年轻对象的槽），
            // 它不会登记 ⇒ 新值永远没人计。值变了就替它登记，收尾时按「新值 +1、计入的值 −1」结算（重复登记无害）
            if let Some(log) = self.log
                && raw.current() != cur
                && raw.log()
            {
                log.push((&raw const *raw).expose_provenance());
            }
            if let Some(cur) = cur {
                self.visit(cur, no_edges);
                self.inc(cur);
            }
        } else {
            raw.promote();
            if let Some(h) = raw.current() {
                self.visit(h, no_edges);
            }
        }
    }

    fn raw_mut(&mut self, raw: &RawSlot, no_edges: bool) {
        if let Some(old) = raw.current() {
            let new = self.forward(old, no_edges);
            if !core::ptr::eq(old, new) {
                raw.relocate(new);
            }
        }
        if self.count.is_none() {
            raw.promote();
        } else if let (Some(cur), _) = raw.recount() {
            self.inc(cur);
        }
    }

    fn forward(&mut self, header: *const Header, no_edges: bool) -> *const Header {
        // SAFETY: header 来自本堆的 Gc / 根，对象活着
        let h = unsafe { &*header };
        // 本轮已经原地标过的不搬（年轻疏散里老对象直接指着的、别的堆指着的年轻对象先原地标上了）
        if let Some(copier) = self.copier.as_deref_mut()
            && !h.is_large()
            && Block::containing(header.cast()).is_evacuating()
            && !h.is_marked(self.epoch)
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

    fn visit(&mut self, header: *const Header, no_edges: bool) {
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

    fn scan(&mut self, item: usize) {
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

// SAFETY: 字里的指针（有的话）就是一条边；目标类型不知道，按有边处理
unsafe impl Trace for WordSlot<'_> {
    fn trace(&self, t: &mut Tracer<'_>) {
        t.raw(self.raw(), false);
    }

    fn relocate(&mut self, t: &mut Tracer<'_>) {
        t.raw_mut(self.raw(), false);
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

/// 一组标记工人：每个工号一个本地双端队列（自己 LIFO 弹），根与写屏障记下的放在公共灰队列；
/// 自己的空了就成批偷（`steal_batch_and_pop`，一次搬走别人队列的一半）。私有堆与公开堆的标记共用它，只是「扫一项」不同。
///
/// 结束判定：自己的队列空、也偷不到时把空闲计数加一，然后边退避边偷；空闲计数等于工人数时
/// 所有队列都空、也没人还在追踪（追踪中的工人没计入空闲），标记结束。偷到了就先减一再干活；
/// 偷到还没来得及减一时别人可能先看到「全空闲」退出，但手里的活仍由偷到的人做完，结果不受影响。
pub struct Crew<'g> {
    /// 每个工号一个本地队列，由那个工号的线程取走（只取一次）
    locals: Vec<CachePadded<UnsafeCell<Option<Worker<usize>>>>>,
    stealers: Vec<Stealer<usize>>,
    /// 公共灰队列
    grey: &'g Injector<usize>,
    idle: CachePadded<AtomicUsize>,
}

// SAFETY: 每个工号的本地队列只被一个线程取走（工号互不相同）；其余是线程安全的
unsafe impl Sync for Crew<'_> {}

impl<'g> Crew<'g> {
    pub fn new(workers: usize, grey: &'g Injector<usize>) -> Self {
        let locals: Vec<Worker<usize>> = (0..workers).map(|_| Worker::new_lifo()).collect();
        let stealers = locals.iter().map(Worker::stealer).collect();
        Self { locals: locals.into_iter().map(|w| CachePadded::new(UnsafeCell::new(Some(w)))).collect(), stealers, grey, idle: CachePadded::new(AtomicUsize::new(0)) }
    }

    pub fn len(&self) -> usize {
        self.locals.len()
    }

    /// 工号 `me` 的本地队列（每个工号只取一次，只在那个工号的线程上取）。
    pub fn take(&self, me: usize) -> Worker<usize> {
        // SAFETY: 工号 me 只在这一个线程上
        unsafe { (*self.locals[me].get()).take() }.expect("每个工号只取一次本地队列")
    }

    /// 工号 `me` 一直干到全部工人空闲：先弹自己的，空了去偷，偷不到就进入空闲等别人。
    pub fn drain(&self, me: usize, local: &Worker<usize>, mut scan: impl FnMut(usize)) {
        loop {
            while let Some(item) = local.pop() {
                scan(item);
            }
            if let Some(item) = self.steal(me, local) {
                scan(item);
                continue;
            }
            self.idle.fetch_add(1, Ordering::AcqRel);
            match self.wait(me, local) {
                Some(item) => scan(item),
                None => return,
            }
        }
    }

    /// 空闲时边退避边偷；全部空闲返回 None。
    fn wait(&self, me: usize, local: &Worker<usize>) -> Option<usize> {
        let backoff = Backoff::new();
        loop {
            if self.idle.load(Ordering::Acquire) == self.locals.len() {
                return None;
            }
            if let Some(item) = self.steal(me, local) {
                self.idle.fetch_sub(1, Ordering::AcqRel);
                return Some(item);
            }
            backoff.snooze();
        }
    }

    /// 先偷公共灰队列，再从下一个工人开始轮着偷；成批搬进自己的队列，返回其中一个。
    fn steal(&self, me: usize, local: &Worker<usize>) -> Option<usize> {
        let n = self.stealers.len();
        loop {
            let found = core::iter::once(self.grey.steal_batch_and_pop(local))
                .chain((1..n).map(|k| self.stealers[(me + k) % n].steal_batch_and_pop(local)))
                .collect::<Steal<usize>>();
            match found {
                Steal::Success(item) => return Some(item),
                Steal::Empty => return None,
                Steal::Retry => {}
            }
        }
    }
}

pub struct Marking<'g> {
    crew: Crew<'g>,
    epoch: u32,
    /// 疏散回收：每个工号一个复制缓冲（空 = 不疏散）
    copiers: Vec<CachePadded<UnsafeCell<Option<Copier>>>>,
    /// 疏散回收：0 号工人先把根表里的槽改成搬走后的新地址（同时把根标灰）
    roots: Option<&'g RootTable>,
    /// 计数模式：扫描的每条边给目标 +1（值是计数奇偶）
    count: Option<bool>,
    /// 并发标记：引用计数登记队列（重新计数时被 mutator 改写的槽补登记）
    log: Option<&'g Injector<usize>>,
    /// 一起回收：追踪到的 Far 报到这里
    group: Option<&'g Injector<(usize, usize)>>,
    /// 年轻疏散（[`crate::YoungEvacuation::Full`]）：0 号工人把记忆集里这些槽改成搬走后的新地址
    slots: &'g [usize],
}

// SAFETY: 每个工号的复制缓冲只被一个线程取走（工号互不相同）；其余是线程安全的
unsafe impl Sync for Marking<'_> {}

impl<'g> Marking<'g> {
    pub fn new(workers: usize, epoch: u32, grey: &'g Injector<usize>) -> Self {
        Self {
            crew: Crew::new(workers, grey),
            epoch,
            copiers: Vec::new(),
            roots: None,
            count: None,
            log: None,
            group: None,
            slots: &[],
        }
    }

    /// 年轻疏散：记忆集里的槽（地址）由 0 号工人改成搬走后的新地址（同时把目标标灰）。
    pub fn relocating_slots(mut self, slots: &'g [usize]) -> Self {
        self.slots = slots;
        self
    }

    /// 并发标记：给引用计数登记队列。
    pub fn logging(mut self, log: &'g Injector<usize>) -> Self {
        self.log = Some(log);
        self
    }

    /// 一起回收几个堆：追踪到的 Far 报到 `group`。
    pub fn grouping(mut self, group: &'g Injector<(usize, usize)>) -> Self {
        self.group = Some(group);
        self
    }

    /// 计数模式（年轻回收的晋升、停顿式完整回收的重新计数）。
    pub fn counting(mut self, parity: Option<bool>) -> Self {
        self.count = parity;
        self
    }

    /// 疏散回收：每个工人带一个复制缓冲，候选块里被追踪到的对象搬进 `targets` 里的块（不够就开新块登记进 `blocks`）；
    /// 根由 0 号工人重定位。`last` 是上次完成的纪元（空行判定）。
    pub fn evacuating(mut self, roots: &'g RootTable, targets: &Injector<Block>, blocks: &SegVec<Block>, pages: &Pages, moved: &AtomicUsize, last: u32) -> Self {
        self.copiers = (0..self.crew.len()).map(|_| CachePadded::new(UnsafeCell::new(Some(Copier::new(targets, blocks, pages, moved, last, self.epoch))))).collect();
        self.roots = Some(roots);
        self
    }

    /// 标记根（顺带写行标记），需要追踪的放进灰队列。
    pub fn seed(grey: &Injector<usize>, roots: &[*const Header], epoch: u32) {
        let seed = Worker::new_lifo();
        let mut t = Tracer::new(&seed, epoch, None);
        for &root in roots {
            t.visit(root, false);
        }
        while let Some(item) = seed.pop() {
            grey.push(item);
        }
    }

    /// 年轻回收：记忆集里的槽（地址）当前指向的对象当作根（年轻的标上、进灰队列；老的碰到就停）。
    pub fn seed_slots(grey: &Injector<usize>, slots: &[usize], epoch: u32) {
        let seed = Worker::new_lifo();
        let mut t = Tracer::new(&seed, epoch, None);
        for &slot in slots {
            // SAFETY: 槽在写入它的对象里；那个对象被写时可达，到这次回收前不会被释放（见 Mutator::written）
            let slot = unsafe { &*core::ptr::with_exposed_provenance::<RawSlot>(slot) };
            if let Some(h) = slot.current() {
                t.visit(h, false);
            }
        }
        while let Some(item) = seed.pop() {
            grey.push(item);
        }
    }

    /// 从灰队列出发标记，直到全部工人空闲。
    pub fn run(&self, pool: &Pool) {
        pool.run(self.crew.len(), |me| self.work(me));
    }

    fn work(&self, me: usize) {
        let local = self.crew.take(me);
        // SAFETY: 复制缓冲也按工号取一次，只在这个工号的线程上
        let mut copier = self.copiers.get(me).and_then(|c| unsafe { (*c.get()).take() });
        // 只有一个工人时没有别人改计数（mutator 不碰计数，减量队列此时为空）
        let mut t = Tracer::new(&local, self.epoch, copier.as_mut()).counting(self.count, self.crew.len() == 1).logging(self.log).grouping(self.group);
        if me == 0
            && let Some(roots) = self.roots
        {
            roots.relocate(|h| t.forward(h, false));
            for &slot in self.slots {
                // SAFETY: 槽在写入它的老对象里，到这次回收前不会被释放（见 Mutator::written）；停顿里没有 mutator 写它
                let slot = unsafe { &*core::ptr::with_exposed_provenance::<RawSlot>(slot) };
                if let Some(old) = slot.current() {
                    let new = t.forward(old, false);
                    if !core::ptr::eq(old, new) {
                        slot.relocate(new);
                    }
                }
            }
        }
        self.crew.drain(me, &local, |item| t.scan(item));
    }
}

pub struct Copier {
    /// 可搬入的块（只在本轮回收期间有效）
    targets: *const Injector<Block>,
    /// 全部块表：新开的块登记进去，本轮清扫会扫到
    blocks: *const SegVec<Block>,
    /// 新开的块从这里领
    pages: *const Pages,
    /// 上次完成的纪元：行标记早于它的是空行
    last: u32,
    /// 本轮纪元
    epoch: u32,
    block: Option<Block>,
    line: usize,
    cursor: *mut u8,
    limit: *mut u8,
    /// 本线程搬了多少字节；析构时并进堆的计数
    moved: usize,
    total: *const AtomicUsize,
}

impl Drop for Copier {
    fn drop(&mut self) {
        // SAFETY: 计数器在本轮回收期间有效（Copier 在回收结束前析构）
        unsafe { &*self.total }.fetch_add(self.moved, Ordering::Relaxed);
    }
}

// SAFETY: 指针指向回收期间一直有效的堆结构；每个回收线程独占自己的 Copier
unsafe impl Send for Copier {}

impl Copier {
    pub fn new(targets: &Injector<Block>, blocks: &SegVec<Block>, pages: &Pages, total: &AtomicUsize, last: u32, epoch: u32) -> Self {
        Self { targets, blocks, pages, last, epoch, block: None, line: 0, cursor: null_mut(), limit: null_mut(), moved: 0, total }
    }

    /// 把 `from` 整个抄到新地方，返回新头。SAFETY（调用方）：本线程认领了 `from`，它是块里的小对象。
    pub unsafe fn copy(&mut self, from: *const Header) -> *const Header {
        // SAFETY: from 有效
        let bytes = unsafe { &*from }.bytes();
        let to = self.alloc(bytes);
        self.moved += bytes;
        // SAFETY: to 是 bytes 字节的新内存；from 归本线程（认领后别人只读头里的标记字）
        unsafe { core::ptr::copy_nonoverlapping(from.cast::<u8>(), to, bytes) };
        let block = Block::containing(to);
        block.set_start(to);
        block.mark_lines(to, bytes, self.epoch);
        let to = to.cast::<Header>().cast_const();
        // SAFETY: 新头已抄好
        unsafe { &*to }.set_mark(self.epoch);
        to
    }

    fn alloc(&mut self, bytes: usize) -> *mut u8 {
        loop {
            if self.limit.addr() - self.cursor.addr() >= bytes {
                let p = self.cursor;
                self.cursor = p.wrapping_add(bytes);
                return p;
            }
            self.refill();
        }
    }

    /// 当前空洞放不下：找当前块的下一个空洞，没有了就换块。
    fn refill(&mut self) {
        if let Some(b) = self.block
            && let Some((start, end)) = b.hole(self.line, self.last)
        {
            self.line = end;
            self.cursor = b.line(start);
            self.limit = b.line(end);
            return;
        }
        // SAFETY: 三个指针在本轮回收期间有效
        let (targets, blocks, pages) = unsafe { (&*self.targets, &*self.blocks, &*self.pages) };
        let b = loop {
            match targets.steal() {
                Steal::Success(b) => {
                    // 年轻疏散直接从可用队列里拿：拿出来，标成用过（落进本次年轻清扫重新分类）
                    b.unlist();
                    b.reused();
                    break b;
                }
                Steal::Retry => {}
                Steal::Empty => {
                    let b = Block::new(pages);
                    blocks.push(b);
                    break b;
                }
            }
        };
        self.block = Some(b);
        self.line = Block::FIRST_LINE;
        self.cursor = null_mut();
        self.limit = null_mut();
    }
}

/// 队列项里「组里第几个堆」放在高位（地址只用到低 48 位）
#[cfg(crossheap)]
const MEMBER_SHIFT: u32 = 56;
/// 队列项里地址那部分
#[cfg(crossheap)]
const ADDR_MASK: usize = (1 << MEMBER_SHIFT) - 1;

/// 组里的一个堆：它和它这一轮的纪元。
#[cfg(crossheap)]
pub struct Member<'h> {
    pub heap: &'h Inner,
    pub epoch: u32,
}

#[cfg(crossheap)]
pub struct Marker<'h> {
    members: &'h [Member<'h>],
    /// 堆号 → 组里第几个（不在组里是 None）
    by_heap: [Option<usize>; WINDOWS],
    /// 一起回收时：发现的组内跨堆边（来源是组里第几个, 目标的公开偏移）
    found: Option<&'h Injector<(usize, u64)>>,
    crew: Crew<'h>,
}

#[cfg(crossheap)]
impl<'h> Marker<'h> {
    /// `grey`：根放这里（空的，标记期间只有这一组工人用）。
    pub fn new(members: &'h [Member<'h>], found: Option<&'h Injector<(usize, u64)>>, grey: &'h Injector<usize>, workers: usize) -> Self {
        let mut by_heap = [None; WINDOWS];
        for (m, member) in members.iter().enumerate() {
            by_heap[member.heap.index()] = Some(m);
        }
        Self {
            members,
            by_heap,
            found,
            crew: Crew::new(workers, grey),
        }
    }

    /// 从 `roots`（组里第几个, 对象头地址）出发标记，直到全部工人空闲。
    pub fn run(&self, pool: &Pool, roots: &[(usize, usize)]) {
        for &(m, header) in roots {
            self.visit(m, header, |item| self.crew.grey.push(item));
        }
        pool.run(self.crew.len(), |me| {
            let local = self.crew.take(me);
            self.crew.drain(me, &local, |item| self.scan(item, &local));
        });
    }

    /// 标记组里第 `m` 个堆的对象（和它跨的行）；本轮第一次见到且它有边时把队列项交给 `push`。
    fn visit(&self, m: usize, header: usize, push: impl FnOnce(usize)) {
        let member = &self.members[m];
        let h = core::ptr::with_exposed_provenance::<PHeader>(header);
        // SAFETY: 来自入口或活对象的指针槽，对象活着
        let (core, ty) = unsafe { (&(*h).core, (*h).ty) };
        if !core.mark(member.epoch) {
            return;
        }
        if !core.is_large() {
            Block::containing(h.cast()).mark_lines(h.cast(), core.bytes(), member.epoch);
        }
        if member.heap.has_edges(ty) {
            push(header | m << MEMBER_SHIFT);
        }
    }

    /// 追踪一个对象：指针图里每个槽，指向组里某个堆的就标上（指向别的堆时报到 `found`）；组外的由它们自己的入口管。
    fn scan(&self, item: usize, local: &Worker<usize>) {
        let (m, header) = (item >> MEMBER_SHIFT, item & ADDR_MASK);
        let source = self.members[m].heap;
        let h = core::ptr::with_exposed_provenance::<PHeader>(header);
        // SAFETY: 队列里只有本轮标记过的活对象
        let ty = unsafe { (*h).ty };
        let data = PHeader::data(h);
        for &edge in source.edges(ty) {
            let (far, off) = (edge & FAR_EDGE != 0, (edge & !FAR_EDGE) as usize);
            // SAFETY: 指针图里的偏移都是这个类型里指针槽（一个 AtomicU64）的位置
            let v = unsafe { &*data.add(off).cast::<AtomicU64>() }.load(Ordering::Acquire);
            if v == 0 {
                continue;
            }
            let Some(t) = Inner::heap_of(v).and_then(|k| self.by_heap[k]) else { continue };
            // 指向的那个堆已经死了、号被组里这个新堆用上：不是它的对象，不跟
            // SAFETY: 跨堆的槽是 CrossHeapRef（repr(C)：偏移后面紧跟目标堆身份，也是一个 AtomicU64）
            if far && unsafe { &*data.add(off + size_of::<AtomicU64>()).cast::<AtomicU64>() }.load(Ordering::Acquire) != self.members[t].heap.incarnation() {
                continue;
            }
            if t != m
                && let Some(found) = self.found
            {
                found.push((m, v));
            }
            let target = self.members[t].heap;
            self.visit(t, PHeader::of(target.ptr(v)).expose_provenance(), |x| local.push(x));
        }
    }
}
