//! Pointers and handles: the private heap's [`Gc`] / [`Slot`] / [`Root`] / [`Weak`] / [`Far`], and the public heap's [`CrossHeapGc`] / [`CrossHeapSlot`] / [`CrossHeapRef`] / [`CrossHeapRoot`] with [`Shared`].
//!
//! The heap pointer [`Gc`] and the atomic pointer slot [`Slot`].
//!
//! Roots: a [`Root`] is a strong handle that can be held outside mutate (and across threads); collection starts from all roots.
//! A weak reference [`Weak`] can likewise be held outside mutate but keeps nothing alive: every collection clears unmarked targets after marking.
//!
//! The root table is an append-only array of slots: new roots take a slot inside mutate (reusing freed slots first), and dropping a handle clears its slot and returns the index.
//! Free slots are compacted into a read-only array only during collection (no mutators), and mutate takes them with an atomic cursor, so it is lock free with no ABA.
//!
//! Cross-heap references [`Far`]: within one process, an object of one heap points directly at an object of another heap, with no copying and no indirection table.
//!
//! - Creating one takes a slot in the target heap's root table (also its entry table) and records the holder heap; the slot is returned when the holding object is finalized.
//!   When the target heap collects on its own the entry is a root, and the block holding the entry object is not evacuated (the other heap stores a direct pointer that cannot be rewritten).
//! - The brand `'a` is the holder heap's mutate brand: a `Far` can only be put into that heap's objects and cannot escape the mutate.
//! - Reading takes the target heap's [`Mutator`]: proof of being inside the target heap's mutate right now (the object is neither swept nor moved).
//! - Cycles across heaps (A's object points to B through a Far and B's points back) cannot be reclaimed by single collections; collect them together with [`crate::Heap::collect_group`].
//!
//! Types and pointers in public heaps: object headers, [`Shared`] (types that can live in a public heap), [`CrossHeapGc`], [`CrossHeapSlot`], [`CrossHeapRoot`].
//!
//! Public heap objects may be read by other processes and collected by processes that do not know the type, so: headers hold no code, only a type number; collection finds edges through the pointer maps in the type table;
//! objects may not have destructors; pointers are stored as "offsets from the start of the public region", so the same value points at the same object in every process.
use core::fmt;
use core::cell::{Cell, UnsafeCell};
use core::marker::PhantomData;
#[cfg(crossheap)]
use core::num::{NonZeroI16, NonZeroI32, NonZeroI64, NonZeroI8, NonZeroU16, NonZeroU32, NonZeroU64, NonZeroU8};
use core::ops::Deref;
use core::ptr::{NonNull, null_mut};
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};
#[cfg(crossheap)]
use core::sync::atomic::{AtomicBool, AtomicI16, AtomicI32, AtomicI64, AtomicI8, AtomicU16, AtomicU32, AtomicU8};
use std::sync::Arc;

use crate::{Heap, Mutator, Trace, Tracer};
use crate::heap::{HeapInternal, MutatorInternal};
#[cfg(crossheap)]
use crate::heap::{Inner, CrossHeap, CrossHeapInternal, CrossHeapMutator, CrossHeapMutatorInternal};
use crate::layout::Header;
#[cfg(crossheap)]
use crate::layout::{GRANULE, PHeader};
use crate::mark::TracerInternal;
#[cfg(crossheap)]
use crate::shm::{REGION, Region};
use crate::sync::{PushList, SegVec};

/// Brand: `'gc` is invariant, so a `Gc` cannot escape the [`crate::Heap::mutate`] closure.
pub type Invariant<'gc> = PhantomData<Cell<&'gc ()>>;

/// The brand of a Gc: borrows `&'gc T`, plus invariance over `'gc`
type GcBrand<'gc, T> = PhantomData<(&'gc T, fn(&'gc ()) -> &'gc ())>;

/// A pointer to a heap object, valid only within one `mutate` (`'gc` is that call's brand). Objects may move only during stop-the-world collections (between two mutates).
///
/// Its layout is guaranteed to be one `NonNull<T>` (`#[repr(transparent)]`), so `Option<Gc<'_, T>>` is also pointer sized.
#[repr(transparent)]
pub struct Gc<'gc, T> {
    ptr: NonNull<T>,
    _brand: GcBrand<'gc, T>,
}

// SAFETY: a Gc only gives out &T, and T: Trace requires Send + Sync
unsafe impl<T: Send + Sync> Send for Gc<'_, T> {}
// SAFETY: as above
unsafe impl<T: Send + Sync> Sync for Gc<'_, T> {}

impl<T> Clone for Gc<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Gc<'_, T> {}

/// [`Gc`] operations for this crate only: the trait lives in a private module and is not exported, so it is unreachable outside the crate.
pub trait GcInternal {
    type Data;

    /// # Safety
    /// `ptr` is the data pointer of a live object in this heap, with the correct brand.
    unsafe fn from_data(ptr: *mut Self::Data) -> Self;

    fn header(self) -> *const Header;
}

impl<T> GcInternal for Gc<'_, T> {
    type Data = T;

    unsafe fn from_data(ptr: *mut T) -> Self {
        // SAFETY: see the trait's documentation
        Self { ptr: unsafe { NonNull::new_unchecked(ptr) }, _brand: PhantomData }
    }

    fn header(self) -> *const Header {
        Header::of(self.ptr.as_ptr().cast())
    }
}

impl<'gc, T> Gc<'gc, T> {
    pub fn as_ptr(self) -> *const T {
        self.ptr.as_ptr()
    }

    /// Get a `Gc` back from a data pointer (the result of [`Gc::as_ptr`]), for example decoded from a NaN-boxed value.
    ///
    /// # Safety
    /// `ptr` must be the data pointer of a live object in this heap, obtained from [`Gc::as_ptr`] within the current `mutate` (brand `'gc`):
    /// stop-the-world collections move objects, so an address obtained in an earlier mutate may be stale now.
    ///
    /// When such a raw address is stored in a heap object, the collector cannot see it: the object's [`crate::Trace`] impl must turn it back into a `Gc` with `from_raw` and report it to
    /// [`crate::Tracer::edge`] (tracing), and in `relocate` hand it to [`crate::Tracer::relocate`] and write the returned new address back (evacuation moves objects);
    /// otherwise the object is collected as garbage, or left as a dangling address after it moves.
    pub unsafe fn from_raw(ptr: *const T) -> Self {
        // SAFETY: see above; the data pointer is non-null
        unsafe { Self::from_data(ptr.cast_mut()) }
    }

    /// A reference valid for all of `'gc`.
    pub fn get(self) -> &'gc T {
        // SAFETY: collection happens only outside mutate, so the object stays alive and in place during 'gc
        unsafe { self.ptr.as_ref() }
    }

    pub fn ptr_eq(a: Self, b: Self) -> bool {
        a.ptr == b.ptr
    }
}

impl<T> Deref for Gc<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: as for get
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: fmt::Debug> fmt::Debug for Gc<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// An atomic nullable pointer slot: edges an object needs to change go here (`Trace` requires `Sync`, so `Cell<Gc>` will not do).
///
/// Writes take a [`Mutator`]: during concurrent marking the overwritten old value goes to the collector through the write barrier (snapshot at the beginning);
/// normally a slot in an old object is logged the first time it is written each round, and collection settles reference counts as "new value +1, last counted value −1".
#[repr(C)]
pub struct Slot<'gc, T> {
    /// The pointer as a word (see [`word_pointer`])
    word: AtomicU64,
    /// The value last counted in the reference count (an address) | [`LOGGED`]; [`UNCOUNTED`] = not counted (a slot in a young object, counted only from promotion on)
    counted: AtomicUsize,
    _brand: PhantomData<Gc<'gc, T>>,
}

