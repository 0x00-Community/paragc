//! 根：[`Root`] 是 mutate 之外也能持有的强引用句柄（可跨线程），回收从全部根出发。
//!
//! 根表是只追加的槽数组：新根在 mutate 里领槽（先用释放过的空槽），句柄析构时把槽清空、下标挂回去。
//! 空槽只在回收时（没有 mutator）整理成只读数组，mutate 期间原子游标领取，所以无锁、没有 ABA。
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::Arc;

use j8lockfree::{PushList, SegVec};

use crate::header::Header;
use crate::{Gc, Mutator, Trace};

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
pub(crate) struct Entry {
    ptr: AtomicPtr<Header>,
    /// 持有这个根的堆（[`crate::Far`] 所在的堆的身份）；普通 [`Root`] 是 [`NO_HOLDER`]
    holder: AtomicUsize,
}

/// [`Entry::holder`]：不属于任何堆（普通 [`Root`]）
pub(crate) const NO_HOLDER: usize = 0;

impl Entry {
    pub(crate) fn load(&self) -> *mut Header {
        self.ptr.load(Ordering::Acquire)
    }

    pub(crate) fn store(&self, header: *mut Header) {
        self.ptr.store(header, Ordering::Release);
    }
}

/// 根表，同时也是堆的入口表：别的堆经 [`crate::Far`] 直接指进来的对象在这里各占一个槽，记着持有方。
#[derive(Default)]
pub(crate) struct RootTable {
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
    pub(crate) fn claim(&self, header: *const Header, holder: usize) -> usize {
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

    pub(crate) fn slot(&self, i: usize) -> &Entry {
        self.slots.get(i).expect("根槽号来自 claim")
    }

    /// 句柄析构：清空槽、槽号挂回去（任何时候、任何线程）。
    pub(crate) fn release(&self, i: usize) {
        self.slot(i).store(null_mut());
        // 这张表从不封口，压入不会被退回
        let _ = self.returned.push(i);
    }

    /// 疏散回收：每个根交给 `f`（标记，必要时搬走），改成它返回的新地址。
    /// 句柄可能同时在别的线程析构（槽被清空），所以用 CAS：清空了就不改回去。
    /// 入口（别的堆直接指着的）对象所在的块事先已经撤掉疏散（[`RootTable::pinned`]），不会动。
    pub(crate) fn relocate(&self, mut f: impl FnMut(*const Header) -> *const Header) {
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
    pub(crate) unsafe fn pinned(&self, mut f: impl FnMut(*const Header)) {
        for slot in self.slots.iter() {
            let p = slot.load();
            if !p.is_null() && slot.holder.load(Ordering::Relaxed) != NO_HOLDER {
                f(p.cast_const());
            }
        }
    }

    /// 回收时：整理空槽，返回全部根。SAFETY（调用方）：没有 mutator（独占堆）。
    pub(crate) unsafe fn settle(&self) -> Vec<*const Header> {
        // SAFETY: 见上
        unsafe { self.settle_outside(|_| false) }
    }

    /// 同 [`RootTable::settle`]，但持有方满足 `inside` 的入口不算根（一起回收的堆里的 Far，由追踪决定死活）。
    pub(crate) unsafe fn settle_outside(&self, inside: impl Fn(usize) -> bool) -> Vec<*const Header> {
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
    pub(crate) unsafe fn clear_dead(&self, inside: impl Fn(usize) -> bool, epoch: u32) {
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
