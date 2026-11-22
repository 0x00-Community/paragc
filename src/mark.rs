//! Marking and tracing: [`Trace`] / [`Tracer`], the work-stealing marking workers [`Crew`], private-heap marking [`Marking`] and evacuation copying [`Copier`], and public-heap marking by pointer map [`Marker`].
//!
//! [`Trace`]: an object tells the collector what it references; [`Tracer`]: the collector's side, marking and handing new objects to the work queue.
//!
//! Parallel marking: each worker has a crossbeam deque (popping its own LIFO); roots and old values recorded by the write barrier go to the heap's shared grey queue;
//! when its own queue is empty a worker steals in batches (`steal_batch_and_pop`, taking half of another queue at once).
//!
//! During concurrent marking mutators keep running and the write barrier keeps adding to the grey queue: when everyone is idle this round stops, and the rest is drained in one more round when the collection finishes (with the heap held exclusively).
//!
//! Termination: a worker whose own queue is empty and that cannot steal increments the idle count, then keeps stealing with backoff; when the idle count equals the number of workers,
//! all queues are empty and nobody is still tracing (tracing workers are not counted idle), so marking is done. A worker that steals something decrements first and then works;
//! if someone sees "all idle" and exits before the decrement, the stolen work is still finished by the thief, so the result is unaffected.
//!
//! During evacuation each collector thread has a copy buffer: it bump-allocates in the holes of target blocks and copies each moved object whole;
//! the new copy is marked with this round (including line marks and start bits). Target blocks come first from the "can move into" queue (non-candidate blocks unused since sweeping), otherwise new blocks are opened.
//!
//! Parallel marking of public heaps: edges are found through the pointer maps in the type table, without calling object code (the collector need not know these types).
//! Work stealing and termination are as for the private heap ([`crate::mark`]): one deque per worker, batch stealing when empty, done when all are idle.
//!
//! Several heaps can be marked together ([`super::CrossHeap::collect_group`]): each queue item is "index of the heap in the group + object header address";
//! edges into another heap of the group go to that heap (marked with its epoch) and are reported to `found` (to rebuild cross-heap registrations later); edges leaving the group are not followed.
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
use crate::handle::{EDGE_KINDS, FAR_EDGE, PTR_EDGE, WORD_EDGE, word_pointer};
#[cfg(crossheap)]
use crate::heap::{Inner, Promoter};
#[cfg(crossheap)]
use crate::shm::Region;
use crate::layout::{Block, Claim, Header, Pages};
#[cfg(crossheap)]
#[cfg(crossheap)]
use crate::layout::PHeader;
#[cfg(crossheap)]
use crate::shm::WINDOWS;
use crate::sync::{Pool, SegVec};

/// A type that can live in the heap. Generate it with `#[derive(Trace)]`; do not write it by hand.
///
/// - `Send + Sync`: objects may be read on any thread and traced and finalized by collector threads
/// - types with `Gc` fields must not implement `Drop` (finalization order is unspecified, so `Drop` could touch an already finalized object); the derive checks this
///
/// # Safety
/// `trace` must call [`Tracer::edge`] once for every directly held `Gc` / `Slot` (or call `trace` on the field); a missed one is a dangling pointer;
/// `relocate` likewise calls [`Tracer::relocate`] once per edge (or `relocate` on the field), and the collector rewrites moved objects to their new address.
/// `NO_EDGES` may be true only when the type really holds no `Gc`.
pub unsafe trait Trace: Send + Sync {
    /// The type holds no `Gc`: the collector need not trace it after marking
    const NO_EDGES: bool = false;