/// [`Slot::counted`]: this slot is not reference counted and its object is still young (the write barrier need not record it)
pub const UNCOUNTED: usize = usize::MAX;
/// [`Slot::counted`]: this slot is not reference counted, but its object has been traced by a collection (it is old, so writing a young value must be recorded).
/// Every larger value also means uncounted (the lowest bit is [`LOGGED`], so it takes two values)
pub const PROMOTED: usize = usize::MAX - 1;
/// Lowest bit of [`Slot::counted`]: logged this round (addresses are granule aligned, so the lowest bit is free)
pub const LOGGED: usize = 1;

/// A slot holds a "word": a word whose top 13 bits are all 1 (`>= WORD_POINTER_MIN`) is a GC pointer whose low 48 bits are the object data's address (user-space addresses fit);
/// 0 is empty; every other word is not a pointer and is interpreted by the user (numbers, booleans…: NaN-boxed values can go into a [`WordSlot`] as they are).
/// [`Slot`] tags its pointers with [`RAW_TAG`].
pub const WORD_POINTER_MIN: u64 = 0xFFF8 << 48;
/// Low 48 bits of a word: the address in a pointer word
pub const WORD_PAYLOAD: u64 = (1 << 48) - 1;
/// The tag [`Slot`] puts on pointers (the highest one in the pointer range; the others are left to users to tell types apart)
pub const RAW_TAG: u64 = 0xFFFF << 48;

/// The GC pointer in a word (object data address); `None` if it is not a pointer.
#[inline]
pub fn word_pointer(word: u64) -> Option<*mut u8> {
    (word >= WORD_POINTER_MIN).then(|| core::ptr::with_exposed_provenance_mut((word & WORD_PAYLOAD) as usize))
}

/// Replace the pointer in a word, keeping the tag (rewritten to the new address during evacuation).
#[inline]
fn word_with_pointer(word: u64, data: *const u8) -> u64 {
    debug_assert!(word >= WORD_POINTER_MIN, "only pointer words are rewritten");
    (word & !WORD_PAYLOAD) | data.expose_provenance() as u64
}

/// A type-erased slot: for logging by the write barrier and settling during collection (same layout as [`Slot`] and [`WordSlot`]).
#[repr(C)]
pub struct RawSlot {
    pub word: AtomicU64,
    pub counted: AtomicUsize,
}

impl RawSlot {
    /// The header of the private heap object a word points at. Pointers into a cross-heap ([`crate::CrossHeap`]) do not count: the private heap never marks, counts or moves those objects.
    fn header(word: u64) -> Option<*const Header> {
        Self::private(word_pointer(word)).map(|p| Header::of(p))
    }

    #[cfg(crossheap)]
    fn private(p: Option<*mut u8>) -> Option<*mut u8> {
        p.filter(|p| !Region::holds_fixed(p.addr()))
    }

    #[cfg(not(crossheap))]
    fn private(p: Option<*mut u8>) -> Option<*mut u8> {
        p
    }

    /// The data address when the word points at a cross-heap object.
    #[cfg(crossheap)]
    pub fn cross(&self) -> Option<*mut u8> {
        word_pointer(self.word.load(Ordering::Acquire)).filter(|p| Region::holds_fixed(p.addr()))
    }

    /// Point at the moved object (`to` is the new header); only during a pause (no mutators).
    pub fn relocate(&self, to: *const Header) {
        let w = self.word.load(Ordering::Acquire);
        self.word.store(word_with_pointer(w, Header::data(to)), Ordering::Release);
    }

    fn uncounted(c: usize) -> bool {
        c >= PROMOTED
    }

    fn from_counted(c: usize) -> Option<*const Header> {
        (!Self::uncounted(c) && c & !LOGGED != 0).then(|| Header::of(core::ptr::with_exposed_provenance::<u8>(c & !LOGGED)))
    }

    /// Written for the first time this round: set the logged flag and return true (the caller hands the slot address to the collector). Uncounted slots return false.
    pub fn log(&self) -> bool {
        let c = self.counted.load(Ordering::Relaxed);
        !Self::uncounted(c) && c & LOGGED == 0 && self.counted.compare_exchange(c, c | LOGGED, Ordering::AcqRel, Ordering::Relaxed).is_ok()
    }

    /// The object holding it is still young (not traced by any collection since it was allocated): writing a young value need not go into the remembered set, the young collection reaches it from the object itself.
    pub fn is_young(&self) -> bool {
        self.counted.load(Ordering::Relaxed) == UNCOUNTED
    }

    /// A collection traced the object holding it (while uncounted): from now on it is an old object and the write barrier must record it.
    pub fn promote(&self) {
        // Failure ⇒ already not young (counted or promoted), nothing to change
        let _ = self.counted.compare_exchange(UNCOUNTED, PROMOTED, Ordering::Relaxed, Ordering::Relaxed);
    }

    /// Clear the logged flag (dropping a log while counts are untrustworthy).
    pub fn unlog(&self) {
        self.counted.fetch_and(!LOGGED, Ordering::AcqRel);
    }

    /// Switch to counting the current value; returns (current value, last counted value).
    pub fn recount(&self) -> (Option<*const Header>, Option<*const Header>) {
        let p = Self::private(word_pointer(self.word.load(Ordering::Acquire)));
        let prev = self.counted.swap(p.map_or(0, <*mut u8>::expose_provenance), Ordering::AcqRel);
        (p.map(|p| Header::of(p.cast_const())), Self::from_counted(prev))
    }

    /// The slot's current value.
    pub fn current(&self) -> Option<*const Header> {
        Self::header(self.word.load(Ordering::Acquire))
    }

    /// The last counted value (decremented when the object holding it is released).
    pub fn counted(&self) -> Option<*const Header> {
        Self::from_counted(self.counted.load(Ordering::Acquire))
    }
}

impl<T> Default for Slot<'_, T> {
    fn default() -> Self {
        Self { word: AtomicU64::new(0), counted: AtomicUsize::new(UNCOUNTED), _brand: PhantomData }
    }
}

impl<'gc, T> From<Option<Gc<'gc, T>>> for Slot<'gc, T> {
    fn from(v: Option<Gc<'gc, T>>) -> Self {
        Self { word: AtomicU64::new(Self::raw(v)), counted: AtomicUsize::new(UNCOUNTED), _brand: PhantomData }
    }
}

impl<'gc, T> From<Gc<'gc, T>> for Slot<'gc, T> {
    fn from(v: Gc<'gc, T>) -> Self {
        Some(v).into()
    }
}

impl<'gc, T> Slot<'gc, T> {
    fn raw(v: Option<Gc<'gc, T>>) -> u64 {
        v.map_or(0, |g| RAW_TAG | g.ptr.as_ptr().expose_provenance() as u64)
    }

    fn wrap(word: u64) -> Option<Gc<'gc, T>> {
        // SAFETY: the slot only ever held Gcs of the same brand, and the object is referenced by the slot's object
        word_pointer(word).map(|p| unsafe { Gc::from_data(p.cast()) })
    }

    pub fn get(&self) -> Option<Gc<'gc, T>> {
        Self::wrap(self.word.load(Ordering::Acquire))
    }

    pub fn set(&self, m: &Mutator<'gc>, v: Option<Gc<'gc, T>>) {
        self.swap(m, v);
    }

    pub fn swap(&self, m: &Mutator<'gc>, v: Option<Gc<'gc, T>>) -> Option<Gc<'gc, T>> {
        let new = Self::raw(v);
        let old = self.word.swap(new, Ordering::AcqRel);
        m.written(self.raw_slot(), RawSlot::header(old), RawSlot::header(new));
        Self::wrap(old)
    }

    /// Swap in `new` only if the slot holds `current`; on failure returns the slot's value.
    pub fn compare_exchange(&self, m: &Mutator<'gc>, current: Option<Gc<'gc, T>>, new: Option<Gc<'gc, T>>) -> Result<Option<Gc<'gc, T>>, Option<Gc<'gc, T>>> {
        let new = Self::raw(new);
        let done = self.word.compare_exchange(Self::raw(current), new, Ordering::AcqRel, Ordering::Acquire);
        // Write barrier: the overwritten old value (concurrent marking's snapshot), the new value written (a young object ⇒ record this slot, young collections start from it),
        // and reference count logging (a slot in an old object written for the first time this round)
        if let Ok(old) = done {
            m.written(self.raw_slot(), RawSlot::header(old), RawSlot::header(new));
        }
        done.map(Self::wrap).map_err(Self::wrap)
    }
}

