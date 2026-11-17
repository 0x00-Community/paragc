//! 跨堆引用 [`Far`]：同一进程里，一个堆的对象直接指着另一个堆的对象，零拷贝、不经过间接表。
//!
//! - 建的时候在目标堆的根表（也是它的入口表）领一个槽、记下持有方堆；持有它的对象析构时还槽。
//!   目标堆单独回收时入口就是根，而且入口对象所在的块不疏散（别的堆存的是直接指针，改不到）。
//! - 品牌 `'a` 是持有方堆那次 mutate 的品牌：`Far` 只能放进那个堆的对象里，逃不出 mutate。
//! - 读要带目标堆的 [`Mutator`]：证明此刻在目标堆的 mutate 里（对象不会被清扫或搬走）。
//! - 跨堆的环（A 的对象经 Far 指向 B、B 的又指回 A）单独回收收不掉，用 [`crate::Heap::collect_group`] 一起回收。
use core::marker::PhantomData;
use core::ptr::NonNull;
use std::sync::Arc;

use crate::gc::Invariant;
use crate::header::Header;
use crate::roots::RootTable;
use crate::{Brand, Gc, Heap, Mutator, Trace, Tracer};

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