    fn trace(&self, t: &mut Tracer<'_>);

    /// Replaces `trace` during evacuating collections (heap held exclusively, no mutators): may rewrite edges. Types without edges use the default.
    fn relocate(&mut self, t: &mut Tracer<'_>) {
        let _ = t;
        assert!(Self::NO_EDGES, "types holding Gc must implement Trace::relocate (use #[derive(Trace)])");
    }
}

/// A marking worker: marks objects and the lines they span, and pushes objects that need tracing onto its own work queue;
/// during evacuation it also moves objects out of candidate blocks and rewrites edges to the new address.
pub struct Tracer<'t> {
    work: &'t Worker<usize>,
    epoch: u32,
    /// Copy buffer during evacuation (present = relocating mode)
    copier: Option<&'t mut Copier>,
    /// Counting mode (promotion / recount): each scanned edge increments its target; the value is the parity of the current count
    count: Option<bool>,
    /// Release mode (objects whose count dropped to 0): the target of each edge is handed over for a decrement, no marking
    decs: Option<&'t mut Vec<usize>>,
    /// Only this thread changes counts (single-threaded marking): increment with plain reads and writes
    exclusive: bool,
    /// Count logging during concurrent marking: slots rewritten by a mutator between the read and the update of a recount are logged again here (see [`Tracer::slot`])
    log: Option<&'t Injector<usize>>,
    /// Collecting several heaps together ([`crate::Heap::collect_group`]): traced Fars report (target heap's root table, target header) here
    group: Option<&'t Injector<(usize, usize)>>,
    /// Young collections promote shareable young objects into the cross-heap (private heaps with a cross-heap attached)
    #[cfg(crossheap)]
    promoter: Option<&'t Promoter<'t>>,
    /// Asking whether an object's type is shareable: shareable types answer with their type number in the cross-heap (see [`Header::new_shared`])
    #[cfg(crossheap)]
    asking: bool,
    /// The answer received
    #[cfg(crossheap)]
    answer: Option<u32>,
    /// Relocating an object promoted into the cross-heap: its edges must not stay in the private heap
    #[cfg(crossheap)]
    in_shared: bool,
    /// The owner collecting its cross-heap: traced edges into cross-heap objects (data addresses) are reported here as cross-heap roots
    #[cfg(crossheap)]
    crossing: Option<&'t Injector<usize>>,
}

/// Marks a promoted object in the work queue (headers are granule aligned, so the lowest bit is free): the item is the old header, and the copy in the cross-heap is what gets relocated
#[cfg(crossheap)]
const PROMOTED_ITEM: usize = 1;

impl Tracer<'_> {

    fn inc(&self, header: *const Header) {
        #[cfg(crossheap)]
        if Region::holds_fixed(header.addr()) {
            return;
        }
        if let Some(parity) = self.count {
            // SAFETY: header is a live object
            let h = unsafe { &*header };
            if self.exclusive {
                h.rc_inc_exclusive(parity);
            } else {
                h.rc_inc(parity);
            }
        }
    }

    /// An edge (read only: concurrent marking, non-evacuating collections).
    pub fn edge<T: Trace>(&mut self, gc: Gc<'_, T>) {
        if let Some(decs) = self.decs.as_deref_mut() {
            #[cfg(crossheap)]
            if Region::holds_fixed(gc.as_ptr().addr()) {
                return;
            }
            decs.push(gc.header().expose_provenance());
            return;
        }
        self.visit(gc.header(), T::NO_EDGES);
        self.inc(gc.header());
    }

    /// A rewritable edge (evacuating collections): rewritten to the new address if the object moved.
    pub fn relocate<T: Trace>(&mut self, gc: &mut Gc<'_, T>) {
        *gc = self.forward_gc(*gc);
        self.inc(gc.header());
    }

    /// Mark (moving if needed) and return the object's current location; no counting.
    fn forward_gc<'gc, T: Trace>(&mut self, gc: Gc<'gc, T>) -> Gc<'gc, T> {
        let old = gc.header();
        let new = self.forward(old, T::NO_EDGES);
        // SAFETY: new is the same object (or its moved header), with the same brand and type
        if core::ptr::eq(old, new) { gc } else { unsafe { Gc::from_data(Header::data(new).cast()) } }
    }

    /// The collector is asking whether the type is shareable (types from [`Header::new_shared`] answer, others do not).
    #[cfg(crossheap)]
    pub fn asking(&self) -> bool {
        self.asking
    }

    /// Answer: the type is `T` (shareable).
    #[cfg(crossheap)]
    pub fn answer<T: crate::Shared>(&mut self) {
        self.answer = self.promoter.map(Promoter::type_of::<T>);
    }

}