/// A slot holding any "word" (see [`word_pointer`]): the user's own value encoding (such as NaN-boxing) goes in as is, and GC pointers in the word are still traced,
/// rewritten during evacuation and counted. As large as a [`Slot`] (the word + the last counted value), one word less than "a value word + a [`Slot`]".
///
/// Writes come only from the one mutator holding its object (one slot is never written by two threads at once), so a write is a read and a store, not an atomic swap.
#[repr(C)]
pub struct WordSlot<'gc> {
    word: AtomicU64,
    counted: AtomicUsize,
    _brand: PhantomData<Gc<'gc, ()>>,
}

impl WordSlot<'_> {
    /// A new slot holding `word` (its object is still young, uncounted).
    pub fn new(word: u64) -> Self {
        Self { word: AtomicU64::new(word), counted: AtomicUsize::new(UNCOUNTED), _brand: PhantomData }
    }

    #[inline]
    pub fn get(&self) -> u64 {
        self.word.load(Ordering::Acquire)
    }

    /// Write `word`: goes through the write barrier (see [`Slot`]) when the old or new value holds a pointer (and they are not the same object).
    #[inline]
    pub fn set(&self, m: &Mutator<'_>, word: u64) {
        let old = self.word.load(Ordering::Relaxed);
        self.word.store(word, Ordering::Release);
        let (o, n) = (RawSlot::header(old), RawSlot::header(word));
        if o != n {
            m.written(self.raw_slot(), o, n);
        }
    }

    /// Write with exclusive access (not yet in the heap, or during a pause)
    pub fn set_mut(&mut self, word: u64) {
        *self.word.get_mut() = word;
    }

    fn raw_slot(&self) -> &RawSlot {
        // SAFETY: WordSlot and RawSlot are both repr(C) with fields of the same types in the same order
        unsafe { &*(&raw const *self).cast::<RawSlot>() }
    }
}

impl fmt::Debug for WordSlot<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WordSlot({:#x})", self.get())
    }
}

/// [`WordSlot`] operations for this crate only.
pub trait WordSlotInternal {
    fn raw(&self) -> &RawSlot;
}

impl WordSlotInternal for WordSlot<'_> {
    fn raw(&self) -> &RawSlot {
        self.raw_slot()
    }
}

/// [`Slot`] operations for this crate only (as [`GcInternal`]).
pub trait SlotInternal<'gc> {
    type Data;

    /// The type-erased view.
    fn raw_slot(&self) -> &RawSlot;
}

impl<'gc, T> SlotInternal<'gc> for Slot<'gc, T> {
    type Data = T;

    fn raw_slot(&self) -> &RawSlot {
        // SAFETY: Slot and RawSlot are both repr(C) with fields of the same types in the same order
        unsafe { &*(&raw const *self).cast::<RawSlot>() }
    }
}

impl<T: fmt::Debug> fmt::Debug for Slot<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Slot").field(&self.get()).finish()
    }
}

/// Rebrand a type carrying `'gc`: `Root<B>` has no lifetime, and the current mutate's `'gc` is put back on when reading it.
/// Declare it with [`brand!`](crate::brand).
pub trait Brand {
    type Of<'gc>: Trace + 'gc;
}

/// Declare a brand type: `brand!(pub NodeB = for<'gc> Node<'gc>);`
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

/// A root slot: the object header pointer (null when free) + the holder.
#[derive(Default)]
pub struct Entry {
    ptr: AtomicPtr<Header>,
    /// The heap holding this root (the identity of the heap the [`crate::Far`] lives in); [`NO_HOLDER`] for a plain [`Root`]
    holder: AtomicUsize,
}

/// [`Entry::holder`]: belongs to no heap (a plain [`Root`])
pub const NO_HOLDER: usize = 0;

impl Entry {
    pub fn load(&self) -> *mut Header {
        self.ptr.load(Ordering::Acquire)
    }

    pub fn store(&self, header: *mut Header) {
        self.ptr.store(header, Ordering::Release);
    }
}

/// The root table, which is also the heap's entry table: objects pointed to directly from other heaps through [`crate::Far`] each take a slot here, recording the holder.
#[derive(Default)]
pub struct RootTable {
    /// One slot per root
    slots: SegVec<Entry>,
    /// Slot indexes returned by dropped handles (merged into ready at the next collection)
    returned: PushList<usize>,
    /// Reusable free slot indexes: changed only during collection, read only during mutate
    ready: UnsafeCell<Vec<usize>>,
    /// Next index of ready not yet taken
    cursor: AtomicUsize,
}

// SAFETY: ready changes only during collection (heap held exclusively, no mutators) and is read only during mutate; the rest are atomics / lock-free structures
unsafe impl Sync for RootTable {}
// SAFETY: as above
unsafe impl Send for RootTable {}

impl RootTable {
    /// The object in the slot lives in a cross-heap (it was promoted): private collections leave it alone, the cross-heap's collection takes care of it.
    #[cfg(crossheap)]
    fn crossed(p: *const Header) -> bool {
        Region::holds_fixed(p.addr())
    }

    #[cfg(not(crossheap))]
    fn crossed(_: *const Header) -> bool {
        false
    }

    /// Slots pointing at cross-heap objects: hand each data address to `f` (roots when the owner collects the cross-heap too). SAFETY (caller): no mutators.
    #[cfg(crossheap)]
    pub unsafe fn crossing(&self, mut f: impl FnMut(*mut u8)) {
        for slot in self.slots.iter() {
            let p = slot.load();
            if !p.is_null() && Self::crossed(p) {
                f(Header::data(p));
            }
        }
    }

    /// After a cross-heap collection: clear the slots pointing at cross-heap objects that `alive` says are dead (for weak tables). SAFETY (caller): no mutators.
    #[cfg(crossheap)]
    pub unsafe fn clear_crossed(&self, alive: impl Fn(*mut u8) -> bool) {
        for slot in self.slots.iter() {
            let p = slot.load();
            if !p.is_null() && Self::crossed(p) && !alive(Header::data(p)) {
                let _ = slot.ptr.compare_exchange(p, null_mut(), Ordering::AcqRel, Ordering::Relaxed);
            }
        }
    }

    /// Take a slot for `header` (only inside this heap's mutate). `holder`: the holder heap, [`NO_HOLDER`] for a plain root.
    pub fn claim(&self, header: *const Header, holder: usize) -> usize {
        // SAFETY: ready does not change during mutate
        let ready = unsafe { &*self.ready.get() };
        let i = self.cursor.fetch_add(1, Ordering::AcqRel);
        match ready.get(i).and_then(|&slot| self.slots.get(slot).map(|s| (slot, s))) {
            Some((slot, s)) => {
                // Write the holder before publishing the pointer: readers read the pointer (Acquire) before the holder
                s.holder.store(holder, Ordering::Relaxed);
                s.store(header.cast_mut());
                slot
            }
            None => self.slots.push(Entry { ptr: AtomicPtr::new(header.cast_mut()), holder: AtomicUsize::new(holder) }),
        }
    }

    pub fn slot(&self, i: usize) -> &Entry {
        self.slots.get(i).expect("root slot indexes come from claim")
    }

    /// A handle is dropped: clear the slot and return its index (at any time, on any thread).
    pub fn release(&self, i: usize) {
        self.slot(i).store(null_mut());
        self.returned.push(i);
    }

