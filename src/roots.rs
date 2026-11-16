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

#[derive(Default)]
pub(crate) struct RootTable {
    /// 每个根一个槽：对象头指针，空槽是 null
    slots: SegVec<AtomicPtr<Header>>,
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
    /// 领一个槽放 `header`（只在 mutate 里调用）。
    fn claim(&self, header: *const Header) -> usize {
        // SAFETY: mutate 期间 ready 不变
        let ready = unsafe { &*self.ready.get() };
        let i = self.cursor.fetch_add(1, Ordering::AcqRel);
        match ready.get(i).and_then(|&slot| self.slots.get(slot).map(|s| (slot, s))) {
            Some((slot, s)) => {
                s.store(header.cast_mut(), Ordering::Release);
                slot
            }
            None => self.slots.push(AtomicPtr::new(header.cast_mut())),
        }
    }

    fn slot(&self, i: usize) -> &AtomicPtr<Header> {
        self.slots.get(i).expect("根槽号来自 claim")
    }

    /// 句柄析构：清空槽、槽号挂回去（任何时候、任何线程）。
    fn release(&self, i: usize) {
        self.slot(i).store(null_mut(), Ordering::Release);
        // 这张表从不封口，压入不会被退回
        let _ = self.returned.push(i);
    }

    /// 疏散回收：每个根交给 `f`（标记，必要时搬走），改成它返回的新地址。
    /// 句柄可能同时在别的线程析构（槽被清空），所以用 CAS：清空了就不改回去。
    pub(crate) fn relocate(&self, mut f: impl FnMut(*const Header) -> *const Header) {
        for slot in self.slots.iter() {
            let old = slot.load(Ordering::Acquire);
            if old.is_null() {
                continue;
            }
            let new = f(old.cast_const());
            if !core::ptr::eq(old, new) {
                let _ = slot.compare_exchange(old, new.cast_mut(), Ordering::AcqRel, Ordering::Acquire);
            }
        }
    }

    /// 回收时：整理空槽，返回全部根。SAFETY（调用方）：没有 mutator（独占堆）。
    pub(crate) unsafe fn settle(&self) -> Vec<*const Header> {
        // SAFETY: 见上
        let ready = unsafe { &mut *self.ready.get() };
        let used = self.cursor.swap(0, Ordering::AcqRel).min(ready.len());
        ready.drain(..used);
        ready.extend(self.returned.take());
        self.slots.iter().map(|s| s.load(Ordering::Acquire).cast_const()).filter(|p| !p.is_null()).collect()
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
        let slot = table.claim(gc.header());
        Root { table, slot, _brand: PhantomData }
    }

    /// 取根指向的对象。
    pub fn get<B: Brand>(&self, root: &Root<B>) -> Gc<'gc, B::Of<'gc>> {
        assert!(Arc::ptr_eq(&root.table, self.heap().roots()), "这个 Root 不属于这个堆");
        let header = root.table.slot(root.slot).load(Ordering::Acquire);
        // SAFETY: 根槽里是同一个 B 建的、被这个根保活的对象
        unsafe { Gc::from_data(Header::data(header).cast()) }
    }

    /// 让根改指另一个对象。
    pub fn set<B: Brand>(&self, root: &Root<B>, gc: Gc<'gc, B::Of<'gc>>) {
        assert!(Arc::ptr_eq(&root.table, self.heap().roots()), "这个 Root 不属于这个堆");
        root.table.slot(root.slot).store(gc.header().cast_mut(), Ordering::Release);
    }
}