#[cfg(crossheap)]
impl Tracer<'_> {
    /// Met a cross-heap object (`header` is computed as for a private header and used only to recognize the address): the private heap leaves it alone; when the owner collects both it is reported as a root.
    fn crossed(&self, header: *const Header) -> bool {
        if !Region::holds_fixed(header.addr()) {
            return false;
        }
        if let Some(c) = self.crossing {
            c.push(Header::data(header).addr());
        }
        true
    }

    /// The object's type number in the cross-heap when its type is shareable.
    fn shareable(&mut self, header: *const Header) -> Option<u32> {
        self.asking = true;
        // SAFETY: header is a live object; visit does not touch the data when asked for the type
        unsafe { Header::visit(header, self) };
        self.asking = false;
        self.answer.take()
    }

    /// A shareable young object in a young collection: promote it into the cross-heap and return its new location (computed as a private header, see [`Promoter::copy`]).
    fn promote(&mut self, header: *const Header, no_edges: bool) -> Option<*const Header> {
        let p = self.promoter?;
        // SAFETY: header is a live object
        let h = unsafe { &*header };
        if h.is_marked(self.epoch) || !(h.is_large() || Block::containing(header.cast()).is_evacuating()) {
            return None;
        }
        let ty = self.shareable(header)?;
        Some(match h.claim() {
            Claim::Moved(to) => to,
            Claim::Mine => {
                // SAFETY: this thread claimed it; the type number is its own answer
                let to = Header::of(unsafe { p.copy(header, ty) });
                // SAFETY: as above
                unsafe { Header::forward(header, to) };
                if !no_edges {
                    self.work.push(header.expose_provenance() | PROMOTED_ITEM);
                }
                to
            }
        })
    }
}