    /// Evacuating collections: hand each root to `f` (mark, moving if needed) and change it to the new address returned.
    /// Handles may be dropped on other threads meanwhile (clearing the slot), hence the CAS: a cleared slot is not written back.
    /// Blocks holding entry objects (pointed to directly from other heaps) were taken off evacuation beforehand ([`RootTable::pinned`]), so they do not move.
    pub fn relocate(&self, mut f: impl FnMut(*const Header) -> *const Header) {
        for slot in self.slots.iter() {
            let old = slot.load();
            if old.is_null() || Self::crossed(old) {
                continue;
            }
            let new = f(old.cast_const());
            if !core::ptr::eq(old, new) {
                let _ = slot.ptr.compare_exchange(old, new.cast_mut(), Ordering::AcqRel, Ordering::Acquire);
            }
        }
    }

    /// Hand each object pointed to directly from another heap to `f`. SAFETY (caller): no mutators.
    pub unsafe fn pinned(&self, mut f: impl FnMut(*const Header)) {
        for slot in self.slots.iter() {
            let p = slot.load();
            if !p.is_null() && !Self::crossed(p) && slot.holder.load(Ordering::Relaxed) != NO_HOLDER {
                f(p.cast_const());
            }
        }
    }

    /// During collection: compact the free slots and return every root. SAFETY (caller): no mutators (heap held exclusively).
    pub unsafe fn settle(&self) -> Vec<*const Header> {
        // SAFETY: see above
        unsafe { self.settle_outside(|_| false) }
    }

    /// As [`RootTable::settle`], but entries whose holder satisfies `inside` are not roots (Fars in the heaps collected together, whose liveness tracing decides).
    pub unsafe fn settle_outside(&self, inside: impl Fn(usize) -> bool) -> Vec<*const Header> {
        // SAFETY: see settle
        unsafe { self.recycle() };
        self.slots
            .iter()
            .filter_map(|s| {
                let p = s.load();
                (!p.is_null() && !Self::crossed(p) && !inside(s.holder.load(Ordering::Relaxed))).then_some(p.cast_const())
            })
            .collect()
    }

    /// Merge returned slot indexes into the reusable ones (dropping the ones taken). SAFETY (caller): no mutators.
    unsafe fn recycle(&self) {
        // SAFETY: see above: ready changes only here
        let ready = unsafe { &mut *self.ready.get() };
        let used = self.cursor.swap(0, Ordering::AcqRel).min(ready.len());
        ready.drain(..used);
        ready.extend(self.returned.take());
    }

    /// A weak table ([`Weak`] slots) after marking and before sweeping: targets not marked with `epoch` are cleared, moved ones rewritten to the new address;
    /// live targets get stuck counts (`parity` is the current count's parity). Handles may be dropped on other threads meanwhile (clearing the slot), hence the CAS.
    /// SAFETY (caller): no mutators; marking (including evacuation) is done and sweeping has not started, so the memory of the objects in the slots is still there.
    pub unsafe fn clear_unreached(&self, epoch: u32, parity: bool) {
        // SAFETY: see above
        unsafe { self.recycle() };
        for slot in self.slots.iter() {
            let old = slot.load();
            if old.is_null() || Self::crossed(old) {
                continue;
            }
            let new = Header::survivor(old, epoch);
            if let Some(new) = new {
                // SAFETY: a live object
                unsafe { &*new }.rc_stick(parity);
            }
            let new = new.map_or(null_mut(), <*const Header>::cast_mut);
            if !core::ptr::eq(old, new) {
                let _ = slot.ptr.compare_exchange(old, new, Ordering::AcqRel, Ordering::Acquire);
            }
        }
    }

    /// After a group collection: entries whose holder satisfies `inside` and whose target was not marked — the Far holding it is dead (live Fars were traced and marked their targets) —
    /// are cleared, so the next collection does not use them as roots and touch reclaimed objects before the holder sweeps that Far. SAFETY (caller): no mutators.
    pub unsafe fn clear_dead(&self, inside: impl Fn(usize) -> bool, epoch: u32) {
        for slot in self.slots.iter() {
            let p = slot.load();
            // SAFETY: every non-null slot holds an object of this heap; the collection has not swept, so the memory is there
            if !p.is_null() && !Self::crossed(p) && inside(slot.holder.load(Ordering::Relaxed)) && !unsafe { &*p }.is_marked(epoch) {
                let _ = slot.ptr.compare_exchange(p, null_mut(), Ordering::AcqRel, Ordering::Relaxed);
            }
        }
    }
}

/// A strong handle: as long as it lives the object is not collected. Carries no `'gc`, so it can be stored anywhere and passed across threads.
/// Create it with [`Mutator::root`] and read it with [`Mutator::get`].
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

// SAFETY: the handle itself holds no Gc; it is a root
unsafe impl<B: Brand> Trace for Root<B> {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut crate::Tracer<'_>) {}
}

impl<'gc> Mutator<'gc> {
    /// Make a root for an object.
    pub fn root<B: Brand>(&self, gc: Gc<'gc, B::Of<'gc>>) -> Root<B> {
        let table = self.heap().roots().clone();
        let slot = table.claim(gc.header(), NO_HOLDER);
        Root { table, slot, _brand: PhantomData }
    }

    /// The object a root points at.
    pub fn get<B: Brand>(&self, root: &Root<B>) -> Gc<'gc, B::Of<'gc>> {
        assert!(Arc::ptr_eq(&root.table, self.heap().roots()), "this Root does not belong to this heap");
        let header = root.table.slot(root.slot).load();
        // SAFETY: the root slot holds an object made with the same B and kept alive by this root
        unsafe { Gc::from_data(Header::data(header).cast()) }
    }

    /// Point the root at another object.
    pub fn set<B: Brand>(&self, root: &Root<B>, gc: Gc<'gc, B::Of<'gc>>) {
        assert!(Arc::ptr_eq(&root.table, self.heap().roots()), "this Root does not belong to this heap");
        root.table.slot(root.slot).store(gc.header().cast_mut());
    }
}

/// A weak handle: keeps nothing alive; once the object is collected [`Mutator::upgrade`] gives None. Like [`Root`] it carries no `'gc`, so it can be stored anywhere
/// (including inside heap objects) and passed across threads. Create it with [`Mutator::weak`].
///
/// Every collection (full, young, the end of concurrent marking, group) clears weak references to unmarked targets after marking and before sweeping, and rewrites moved ones to the new address;
/// so within any mutate a weak reference is either empty or points at a live object.
/// A young object without strong references dies at the next young collection; an old one (its count stuck by the weak reference, see [`crate::Heap::collect_young`]) waits for a full collection.
pub struct Weak<B: Brand> {
    /// The heap's weak table
    table: Arc<RootTable>,
    slot: usize,
    _brand: PhantomData<fn() -> B>,
}

impl<B: Brand> Weak<B> {
    /// The target has been collected (and stays so). No mutate needed; when false the target may still die at the next collection.
    pub fn is_cleared(&self) -> bool {
        self.table.slot(self.slot).load().is_null()
    }
}

impl<B: Brand> Drop for Weak<B> {
    fn drop(&mut self) {
        self.table.release(self.slot);
    }
}

// SAFETY: the handle itself holds no Gc and has no strong edge to its target
unsafe impl<B: Brand> Trace for Weak<B> {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut crate::Tracer<'_>) {}
}

impl<'gc> Mutator<'gc> {
    /// Make a weak reference to an object.
    ///
    /// From the next collection on the target's reference count sticks (no more release by count, left to tracing): release by count runs in the background concurrently with mutators and could not clear
    /// weak references first. Counts stick when each pause processes the weak table, before the decrements computed in that pause, so it is not released by count before then.
    pub fn weak<B: Brand>(&self, gc: Gc<'gc, B::Of<'gc>>) -> Weak<B> {
        let table = self.heap().weaks().clone();
        let slot = table.claim(gc.header(), NO_HOLDER);
        Weak { table, slot, _brand: PhantomData }
    }

    /// The object a weak reference points at; None once collected. An object obtained during concurrent marking counts as referenced (it gets marked) and does not die this round.
    pub fn upgrade<B: Brand>(&self, weak: &Weak<B>) -> Option<Gc<'gc, B::Of<'gc>>> {
        assert!(Arc::ptr_eq(&weak.table, self.heap().weaks()), "this Weak does not belong to this heap");
        let header = weak.table.slot(weak.slot).load();
        if header.is_null() {
            return None;
        }
        // Read barrier: snapshot at the beginning only marks objects strongly reachable at the snapshot; one taken from a weak reference and stored into a black object would be missed
        self.reached(header);
        // SAFETY: non-null slots of the weak table point at live objects (each collection clears the dead before sweeping), of the type B it was created with
        Some(unsafe { Gc::from_data(Header::data(header).cast()) })
    }
}

/// A direct pointer to an object in another heap, stored in an object of the holder heap (brand `'a`).
pub struct Far<'a, B: Brand> {
    /// The target heap's root table: the slot taken lives here; the Arc makes returning the slot safe even when the target heap is dropped first
    table: Arc<RootTable>,
    slot: usize,
    /// The target object's header (entry objects are not evacuated, so it does not change)
    ptr: NonNull<Header>,
    _brand: PhantomData<(Invariant<'a>, fn() -> B)>,
}

// SAFETY: only shared references to B::Of are given out, and B::Of: Trace requires Send + Sync; the root table is thread safe
unsafe impl<B: Brand> Send for Far<'_, B> {}
// SAFETY: as above
unsafe impl<B: Brand> Sync for Far<'_, B> {}

impl<'a, B: Brand> Far<'a, B> {
    /// `holder`: the holder heap (this Far goes into one of its objects); `target`: the heap containing `gc`, which must differ (edges within a heap use [`crate::Slot`]).
    pub fn new<'b>(holder: &Mutator<'a>, target: &Mutator<'b>, gc: Gc<'b, B::Of<'b>>) -> Self {
        assert!(!core::ptr::eq(holder.heap(), target.heap()), "edges within one heap use Slot, not Far");
        let table = target.heap().roots().clone();
        let slot = table.claim(gc.header(), holder.heap().id());
        // SAFETY: a Gc's header is non-null
        let ptr = unsafe { NonNull::new_unchecked(gc.header().cast_mut()) };
        Self { table, slot, ptr, _brand: PhantomData }
    }

    /// The target object; `m` must be the target heap's mutator.
    pub fn get<'b>(&self, m: &Mutator<'b>) -> Gc<'b, B::Of<'b>> {
        assert!(Arc::ptr_eq(&self.table, m.heap().roots()), "this Far does not point into this heap");
        // SAFETY: the slot keeps the object alive and entry objects do not move; m proves we are inside the target heap's mutate; B is the same as at creation
        unsafe { Gc::from_data(Header::data(self.ptr.as_ptr()).cast()) }
    }

    /// Whether the target object is in `heap`.
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

// SAFETY: no edges into the holder heap; in a group collection the target is reported to the collector
unsafe impl<B: Brand> Trace for Far<'_, B> {
    fn trace(&self, t: &mut Tracer<'_>) {
        self.report(t);
    }

    fn relocate(&mut self, t: &mut Tracer<'_>) {
        self.report(t);
    }
}

/// A type that can live in a public heap: `#[repr(C)]`, only plain data, [`CrossHeapSlot`] and nested types like this, fixed-size arrays, no destructor.
/// Generate it with `#[derive(Shared)]`; do not write it by hand.
///
/// # Safety
/// `edges` must report the byte offset (relative to `at`) of every [`CrossHeapSlot`] to `map`; a missed one is a dangling pointer. `NO_EDGES` may be true only when there really are none.
#[cfg(crossheap)]
pub unsafe trait Shared: Send + Sync {
    /// The type holds no [`CrossHeapSlot`]
    const NO_EDGES: bool = false;

    /// Report the offset of each pointer slot inside itself (`at` + the slot's offset within it) to `map`.
    fn edges(map: &mut EdgeMap, at: u32);
}

/// An offset in the pointer map with this bit is a [`CrossHeapRef`] (offset + target heap identity) pointing into another heap; without it, a [`CrossHeapSlot`] within the same heap
#[cfg(crossheap)]
pub const FAR_EDGE: u32 = 1 << 31;
/// An offset in the pointer map with this bit is a word ([`Slot`], [`WordSlot`]: only what [`word_pointer`] recognizes is a pointer) holding the object data's absolute address
/// (in the fixed-address region, the same in every process; see [`crate::CrossHeapSpace::reserve_fixed`])
#[cfg(crossheap)]
pub const WORD_EDGE: u32 = 1 << 30;
/// An offset in the pointer map with this bit is a nullable raw pointer ([`Gc`], `Option<Gc>`) holding the object data's absolute address
#[cfg(crossheap)]
pub const PTR_EDGE: u32 = 1 << 29;
/// The bits of a pointer map offset that give the kind of edge
#[cfg(crossheap)]
pub const EDGE_KINDS: u32 = FAR_EDGE | WORD_EDGE | PTR_EDGE;

/// A type's pointer map: the byte offset of every pointer slot in the object data.
#[derive(Default)]
#[cfg(crossheap)]
pub struct EdgeMap(Vec<u32>);

#[cfg(crossheap)]
impl EdgeMap {
    pub fn edge(&mut self, at: u32) {
        self.0.push(at);
    }

    /// A [`CrossHeapRef`] pointing into another public heap (the offset is followed by the target heap's identity).
    pub fn far(&mut self, at: u32) {
        self.0.push(at | FAR_EDGE);
    }

    /// A word holding an absolute address ([`Slot`], [`WordSlot`]).
    pub fn word(&mut self, at: u32) {
        self.0.push(at | WORD_EDGE);
    }

    /// A nullable raw pointer holding an absolute address ([`Gc`]).
    pub fn ptr(&mut self, at: u32) {
        self.0.push(at | PTR_EDGE);
    }

    /// Every offset reported (the [`FAR_EDGE`] bit marks cross-heap ones).
    pub fn as_slice(&self) -> &[u32] {
        &self.0
    }

    /// Turn a field offset into a pointer map offset (for the derive).
    pub fn offset(bytes: usize) -> u32 {
        u32::try_from(bytes).ok().filter(|&b| b & EDGE_KINDS == 0).expect("public heap objects are at most 512 MiB")
    }
}

// SAFETY: a Gc is itself a raw pointer
#[cfg(crossheap)]
unsafe impl<T: Send + Sync> Shared for Gc<'_, T> {
    fn edges(map: &mut EdgeMap, at: u32) {
        map.ptr(at);
    }
}

// SAFETY: None is the null pointer (Gc is NonNull), as for Gc
#[cfg(crossheap)]
unsafe impl<T: Send + Sync> Shared for Option<Gc<'_, T>> {
    fn edges(map: &mut EdgeMap, at: u32) {
        map.ptr(at);
    }
}

// SAFETY: repr(C), the word comes first
#[cfg(crossheap)]
unsafe impl<T: Send + Sync> Shared for Slot<'_, T> {
    fn edges(map: &mut EdgeMap, at: u32) {
        map.word(at);
    }
}

// SAFETY: the word comes first
#[cfg(crossheap)]
unsafe impl Shared for WordSlot<'_> {
    fn edges(map: &mut EdgeMap, at: u32) {
        map.word(at);
    }
}

/// Public heap types without pointer slots.
#[cfg(crossheap)]
macro_rules! shared_leaf {
    ($($ty:ty),* $(,)?) => {$(
        // SAFETY: no pointer slots
        unsafe impl Shared for $ty {
            const NO_EDGES: bool = true;
            fn edges(_: &mut EdgeMap, _: u32) {}
        }
    )*};
}

#[cfg(crossheap)]
shared_leaf!(
    (), bool, char, u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, f32, f64,
    NonZeroU8, NonZeroU16, NonZeroU32, NonZeroU64, NonZeroI8, NonZeroI16, NonZeroI32, NonZeroI64,
    AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicI8, AtomicI16, AtomicI32, AtomicI64,
);