/// Operations for this crate only: the trait lives in a private module and is not exported, so it is unreachable outside the crate.
pub trait TracerInternal<'t> {
    fn new(work: &'t Worker<usize>, epoch: u32, copier: Option<&'t mut Copier>) -> Self;

    /// Counting mode: every edge scanned increments its target (seeds — roots and the remembered set — go straight through visit, uncounted).
    /// `exclusive`: only this thread changes counts.
    fn counting(self, parity: Option<bool>, exclusive: bool) -> Self;

    /// Release mode: when visiting a dead object, put the target of each edge (for slots, the last counted value) into `decs`.
    fn releasing(work: &'t Worker<usize>, decs: &'t mut Vec<usize>) -> Self;

    /// Concurrent marking: the count logging queue (see [`Tracer::log`]).
    fn logging(self, log: Option<&'t Injector<usize>>) -> Self;

    /// Group collection: the queue that receives Far targets.
    fn grouping(self, group: Option<&'t Injector<(usize, usize)>>) -> Self;

    /// Traced a [`crate::Far`]: reported in a group collection (the target heap continues marking); otherwise nothing (the target heap's entry is a root).
    fn far(&mut self, table: usize, header: *const Header);

    /// Relocating mode (evacuating collections): objects taken from the queue go through `relocate` instead of `trace`.
    fn relocating(&self) -> bool;

    /// A slot (read only): normally just an edge; in counting mode it switches to counting the current value and increments it; in release mode it hands over the last counted value for a decrement.
    fn slot<T: Trace>(&mut self, slot: &Slot<'_, T>);

    /// A rewritable slot (evacuating collections): rewritten to the new address if moved; in counting mode counts the new value and increments it.
    fn slot_mut<T: Trace>(&mut self, slot: &mut Slot<'_, T>);

    /// A type-erased slot ([`Slot`], [`crate::WordSlot`]), read only; `no_edges`: the target has no edges
    fn raw(&mut self, raw: &RawSlot, no_edges: bool);

    /// A type-erased slot, rewritable (evacuating collections)
    fn raw_mut(&mut self, raw: &RawSlot, no_edges: bool);

    /// Mark an object, moving it if it is in an evacuation candidate block; return its current header.
    fn forward(&mut self, header: *const Header, no_edges: bool) -> *const Header;

    /// Mark an object; push it on the work queue when it is first seen this round and has edges.
    fn visit(&mut self, header: *const Header, no_edges: bool);

    /// Trace (or relocate) one object taken from the work queue.
    fn scan(&mut self, item: usize);
}

impl<'t> TracerInternal<'t> for Tracer<'t> {
    fn new(work: &'t Worker<usize>, epoch: u32, copier: Option<&'t mut Copier>) -> Self {
        Self {
            work,
            epoch,
            copier,
            count: None,
            decs: None,
            exclusive: false,
            log: None,
            group: None,
            #[cfg(crossheap)]
            promoter: None,
            #[cfg(crossheap)]
            asking: false,
            #[cfg(crossheap)]
            answer: None,
            #[cfg(crossheap)]
            in_shared: false,
            #[cfg(crossheap)]
            crossing: None,
        }
    }

    fn counting(mut self, parity: Option<bool>, exclusive: bool) -> Self {
        self.count = parity;
        self.exclusive = exclusive;
        self
    }

    fn releasing(work: &'t Worker<usize>, decs: &'t mut Vec<usize>) -> Self {
        let mut t = Self::new(work, 0, None);
        t.decs = Some(decs);
        t
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
        #[cfg(crossheap)]
        if let Some(p) = raw.cross() {
            self.crossed(Header::of(p));
        }
        if let Some(decs) = self.decs.as_deref_mut() {
            if let Some(h) = raw.counted() {
                decs.push(h.expose_provenance());
            }
            return;
        }
        if self.count.is_some() {
            let (cur, _) = raw.recount();
            // Concurrent marking: between reading the current value and updating "last counted" a mutator may rewrite the slot; if the slot was not counted yet (a young object's slot),
            // it would not log ⇒ nobody would ever count the new value. If the value changed, log it on its behalf and settle "new +1, counted −1" at the end (logging twice is harmless)
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
        #[cfg(crossheap)]
        if let Some(p) = raw.cross() {
            self.crossed(Header::of(p));
        }
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
        #[cfg(crossheap)]
        {
            if self.crossed(header) {
                return header;
            }
            if let Some(to) = self.promote(header, no_edges) {
                return to;
            }
            assert!(!self.in_shared, "a shareable object points at a private object that cannot be promoted (a non-shareable type, an old object, or one pointed to directly from another heap)");
        }
        // SAFETY: header comes from this heap's Gc / roots, the object is alive
        let h = unsafe { &*header };
        // Objects already marked in place this round are not moved (young objects pointed to directly from old objects or from another heap are marked in place first during young evacuation)
        if let Some(copier) = self.copier.as_deref_mut()
            && !h.is_large()
            && Block::containing(header.cast()).is_evacuating()
            && !h.is_marked(self.epoch)
        {
            return match h.claim() {
                Claim::Moved(to) => to,
                Claim::Mine => {
                    // SAFETY: this thread claimed it; it lives in a block
                    let to = unsafe { copier.copy(header) };
                    // SAFETY: as above
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
        #[cfg(crossheap)]
        if self.crossed(header) {
            return;
        }
        // SAFETY: header comes from this heap's Gc / roots, the object is alive
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
        #[cfg(crossheap)]
        if item & PROMOTED_ITEM != 0 {
            let old = core::ptr::with_exposed_provenance::<Header>(item & !PROMOTED_ITEM);
            // SAFETY: an old header pushed during promotion, with the forwarding address written; the old data is freed only by sweeping, so its visit function is still there
            let to = unsafe { &*old }.forwardee();
            self.in_shared = true;
            // SAFETY: the cross-heap copy is the same type, just copied, and taken out by this thread only (exclusive)
            unsafe { Header::visit_data(old, Header::data(to), self) };
            self.in_shared = false;
            return;
        }
        let header = core::ptr::with_exposed_provenance::<Header>(item);
        // SAFETY: the queue holds only live objects marked this round; in relocating mode each is taken out by one thread only (exclusive)
        unsafe { Header::visit(header, self) };
    }
}


// SAFETY: a Gc is itself an edge
unsafe impl<T: Trace> Trace for Gc<'_, T> {
    fn trace(&self, t: &mut Tracer<'_>) {
        t.edge(*self);
    }

    fn relocate(&mut self, t: &mut Tracer<'_>) {
        t.relocate(self);
    }
}

// SAFETY: the value in the slot is an edge
unsafe impl<T: Trace> Trace for Slot<'_, T> {
    fn trace(&self, t: &mut Tracer<'_>) {
        t.slot(self);
    }

    fn relocate(&mut self, t: &mut Tracer<'_>) {
        t.slot_mut(self);
    }
}

// SAFETY: the pointer in the word (if any) is an edge; the target type is unknown, so it is treated as having edges
unsafe impl Trace for WordSlot<'_> {
    fn trace(&self, t: &mut Tracer<'_>) {
        t.raw(self.raw(), false);
    }

    fn relocate(&mut self, t: &mut Tracer<'_>) {
        t.raw_mut(self.raw(), false);
    }
}

/// Types without `Gc`: nothing to trace.
macro_rules! leaf {
    ($($ty:ty),* $(,)?) => {$(
        // SAFETY: no Gc
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

// SAFETY: no value
unsafe impl<T: ?Sized + Send + Sync> Trace for PhantomData<T> {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut Tracer<'_>) {}
}

/// Containers traced element by element: `$ty` uses `T` as the element type, and `$iter` turns `&self` into an iterator over the elements.
macro_rules! each {
    ($([$($gen:tt)*] $ty:ty => |$s:ident| $iter:expr, |$m:ident| $iter_mut:expr;)*) => {$(
        // SAFETY: traces every element
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

/// Tuples: traced field by field.
macro_rules! tuple {
    ($($name:ident)+) => {
        // SAFETY: traces every field
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

/// A crew of marking workers: each worker id has a local deque (popping its own LIFO); roots and values recorded by the write barrier go to the shared grey queue;
/// when its own queue is empty a worker steals in batches (`steal_batch_and_pop`, taking half of another queue at once). Private and public heap marking both use it; only "scan one item" differs.
///
/// Termination: a worker whose own queue is empty and that cannot steal increments the idle count, then keeps stealing with backoff; when the idle count equals the number of workers,
/// all queues are empty and nobody is still tracing (tracing workers are not counted idle), so marking is done. A worker that steals something decrements first and then works;
/// if someone sees "all idle" and exits before the decrement, the stolen work is still finished by the thief, so the result is unaffected.
pub struct Crew<'g> {
    /// One local queue per worker id, taken (once) by that worker's thread
    locals: Vec<CachePadded<UnsafeCell<Option<Worker<usize>>>>>,
    stealers: Vec<Stealer<usize>>,
    /// The shared grey queue
    grey: &'g Injector<usize>,
    idle: CachePadded<AtomicUsize>,
}

// SAFETY: each worker id's local queue is taken by one thread only (ids are distinct); the rest is thread safe
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

    /// The local queue of worker `me` (taken once per id, only on that worker's thread).
    pub fn take(&self, me: usize) -> Worker<usize> {
        // SAFETY: worker id me runs on this one thread only
        unsafe { (*self.locals[me].get()).take() }.expect("each worker id takes its local queue once")
    }

    /// Worker `me` works until every worker is idle: pop its own first, steal when empty, and go idle to wait for the others when there is nothing to steal.
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

    /// While idle, keep stealing with backoff; returns None once everyone is idle.
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

    /// Steal from the shared grey queue first, then from the other workers starting with the next one; move a batch into its own queue and return one of them.
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
    /// Evacuating collections: one copy buffer per worker id (empty = no evacuation)
    copiers: Vec<CachePadded<UnsafeCell<Option<Copier>>>>,
    /// Evacuating collections: worker 0 first rewrites the root table's slots to the moved addresses (greying the roots at the same time)
    roots: Option<&'g RootTable>,
    /// Counting mode: every scanned edge increments its target (the value is the count parity)
    count: Option<bool>,
    /// Concurrent marking: the count logging queue (slots rewritten by a mutator during a recount are logged again)
    log: Option<&'g Injector<usize>>,
    /// Group collection: traced Fars are reported here
    group: Option<&'g Injector<(usize, usize)>>,
    /// Young evacuation ([`crate::YoungEvacuation::Full`]): worker 0 rewrites these remembered slots to the moved addresses
    slots: &'g [usize],
    /// Shareable young objects are promoted into this cross-heap
    #[cfg(crossheap)]
    promote: Option<&'g Inner>,
    /// Edges into the cross-heap are reported here
    #[cfg(crossheap)]
    crossing: Option<&'g Injector<usize>>,
}

// SAFETY: each worker id's copy buffer is taken by one thread only (ids are distinct); the rest is thread safe
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
            #[cfg(crossheap)]
            promote: None,
            #[cfg(crossheap)]
            crossing: None,
        }
    }

    /// During young evacuation, promote shareable young objects into `cross`.
    #[cfg(crossheap)]
    pub fn promoting(mut self, cross: Option<&'g Inner>) -> Self {
        self.promote = cross;
        self
    }

    /// Report traced edges into cross-heap objects to `crossing`.
    #[cfg(crossheap)]
    pub fn crossing(mut self, crossing: Option<&'g Injector<usize>>) -> Self {
        self.crossing = crossing;
        self
    }

    /// Young evacuation: worker 0 rewrites these remembered slots (addresses) to the moved addresses (greying the targets at the same time).
    pub fn relocating_slots(mut self, slots: &'g [usize]) -> Self {
        self.slots = slots;
        self
    }

    /// Concurrent marking: the count logging queue.
    pub fn logging(mut self, log: &'g Injector<usize>) -> Self {
        self.log = Some(log);
        self
    }

    /// Collecting several heaps together: traced Fars are reported to `group`.
    pub fn grouping(mut self, group: &'g Injector<(usize, usize)>) -> Self {
        self.group = Some(group);
        self
    }

    /// Counting mode (promotion in young collections, recount in stop-the-world full collections).
    pub fn counting(mut self, parity: Option<bool>) -> Self {
        self.count = parity;
        self
    }

    /// Evacuating collections: each worker carries a copy buffer; traced objects in candidate blocks move into the blocks in `targets` (opening new blocks registered in `blocks` when short);
    /// worker 0 relocates the roots. `last` is the last completed epoch (to judge free lines).
    pub fn evacuating(mut self, roots: &'g RootTable, targets: &Injector<Block>, blocks: &SegVec<Block>, pages: &Pages, moved: &AtomicUsize, last: u32) -> Self {
        self.copiers = (0..self.crew.len()).map(|_| CachePadded::new(UnsafeCell::new(Some(Copier::new(targets, blocks, pages, moved, last, self.epoch))))).collect();
        self.roots = Some(roots);
        self
    }

    /// Mark the roots (writing line marks too) and put the ones needing tracing on the grey queue.
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

    /// Young collections: the objects currently referenced by the remembered slots (addresses) act as roots (young ones are marked and greyed; old ones stop the trace).
    pub fn seed_slots(grey: &Injector<usize>, slots: &[usize], epoch: u32) {
        let seed = Worker::new_lifo();
        let mut t = Tracer::new(&seed, epoch, None);
        for &slot in slots {
            // SAFETY: the slot lives in the object written to; that object was reachable when written and is not freed before this collection (see Mutator::written)
            let slot = unsafe { &*core::ptr::with_exposed_provenance::<RawSlot>(slot) };
            if let Some(h) = slot.current() {
                t.visit(h, false);
            }
        }
        while let Some(item) = seed.pop() {
            grey.push(item);
        }
    }

    /// Mark from the grey queue until every worker is idle.
    pub fn run(&self, pool: &Pool) {
        pool.run(self.crew.len(), |me| self.work(me));
    }

    fn work(&self, me: usize) {
        let local = self.crew.take(me);
        // SAFETY: copy buffers are also taken once per worker id, only on that worker's thread
        let mut copier = self.copiers.get(me).and_then(|c| unsafe { (*c.get()).take() });
        #[cfg(crossheap)]
        let promoter = self.promote.map(Promoter::new);
        // With a single worker nobody else changes counts (mutators do not touch counts, and the decrement queue is empty now)
        let mut t = Tracer::new(&local, self.epoch, copier.as_mut()).counting(self.count, self.crew.len() == 1).logging(self.log).grouping(self.group);
        #[cfg(crossheap)]
        {
            t.promoter = promoter.as_ref();
            t.crossing = self.crossing;
        }
        if me == 0
            && let Some(roots) = self.roots
        {
            roots.relocate(|h| t.forward(h, false));
            for &slot in self.slots {
                // SAFETY: the slot lives in the old object written to and is not freed before this collection (see Mutator::written); no mutator writes it during the pause
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
    /// Blocks that can be moved into (valid only during this collection)
    targets: *const Injector<Block>,
    /// The table of all blocks: new blocks are registered there so this round's sweep sees them
    blocks: *const SegVec<Block>,
    /// New blocks come from here
    pages: *const Pages,
    /// The last completed epoch: lines with older marks are free
    last: u32,
    /// This round's epoch
    epoch: u32,
    block: Option<Block>,
    line: usize,
    cursor: *mut u8,
    limit: *mut u8,
    /// Bytes moved by this thread; added to the heap's count when dropped
    moved: usize,
    total: *const AtomicUsize,
}

impl Drop for Copier {
    fn drop(&mut self) {
        // SAFETY: the counter is valid during this collection (the Copier is dropped before the collection ends)
        unsafe { &*self.total }.fetch_add(self.moved, Ordering::Relaxed);
    }
}

// SAFETY: the pointers refer to heap structures valid throughout the collection; each collector thread owns its own Copier
unsafe impl Send for Copier {}

impl Copier {
    pub fn new(targets: &Injector<Block>, blocks: &SegVec<Block>, pages: &Pages, total: &AtomicUsize, last: u32, epoch: u32) -> Self {
        Self { targets, blocks, pages, last, epoch, block: None, line: 0, cursor: null_mut(), limit: null_mut(), moved: 0, total }
    }

    /// Copy `from` whole to a new place and return the new header. SAFETY (caller): this thread claimed `from`, a small object in a block.
    pub unsafe fn copy(&mut self, from: *const Header) -> *const Header {
        // SAFETY: from is valid
        let bytes = unsafe { &*from }.bytes();
        let to = self.alloc(bytes);
        self.moved += bytes;
        // SAFETY: to is new memory of bytes bytes; from belongs to this thread (after the claim others only read the mark word in its header)
        unsafe { core::ptr::copy_nonoverlapping(from.cast::<u8>(), to, bytes) };
        let block = Block::containing(to);
        block.set_start(to);
        block.mark_lines(to, bytes, self.epoch);
        let to = to.cast::<Header>().cast_const();
        // SAFETY: the new header is copied
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

    /// The current hole is too small: find the next hole in the current block, or switch blocks when there is none.
    fn refill(&mut self) {
        if let Some(b) = self.block
            && let Some((start, end)) = b.hole(self.line, self.last)
        {
            self.line = end;
            self.cursor = b.line(start);
            self.limit = b.line(end);
            return;
        }
        // SAFETY: the three pointers are valid during this collection
        let (targets, blocks, pages) = unsafe { (&*self.targets, &*self.blocks, &*self.pages) };
        let b = loop {
            match targets.steal() {
                Steal::Success(b) => {
                    // Young evacuation takes straight from the available queue: take it out and mark it used (so this young sweep reclassifies it)
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

/// The queue item's "index in the group" sits in the high bits (addresses use only the low 48 bits)
#[cfg(crossheap)]
const MEMBER_SHIFT: u32 = 56;
/// The address part of a queue item
#[cfg(crossheap)]
const ADDR_MASK: usize = (1 << MEMBER_SHIFT) - 1;

/// One heap of the group: the heap and its epoch this round.
#[cfg(crossheap)]
pub struct Member<'h> {
    pub heap: &'h Inner,
    pub epoch: u32,
}

#[cfg(crossheap)]
pub struct Marker<'h> {
    members: &'h [Member<'h>],
    /// Heap number → index in the group (None when not in the group)
    by_heap: [Option<usize>; WINDOWS],
    /// In a group collection: cross-heap edges found inside the group (index of the source, public offset of the target)
    found: Option<&'h Injector<(usize, u64)>>,
    crew: Crew<'h>,
}

#[cfg(crossheap)]
impl<'h> Marker<'h> {
    /// `grey`: the roots go here (empty; only this crew uses it during marking).
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

    /// Mark from `roots` (index in the group, object header address) until every worker is idle.
    pub fn run(&self, pool: &Pool, roots: &[(usize, usize)]) {
        for &(m, header) in roots {
            self.visit(m, header, |item| self.crew.grey.push(item));
        }
        pool.run(self.crew.len(), |me| {
            let local = self.crew.take(me);
            self.crew.drain(me, &local, |item| self.scan(item, &local));
        });
    }

    /// Mark an object of heap `m` in the group (and the lines it spans); when it is first seen this round and has edges, hand its queue item to `push`.
    fn visit(&self, m: usize, header: usize, push: impl FnOnce(usize)) {
        let member = &self.members[m];
        let h = core::ptr::with_exposed_provenance::<PHeader>(header);
        // SAFETY: from an entry or a pointer slot of a live object, so the object is alive
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

    /// Trace an object: every slot in the pointer map that points into a heap of the group is marked (edges into another heap are reported to `found`); heaps outside the group are kept by their own entries.
    fn scan(&self, item: usize, local: &Worker<usize>) {
        let (m, header) = (item >> MEMBER_SHIFT, item & ADDR_MASK);
        let source = self.members[m].heap;
        let h = core::ptr::with_exposed_provenance::<PHeader>(header);
        // SAFETY: the queue holds only live objects marked this round
        let ty = unsafe { (*h).ty };
        let data = PHeader::data(h);
        for &edge in source.edges(ty) {
            let (far, off) = (edge & FAR_EDGE != 0, (edge & !EDGE_KINDS) as usize);
            // SAFETY: every offset in the pointer map is the position of a pointer slot (one AtomicU64) in this type
            let raw = unsafe { &*data.add(off).cast::<AtomicU64>() }.load(Ordering::Acquire);
            // Edges storing absolute addresses become public offsets (addresses outside the region, private heap objects, are not handled here)
            let absolute = match edge & (WORD_EDGE | PTR_EDGE) {
                WORD_EDGE => Some(word_pointer(raw).map_or(0, <*mut u8>::addr)),
                PTR_EDGE => usize::try_from(raw).ok(),
                _ => None,
            };
            let v = match absolute {
                Some(addr) => source.offset_of(addr).unwrap_or(0),
                None => raw,
            };
            if v == 0 {
                continue;
            }
            let Some(t) = Inner::heap_of(v).and_then(|k| self.by_heap[k]) else { continue };
            // The heap pointed to has died and its number was reused by this new heap of the group: not its object, do not follow
            // SAFETY: a cross-heap slot is a CrossHeapRef (repr(C): the offset is followed by the target heap's identity, also one AtomicU64)
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