// SAFETY: no value
#[cfg(crossheap)]
unsafe impl<T: ?Sized + Send + Sync> Shared for PhantomData<T> {
    const NO_EDGES: bool = true;
    fn edges(_: &mut EdgeMap, _: u32) {}
}

// SAFETY: reports element by element
#[cfg(crossheap)]
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

/// An object in a public heap, valid only within one [`CrossHeap::mutate`] (`'p` is that call's brand). An absolute address in this process.
#[cfg(crossheap)]
pub struct CrossHeapGc<'p, T> {
    ptr: NonNull<T>,
    _brand: PhantomData<(&'p T, Invariant<'p>)>,
}

// SAFETY: only &T is given out, and T: Shared requires Send + Sync
#[cfg(crossheap)]
unsafe impl<T: Send + Sync> Send for CrossHeapGc<'_, T> {}
// SAFETY: as above
#[cfg(crossheap)]
unsafe impl<T: Send + Sync> Sync for CrossHeapGc<'_, T> {}

#[cfg(crossheap)]
impl<T> Clone for CrossHeapGc<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

#[cfg(crossheap)]
impl<T> Copy for CrossHeapGc<'_, T> {}

#[cfg(crossheap)]
impl<'p, T> CrossHeapGc<'p, T> {

    pub fn as_ptr(self) -> *const T {
        self.ptr.as_ptr()
    }

    /// A reference valid for all of `'p`.
    pub fn get(self) -> &'p T {
        // SAFETY: collection happens only with no mutators, so the object stays alive and in place during 'p
        unsafe { self.ptr.as_ref() }
    }

    pub fn ptr_eq(a: Self, b: Self) -> bool {
        a.ptr == b.ptr
    }

}

/// Operations for this crate only: the trait lives in a private module and is not exported, so it is unreachable outside the crate.
#[cfg(crossheap)]
pub trait CrossHeapGcInternal {
    /// SAFETY (caller): `ptr` is the data pointer of a live object in this public heap, with the correct brand `'p`.
    unsafe fn from_data(ptr: *mut u8) -> Self;

    /// The public offset: relative to the start of the public region (the same in every process).
    fn offset(self) -> u64;
}

#[cfg(crossheap)]
impl<T> CrossHeapGcInternal for CrossHeapGc<'_, T> {
    unsafe fn from_data(ptr: *mut u8) -> Self {
        // SAFETY: see above
        Self { ptr: unsafe { NonNull::new_unchecked(ptr.cast()) }, _brand: PhantomData }
    }

    fn offset(self) -> u64 {
        let addr = self.ptr.as_ptr().addr();
        (addr - Region::base_of(addr)) as u64
    }
}


#[cfg(crossheap)]
impl<T> Deref for CrossHeapGc<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: as for get
        unsafe { self.ptr.as_ref() }
    }
}

#[cfg(crossheap)]
impl<T: fmt::Debug> fmt::Debug for CrossHeapGc<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// An atomic pointer slot in a public heap object: holds the target's public offset (0 = empty), so the same value is right in every process mapping the heap.
/// Reads and writes take a [`CrossHeapMutator`] (its region start converts to this process's address).
#[repr(transparent)]
#[cfg(crossheap)]
pub struct CrossHeapSlot<'p, T> {
    off: AtomicU64,
    _brand: PhantomData<CrossHeapGc<'p, T>>,
}

#[cfg(crossheap)]
impl<T> Default for CrossHeapSlot<'_, T> {
    fn default() -> Self {
        Self { off: AtomicU64::new(0), _brand: PhantomData }
    }
}

#[cfg(crossheap)]
impl<'p, T> From<Option<CrossHeapGc<'p, T>>> for CrossHeapSlot<'p, T> {
    fn from(v: Option<CrossHeapGc<'p, T>>) -> Self {
        Self { off: AtomicU64::new(Self::raw(v)), _brand: PhantomData }
    }
}

#[cfg(crossheap)]
impl<'p, T> From<CrossHeapGc<'p, T>> for CrossHeapSlot<'p, T> {
    fn from(v: CrossHeapGc<'p, T>) -> Self {
        Some(v).into()
    }
}

#[cfg(crossheap)]
impl<'p, T> CrossHeapSlot<'p, T> {
    fn raw(v: Option<CrossHeapGc<'p, T>>) -> u64 {
        v.map_or(0, CrossHeapGc::offset)
    }

    fn wrap(m: &CrossHeapMutator<'p>, off: u64) -> Option<CrossHeapGc<'p, T>> {
        // SAFETY: the slot only ever held offsets of CrossHeapGcs of the same brand, and the object is referenced by the slot's object
        (off != 0).then(|| unsafe { CrossHeapGc::from_data(m.at(off)) })
    }

    pub fn get(&self, m: &CrossHeapMutator<'p>) -> Option<CrossHeapGc<'p, T>> {
        Self::wrap(m, self.off.load(Ordering::Acquire))
    }

    pub fn set(&self, m: &CrossHeapMutator<'p>, v: Option<CrossHeapGc<'p, T>>) {
        self.swap(m, v);
    }

    pub fn swap(&self, m: &CrossHeapMutator<'p>, v: Option<CrossHeapGc<'p, T>>) -> Option<CrossHeapGc<'p, T>> {
        Self::wrap(m, self.off.swap(Self::raw(v), Ordering::AcqRel))
    }

    /// Swap in `new` only if the slot holds `current`; on failure returns the slot's value.
    pub fn compare_exchange(&self, m: &CrossHeapMutator<'p>, current: Option<CrossHeapGc<'p, T>>, new: Option<CrossHeapGc<'p, T>>) -> Result<Option<CrossHeapGc<'p, T>>, Option<CrossHeapGc<'p, T>>> {
        self.off
            .compare_exchange(Self::raw(current), Self::raw(new), Ordering::AcqRel, Ordering::Acquire)
            .map(|o| Self::wrap(m, o))
            .map_err(|o| Self::wrap(m, o))
    }
}

// SAFETY: the slot is itself a pointer
#[cfg(crossheap)]
unsafe impl<T: Send + Sync> Shared for CrossHeapSlot<'_, T> {
    fn edges(map: &mut EdgeMap, at: u32) {
        map.edge(at);
    }
}

/// A reference from an object in public heap A (brand `'a`) to an object in another public heap B: holds the target's public offset and B's identity, pointing directly with no copy.
///
/// Writing it registers an entry in B's entry table with holder "heap A", so it is a root when B collects on its own (the process collecting B need not be able to open A);
/// A's own collection does not follow into B. Cycles across heaps, and registrations no longer used, are reclaimed by [`CrossHeap::collect_group`]. Reading takes B's mutator.
/// The brand `'a` ensures it can only go into A's objects (so the registered holder is right).
///
/// After B dies its heap number may be reused by a new heap: a reference whose identity does not match reads as None and is not followed by collections, so it is never taken as an address in the new heap.
#[repr(C)]
#[cfg(crossheap)]
pub struct CrossHeapRef<'a, B: CrossHeapBrand> {
    off: AtomicU64,
    /// The target heap's identity ([`crate::layout::Super::incarnation`])
    incarnation: AtomicU64,
    _brand: PhantomData<(HolderBrand<'a>, fn() -> B)>,
}

/// The holder heap's brand: invariant over `'a`; a fn pointer form so it can cross threads (the Cell used by [`Invariant`] cannot)
#[cfg(crossheap)]
type HolderBrand<'a> = fn(&'a ()) -> &'a ();

#[cfg(crossheap)]
impl<B: CrossHeapBrand> Default for CrossHeapRef<'_, B> {
    fn default() -> Self {
        Self { off: AtomicU64::new(0), incarnation: AtomicU64::new(0), _brand: PhantomData }
    }
}

#[cfg(crossheap)]
impl<'a, B: CrossHeapBrand> CrossHeapRef<'a, B> {
    /// A new reference to `gc` (in heap `target`) to put into an object of heap `holder`.
    pub fn to<'b>(holder: &CrossHeapMutator<'a>, target: &CrossHeapMutator<'b>, gc: CrossHeapGc<'b, B::Of<'b>>) -> Self {
        let r = Self::default();
        r.set(holder, target, Some(gc));
        r
    }

    /// Point elsewhere (`None` clears it). The old value's registration is returned at the next group collection.
    pub fn set<'b>(&self, holder: &CrossHeapMutator<'a>, target: &CrossHeapMutator<'b>, v: Option<CrossHeapGc<'b, B::Of<'b>>>) {
        assert!(holder.index() != target.index(), "edges within one public heap use CrossHeapSlot");
        let off = v.map_or(0, CrossHeapGc::offset);
        if off != 0 {
            target.register(off, holder);
        }
        // Write the identity before the offset: a reader seeing the new offset also sees the new identity (when both words are rewritten concurrently, a mismatched pair only reads as None)
        self.incarnation.store(target.incarnation(), Ordering::Release);
        self.off.store(off, Ordering::Release);
    }

    /// The target object; `target` is the mutator of the heap it points into. Empty references, references into another heap, and references into an old heap that died (its number reused by `target`) are all None.
    pub fn get<'b>(&self, target: &CrossHeapMutator<'b>) -> Option<CrossHeapGc<'b, B::Of<'b>>> {
        let off = self.off.load(Ordering::Acquire);
        if off == 0 || Inner::heap_of(off) != Some(target.index()) || self.incarnation.load(Ordering::Acquire) != target.incarnation() {
            return None;
        }
        // SAFETY: the target heap's entry keeps it alive (registered on write, rebuilt in group collections), and public heaps do not move objects; target proves we are inside the target heap's mutate
        Some(unsafe { CrossHeapGc::from_data(target.at(off)) })
    }

    /// Points at an object in `heap` (a heap with the same identity).
    pub fn points_into(&self, heap: &CrossHeap) -> bool {
        Inner::heap_of(self.off.load(Ordering::Acquire)) == Some(heap.id()) && self.incarnation.load(Ordering::Acquire) == heap.incarnation()
    }
}

// SAFETY: it is itself a cross-heap pointer slot (collections recognize the target heap by identity)
#[cfg(crossheap)]
unsafe impl<B: CrossHeapBrand> Shared for CrossHeapRef<'_, B> {
    fn edges(map: &mut EdgeMap, at: u32) {
        map.far(at);
    }
}

/// The single word inside cross-heap weak references and signatures: the high bits are the object data's public offset (without the low bits that granule alignment zeroes), the low bits the object's birth stamp; 0 = empty.
/// One word, so every read and write is a single atomic operation with no locking.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[cfg(crossheap)]
pub struct WeakWord(u64);

/// Offsets are granule aligned, so this many low bits are always 0
#[cfg(crossheap)]
const OFF_SHIFT: u32 = GRANULE.trailing_zeros();
/// Bits an offset needs once the low bits are dropped (the whole public region)
#[cfg(crossheap)]
const OFF_BITS: u32 = REGION.trailing_zeros() - OFF_SHIFT;
/// Bits left for the birth stamp (the stamp in the object header is 32 bits)
#[cfg(crossheap)]
const STAMP_BITS: u32 = if u64::BITS - OFF_BITS < u32::BITS { u64::BITS - OFF_BITS } else { u32::BITS };
/// The bits of the birth stamp kept in the word
#[cfg(crossheap)]
pub const STAMP_MASK: u32 = u32::MAX >> (u32::BITS - STAMP_BITS);

#[cfg(crossheap)]
impl From<(u64, u32)> for WeakWord {
    /// (public offset of the object data, birth stamp)
    fn from((off, stamp): (u64, u32)) -> Self {
        Self(((off >> OFF_SHIFT) << STAMP_BITS) | u64::from(stamp & STAMP_MASK))
    }
}

#[cfg(crossheap)]
impl From<u64> for WeakWord {
    fn from(raw: u64) -> Self {
        Self(raw)
    }
}

#[cfg(crossheap)]
impl From<WeakWord> for u64 {
    fn from(w: WeakWord) -> Self {
        w.0
    }
}

#[cfg(crossheap)]
impl<T> From<CrossHeapGc<'_, T>> for WeakWord {
    fn from(gc: CrossHeapGc<'_, T>) -> Self {
        // SAFETY: gc is a live object in a public heap, with its header before the data
        let stamp = unsafe { (*PHeader::of(gc.as_ptr().cast())).stamp };
        (gc.offset(), stamp).into()
    }
}

#[cfg(crossheap)]
impl WeakWord {
    /// The public offset of the object data (0 when empty).
    pub fn offset(self) -> u64 {
        (self.0 >> STAMP_BITS) << OFF_SHIFT
    }

    pub fn stamp(self) -> u32 {
        u32::try_from(self.0 & u64::from(STAMP_MASK)).expect("the stamp fits in the low 32 bits")
    }

    /// Which public heap it points into (None when empty or out of range).
    pub fn heap(self) -> Option<usize> {
        (self.0 != 0).then(|| Inner::heap_of(self.offset())).flatten()
    }
}

/// A weak reference to an object in some public heap, stored in a public heap object (or anywhere): one word (offset + birth stamp). It registers nothing in the target heap, keeps nothing alive,
/// and no collection looks at it, so even across heaps and processes it holds up nobody's collection. Once the target is collected it reads as None — also when a new object took its place
/// (the birth stamp does not match). Reading takes the target heap's mutator (inside its mutate the target cannot be collected right now).
///
/// Reads and writes are single atomic operations without locks: when several processes change it at once, a read returns one of the values written.
#[repr(transparent)]
#[cfg(crossheap)]
pub struct CrossHeapWeak<B: CrossHeapBrand> {
    word: AtomicU64,
    _brand: PhantomData<fn() -> B>,
}

#[cfg(crossheap)]
impl<B: CrossHeapBrand> Default for CrossHeapWeak<B> {
    fn default() -> Self {
        Self { word: AtomicU64::new(0), _brand: PhantomData }
    }
}

#[cfg(crossheap)]
impl<B: CrossHeapBrand> From<Option<CrossHeapGc<'_, B::Of<'_>>>> for CrossHeapWeak<B> {
    fn from(v: Option<CrossHeapGc<'_, B::Of<'_>>>) -> Self {
        Self { word: AtomicU64::new(v.map_or(0, |gc| WeakWord::from(gc).into())), _brand: PhantomData }
    }
}

#[cfg(crossheap)]
impl<B: CrossHeapBrand> From<CrossHeapGc<'_, B::Of<'_>>> for CrossHeapWeak<B> {
    fn from(gc: CrossHeapGc<'_, B::Of<'_>>) -> Self {
        Some(gc).into()
    }
}

#[cfg(crossheap)]
impl<B: CrossHeapBrand> CrossHeapWeak<B> {
    /// Point elsewhere (`None` clears it). Needs no target mutator: the offset and birth stamp are read from the object itself.
    pub fn set(&self, v: Option<CrossHeapGc<'_, B::Of<'_>>>) {
        self.word.store(v.map_or(0, |gc| WeakWord::from(gc).into()), Ordering::Release);
    }

    /// The target object; `target` is the mutator of the heap it lives in. Empty references, references into another heap, targets already collected (the place is empty or taken by a new object)
    /// and type mismatches are all None.
    pub fn get<'b>(&self, target: &CrossHeapMutator<'b>) -> Option<CrossHeapGc<'b, B::Of<'b>>> {
        let data = target.locate(self.word.load(Ordering::Acquire).into())?;
        // SAFETY: checked by locate: the data of an object alive in the target heap right now; it cannot be collected inside target's mutate
        target.is_type::<B::Of<'b>>(data).then(|| unsafe { CrossHeapGc::from_data(data) })
    }

    /// Which public heap it points into (it must be opened to read); None when empty.
    pub fn heap(&self) -> Option<usize> {
        WeakWord::from(self.word.load(Ordering::Acquire)).heap()
    }

    /// The word inside (to send elsewhere and restore with [`CrossHeapWeak::from_word`]).
    pub fn word(&self) -> u64 {
        self.word.load(Ordering::Acquire)
    }

    pub fn from_word(word: u64) -> Self {
        Self { word: AtomicU64::new(word), _brand: PhantomData }
    }
}

// SAFETY: a weak reference is not an edge: collections do not follow it
#[cfg(crossheap)]
unsafe impl<B: CrossHeapBrand> Shared for CrossHeapWeak<B> {
    const NO_EDGES: bool = true;
    fn edges(_: &mut EdgeMap, _: u32) {}
}

// SAFETY: no edges into the private heap
#[cfg(crossheap)]
unsafe impl<B: CrossHeapBrand> Trace for CrossHeapWeak<B> {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut Tracer<'_>) {}
}

/// The signature of a public heap object: it can be sent to another process (over a socket, relayed by dowin), which opens the same heap and finds the object with it.
/// It holds the weak reference word (offset + birth stamp), the target heap's identity and the object's type key (a hash of the type name, size and pointer map): 32 bytes.
///
/// A signature keeps nothing alive (as [`CrossHeapWeak`]): once the sender stops referencing the object it dies at the next collection, and looking it up by signature gives None.
/// The receiver can check it without knowing the Rust type ([`CrossHeapMutator::verify`]); to get it by type use [`CrossHeapMutator::resolve`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg(crossheap)]
pub struct Signature {
    word: WeakWord,
    incarnation: u64,
    key: [u64; 2],
}

/// Bytes in a serialized signature
#[cfg(crossheap)]
pub const SIGNATURE_BYTES: usize = 32;

#[cfg(crossheap)]
impl Signature {
    /// The public heap number of the object (it must be opened to read).
    pub fn heap(&self) -> Option<usize> {
        self.word.heap()
    }

    /// The identity of the object's heap (heap numbers are reused, identities are not).
    pub fn incarnation(&self) -> u64 {
        self.incarnation
    }

    /// The object's type key.
    pub fn key(&self) -> [u64; 2] {
        self.key
    }
}

#[cfg(crossheap)]
impl From<(WeakWord, u64, [u64; 2])> for Signature {
    fn from((word, incarnation, key): (WeakWord, u64, [u64; 2])) -> Self {
        Self { word, incarnation, key }
    }
}

#[cfg(crossheap)]
impl From<&Signature> for WeakWord {
    fn from(s: &Signature) -> Self {
        s.word
    }
}

#[cfg(crossheap)]
impl From<Signature> for [u8; SIGNATURE_BYTES] {
    fn from(s: Signature) -> Self {
        let mut out = [0; SIGNATURE_BYTES];
        for (chunk, v) in out.as_chunks_mut::<{ size_of::<u64>() }>().0.iter_mut().zip([s.word.into(), s.incarnation, s.key[0], s.key[1]]) {
            chunk.copy_from_slice(&u64::to_le_bytes(v));
        }
        out
    }
}

#[cfg(crossheap)]
impl From<&[u8; SIGNATURE_BYTES]> for Signature {
    fn from(bytes: &[u8; SIGNATURE_BYTES]) -> Self {
        let mut words = bytes.as_chunks::<{ size_of::<u64>() }>().0.iter().map(|c| u64::from_le_bytes(*c));
        let mut next = || words.next().expect("4 words");
        Self { word: next().into(), incarnation: next(), key: [next(), next()] }
    }
}

/// Rebrand a public heap type carrying `'p`: [`CrossHeapRoot<B>`] has no lifetime, and the current mutate's `'p` is put back on when reading it. Declare it with [`cross_heap_brand!`](crate::cross_heap_brand).
#[cfg(crossheap)]
pub trait CrossHeapBrand {
    type Of<'p>: Shared + 'p;
}

/// Declare a public heap brand type: `cross_heap_brand!(pub NodeB = for<'p> Node<'p>);`
#[cfg(crossheap)]
#[macro_export]
macro_rules! cross_heap_brand {
    ($(#[$meta:meta])* $vis:vis $name:ident = for<$l:lifetime> $ty:ty) => {
        $(#[$meta])*
        $vis struct $name;
        impl $crate::CrossHeapBrand for $name {
            type Of<$l> = $ty;
        }
    };
}

/// A handle to a public heap object: it takes an entry in the entry table, and the object is not collected while it lives. Carries no lifetime, so it can go into private heap objects and across threads.
/// Create it with [`CrossHeapMutator::root`] and read it with [`CrossHeapRoot::get`].
#[cfg(crossheap)]
pub struct CrossHeapRoot<B: CrossHeapBrand> {
    /// This process's side of the heap: the Arc keeps the window mapped while handles exist
    heap: Arc<Inner>,
    entry: u32,
    /// The target object's data (public heaps do not move objects)
    data: NonNull<u8>,
    _brand: PhantomData<fn() -> B>,
}

// SAFETY: only shared references to B::Of are given out (Shared: Send + Sync); the entry table is atomic
#[cfg(crossheap)]
unsafe impl<B: CrossHeapBrand> Send for CrossHeapRoot<B> {}
// SAFETY: as above
#[cfg(crossheap)]
unsafe impl<B: CrossHeapBrand> Sync for CrossHeapRoot<B> {}

#[cfg(crossheap)]
impl<B: CrossHeapBrand> CrossHeapRoot<B> {

    /// The target object; `m` must be the target heap's mutator.
    pub fn get<'p>(&self, m: &CrossHeapMutator<'p>) -> CrossHeapGc<'p, B::Of<'p>> {
        assert!(m.is(&self.heap), "this CrossHeapRoot does not point into this heap");
        // SAFETY: the entry keeps the object alive and public heaps do not move objects; m proves we are inside this heap's mutate
        unsafe { CrossHeapGc::from_data(self.data.as_ptr()) }
    }

    /// Whether the target object is in `heap`.
    pub fn points_into(&self, heap: &CrossHeap) -> bool {
        heap.is(&self.heap)
    }

    /// The entry number: the same wherever the heap is mapped, so elsewhere [`CrossHeapMutator::find`] gets the same object with it.
    pub fn index(&self) -> u32 {
        self.entry
    }

    /// Turn this entry into a root owned by no process and return the entry number: it is not returned when this process exits (or is killed), and after a restart the main process
    /// continues with the entry number ([`CrossHeapMutator::find`]); return it with [`CrossHeapMutator::release`] when no longer needed.
    pub fn persist(self) -> u32 {
        let me = core::mem::ManuallyDrop::new(self);
        me.heap.disown_entry(me.entry);
        let entry = me.entry;
        // SAFETY: me is never dropped again, and the Arc is taken out only here and released right away
        drop(unsafe { core::ptr::read(&raw const me.heap) });
        entry
    }
}

/// Operations for this crate only: the trait lives in a private module and is not exported, so it is unreachable outside the crate.
#[cfg(crossheap)]
pub trait CrossHeapRootInternal {
    fn new(heap: Arc<Inner>, entry: u32, data: NonNull<u8>) -> Self;
}

#[cfg(crossheap)]
impl<B: CrossHeapBrand> CrossHeapRootInternal for CrossHeapRoot<B> {
    fn new(heap: Arc<Inner>, entry: u32, data: NonNull<u8>) -> Self {
        Self { heap, entry, data, _brand: PhantomData }
    }
}


#[cfg(crossheap)]
impl<B: CrossHeapBrand> Drop for CrossHeapRoot<B> {
    fn drop(&mut self) {
        self.heap.release_entry(self.entry);
    }
}

// SAFETY: the handle has no edges into the private heap (it is itself a root of the public heap)
#[cfg(crossheap)]
unsafe impl<B: CrossHeapBrand> Trace for CrossHeapRoot<B> {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut Tracer<'_>) {}
}
