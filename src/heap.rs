//! Two kinds of heap: the private [`Heap`] / [`Mutator`] and the public [`CrossHeap`] / [`CrossHeapMutator`] (on a shared-memory file, multi-process), sharing the allocation buffer [`Tlab`].
//!
//! A heap: several threads can [`Heap::mutate`] at once (each bump-allocating on its own).
//!
//! Two ways to collect:
//! - [`Heap::collect`] (`&mut self`): the borrow rules exclude mutators, and marking + sweeping happen in this one call
//! - [`Heap::mark_concurrently`] (`&self`): one short handshake at the start (wait until no thread is inside mutate, snapshot the roots); afterwards mutators keep running while
//!   collector threads mark concurrently; on the mutator side, `Slot` writes go through a snapshot-at-the-beginning (SATB) write barrier and new objects are marked with this round (black allocation).
//!   The [`Heap::collect`] that follows only drains the grey objects gathered by the write barrier and then sweeps
//!
//! The handshake word: the top bit is the "start marking" request, the low bits count threads inside mutate. While the request is set, threads entering mutate wait for it to clear.
//!
//! Public heaps: heaps on a shared-memory file. All heap state (superblock, type table, entry table, block state table, blocks) lives in the window; the process side holds only caches.
//!
//! Phase one (single process): several threads mutate and bump-allocate at once; collection is stop-the-world parallel marking + parallel sweeping without moving objects. The handshake word is in the superblock,
//! so a heap mapped twice (two regions in one process, or across processes) is still mutually exclusive.
#[cfg(crossheap)]
use core::any::type_name;
use core::cell::Cell;
use core::marker::PhantomData;
use core::num::{NonZeroU32, NonZeroUsize};
use core::ptr::{NonNull, null_mut};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
#[cfg(crossheap)]
use core::time::Duration;
#[cfg(crossheap)]
use std::io;
#[cfg(crossheap)]
use std::fs::File;
#[cfg(crossheap)]
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crossbeam_deque::{Injector, Steal};
use crossbeam_utils::{Backoff, CachePadded};

use crate::{Gc, Trace};
use crate::handle::{GcInternal, Invariant, RawSlot, RootTable};
#[cfg(crossheap)]
use crate::handle::{EdgeMap, CrossHeapRoot, CrossHeapBrand, CrossHeapGc, CrossHeapGcInternal, CrossHeapRootInternal, STAMP_MASK, Shared, Signature, WeakWord};
use crate::layout::{BLOCK, Block, Pages, WARM_CYCLES, YOUNG_COPY_DEN, YOUNG_COPY_NUM, LINE, EVACUATE_LIVE, EVACUATE_MAX, EVACUATE_MIN, GRANULE, GROWTH, HEADER, Header, LARGE, MAX_EPOCH, MIN_SPARE_BLOCKS, MIN_TRIGGER, OVERFLOW_TRIES, FRAG_NUM, FRAG_DEN, YOUNG_PER_FULL, YOUNG_PER_FULL_RC};
#[cfg(crossheap)]
use crate::layout::{Core, State};
#[cfg(crossheap)]
use crate::layout::{ATTACH_CAP, ATTACH_OFF, Attachment, BlockState, Entry, Kind, MAP_CAP, PAGE, PHEADER, PHeader, Segment, Super, TYPE_CAP, TypeEntry, WindowLayout};
use crate::mark::Marking;
#[cfg(crossheap)]
use crate::mark::{Marker, Member};
#[cfg(crossheap)]
use crate::shm::{Liveness, Process, REGION, Region, Registry, Space, WINDOW, WINDOWS};
use crate::sweep::{Large, Outcome, Releaser, Sink, Sweeper};
use crate::sync::{Pool, SegVec};
#[cfg(crossheap)]
use crate::sync::{FutexWord, Retry, Stack};

pub struct Heap {
    /// Collect with at most this many threads
    threads: NonZeroUsize,
    /// Collector thread pool: threads start at the first collection
    pool: OnceLock<Pool>,
    /// The epoch of the last completed marking (from 1; lines with older marks are free)
    epoch: u32,
    /// The epoch being marked concurrently; 0 = none
    marking: AtomicU32,
    /// The handshake word: the [`REQUEST`] bit + threads inside mutate
    gate: CachePadded<AtomicU64>,
    /// Grey queue: roots and old values recorded by the write barrier (concurrent marking spans several calls, so it lives in the heap)
    grey: Injector<usize>,
    /// Remembered set: addresses of slots written with young objects since the last collection (young collections start from these slots; cleared after any collection)
    remembered: Injector<usize>,
    /// Reference count log: counted slots written this round (each slot once per round)
    rc_log: Injector<usize>,
    /// Pending decrements (queued by the young collection's pause and processed in the background by sweep_concurrently)
    decs: Injector<usize>,
    /// Blocks in which objects were released in the background (lines recomputed at the start of the next pause)
    dirty: Injector<Block>,
    /// Reference counts are trustworthy: true after a stop-the-world full collection recounts; false once concurrent marking starts (black-allocated objects did not count their edges) until the next recount
    rc_valid: AtomicBool,
    /// Parity of the current count (flipped on recount, invalidating old counts at once; concurrent marking flips it at the handshake)
    rc_parity: AtomicBool,
    /// Objects black-allocated during concurrent marking (header addresses): marking never scans them, so their edges are counted at the end
    black: Injector<usize>,
    /// Roots counted in the previous round (decremented in this round, deferred)
    rc_roots: Vec<usize>,
    /// Young collections since the last full collection
    young_since_full: usize,
    /// Young collections in total
    young_collections: u64,
    /// Collections so far (allocation buffers use it to tell whether they are stale)
    collections: u64,
    /// All blocks
    blocks: SegVec<Block>,
    /// Swept blocks with free lines (including empty ones); allocation buffers take from here first
    available: Injector<Block>,
    large: SegVec<Large>,
    /// The previous round's sweep (runs concurrently with mutators)
    sweeper: Sweeper,
    /// Finalization of dead objects runs in parallel on all collector threads (false: only on the thread calling collect / sweep); see [`Heap::set_parallel_drop`]
    parallel_drop: AtomicBool,
    /// How young collections move surviving young objects (a [`YoungEvacuation`] value)
    young_evacuation: AtomicU8,
    /// [`Heap::shrink`] in progress: this round's sweep keeps no spare empty blocks and gives free lines back to the system
    shrinking: AtomicBool,
    /// Where blocks come from (Drop gives every block back first, and only then is it unmapped)
    pages: Pages,
    roots: Arc<RootTable>,
    /// Weak table: slots of [`crate::Weak`] (not roots; every collection clears the unmarked ones after marking)
    weaks: Arc<RootTable>,
    /// Bytes newly taken since the last collection (counted by blocks / large objects)
    allocated: AtomicUsize,
    /// Blocks taken in each of the last [`WARM_CYCLES`] rounds (caps the warm pool of [`Pages`])
    recent: [usize; WARM_CYCLES],
    /// This collection was started by [`Heap::collect_if_needed`] (automatic, by allocation volume): only these keep a warm pool of [`Pages`]
    automatic: bool,
    /// Bytes taken before the last collection when it was a young collection (surviving young bytes after sweeping divided by it is the survival rate); 0 after a full collection
    young_allocated: usize,
    /// Blocks / bytes evacuated by the last collection
    evacuated: (usize, usize),
    /// An allocation buffer reused by [`Heap::mutate`]
    spare: AtomicPtr<Tlab>,
    /// The cross-heap used as the old generation ([`Heap::attach_cross_heap`])
    #[cfg(crossheap)]
    cross: Option<CrossHeap>,
    /// A private object was written into a cross-heap object since the last young collection
    #[cfg(crossheap)]
    promotion_pending: AtomicBool,
}

/// Whether young collections move surviving young objects into compact blocks ([`Heap::set_young_evacuation`]).
///
/// Young collections do not scan old objects: the only edges from old objects to young ones are the remembered slots (recorded when a [`crate::Slot`] is written).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum YoungEvacuation {
    /// Do not move: surviving young objects are promoted in place
    Off,
    /// Default. Objects pointed to directly by old objects (targets of remembered slots) or from other heaps through [`crate::Far`] are promoted in place, the rest move:
    /// moved objects are referenced only by young objects and roots, which are all relocated (`Trace::relocate`), so not a byte of an old object changes
    #[default]
    SkipRemembered,
    /// Remembered slots are rewritten to the new address as well, so objects pointed to directly by old objects also move. Usable only when old objects store the address nowhere but in a `Slot`
    /// (for example, a copy of the address kept next to the `Slot` would not be updated)
    Full,
}

impl From<u8> for YoungEvacuation {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Off,
            2 => Self::Full,
            _ => Self::SkipRemembered,
        }
    }
}

/// Heap statistics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Live bytes after the last collection (including headers)
    pub live: usize,
    /// Bytes newly taken since the last collection
    pub allocated: usize,
    pub blocks: usize,
    pub large: usize,
    pub collections: u64,
    /// How many of them were young collections
    pub young_collections: u64,
    /// Blocks evacuated and bytes moved by the last collection
    pub evacuated_blocks: usize,
    pub evacuated_bytes: usize,
}

impl Default for Heap {
    fn default() -> Self {
        Self::new(std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN))
    }
}

impl Heap {
    /// Collect with at most `threads` threads.
    pub fn new(threads: NonZeroUsize) -> Self {
        Self {
            threads,
            pool: OnceLock::new(),
            epoch: 1,
            marking: AtomicU32::new(0),
            gate: CachePadded::new(AtomicU64::new(0)),
            grey: Injector::new(),
            remembered: Injector::new(),
            rc_log: Injector::new(),
            decs: Injector::new(),
            dirty: Injector::new(),
            rc_valid: AtomicBool::new(false),
            rc_parity: AtomicBool::new(false),
            black: Injector::new(),
            rc_roots: Vec::new(),
            young_since_full: 0,
            young_collections: 0,
            collections: 0,
            blocks: SegVec::default(),
            available: Injector::new(),
            large: SegVec::default(),
            sweeper: Sweeper::default(),
            parallel_drop: AtomicBool::new(true),
            young_evacuation: AtomicU8::new(YoungEvacuation::default() as u8),
            shrinking: AtomicBool::new(false),
            pages: Pages::default(),
            roots: Arc::default(),
            weaks: Arc::default(),
            allocated: AtomicUsize::new(0),
            recent: [0; WARM_CYCLES],
            young_allocated: 0,
            automatic: false,
            evacuated: (0, 0),
            spare: AtomicPtr::new(null_mut()),
            #[cfg(crossheap)]
            cross: None,
            #[cfg(crossheap)]
            promotion_pending: AtomicBool::new(false),
        }
    }

    /// A private object was written into a cross-heap object since the last young collection: it is visible to other processes only after the next
    /// young collection promotes it (call [`Heap::collect_young`] when a turn ends).
    #[cfg(crossheap)]
    pub fn promotion_pending(&self) -> bool {
        self.promotion_pending.load(Ordering::Acquire)
    }

    /// Attach a cross-heap created by this process as the old generation: from then on, every young collection moves surviving shareable young objects (allocated with [`Mutator::alloc_shared`])
    /// into `cross` and rewrites every place in the private heap that points at them; a full collection ([`Heap::collect`]) collects `cross` together with the private heap —
    /// edges from the private heap are its roots. From now on `cross` is collected only by this private heap (its `collect` does nothing when called elsewhere).
    ///
    /// `cross` must be created in the fixed-address region ([`crate::CrossHeapSpace::reserve_fixed`]) so absolute addresses stored in objects are valid in every process.
    /// Shareable objects may only point at shareable objects: promotion panics on an edge to a non-shareable object. Concurrent marking and `collect_group` are not supported once attached.
    #[cfg(crossheap)]
    pub fn attach_cross_heap(&mut self, cross: &CrossHeap) -> io::Result<()> {
        let inner = &cross.0;
        if !inner.space.is_fixed() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a cross-heap used as the old generation must be created in the fixed-address region (CrossHeapSpace::reserve_fixed)"));
        }
        if inner.sup().owner_pid.load(Ordering::Acquire) != Process::current().pid {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "only a cross-heap created by this process can be attached"));
        }
        if self.cross.is_some() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "a cross-heap is already attached"));
        }
        inner.sup().owner_only.store(1, Ordering::Release);
        self.cross = Some(CrossHeap(Arc::clone(inner)));
        Ok(())
    }

    /// The owner collects the cross-heap too (private marking done, not yet swept): the roots are the cross-heap's own entries, the cross-heap edges traced by the private heap (`crossing`)
    /// and private root-table slots pointing into the cross-heap; afterwards private weak-table slots pointing at dead cross-heap objects are cleared.
    #[cfg(crossheap)]
    fn collect_cross(&mut self, crossing: &Injector<usize>) {
        let Some(cross) = &self.cross else { return };
        let mut extra = Self::drain(crossing);
        // SAFETY: &mut self ⇒ no mutators
        unsafe { self.roots.crossing(|p| extra.push(p.addr())) };
        extra.sort_unstable();
        extra.dedup();
        let epoch = cross.0.collect_owned(&extra, *self.shrinking.get_mut());
        // SAFETY: no mutators
        unsafe { self.weaks.clear_crossed(|p| Inner::survived(p, epoch)) };
    }

    /// Work on the heap; can be called on several threads at once. Objects allocated in the closure survive the next collection only if they hang from a [`crate::Root`] (directly or indirectly).
    pub fn mutate<R>(&self, f: impl for<'gc> FnOnce(&Mutator<'gc>) -> R) -> R {
        let spare = self.spare.swap(null_mut(), Ordering::AcqRel);
        let mut tlab = if spare.is_null() {
            Box::default()
        } else {
            // SAFETY: spare only ever holds Tlabs from Box::into_raw, and after the swap it is ours alone
            unsafe { Box::from_raw(spare) }
        };
        let r = self.mutate_in(&mut tlab, f);
        let tlab = Box::into_raw(tlab);
        if self.spare.compare_exchange(null_mut(), tlab, Ordering::AcqRel, Ordering::Relaxed).is_err() {
            // SAFETY: it was not put back, so it is still ours alone
            drop(unsafe { Box::from_raw(tlab) });
        }
        r
    }

    /// As [`Heap::mutate`], but with the caller's own allocation buffer (a thread that mutates repeatedly keeps it, saving a new block every time).
    pub fn mutate_in<R>(&self, tlab: &mut Tlab, f: impl for<'gc> FnOnce(&Mutator<'gc>) -> R) -> R {
        let _inside = Inside::enter(self);
        tlab.adopt((self.id(), self.collections));
        // Concurrent marking starts only with no mutators and ends only with the heap held exclusively, so it does not change within one mutate
        let marking = NonZeroU32::new(self.marking.load(Ordering::Acquire));
        // Even if the closure panics, the gathered start bits must be written: if they were missed, the next pause recomputing lines would take lines of live objects as free
        let tlab = FlushStarts(tlab);
        f(&Mutator { heap: self, tlab: tlab.0, marking, _brand: PhantomData })
    }

    fn pool(&self) -> &Pool {
        self.pool.get_or_init(|| Pool::new(self.threads))
    }

    /// Concurrent marking: a short handshake at the start (wait until no thread is inside mutate, snapshot the roots); afterwards other threads mutate as usual
    /// while this thread, using the collector pool, marks everything reachable from the roots (what the write barrier gathers meanwhile is left to the next [`Heap::collect`]).
    /// Does nothing and returns false when a round is already marking.
    ///
    /// Do not call it inside this heap's mutate (it would wait forever for itself to leave).
    pub fn mark_concurrently(&self) -> bool {
        #[cfg(crossheap)]
        assert!(self.cross.is_none(), "concurrent marking is not supported with a cross-heap attached");
        // The previous round judges death as "epoch ≠ this round", so it must be fully swept before the new round marks live objects with a newer epoch
        self.sweep_concurrently();
        let epoch = self.next_epoch();
        // The request bit doubles as a lock: whoever sets it first starts; if marking is already running, withdraw the request and return
        if self.gate.fetch_or(REQUEST, Ordering::AcqRel) & REQUEST != 0 {
            return false;
        }
        if self.marking.load(Ordering::Acquire) != 0 {
            self.gate.fetch_and(!REQUEST, Ordering::AcqRel);
            return false;
        }
        let backoff = Backoff::new();
        while self.gate.load(Ordering::Acquire) & !REQUEST != 0 {
            backoff.snooze();
        }
        // The epoch must be published only after mutate is empty: otherwise a mutator that entered first and sees "marking" allocates black,
        // while an earlier mutator without the write barrier could still hang things under that black object, which is not traced again after the snapshot ⇒ missed marks
        self.marking.store(epoch, Ordering::Release);
        // Reference counts are recounted along the way: flip the parity (old counts are void) and increment every edge while scanning. Counts are untrustworthy, and nothing is released by count,
        // until the end fills in the rest (edges of black objects, slots rewritten during marking)
        let parity = !self.rc_parity.fetch_xor(true, Ordering::AcqRel);
        self.rc_valid.store(false, Ordering::Release);
        // Logs from before the handshake recorded "last counted" under the old parity: if the object is alive it gets scanned and switched to counting the current value; if it died, settling it at the end
        // would subtract the old value from counts made by other edges in the new round ⇒ drop them (logged flags left on slots are cleared when scanning recounts them)
        drop(Self::drain(&self.rc_log));
        // SAFETY: the request is set and nobody is inside mutate ⇒ no mutators (dropping root handles is unaffected)
        let roots = unsafe { self.roots.settle() };
        Marking::seed(&self.grey, &roots, epoch);
        self.gate.fetch_and(!REQUEST, Ordering::AcqRel);
        let pool = self.pool();
        Marking::new(pool.threads(), epoch, &self.grey).counting(Some(parity)).logging(&self.rc_log).run(pool);
        true
    }

    /// Slots rewritten during concurrent marking: new value +1, last counted value −1 (the scan may have counted before or after the write; settling is right either way).
    /// Decrements apply only to objects alive this round (dead ones are about to be swept and must not be touched) and go on the queue for the background. SAFETY: see the caller (heap held exclusively).
    fn settle_concurrent_log(&mut self, epoch: u32) {
        let parity = *self.rc_parity.get_mut();
        for addr in Self::drain(&self.rc_log) {
            // SAFETY: the logged slot's object was reachable when written; the heap is held exclusively and sweeping has not started, so the memory is there
            let slot = unsafe { &*core::ptr::with_exposed_provenance::<RawSlot>(addr) };
            let (cur, prev) = slot.recount();
            if let Some(cur) = cur {
                // SAFETY: the object the slot points at now is still there (sweeping has not started)
                unsafe { &*cur }.rc_inc(parity);
            }
            // SAFETY: as above
            if let Some(prev) = prev
                && unsafe { &*prev }.is_marked(epoch)
            {
                self.decs.push(prev.expose_provenance());
            }
        }
    }

    /// The next round's epoch.
    fn next_epoch(&self) -> u32 {
        self.epoch.checked_add(1).filter(|&e| e <= MAX_EPOCH).expect("out of epochs")
    }

    /// Pick evacuation candidates among the available blocks unused since sweeping: sparse blocks with less than [`EVACUATE_LIVE`] live bytes (and not empty), at most
    /// [`EVACUATE_MAX`]; evacuate only with at least [`EVACUATE_MIN`]. The other blocks go to `targets` as destinations. Returns the number of blocks to evacuate (0 = none).
    fn pick_evacuation(available: &[Block], targets: &Injector<Block>) -> usize {
        let live = |b: &Block| b.live_since_sweep().filter(|&live| live > 0 && live < EVACUATE_LIVE);
        let mut sparse: Vec<(usize, Block)> = available.iter().filter_map(|b| live(b).map(|l| (l, *b))).collect();
        // Move the sparsest first (least to move, most freed); not enough candidates means no evacuation
        sparse.sort_unstable_by_key(|&(l, _)| l);
        sparse.truncate(EVACUATE_MAX);
        if sparse.len() < EVACUATE_MIN {
            sparse.clear();
        }
        for &(_, b) in &sparse {
            b.set_evacuate();
        }
        for &b in available {
            if !b.is_evacuating() {
                targets.push(b);
            }
        }
        sparse.len()
    }

    /// Write barrier: an old value overwritten during concurrent marking is marked (with the lines it spans) and put on the grey queue if not yet marked (preserving the snapshot).
    /// Objects on the grey queue are already marked and are only traced when taken out, so marking must happen here first.
    fn remember(&self, old: *const Header, epoch: u32) {
        // SAFETY: old comes from a slot of this heap (alive at the snapshot) or from the weak table (alive since the last pause, count stuck), so it is not collected this round
        let h = unsafe { &*old };
        if h.mark(epoch) {
            if !h.is_large() {
                Block::containing(old.cast()).mark_lines(old.cast(), h.bytes(), epoch);
            }
            self.grey.push(old.expose_provenance());
        }
    }

    /// Take a block for an allocation buffer: a swept one first; otherwise take an unswept one and sweep it on the spot (lazy sweeping), using it if possible; finally open a new one.
    fn acquire(&self) -> Block {
        self.allocated.fetch_add(BLOCK, Ordering::Relaxed);
        loop {
            match self.available.steal() {
                Steal::Success(b) => {
                    b.unlist();
                    b.reused();
                    return b;
                }
                Steal::Retry => continue,
                Steal::Empty => {}
            }
            match self.sweeper.sweep_next_block() {
                Some(Outcome::Usable(b)) => {
                    b.reused();
                    self.blocks.push(b);
                    self.sweeper.finished();
                    return b;
                }
                Some(other) => {
                    self.block(other);
                    self.sweeper.finished();
                }
                None => break,
            }
        }
        self.fresh()
    }

    /// Take a block for overflow allocation: look among available blocks for one with a hole fitting `bytes` (at most [`OVERFLOW_TRIES`]; those that do not fit go back on the available queue for small objects),
    /// otherwise open a new block.
    fn acquire_for(&self, bytes: usize) -> Block {
        self.allocated.fetch_add(BLOCK, Ordering::Relaxed);
        let mut rejected = Vec::new();
        let mut found = None;
        while rejected.len() < OVERFLOW_TRIES {
            match self.available.steal() {
                Steal::Success(b) if b.has_hole(bytes, self.epoch) => {
                    found = Some(b);
                    break;
                }
                Steal::Success(b) => rejected.push(b),
                Steal::Retry => {}
                Steal::Empty => break,
            }
        }
        for b in rejected {
            self.available.push(b);
        }
        match found {
            Some(b) => {
                b.unlist();
                b.reused();
                b
            }
            None => self.fresh(),
        }
    }

    fn fresh(&self) -> Block {
        let b = Block::new(&self.pages);
        self.blocks.push(b);
        b
    }

    /// How young collections move surviving young objects (default [`YoungEvacuation::SkipRemembered`]). Moved objects change address:
    /// structures indexed by address must update in `Trace::relocate`, and as long as they point at young objects they must themselves be young
    /// (young collections do not scan old objects; for example, move into a newly allocated object when changing).
    pub fn set_young_evacuation(&self, mode: YoungEvacuation) {
        self.young_evacuation.store(mode as u8, Ordering::Relaxed);
    }

    /// The previous round's sweep is not finished yet (mutators sweep along when they need blocks, or call [`Heap::sweep_concurrently`]).
    /// Where dead objects are finalized: true (default) = in parallel on all collector threads; false = only on the thread calling [`Heap::collect`] / [`Heap::collect_young`] /
    /// [`Heap::sweep_concurrently`] (marking, judging death, clearing start bits and computing lines stay parallel; blocks a mutator sweeps along are still finalized on its own thread).
    ///
    /// Turn it off when destructors free memory allocated on mutator threads (`Box`, `String`…): freeing across threads makes the allocator contend (glibc's arena locks),
    /// often slower than finalizing on one thread.
    pub fn set_parallel_drop(&self, on: bool) {
        self.parallel_drop.store(on, Ordering::Relaxed);
    }

    pub fn is_sweeping(&self) -> bool {
        !self.sweeper.is_done()
    }

    /// Concurrent sweeping: on the collector pool, first finish the reference count decrements (cascading releases), then sweep the previous round's remaining blocks and large objects
    /// (mutators may run meanwhile and also sweep along); on return all of the previous round's dead objects are finalized.
    pub fn sweep_concurrently(&self) {
        let pool = self.pool();
        if !self.decs.is_empty() {
            Releaser::new(&self.decs, &self.dirty, pool.threads(), self.epoch, self.rc_parity.load(Ordering::Acquire), self.parallel_drop.load(Ordering::Relaxed)).run(pool);
        }
        self.sweeper.finish(pool, self, self.parallel_drop.load(Ordering::Relaxed));
    }

    fn alloc_large(&self, bytes: usize) -> *mut u8 {
        // SAFETY: Large::alloc never returns null (failure goes to handle_alloc_error)
        let p = unsafe { NonNull::new_unchecked(Large::alloc(bytes)) };
        self.large.push(Large(p.cast()));
        self.allocated.fetch_add(bytes, Ordering::Relaxed);
        p.as_ptr()
    }

    pub fn stats(&self) -> Stats {
        Stats {
            live: self.sweeper.live(),
            allocated: self.allocated.load(Ordering::Relaxed),
            blocks: self.blocks.len(),
            large: self.large.len(),
            collections: self.collections,
            young_collections: self.young_collections,
            evacuated_blocks: self.evacuated.0,
            evacuated_bytes: self.evacuated.1,
        }
    }

    /// Collect once enough was newly taken since the last collection: usually a young collection (dead old objects are reclaimed by reference counting); tracing full collections run rarely, only for cycles
    /// and stuck counts — one per [`YOUNG_PER_FULL_RC`] young collections while counts are trustworthy; while they are not (after concurrent marking)
    /// one per [`YOUNG_PER_FULL`], to recount soon. Returns true if it collected.
    pub fn collect_if_needed(&mut self) -> bool {
        let live = self.sweeper.live().max(MIN_TRIGGER);
        let due = *self.allocated.get_mut() >= live * GROWTH;
        if due {
            let every = if *self.rc_valid.get_mut() { YOUNG_PER_FULL_RC } else { YOUNG_PER_FULL };
            // Fragmented: blocks take FRAG_RATIO times more than "live + what this round may allocate" ⇒ a full collection (with evacuation) without waiting for its turn
            let fragmented = self.blocks.len() * BLOCK * FRAG_DEN > live * (1 + GROWTH) * FRAG_NUM;
            self.automatic = true;
            if self.young_since_full < every && !fragmented {
                self.collect_young();
            } else {
                self.collect();
            }
            self.automatic = false;
        }
        due
    }

    /// Young collection: traces only objects allocated since the last collection (mark epoch not the last completed one), starting from the roots and remembered slots; it stops at old objects.
    /// Young objects reached are marked with the last completed epoch (promoted to old), then only blocks taken for allocation since the last collection are swept: unreached young objects are finalized
    /// and their lines freed. Old objects are not touched (dead old objects wait for the next full collection). On return all dead young objects are finalized.
    ///
    /// When concurrent marking is in progress, finishes it instead (a full collection).
    pub fn collect_young(&mut self) {
        self.finish_young();
        self.sweep_concurrently();
    }

    /// The pause of a young collection: mark and settle reference count increments; decrements and sweeping are left to a later [`Heap::sweep_concurrently`]
    /// (concurrent with mutators; mutators also sweep along when they need blocks).
    pub fn finish_young(&mut self) {
        if *self.marking.get_mut() != 0 {
            self.finish_marking();
            return;
        }
        self.sweep_concurrently();
        // Blocks in which objects were released in the background: recompute lines from the old objects still there (young objects' lines are marked by the trace below, dead ones reclaimed by sweeping).
        // Blocks taken for allocation may hold unswept dead young objects (start bits still set): only recompute lines without putting them back on the available queue, and let the young sweep classify them
        let epoch = self.epoch;
        let mut dirty = Self::drain(&self.dirty);
        dirty.sort_unstable_by_key(|b| b.line(0).addr());
        dirty.dedup();
        for b in dirty {
            // SAFETY: exclusive; the objects marked in the start bitmap are not finalized yet
            if unsafe { b.recompute_lines(epoch) } && b.list() {
                self.available.push(b);
            }
        }
        let epoch = self.epoch;
        let mode = self.young_mode();
        // With a cross-heap attached: every shareable young object must move over (including ones pointed to directly by old objects, with remembered slots rewritten)
        #[cfg(crossheap)]
        let mode = if self.cross.is_some() { YoungEvacuation::Full } else { mode };
        #[cfg(crossheap)]
        let cross = self.cross.as_ref().map(|c| Arc::clone(&c.0));
        // Stay inside the cross-heap's door while promoting (like the other threads of this process and the mutators of other processes)
        #[cfg(crossheap)]
        let _inside = cross.as_deref().map(Gate::enter);
        let remembered = Self::drain(&self.remembered);
        #[cfg(crossheap)]
        self.promotion_pending.store(false, Ordering::Release);
        let counting = *self.rc_valid.get_mut();
        let parity = *self.rc_parity.get_mut();
        let pool = self.pool.get_or_init(|| Pool::new(self.threads));
        let workers = pool.sized(self.blocks.len() + self.large.len());
        // While a promoted object is scanned, each of its edges is incremented (young objects' edges are counted from promotion on)
        let mark = Marking::new(workers, epoch, &self.grey).counting(counting.then_some(parity));
        let moved = AtomicUsize::new(0);
        if mode == YoungEvacuation::Off {
            // SAFETY: &mut self ⇒ no mutators
            Marking::seed(&self.grey, &unsafe { self.roots.settle() }, epoch);
            Marking::seed_slots(&self.grey, &remembered, epoch);
            mark.run(pool);
        } else {
            // Every block taken for allocation since the last collection is evacuated (old objects in it do not move: they were marked long ago this round); objects move into available and new blocks
            let young: Vec<Block> = self.blocks.take();
            for &b in &young {
                if b.live_since_sweep().is_none() {
                    b.set_evacuate();
                }
            }
            self.blocks.extend(young);
            // Young objects pointed to directly from other heaps are marked in place first (not moved)
            let mut pinned = Vec::new();
            // SAFETY: no mutators
            unsafe { self.roots.pinned(|h| pinned.push(h)) };
            Marking::seed(&self.grey, &pinned, epoch);
            let slots: &[usize] = if mode == YoungEvacuation::Full { &remembered } else {
                // Objects pointed to directly by old objects are marked in place too
                Marking::seed_slots(&self.grey, &remembered, epoch);
                &[]
            };
            let mark = mark.evacuating(&self.roots, &self.available, &self.blocks, &self.pages, &moved, epoch).relocating_slots(slots);
            #[cfg(crossheap)]
            let mark = mark.promoting(cross.as_deref());
            mark.run(pool);
        }
        // Reference counts (when trustworthy) are settled after marking (objects may have moved, so the new addresses are used): current roots +1, logged slots settle "new value +1,
        // last counted value −1", and the previous round's roots are decremented now (deferred)
        // SAFETY: &mut self ⇒ no mutators
        let roots = unsafe { self.roots.settle() };
        let mut decs = Vec::new();
        if counting {
            for &root in &roots {
                // SAFETY: roots point at live objects
                unsafe { &*root }.rc_inc(parity);
            }
            decs.append(&mut self.rc_roots);
        } else {
            self.rc_roots.clear();
        }
        for addr in Self::drain(&self.rc_log) {
            // SAFETY: the logged slot's object was reachable when written and is not freed before this collection
            let slot = unsafe { &*core::ptr::with_exposed_provenance::<RawSlot>(addr) };
            if counting {
                let (cur, prev) = slot.recount();
                if let Some(cur) = cur {
                    // SAFETY: the slot currently points at a live object
                    unsafe { &*cur }.rc_inc(parity);
                }
                decs.extend(prev.map(<*const Header>::expose_provenance));
            } else {
                slot.unlog();
            }
        }
        if counting {
            self.rc_roots = roots.iter().map(|r| r.expose_provenance()).collect();
            // All increments are done; decrements go to the background (sweep_concurrently)
            for addr in decs {
                self.decs.push(addr);
            }
        }
        // SAFETY: no mutators; marking (including young evacuation) is done and sweeping has not started
        unsafe { self.weaks.clear_unreached(epoch, parity) };
        // Sweep only blocks taken for allocation since the last collection (young objects can only be there); untouched old blocks go back as they are
        let (young, old): (Vec<Block>, Vec<Block>) = self.blocks.take().into_iter().partition(|b| b.live_since_sweep().is_none());
        self.blocks.extend(old);
        let large = self.large.take();
        let spare = (*self.allocated.get_mut() / BLOCK).max(MIN_SPARE_BLOCKS);
        self.warm_for_next_cycle(false);
        self.sweeper.start(epoch, young, large, spare, false);
        self.evacuated = (0, moved.into_inner());
        self.young_allocated = *self.allocated.get_mut();
        *self.allocated.get_mut() = 0;
        self.collections += 1;
        self.young_collections += 1;
        self.young_since_full += 1;
    }

    /// How this young collection moves objects: as configured, except that when the last young collection kept nearly everything (young objects that tend to live a while, such as batches built and then
    /// dropped whole), they are promoted in place — moving them would only copy what was just allocated, and the blocks are not fragmented (survivors are packed); fragmentation is left to full-collection evacuation.
    /// Call after the previous round is swept (so the live byte count is complete).
    fn young_mode(&mut self) -> YoungEvacuation {
        let allocated = core::mem::take(&mut self.young_allocated);
        let mode = YoungEvacuation::from(*self.young_evacuation.get_mut());
        if allocated > 0 && self.sweeper.live() * YOUNG_COPY_DEN > allocated * YOUNG_COPY_NUM { YoungEvacuation::Off } else { mode }
    }

    /// Empty a queue.
    fn drain<T>(queue: &Injector<T>) -> Vec<T> {
        let mut out = Vec::new();
        loop {
            match queue.steal() {
                Steal::Success(v) => out.push(v),
                Steal::Retry => {}
                Steal::Empty => return out,
            }
        }
    }

    /// Full collection: [`Heap::finish_marking`], then finish sweeping right away (on return dead objects are finalized).
    pub fn collect(&mut self) {
        // With a cross-heap attached: first run a young collection to promote shareable young objects (a full collection would mark them old, and they would never be promoted after that)
        #[cfg(crossheap)]
        if self.cross.is_some() {
            self.collect_young();
        }
        self.finish_marking();
        self.sweep_concurrently();
    }

    /// Set the cap of the [`Pages`] warm pool before sweeping. The spare empty blocks kept by sweeping follow this round's take; under a growing load (more live data, a higher trigger)
    /// the next round needs more, and the excess blocks given back and taken again cost `madvise` plus faulting in whole blocks. So spares plus the warm pool together keep as many as the most
    /// taken in any of the last [`WARM_CYCLES`] rounds; once the load drops the excess goes back to the system within a few rounds. Only automatic collections
    /// ([`Heap::collect_if_needed`]) keep these; direct collections ([`Heap::collect`] and others) and `shrinking` keep no warm pool.
    fn warm_for_next_cycle(&mut self, shrinking: bool) {
        self.recent.rotate_right(1);
        self.recent[0] = *self.allocated.get_mut() / BLOCK;
        let most = self.recent.iter().copied().max().unwrap_or(0);
        let cap = if shrinking || !self.automatic { 0 } else { most - self.recent[0] };
        self.pages.set_warm_cap(cap);
    }

    /// Give as much memory as possible back to the system (like cppgc's memory-reducing GC): a full collection (with evacuation) that keeps no empty blocks
    /// and `madvise`s whole pages of free lines in partially used blocks (allocating there later faults pages in again, so ordinary collections do not do it).
    /// Suited to idle times or when the process is under memory pressure.
    pub fn shrink(&mut self) {
        *self.shrinking.get_mut() = true;
        self.collect();
        *self.shrinking.get_mut() = false;
    }

    /// Finish marking: if concurrent marking is in progress, drain the grey objects gathered by the write barrier to complete it; otherwise mark in parallel from every [`crate::Root`];
    /// then hand every block to the sweeper and return — sweeping then proceeds concurrently with mutators (mutators sweep along when they need blocks,
    /// or some thread calls [`Heap::sweep_concurrently`]). The pause is only the end of marking.
    pub fn finish_marking(&mut self) {
        self.sweep_concurrently();
        // Reference counts are recounted along the way (old counts void; every edge scanned +1 and slots switch to counting the current value): stop-the-world flips the parity here;
        // concurrent marking flipped it at the handshake and its scan already counted part; below, black objects' edges are added and slots rewritten during marking are settled
        let concurrent = *self.marking.get_mut() != 0;
        self.begin_full(concurrent);
        let count = Some(*self.rc_parity.get_mut());
        // SAFETY: &mut self ⇒ no mutators
        let roots = unsafe { self.roots.settle() };
        let marking = *self.marking.get_mut();
        let epoch = if marking == 0 { self.next_epoch() } else { marking };
        let pool = self.pool.get_or_init(|| Pool::new(self.threads));
        let workers = pool.sized(self.blocks.len() + self.large.len());
        // Swept available blocks unused since: without concurrent marking, the sparse ones are picked for evacuation and the rest become destinations
        let available = self.take_available();
        let targets = Injector::new();
        let mut evacuate = if marking == 0 { Self::pick_evacuation(&available, &targets) } else { 0 };
        if evacuate > 0 {
            // Objects pointed to directly from other heaps must not move: their blocks are taken off evacuation and become destinations. This must happen before marking starts,
            // otherwise another worker might reach the object through an edge inside the heap first and move it
            let pin = |h: *const Header| {
                // SAFETY: entries point at live objects of this heap
                if !unsafe { &*h }.is_large() && Block::containing(h.cast()).pin() {
                    evacuate -= 1;
                    targets.push(Block::containing(h.cast()));
                }
            };
            // SAFETY: no mutators
            unsafe { self.roots.pinned(pin) };
        }
        // Black objects from concurrent marking: marked but never scanned, so they go on the grey queue for the final marking to scan once (only for counting)
        for addr in Self::drain(&self.black) {
            self.grey.push(addr);
        }
        #[cfg(crossheap)]
        let crossing = Injector::new();
        let mark = Marking::new(workers, epoch, &self.grey).counting(count);
        #[cfg(crossheap)]
        let mark = mark.crossing(self.cross.is_some().then_some(&crossing));
        let moved = AtomicUsize::new(0);
        if evacuate > 0 {
            mark.evacuating(&self.roots, &targets, &self.blocks, &self.pages, &moved, self.epoch).run(pool);
        } else {
            if marking == 0 {
                Marking::seed(&self.grey, &roots, epoch);
            }
            mark.run(pool);
        }
        if concurrent {
            self.settle_concurrent_log(epoch);
        }
        // SAFETY: no mutators; marking (including evacuation) is done and sweeping has not started
        unsafe { self.weaks.clear_unreached(epoch, *self.rc_parity.get_mut()) };
        #[cfg(crossheap)]
        self.collect_cross(&crossing);
        self.end_full(epoch, (evacuate, moved.into_inner()));
    }

    /// Bookkeeping at the start of a full collection (exclusive): full marking re-marks lines and finds every live object, so dirty blocks and the remembered set are no longer needed;
    /// a stop-the-world one flips the count parity here (slots logged before marking: if their object is alive it is scanned and switched to the current value; if dead, nothing to do).
    fn begin_full(&mut self, concurrent: bool) {
        drop(Self::drain(&self.dirty));
        drop(Self::drain(&self.remembered));
        self.young_since_full = 0;
        if !concurrent {
            drop(Self::drain(&self.rc_log));
            let parity = self.rc_parity.get_mut();
            *parity = !*parity;
        }
        self.rc_roots.clear();
    }

    /// Take every available block out (after a full collection the sweeper reclassifies them and puts them back).
    fn take_available(&self) -> Vec<Block> {
        let available = Self::drain(&self.available);
        for b in &available {
            b.unlist();
        }
        available
    }

    /// End of a full collection (exclusive): counts are trustworthy; every block goes to the sweeper (blocks opened during evacuation are registered too), and the swept available blocks return
    /// to available one by one later (they are all in blocks anyway). `evacuated`: blocks and bytes evacuated.
    fn end_full(&mut self, epoch: u32, evacuated: (usize, usize)) {
        self.young_allocated = 0;
        *self.rc_valid.get_mut() = true;
        let blocks = self.blocks.take();
        let large = self.large.take();
        // Spare empty blocks: the next round probably takes about as many blocks as the last one did; the rest go back to the system.
        // Keeping too few means opening new blocks every round (zeroing + page faults), e.g. a load with many temporaries is nearly all empty every round
        let shrinking = *self.shrinking.get_mut();
        let spare = if shrinking { 0 } else { (*self.allocated.get_mut() / BLOCK).max(MIN_SPARE_BLOCKS) };
        self.warm_for_next_cycle(shrinking);
        self.sweeper.start(epoch, blocks, large, spare, shrinking);
        self.evacuated = evacuated;
        self.epoch = epoch;
        *self.marking.get_mut() = 0;
        *self.allocated.get_mut() = 0;
        self.collections += 1;
    }

    /// Fully collect several heaps together, reclaiming cycles across heaps (A's object points to B through a [`crate::Far`] and B's points back; in single collections each other's entries are roots).
    ///
    /// Fars held by heaps of the group are not roots: each heap marks from its own [`crate::Root`]s (and Fars held from outside the group), and a traced Far hands its target to the target heap
    /// to continue marking until nothing new appears; an unmarked entry means the Far holding it is dead, so it is cleared. No evacuation. On return dead objects are finalized.
    /// Heaps with concurrent marking in progress finish it on their own first.
    pub fn collect_group(heaps: &mut [&mut Heap]) {
        for h in heaps.iter_mut() {
            if *h.marking.get_mut() != 0 {
                h.finish_marking();
            }
            h.sweep_concurrently();
        }
        let ids: Vec<usize> = heaps.iter().map(|h| h.id()).collect();
        let inside = |holder: usize| ids.contains(&holder);
        let mut epochs = Vec::with_capacity(heaps.len());
        for h in heaps.iter_mut() {
            h.begin_full(false);
            drop(h.take_available());
            let epoch = h.next_epoch();
            // SAFETY: &mut ⇒ no mutators
            let roots = unsafe { h.roots.settle_outside(inside) };
            Marking::seed(&h.grey, &roots, epoch);
            epochs.push(epoch);
        }
        let found = Injector::new();
        loop {
            for (h, &epoch) in heaps.iter().zip(&epochs) {
                if h.grey.is_empty() {
                    continue;
                }
                let pool = h.pool();
                let workers = pool.sized(h.blocks.len() + h.large.len());
                Marking::new(workers, epoch, &h.grey).counting(Some(h.rc_parity.load(Ordering::Acquire))).grouping(&found).run(pool);
            }
            for (table, header) in Self::drain(&found) {
                // Heaps outside the group are not part of this collection; their entries are their own roots, nothing to do
                if let Some(j) = ids.iter().position(|&id| id == table) {
                    Marking::seed(&heaps[j].grey, &[core::ptr::with_exposed_provenance(header)], epochs[j]);
                }
            }
            if heaps.iter().all(|h| h.grey.is_empty()) {
                break;
            }
        }
        for (h, &epoch) in heaps.iter_mut().zip(&epochs) {
            // SAFETY: no mutators; sweeping has not started
            unsafe { h.roots.clear_dead(inside, epoch) };
            // SAFETY: as above
            unsafe { h.weaks.clear_unreached(epoch, *h.rc_parity.get_mut()) };
            h.end_full(epoch, (0, 0));
        }
        for h in heaps.iter() {
            h.sweep_concurrently();
        }
    }
}

/// Operations for this crate only: the trait lives in a private module and is not exported, so it is unreachable outside the crate.
pub trait HeapInternal {
    fn roots(&self) -> &Arc<RootTable>;

    fn weaks(&self) -> &Arc<RootTable>;

    /// The heap's identity: the address of its root table (the heap itself may move); never [`crate::handle::NO_HOLDER`].
    fn id(&self) -> usize;
}

impl HeapInternal for Heap {
    fn roots(&self) -> &Arc<RootTable> {
        &self.roots
    }

    fn weaks(&self) -> &Arc<RootTable> {
        &self.weaks
    }

    fn id(&self) -> usize {
        Arc::as_ptr(&self.roots).addr()
    }
}


/// The sweeper hands a swept item back to the heap: blocks go into the block table and usable ones onto available; live large objects return to the large object table.
impl Sink for Heap {
    fn block(&self, outcome: Outcome) {
        match outcome {
            Outcome::Usable(b) => {
                self.blocks.push(b);
                if b.list() {
                    self.available.push(b);
                }
            }
            Outcome::Full(b) => {
                self.blocks.push(b);
            }
            // SAFETY: an empty block (dead objects finalized) from this heap's pages, never used again
            Outcome::Release(b) => unsafe { b.release(&self.pages) },
        }
    }

    fn large(&self, survivor: Large) {
        self.large.push(survivor);
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        // A heap that never collected does not start threads just to finalize
        let pool = self.pool.take().unwrap_or_else(|| Pool::new(NonZeroUsize::MIN));
        let parallel_drop = *self.parallel_drop.get_mut();
        // The blocks are unmapped as a whole right after, so returned ones need no madvise first
        self.pages.set_warm_cap(usize::MAX);
        // Finish the previous round, then sweep once without marking using an epoch newer than any (even objects under concurrent marking count as dead): everything is finalized and every empty block goes back, none kept as spare
        self.sweeper.finish(&pool, self, parallel_drop);
        let epoch = self.epoch.max(*self.marking.get_mut()).wrapping_add(1);
        let blocks = self.blocks.take();
        let large = self.large.take();
        while !matches!(self.available.steal(), Steal::Empty) {}
        self.sweeper.start(epoch, blocks, large, 0, false);
        self.sweeper.finish(&pool, self, parallel_drop);
        debug_assert!(self.blocks.is_empty() && self.large.is_empty());
        let spare = *self.spare.get_mut();
        if !spare.is_null() {
            // SAFETY: from Box::into_raw
            drop(unsafe { Box::from_raw(spare) });
        }
    }
}

/// When a mutate ends (including by panic), write the start bits gathered by the allocation buffer into the blocks.
struct FlushStarts<'a>(&'a Tlab);

impl Drop for FlushStarts<'_> {
    fn drop(&mut self) {
        self.0.flush_starts();
    }
}

/// A thread's allocation buffer: a main cursor and an overflow cursor (each with its block, the line to search from, and the current hole's cursor and limit). Between two collections the blocks belong to it alone.
pub struct Tlab {
    /// (owning heap, collection count at the time); stale when it does not match
    owner: Cell<(usize, u64)>,
    main: Bump,
    /// Overflow allocation (medium objects: larger than a line and not fitting the main hole): a separate block / hole, leaving the main hole to small objects (Immix's overflow allocation)
    overflow: Bump,
    /// Public heaps: the last allocation's (address of the type name, type number)
    last: Cell<Option<(usize, u32)>>,
    /// Start bits not yet written into the block: (a word of the start bitmap, bits to set). Consecutive objects mostly fall in the same word, so they are gathered and written at once when the word changes,
    /// instead of one atomic RMW per object. Between two collections the block belongs to this buffer only; start bits are read only during pauses (when every mutate has ended and written them),
    /// and concurrent release by reference counting only clears other (old objects') bits, so writing late does no harm
    starts: Cell<(*const AtomicU64, u64)>,
}

impl Default for Tlab {
    fn default() -> Self {
        Self { owner: Cell::default(), main: Bump::default(), overflow: Bump::default(), last: Cell::default(), starts: Cell::new((core::ptr::null(), 0)) }
    }
}

// SAFETY: the pointers refer to blocks it owns exclusively; after moving to another thread that thread owns them
unsafe impl Send for Tlab {}

impl Tlab {
    /// The heap changed or collected (`owner` = (heap, collection count)): the old blocks were swept and reclassified, so drop them; the type number cache of the same heap stays.
    fn adopt(&mut self, owner: (usize, u64)) {
        self.flush_starts();
        if self.owner.get() != owner {
            let last = if self.owner.get().0 == owner.0 { self.last.get() } else { None };
            *self = Self { owner: Cell::new(owner), last: Cell::new(last), ..Self::default() };
        }
    }

    /// Record that an object starts at `p` (see [`Tlab::starts`]).
    #[inline]
    fn note_start(&self, p: *const u8) {
        let (word, bit) = Block::containing(p).start_bit(p);
        let (cur, bits) = self.starts.get();
        if core::ptr::eq(cur, word) {
            self.starts.set((cur, bits | bit));
        } else {
            self.flush_starts();
            self.starts.set((word, bit));
        }
    }

    /// Write the gathered start bits into the block.
    fn flush_starts(&self) {
        let (word, bits) = self.starts.replace((core::ptr::null(), 0));
        if bits != 0 {
            // SAFETY: the word lies in a block owned by this buffer, which is not given back before a collection (and the bits were written when the mutate before it ended)
            unsafe { &*word }.fetch_or(bits, Ordering::Relaxed);
        }
    }

    /// Bump-allocate `bytes` bytes (lines with marks older than `epoch` are free; take a block with `acquire` when no hole is left).
    /// Objects larger than a line that do not fit the main hole use the overflow cursor (its blocks come from `roomy(bytes)`: they must have a hole that fits), without discarding the rest of the main hole.
    fn bump(&self, bytes: usize, epoch: u32, acquire: impl Fn() -> Block, roomy: impl Fn(usize) -> Block) -> *mut u8 {
        if let Some(p) = self.main.fit(bytes) {
            return p;
        }
        if bytes > LINE {
            return self.overflow.take(bytes, epoch, &|| roomy(bytes));
        }
        self.main.take(bytes, epoch, &acquire)
    }
}

/// A bump-allocation cursor: the current block, the line to search for holes from next, and the current hole's cursor and limit.
struct Bump {
    block: Cell<Option<Block>>,
    line: Cell<usize>,
    cursor: Cell<*mut u8>,
    limit: Cell<*mut u8>,
}

impl Default for Bump {
    fn default() -> Self {
        Self { block: Cell::default(), line: Cell::default(), cursor: Cell::new(null_mut()), limit: Cell::new(null_mut()) }
    }
}

impl Bump {
    /// Cut it from the current hole if it fits.
    fn fit(&self, bytes: usize) -> Option<*mut u8> {
        let cursor = self.cursor.get();
        (self.limit.get().addr() - cursor.addr() >= bytes).then(|| {
            self.cursor.set(cursor.wrapping_add(bytes));
            cursor
        })
    }

    /// If the current hole is too small, find the next hole in the current block (skipping those too small), or take a new block with `acquire` when none is left.
    fn take(&self, bytes: usize, epoch: u32, acquire: &impl Fn() -> Block) -> *mut u8 {
        loop {
            if let Some(p) = self.fit(bytes) {
                return p;
            }
            if let Some(b) = self.block.get()
                && let Some((start, end)) = b.hole(self.line.get(), epoch)
            {
                self.line.set(end);
                self.cursor.set(b.line(start));
                self.limit.set(b.line(end));
                continue;
            }
            self.block.set(Some(acquire()));
            self.line.set(Block::FIRST_LINE);
            self.cursor.set(null_mut());
            self.limit.set(null_mut());
        }
    }
}

/// A new object's mark: equals no epoch (epochs start at 1) ⇒ young
const YOUNG: u32 = 0;

/// Top bit of the handshake word: concurrent marking is about to start, new mutates wait first
const REQUEST: u64 = 1 << 63;

/// Inside mutate: increment the entry count on the way in (waiting first if a marking request is pending) and decrement on the way out (also on panic).
struct Inside<'h>(&'h Heap);

impl<'h> Inside<'h> {
    fn enter(heap: &'h Heap) -> Self {
        let backoff = Backoff::new();
        let mut word = heap.gate.load(Ordering::Acquire);
        loop {
            if word & REQUEST != 0 {
                backoff.snooze();
                word = heap.gate.load(Ordering::Acquire);
                continue;
            }
            match heap.gate.compare_exchange_weak(word, word + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Self(heap),
                Err(now) => word = now,
            }
        }
    }
}

impl Drop for Inside<'_> {
    fn drop(&mut self) {
        self.0.gate.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The allocator and brand of one mutate; used only on the calling thread.
pub struct Mutator<'gc> {
    heap: &'gc Heap,
    tlab: &'gc Tlab,
    /// The epoch being marked concurrently when the mutate began: for the write barrier and black allocation
    marking: Option<NonZeroU32>,
    _brand: Invariant<'gc>,
}

impl<'gc> Mutator<'gc> {

    /// Whether this mutate falls within concurrent marking (write barrier and black allocation in effect).
    pub fn is_marking(&self) -> bool {
        self.marking.is_some()
    }

    /// Allocate an object.
    pub fn alloc<T: Trace + 'gc>(&self, value: T) -> Gc<'gc, T> {
        self.alloc_with(value, Header::new::<T>)
    }

    /// Allocate a shareable object (its type is also [`Shared`], with no destructor): when a cross-heap is attached ([`Heap::attach_cross_heap`]),
    /// it is promoted into the cross-heap once it survives a young collection, and other processes can find it by signature or weak reference. Its edges may only point at shareable objects.
    #[cfg(crossheap)]
    pub fn alloc_shared<T: Trace + Shared + 'gc>(&self, value: T) -> Gc<'gc, T> {
        self.alloc_with(value, Header::new_shared::<T>)
    }

    fn alloc_with<T: Trace + 'gc>(&self, value: T, header: fn(usize, bool, u32) -> Header) -> Gc<'gc, T> {
        const { assert!(align_of::<T>() <= GRANULE, "object data alignment must not exceed 8 bytes") };
        let bytes = (HEADER + size_of::<T>()).next_multiple_of(GRANULE);
        let large = bytes > LARGE;
        let p = if large { self.heap.alloc_large(bytes) } else { self.alloc_small(bytes) };
        // Normally: young (mark 0 ≠ the last completed epoch; it lives in a block swept before being handed out, or a new one, so the sweep in progress never touches it).
        // During concurrent marking: black allocation (marked with this round right away, together with its lines)
        let mark = self.marking.map_or(YOUNG, NonZeroU32::get);
        if self.marking.is_some() {
            if !large {
                Block::containing(p).mark_lines(p, bytes, mark);
            }
            // Marking never scans black objects: record them so their edges are counted at the end (reference counting)
            self.heap.black.push(p.expose_provenance());
        }
        // SAFETY: p is new memory of bytes bytes, granule aligned, owned by this allocation
        unsafe {
            p.cast::<Header>().write(header(bytes, large, mark));
            let data = Header::data(p.cast()).cast::<T>();
            data.write(value);
            Gc::from_data(data)
        }
    }

    fn alloc_small(&self, bytes: usize) -> *mut u8 {
        let p = self.tlab.bump(bytes, self.heap.epoch, || self.heap.acquire(), |need| self.heap.acquire_for(need));
        self.tlab.note_start(p);
        p
    }
}

/// Operations for this crate only: the trait lives in a private module and is not exported, so it is unreachable outside the crate.
pub trait MutatorInternal<'gc> {
    fn heap(&self) -> &'gc Heap;

    /// Write barrier. During concurrent marking: the overwritten old value is marked and greyed if not yet marked (snapshot at the beginning).
    /// Normally: when a young object (mark epoch not the last completed one) is written into a slot of an old object ([`RawSlot::is_young`] false), record the slot's address —
    /// that is the only way edges from old to young objects arise, and young collections start from these slots; slots in young objects need no record, as young collections reach them from the object. (During concurrent marking new objects are black and all old afterwards, so no record.)
    ///
    /// A counted slot (in an old object; young objects' slots are uncounted) written for the first time this round is also logged and settled at collection.
    fn written(&self, slot: &RawSlot, old: Option<*const Header>, new: Option<*const Header>);

    /// Read barrier (only during concurrent marking): a mutator got `h` from a weak reference; mark and grey it if not yet marked.
    fn reached(&self, h: *const Header);
}

impl<'gc> MutatorInternal<'gc> for Mutator<'gc> {
    fn heap(&self) -> &'gc Heap {
        self.heap
    }

    fn written(&self, slot: &RawSlot, old: Option<*const Header>, new: Option<*const Header>) {
        let addr = (&raw const *slot).expose_provenance();
        // A slot inside a cross-heap object: no counting. A private object written into it stays a private address (other processes read it as empty) until the next young collection
        // promotes it and rewrites the slot through the remembered set
        #[cfg(crossheap)]
        if Region::holds_fixed(addr) {
            if new.is_some() {
                assert!(self.heap.cross.is_some(), "writing a private object into a cross-heap object needs a cross-heap attached (Heap::attach_cross_heap)");
                self.heap.remembered.push(addr);
                self.heap.promotion_pending.store(true, Ordering::Release);
            }
            return;
        }
        if let Some(epoch) = self.marking {
            if let Some(old) = old {
                self.heap.remember(old, epoch.get());
            }
        } else if !slot.is_young()
            && let Some(new) = new
            // SAFETY: new is the live object just written into the slot
            && !unsafe { &*new }.is_marked(self.heap.epoch)
        {
            self.heap.remembered.push(addr);
        }
        // Reference count logging: both normally and during concurrent marking (slots rewritten during marking are settled at the end)
        if slot.log() {
            self.heap.rc_log.push(addr);
        }
    }

    fn reached(&self, h: *const Header) {
        if let Some(epoch) = self.marking {
            self.heap.remember(h, epoch.get());
        }
    }
}

/// When waiting for others (the collector, threads inside mutate), wake up this often to check whether their process is still alive
#[cfg(crossheap)]
const POLL: Duration = Duration::from_millis(10);
/// [`Attachment::pid`]: someone is cleaning up this dead attachment (keeps two collectors from returning its entries twice)
#[cfg(crossheap)]
const REAPING: u32 = u32::MAX;
/// Before claiming a collection, wait at most this many rounds (each [`POLL`]) for mutators held at the door to get in
#[cfg(crossheap)]
const YIELD_ROUNDS: usize = 10;
/// The longest a collector waits for mutate to empty; if it does not, give up the request and try again later: when a thread inside A's mutate enters B,
/// two collectors (or a group collection) holding A and B would wait on each other forever, and giving up the request breaks that
#[cfg(crossheap)]
const PATIENCE: Duration = Duration::from_millis(50);
/// Top bit of [`Entry::holder`]: the holder is another public heap (the low bits are its heap number and [`Entry::holder_incarnation`] its identity), not an attachment (slot + 1)
#[cfg(crossheap)]
pub const HEAP_HOLDER: u32 = 1 << 31;
/// [`Entry::holder`]: the holder heap is in the same directory as this heap (the same heap-number registry), so its liveness can be judged by directory and its registrations returned once it dies
#[cfg(crossheap)]
pub const SAME_DIR: u32 = 1 << 30;
/// The heap number bits of [`Entry::holder`]
#[cfg(crossheap)]
const HOLDER_INDEX: u32 = SAME_DIR - 1;
/// How often `try_mutate` checks again that the owner is still there (reading /proc)
#[cfg(crossheap)]
const ALIVE_RECHECK: Duration = Duration::from_millis(10);
/// Blocks taken at once while sweeping
#[cfg(crossheap)]
const SWEEP_BATCH: usize = 16;
/// SplitMix64 mixes the birth stamp: the golden-ratio constant the epoch is multiplied by
#[cfg(crossheap)]
const SPLITMIX_GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;
/// Right shifts of SplitMix64's final three steps
#[cfg(crossheap)]
const SPLITMIX_SHIFTS: [u32; 3] = [30, 27, 31];
/// Multipliers of SplitMix64's final two steps
#[cfg(crossheap)]
const SPLITMIX_MULS: [u64; 2] = [0xbf58_476d_1ce4_e5b9, 0x94d0_49bb_1331_11eb];
/// What backs a window.
#[cfg(crossheap)]
pub enum Backing {
    /// A file on a shared-memory file system; `owner` = created by this handle (this process is the owner): on drop the heap dies and the file and marker are deleted (unless persisted).
    /// Heaps opened from a descriptor passed by someone else have no path ([`crate::CrossHeapSpace::open_fd`])
    File { file: File, path: Option<PathBuf>, owner: bool },
    /// Shared anonymous memory (this process only)
    Anonymous,
}

#[cfg(crossheap)]
impl Backing {
    /// Make `[offset, offset + len)` of the window usable (a file is extended; anonymous memory already is).
    fn commit(&self, offset: usize, len: usize) {
        if let Self::File { file, .. } = self {
            Region::commit(file, offset, len).expect("the file system holding the public heap is full");
        }
    }

    /// Give `[offset, offset + len)` of the window (counted from `window`) back to the system: a hole punched in a file (the length stays and it reads as zeros), `MADV_REMOVE` for anonymous memory.
    /// Best effort: left alone when the file system does not support it (such as hugetlbfs without huge page alignment).
    fn punch(&self, window: *mut u8, offset: usize, len: usize) {
        match self {
            Self::File { file, .. } => Region::punch(file, offset, len),
            Self::Anonymous => Region::remove(window.wrapping_add(offset), len),
        }
    }
}

/// 64-bit FNV-1a for type keys.
#[cfg(crossheap)]
struct Fnv(u64);

#[cfg(crossheap)]
impl Fnv {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    /// Two different starting points, combined into 128 bits
    const SEEDS: [u64; 2] = [0xcbf2_9ce4_8422_2325, 0x6c62_272e_07bb_0142];

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(Self::PRIME);
        }
    }

    /// The pointer map and type key of type `T`.
    fn of<T: Shared>() -> (EdgeMap, [u64; 2]) {
        let mut map = EdgeMap::default();
        T::edges(&mut map, 0);
        let key = Self::key(type_name::<T>(), size_of::<T>(), map.as_slice());
        (map, key)
    }

    /// Type name + size + pointer map.
    fn key(name: &str, size: usize, edges: &[u32]) -> [u64; 2] {
        Self::SEEDS.map(|seed| {
            let mut h = Self(seed);
            h.write(name.as_bytes());
            h.write(&size.to_le_bytes());
            for e in edges {
                h.write(&e.to_le_bytes());
            }
            h.0
        })
    }
}

/// This process's side of a public heap: where the window is, how to extend the file, the collector thread pool, the type number cache.
#[cfg(crossheap)]
pub struct Inner {
    /// The region: the window lies inside it, and the Arc makes it outlive the heap
    space: Arc<Space>,
    index: usize,
    window: NonNull<u8>,
    layout: WindowLayout,
    backing: Backing,
    threads: NonZeroUsize,
    pool: OnceLock<Pool>,
    /// Types seen by this process: (address of the type name, type number)
    types: SegVec<(usize, u32)>,
    /// An allocation buffer reused by mutate
    spare: AtomicPtr<Tlab>,
    /// This handle's slot in the attachment table
    slot: usize,
    /// When the handle was created: `checked` counts from here
    born: std::time::Instant,
    /// When the owner was last confirmed alive (nanoseconds since `born`)
    checked: AtomicU64,
}

// SAFETY: the state in the window is all atomics or protected by the handshake; the rest is thread safe
#[cfg(crossheap)]
unsafe impl Send for Inner {}
// SAFETY: as above
#[cfg(crossheap)]
unsafe impl Sync for Inner {}

#[cfg(crossheap)]
impl Inner {
    /// Window `index` is already mapped onto `backing`; with `fresh` write a new superblock, otherwise check it. On error, drop gives the window back.
    pub fn new(space: Arc<Space>, index: usize, backing: Backing, threads: NonZeroUsize, fresh: bool) -> io::Result<Self> {
        let window = NonNull::new(space.region().window(index)).expect("the window address is non-null");
        let mut me = Self { space, index, window, layout: WindowLayout::new(), backing, threads, pool: OnceLock::new(), types: SegVec::default(), spare: AtomicPtr::new(null_mut()), slot: ATTACH_CAP, born: std::time::Instant::now(), checked: AtomicU64::new(0) };
        if fresh {
            me.backing.commit(0, PAGE);
            // SAFETY: a new heap; the first page of the window just got memory and nobody reads it yet
            let incarnation = std::hash::BuildHasher::hash_one(&std::collections::hash_map::RandomState::new(), (index, Process::current().pid));
            // SAFETY: as above
            unsafe { me.window.as_ptr().cast::<Super>().write(Super::new(index as u64, incarnation, Process::current())) };
        } else if me.sup().heap.load(Ordering::Acquire) != index as u64 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "the heap number in the superblock does not match the window"));
        }
        me.slot = me.attach().ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "this public heap's attachment table is full"))?;
        Ok(me)
    }

    /// Take a slot in the attachment table to register this process; when full, clean up dead attachments and try again.
    fn attach(&self) -> Option<usize> {
        let me = Process::current();
        let claim = || (0..ATTACH_CAP).find(|&i| self.attachment(i).pid.compare_exchange(0, me.pid, Ordering::AcqRel, Ordering::Relaxed).is_ok());
        let slot = claim().or_else(|| {
            self.reap_dead();
            claim()
        })?;
        let a = self.attachment(slot);
        a.inside.store(0, Ordering::Relaxed);
        a.waiting.store(0, Ordering::Relaxed);
        a.start.store(me.start, Ordering::Release);
        Some(slot)
    }

    fn attachment(&self, i: usize) -> &Attachment {
        // SAFETY: the attachment table is in the superblock page (with memory since creation), i < ATTACH_CAP; its entries are all atomics
        unsafe { &*self.at::<Attachment>(ATTACH_OFF).add(i) }
    }

    /// The process of attachment slot `i` is alive (free slots and ones being cleaned up count as not alive).
    fn alive(&self, i: usize) -> bool {
        let a = self.attachment(i);
        let pid = a.pid.load(Ordering::Acquire);
        pid != 0 && pid != REAPING && Process { pid, start: a.start.load(Ordering::Acquire) }.alive()
    }

    /// Clean up every dead attachment.
    ///
    /// The one holding the collection request is left alone: if its slot went to a new process everyone would think the collector is alive and wait forever; it is cleaned up after the collector taking over replaces it.
    fn reap_dead(&self) {
        let owner = self.sup().request.load(Ordering::SeqCst) as usize;
        for i in 0..ATTACH_CAP {
            let pid = self.attachment(i).pid.load(Ordering::Acquire);
            if pid != 0 && pid != REAPING && i + 1 != owner && !self.alive(i) {
                self.reap(i, pid);
            }
        }
    }

    /// Clean up dead attachment `i` (pid `pid`): return the entries it holds, clear its mutate count and free the slot. The pid is first swapped for [`REAPING`]; only the one that succeeds does the work.
    fn reap(&self, i: usize, pid: u32) {
        let a = self.attachment(i);
        if a.pid.compare_exchange(pid, REAPING, Ordering::AcqRel, Ordering::Relaxed).is_err() {
            return;
        }
        let holder = u32::try_from(i + 1).expect("attachment slot");
        // Return every entry it holds (including ones half claimed, with the pointer not yet written)
        for j in 0..self.sup().entries.load(Ordering::Acquire) {
            if self.entry(j).holder.load(Ordering::Acquire) == holder {
                self.release_entry(j);
            }
        }
        a.inside.store(0, Ordering::Release);
        a.waiting.store(0, Ordering::Release);
        a.start.store(0, Ordering::Relaxed);
        a.pid.store(0, Ordering::Release);
    }

    /// Before claiming a collection: let mutators held at the door by the previous collection in first (waking takes time, and without giving way they would starve under back-to-back collections).
    /// Dead attachments are not waited for; at most [`YIELD_ROUNDS`] rounds.
    fn yield_to_waiting(&self) {
        for _ in 0..YIELD_ROUNDS {
            let Some(i) = (0..ATTACH_CAP).find(|&i| self.attachment(i).waiting.load(Ordering::SeqCst) != 0 && self.alive(i)) else {
                return;
            };
            let waiting = &self.attachment(i).waiting;
            waiting.wait(waiting.load(Ordering::SeqCst), POLL);
        }
    }

    /// Claim the collection: take it if nobody is collecting; wait while someone alive is collecting; take over if they died (killed in the middle of collecting).
    /// `done_by_other`: return false right away once someone else finished (for single collections: no need to collect again right after someone did); otherwise wait until it is ours.
    fn claim_collection(&self, done_by_other: bool) -> bool {
        let sup = self.sup();
        let me = u32::try_from(self.slot + 1).expect("attachment slot");
        loop {
            let owner = sup.request.load(Ordering::SeqCst);
            if owner == 0 {
                self.yield_to_waiting();
                if sup.request.compare_exchange(0, me, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                    return true;
                }
            } else if self.alive(owner as usize - 1) {
                sup.request.wait(owner, POLL);
                if done_by_other && sup.request.load(Ordering::SeqCst) != owner {
                    return false;
                }
            } else if sup.request.compare_exchange(owner, me, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                sup.takeovers.fetch_add(1, Ordering::Relaxed);
                return true;
            }
        }
    }

    /// Wait until every attachment's mutate is empty (not waiting for dead ones), at most [`PATIENCE`]; true once empty.
    fn drain(&self) -> bool {
        let start = std::time::Instant::now();
        for i in 0..ATTACH_CAP {
            let inside = &self.attachment(i).inside;
            loop {
                let n = inside.load(Ordering::SeqCst);
                if n == 0 || !self.alive(i) {
                    break;
                }
                if start.elapsed() >= PATIENCE {
                    return false;
                }
                inside.wait(n, POLL);
            }
        }
        true
    }

    /// Give up the collection claim and wake those waiting at the door.
    fn release(&self) {
        let sup = self.sup();
        sup.request.store(0, Ordering::SeqCst);
        sup.request.wake_all();
    }

    /// Stop-the-world collection: claim it (returning if someone just collected), wait for mutate to empty (giving up and retrying if it does not), clean up dead attachments, mark + sweep.
    /// Marking writes only a new epoch and the epoch is published last, and sweeping starts only after marking is done, so redoing it from scratch after the collector is killed is safe.
    fn collect(&self) {
        self.collect_with(false);
    }

    /// As [`Inner::collect`]; with `shrink`, every empty block found by sweeping goes back to the system, none kept.
    /// For heaps collected only by their owner ([`crate::Heap::attach_cross_heap`]) this does nothing while the owner lives: the roots in the owner's private heap are not visible here.
    fn collect_with(&self, shrink: bool) {
        if self.sup().owner_only.load(Ordering::Acquire) != 0 && self.is_alive() {
            return;
        }
        self.collect_rooted(&[], shrink);
    }

    /// Collection by the owner (the private heap it is attached to): `extra` holds data addresses of this heap's objects traced from the private heap, also roots. Returns this round's epoch.
    pub fn collect_owned(&self, extra: &[usize], shrink: bool) -> u32 {
        self.collect_rooted(extra, shrink);
        self.sup().epoch.load(Ordering::Acquire)
    }

    /// `data` (the data address of an object of this heap) survived the collection of epoch `epoch`.
    pub fn survived(data: *mut u8, epoch: u32) -> bool {
        // SAFETY: an object of this heap not yet overwritten by a new allocation (the caller asks right after the collection, with no mutators)
        unsafe { &*PHeader::of(data) }.core.is_marked(epoch)
    }

    fn collect_rooted(&self, extra: &[usize], shrink: bool) {
        let mut retry = Retry::new();
        loop {
            if !self.claim_collection(true) {
                return;
            }
            if self.drain() {
                break;
            }
            self.release();
            retry.pause();
        }
        self.reap_dead();
        Self::collect_members(&[self], extra, shrink);
        self.release();
    }

    /// Collect several heaps together: claim each heap's collection in increasing heap number and start only when all are empty; if one cannot be waited for, give everything up and retry (so group collections never deadlock each other).
    ///
    /// One at a time: claim heap k's request and wait for it to empty before claiming heap k+1. So as long as mutators nest by increasing heap number
    /// ([`CrossHeap::mutate_with`]), a thread in a lower heap waiting to enter a higher one does not block it: the higher heap is not held yet. Only reverse nesting waits on each other,
    /// and then everything is given up and retried after backing off.
    fn collect_group(members: &[&Self]) {
        let mut retry = Retry::new();
        'retry: loop {
            for (k, m) in members.iter().enumerate() {
                m.claim_collection(false);
                if !m.drain() {
                    for held in members[..=k].iter().rev() {
                        held.release();
                    }
                    retry.pause();
                    continue 'retry;
                }
            }
            break;
        }
        for m in members {
            m.reap_dead();
        }
        Self::collect_members(members, &[], false);
        for m in members.iter().rev() {
            m.release();
        }
    }

    pub fn index(&self) -> usize {
        self.index
    }

    /// Which heap (window) a public offset falls in.
    pub fn heap_of(off: u64) -> Option<usize> {
        usize::try_from(off).ok().map(|o| o / WINDOW).filter(|&k| k < WINDOWS)
    }

    /// Heap identity (see [`Super::incarnation`]).
    pub fn incarnation(&self) -> u64 {
        self.sup().incarnation.load(Ordering::Acquire)
    }

    /// The heap file's path (anonymous heaps have none).
    fn path(&self) -> Option<&std::path::Path> {
        match &self.backing {
            Backing::File { path, .. } => path.as_deref(),
            Backing::Anonymous => None,
        }
    }

    /// The holder word this heap uses when registering in `target` as the holder: with [`SAME_DIR`] when in the same directory.
    pub fn holder_word(&self, target: &Self) -> u32 {
        let same_dir = self.path().zip(target.path()).is_some_and(|(a, b)| Registry::of(a).dir() == Registry::of(b).dir());
        HEAP_HOLDER | if same_dir { SAME_DIR } else { 0 } | u32::try_from(self.index).expect("heap number")
    }

    /// The heap is alive: never marked dead, and the owner is still there (or it is persisted, or anonymous). On finding the owner gone, mark it dead and delete the heap file and marker.
    pub fn is_alive(&self) -> bool {
        let sup = self.sup();
        if sup.dead.load(Ordering::Acquire) != 0 {
            return false;
        }
        if matches!(self.backing, Backing::Anonymous) {
            return true;
        }
        let owner = Process { pid: sup.owner_pid.load(Ordering::Acquire), start: sup.owner_start.load(Ordering::Acquire) };
        if sup.persist.load(Ordering::Acquire) != 0 || owner.alive() {
            return true;
        }
        sup.dead.store(1, Ordering::Release);
        if let Some(path) = self.path() {
            Registry::of(path).bury(path, self.index, self.incarnation());
        }
        false
    }

    /// As [`Inner::is_alive`], but within [`ALIVE_RECHECK`] of the last confirmation only the "dead" flag is checked (reading /proc is not cheap).
    fn alive_cached(&self) -> bool {
        if self.sup().dead.load(Ordering::Acquire) != 0 {
            return false;
        }
        let now = u64::try_from(self.born.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let last = self.checked.load(Ordering::Relaxed);
        if u128::from(now.saturating_sub(last)) < ALIVE_RECHECK.as_nanos() {
            return true;
        }
        let alive = self.is_alive();
        if alive {
            self.checked.store(now, Ordering::Relaxed);
        }
        alive
    }

    /// Whether the holder word + identity is one of the heaps in the group (registrations held by heaps of the group are decided by tracing in a group collection).
    fn held_by(members: &[&Self], holder: u32, incarnation: u64) -> bool {
        holder & HEAP_HOLDER != 0 && members.iter().any(|m| m.index == (holder & HOLDER_INDEX) as usize && m.incarnation() == incarnation)
    }

    /// Before collecting: return registrations whose holder is another heap in the same directory that has died (or whose number changed owner). Uncertain ones stay.
    fn release_dead_holders(&self) {
        let Some(path) = self.path() else { return };
        let registry = Registry::of(path);
        let mut verdicts: Vec<((usize, u64), Liveness)> = Vec::new();
        for i in 0..self.sup().entries.load(Ordering::Acquire) {
            let e = self.entry(i);
            let holder = e.holder.load(Ordering::Acquire);
            if e.ptr.load(Ordering::Acquire) == 0 || holder & (HEAP_HOLDER | SAME_DIR) != HEAP_HOLDER | SAME_DIR {
                continue;
            }
            let key = ((holder & HOLDER_INDEX) as usize, e.holder_incarnation.load(Ordering::Acquire));
            let verdict = if let Some(&(_, v)) = verdicts.iter().find(|(k, _)| *k == key) {
                v
            } else {
                let v = registry.liveness(key.0, Some(key.1));
                verdicts.push((key, v));
                v
            };
            if matches!(verdict, Liveness::Dead | Liveness::Gone) {
                self.release_entry(i);
            }
        }
    }

    pub fn sup(&self) -> &Super {
        // SAFETY: the first page of the window exists since creation and holds the superblock (all atomics)
        unsafe { &*self.window.as_ptr().cast() }
    }

    fn at<T>(&self, offset: usize) -> *mut T {
        self.window.as_ptr().wrapping_add(offset).cast()
    }

    /// The address of a public offset in this process.
    pub fn ptr(&self, off: u64) -> *mut u8 {
        self.space.region().base().as_ptr().wrapping_add(usize::try_from(off).expect("public offset"))
    }

    /// The public offset of an absolute address in this process, when it lies in the public region.
    pub fn offset_of(&self, addr: usize) -> Option<u64> {
        let base = self.space.region().base().as_ptr().addr();
        addr.checked_sub(base).filter(|&o| o < REGION && o != 0).map(|o| o as u64)
    }

    /// The public offset falls within this heap's window.
    pub fn owns(&self, off: u64) -> bool {
        usize::try_from(off).is_ok_and(|o| o / WINDOW == self.index)
    }

    fn pool(&self) -> &Pool {
        self.pool.get_or_init(|| Pool::new(self.threads))
    }

    /// Make a part cover at least up to window offset `end`.
    fn commit(&self, segment: Segment, end: usize) {
        let committed = &self.sup().committed[segment.index()];
        let have = usize::try_from(committed.load(Ordering::Acquire)).expect("offset within the window");
        if have >= end {
            return;
        }
        let (start, limit) = self.layout.segment(segment);
        let to = end.next_multiple_of(PAGE).min(limit);
        let from = have.max(start);
        // Nothing used in this part yet (for example a collection with no entries): nothing to extend (fallocate rejects length 0)
        if to <= from {
            return;
        }
        self.backing.commit(from, to - from);
        committed.fetch_max(to as u64, Ordering::AcqRel);
    }

    fn type_entry(&self, ty: u32) -> &TypeEntry {
        // SAFETY: the index comes from the type table cursor, and its page was extended; entries are all atomics
        unsafe { &*self.at::<TypeEntry>(self.layout.types).add(ty as usize) }
    }

    /// The pointer map of type `ty`.
    pub fn edges(&self, ty: u32) -> &[u32] {
        let e = self.type_entry(ty);
        let (map, n) = (e.map.load(Ordering::Relaxed) as usize, e.edges.load(Ordering::Relaxed) as usize);
        // SAFETY: ready is set only after the pointer map is written, and readers (collections after objects of this type were allocated) come after it
        unsafe { core::slice::from_raw_parts(self.at::<u32>(self.layout.map).add(map), n) }
    }

    pub fn has_edges(&self, ty: u32) -> bool {
        self.type_entry(ty).edges.load(Ordering::Relaxed) != 0
    }

    /// The type number: the last one in the buffer first, then this process's cache, finally look it up in (or register it in) the type table.
    pub fn type_of<T: Shared>(&self, tlab: &Tlab) -> u32 {
        let tag = type_name::<T>().as_ptr().addr();
        if let Some((t, ty)) = tlab.last.get()
            && t == tag
        {
            return ty;
        }
        let ty = self.types.iter().find(|(t, _)| *t == tag).map_or_else(
            || {
                let ty = self.register::<T>();
                self.types.push((tag, ty));
                ty
            },
            |&(_, ty)| ty,
        );
        tlab.last.set(Some((tag, ty)));
        ty
    }

    /// Look the type up in the type table (another thread / process may have registered it), registering it if missing: the pointer map goes into the pointer map area and the entry is marked ready last.
    /// Two threads registering the same type at once get two numbers with identical pointer maps, which is harmless.
    fn register<T: Shared>(&self) -> u32 {
        let (map, key) = Fnv::of::<T>();
        let sup = self.sup();
        let known = (sup.types.load(Ordering::Acquire) as usize).min(TYPE_CAP);
        if let Some(ty) = (0..u32::try_from(known).expect("type table capacity")).find(|&ty| self.type_entry(ty).has_key(key)) {
            return ty;
        }
        let ty = sup.types.fetch_add(1, Ordering::AcqRel);
        assert!((ty as usize) < TYPE_CAP, "the public heap's type table is full");
        let edges = map.as_slice();
        let n = u32::try_from(edges.len()).expect("pointer slot count");
        let start = sup.map.fetch_add(n, Ordering::AcqRel);
        assert!((start + n) as usize <= MAP_CAP, "the public heap's pointer maps are full");
        self.commit(Segment::Types, self.layout.types + (ty as usize + 1) * size_of::<TypeEntry>());
        self.commit(Segment::Map, self.layout.map + (start + n) as usize * size_of::<u32>());
        // SAFETY: [start, start + n) is the part of the pointer map area this thread just took, with memory
        unsafe { self.at::<u32>(self.layout.map).add(start as usize).copy_from_nonoverlapping(edges.as_ptr(), edges.len()) };
        let e = self.type_entry(ty);
        e.edges.store(n, Ordering::Relaxed);
        e.map.store(start, Ordering::Relaxed);
        e.size.store(u32::try_from(size_of::<T>()).expect("objects are at most 4 GiB"), Ordering::Relaxed);
        for (k, v) in e.key.iter().zip(key) {
            k.store(v, Ordering::Relaxed);
        }
        e.ready.store(1, Ordering::Release);
        ty
    }

    /// Whether `ty` in the type table is `T` (comparing type keys: type name + size + pointer map).
    fn same_type<T: Shared>(&self, ty: u32) -> bool {
        let (_, key) = Fnv::of::<T>();
        self.type_entry(ty).has_key(key)
    }

    fn entry(&self, i: u32) -> &Entry {
        // SAFETY: the index comes from the entry table cursor (or the returned stack), and its page was extended; entries are all atomics
        unsafe { &*self.at::<Entry>(self.layout.entries).add(i as usize) }
    }

    /// Take an entry for public offset `off`, held by this attachment (only inside this heap's mutate).
    pub fn claim_entry(&self, off: u64) -> u32 {
        self.claim_entry_for(off, u32::try_from(self.slot + 1).expect("attachment slot"), 0)
    }

    /// As [`Inner::claim_entry`], held by `holder` (another public heap: [`Inner::holder_word`], with `incarnation` its identity).
    pub fn claim_entry_for(&self, off: u64, holder: u32, incarnation: u64) -> u32 {
        let sup = self.sup();
        let i = Stack::on(&sup.free_entries).pop(|i| &self.entry(i).next).unwrap_or_else(|| {
            let i = sup.entries.fetch_add(1, Ordering::AcqRel);
            assert!((i as usize) < self.layout.entry_cap, "the public heap's entry table is full");
            self.commit(Segment::Entries, self.layout.entries + (i as usize + 1) * size_of::<Entry>());
            i
        });
        let e = self.entry(i);
        e.holder.store(holder, Ordering::Relaxed);
        e.holder_incarnation.store(incarnation, Ordering::Relaxed);
        e.ptr.store(off, Ordering::Release);
        i
    }

    /// The entry is owned by nobody (holder 0): cleaning up dead processes and rebuilding registrations in group collections leave it alone.
    pub fn disown_entry(&self, i: u32) {
        self.entry(i).holder.store(0, Ordering::Release);
    }

    /// A handle is dropped: clear the entry and put it back on the stack (at any time, on any thread).
    pub fn release_entry(&self, i: u32) {
        let e = self.entry(i);
        // Clear the holder first: cleanup of dead processes finds unreturned entries by holder, and an entry already returned must not be returned twice
        e.holder.store(0, Ordering::Release);
        e.ptr.store(0, Ordering::Release);
        Stack::on(&self.sup().free_entries).push(i, &e.next);
    }

    fn state(&self, i: u32) -> &BlockState {
        // SAFETY: the index comes from the block cursor, and its page was extended; entries are all atomics
        unsafe { &*self.at::<BlockState>(self.layout.states).add(i as usize) }
    }

    fn block(&self, i: u32) -> Block {
        // SAFETY: block i of the block area, opened (with memory), block aligned (the window is window aligned and the block area starts block aligned)
        unsafe { Block::at(self.at(self.layout.blocks + i as usize * BLOCK)) }
    }

    /// Open `n` consecutive new blocks at the block cursor (extending the file) and return the first index.
    fn open_blocks(&self, n: usize) -> u32 {
        let count = u32::try_from(n).expect("block count");
        let i = self.sup().blocks.fetch_add(count, Ordering::AcqRel);
        assert!(i as usize + n <= self.layout.max_blocks, "the public heap's window is used up");
        self.commit(Segment::States, self.layout.states + (i as usize + n) * size_of::<BlockState>());
        self.backing.commit(self.layout.blocks + i as usize * BLOCK, n * BLOCK);
        i
    }

    /// Take a block for an allocation buffer: a swept one from the available stack first, otherwise open a new block.
    fn acquire(&self) -> Block {
        let sup = self.sup();
        sup.allocated.fetch_add(BLOCK as u64, Ordering::Relaxed);
        let i = Stack::on(&sup.available).pop(|i| &self.state(i).next).unwrap_or_else(|| {
            let i = self.open_blocks(1);
            self.state(i).kind.store(Kind::Small as u32, Ordering::Release);
            i
        });
        let b = self.block(i);
        b.reused();
        b
    }

    /// Large objects: open a run of new blocks, with the object header at the start of the first one.
    /// Large objects: open a run of new blocks, with the object header at the start of the first one; returns (address, first block index, block count). The blocks' kind is registered once the object is written
    /// ([`Inner::publish_large`]): if the process dies in between, the blocks stay "never opened" and sweeping does not read a half-written header.
    fn alloc_large(&self, bytes: usize) -> (*mut u8, u32, u32) {
        let n = bytes.div_ceil(BLOCK);
        let i = self.open_blocks(n);
        self.sup().allocated.fetch_add((n * BLOCK) as u64, Ordering::Relaxed);
        (self.block(i).line(0), i, u32::try_from(n).expect("block count"))
    }

    /// The large object is written: register the blocks' kind, tail first and head last (sweeping recognizes a large object from its head).
    fn publish_large(&self, first: u32, n: u32) {
        for j in first + 1..first + n {
            self.state(j).kind.store(Kind::LargeTail as u32, Ordering::Release);
        }
        self.state(first).kind.store(Kind::LargeHead as u32, Ordering::Release);
    }

    /// Collect (the handshake already holds back the mutators of every heap in `members`): mark in parallel from the entry tables, then sweep each heap in parallel.
    ///
    /// The epoch is the larger of "last completed" and "last started" + 1: if the previous collection was killed half way, objects it marked must not count as marked this time.
    /// Other processes may die in the middle of opening blocks / taking entries (cursor advanced, file not yet extended): first extend those two tables up to their cursors so scanning never touches pages without memory.
    ///
    /// For several heaps together: entries whose holder is a heap of the group are not roots (tracing decides), and after marking these registrations are rebuilt from the cross-heap edges traced — new ones registered first,
    /// old ones returned after, so being killed in between leaves only extras (a single collection taking over still treats them as roots).
    /// `extra`: additional roots for a single collection (data addresses of objects in the first heap, traced by the owner's private heap).
    fn collect_members(members: &[&Self], extra: &[usize], shrink: bool) {
        let in_group = |e: &Entry| members.len() > 1 && Self::held_by(members, e.holder.load(Ordering::Acquire), e.holder_incarnation.load(Ordering::Acquire));
        let mut epochs = Vec::with_capacity(members.len());
        let mut roots = Vec::new();
        let mut blocks = 0;
        for (m, heap) in members.iter().enumerate() {
            let sup = heap.sup();
            let last = sup.epoch.load(Ordering::Acquire).max(sup.marking.load(Ordering::Acquire));
            let epoch = last.checked_add(1).filter(|&e| e <= MAX_EPOCH).expect("out of epochs");
            sup.marking.store(epoch, Ordering::Release);
            epochs.push(epoch);
            let n = sup.blocks.load(Ordering::Acquire) as usize;
            blocks += n;
            heap.commit(Segment::States, heap.layout.states + n * size_of::<BlockState>());
            heap.commit(Segment::Entries, heap.layout.entries + sup.entries.load(Ordering::Acquire) as usize * size_of::<Entry>());
            heap.release_dead_holders();
            for i in 0..sup.entries.load(Ordering::Acquire) {
                let e = heap.entry(i);
                let off = e.ptr.load(Ordering::Acquire);
                if off != 0 && !in_group(e) {
                    roots.push((m, PHeader::of(heap.ptr(off)).expose_provenance()));
                }
            }
        }
        roots.extend(extra.iter().map(|&data| (0, PHeader::of(core::ptr::with_exposed_provenance(data)).expose_provenance())));
        let marked: Vec<Member<'_>> = members.iter().zip(&epochs).map(|(&heap, &epoch)| Member { heap, epoch }).collect();
        let found = Injector::new();
        let pool = members[0].pool();
        let grey = Injector::new();
        Marker::new(&marked, (members.len() > 1).then_some(&found), &grey, pool.sized(blocks)).run(pool, &roots);
        if members.len() > 1 {
            Self::reregister(members, &found);
        }
        for (heap, epoch) in members.iter().zip(epochs) {
            heap.sweep_all(epoch, shrink);
        }
    }

    /// After a group collection marks: the cross-heap registrations inside the group are replaced by the ones traced this time (one per (source heap, target) pair). New ones first, old ones returned after.
    fn reregister(members: &[&Self], found: &Injector<(usize, u64)>) {
        let old: Vec<Vec<u32>> = members
            .iter()
            .map(|heap| {
                (0..heap.sup().entries.load(Ordering::Acquire))
                    .filter(|&i| {
                        let e = heap.entry(i);
                        e.ptr.load(Ordering::Acquire) != 0 && Self::held_by(members, e.holder.load(Ordering::Acquire), e.holder_incarnation.load(Ordering::Acquire))
                    })
                    .collect()
            })
            .collect();
        let mut edges = Vec::new();
        loop {
            match found.steal() {
                Steal::Success(e) => edges.push(e),
                Steal::Retry => {}
                Steal::Empty => break,
            }
        }
        edges.sort_unstable();
        edges.dedup();
        for (source, off) in edges {
            let target = members.iter().find(|m| m.owns(off)).expect("every edge found points into a heap of the group");
            target.claim_entry_for(off, members[source].holder_word(target), members[source].incarnation());
        }
        for (heap, old) in members.iter().zip(old) {
            for i in old {
                heap.release_entry(i);
            }
        }
    }

    /// Sweep (marking with `epoch` is done): empty the available stack, reclassify every block and publish the epoch.
    fn sweep_all(&self, epoch: u32, shrink: bool) {
        let sup = self.sup();
        let spare = if shrink { 0 } else { (usize::try_from(sup.allocated.load(Ordering::Relaxed)).unwrap_or(usize::MAX) / BLOCK).max(MIN_SPARE_BLOCKS) };
        let blocks = sup.blocks.load(Ordering::Acquire) as usize;
        let pool = self.pool();
        let workers = pool.sized(blocks);
        // Empty the available stack; blocks with free lines are pushed again by sweeping
        sup.available.store(0, Ordering::Release);
        let cursor = AtomicUsize::new(0);
        let swept = pool.run(workers, |_| self.sweep_some(&cursor, blocks, epoch));
        let (mut live, mut empty) = (0, Vec::new());
        for (bytes, dead, emptied) in swept {
            live += bytes;
            empty.extend(emptied);
            // Dead large objects are taken apart only after every block is swept: their blocks become small-object blocks, and workers still sweeping must not sweep them again as small blocks
            for head in dead {
                self.free_large(head, &mut empty);
            }
        }
        empty.sort_unstable();
        self.punch_blocks(empty.get(spare..).unwrap_or_default());
        sup.live.store(live as u64, Ordering::Relaxed);
        sup.allocated.store(0, Ordering::Relaxed);
        sup.collections.fetch_add(1, Ordering::AcqRel);
        sup.epoch.store(epoch, Ordering::Release);
    }

    /// Sweep batches of blocks taken in turn; returns live bytes, dead large objects (first block index) and small-object blocks swept empty.
    fn sweep_some(&self, cursor: &AtomicUsize, blocks: usize, epoch: u32) -> (usize, Vec<u32>, Vec<u32>) {
        let (mut live, mut dead, mut empty) = (0, Vec::new(), Vec::new());
        loop {
            let start = cursor.fetch_add(SWEEP_BATCH, Ordering::AcqRel);
            if start >= blocks {
                return (live, dead, empty);
            }
            for i in start..(start + SWEEP_BATCH).min(blocks) {
                let i = u32::try_from(i).expect("block index");
                match Kind::from(self.state(i).kind.load(Ordering::Acquire)) {
                    Kind::Small => {
                        // SAFETY: marking is done and there are no mutators; the block holds public heap objects
                        let (state, bytes) = unsafe { self.block(i).sweep::<PHeader>(epoch, None) };
                        live += bytes;
                        if matches!(state, State::Empty) {
                            empty.push(i);
                        }
                        if !matches!(state, State::Full) {
                            Stack::on(&self.sup().available).push(i, &self.state(i).next);
                        }
                    }
                    Kind::LargeHead => {
                        // SAFETY: a large object's header is at the start of its first block
                        let h = unsafe { &*self.block(i).line(0).cast::<Core>() };
                        if h.is_marked(epoch) {
                            live += h.bytes();
                        } else {
                            dead.push(i);
                        }
                    }
                    Kind::LargeTail | Kind::Unused => {}
                }
            }
        }
    }

    /// Take apart a dead large object: zero the metadata of its blocks, turn them into small-object blocks and push them on the available stack. Tail blocks first, the head last:
    /// if the process is killed half way the head remains, and the next collection takes it apart again, skipping blocks already done (no longer LargeTail), so nothing is pushed twice.
    fn free_large(&self, head: u32, empty: &mut Vec<u32>) {
        // SAFETY: a large object's header is at the start of its first block, not yet taken apart
        let n = unsafe { &*self.block(head).line(0).cast::<Core>() }.bytes().div_ceil(BLOCK);
        let tails = head + 1..head + u32::try_from(n).expect("block count");
        for j in tails.filter(|&j| Kind::from(self.state(j).kind.load(Ordering::Acquire)) == Kind::LargeTail).chain([head]) {
            let b = self.block(j);
            // SAFETY: the object is dead (no destructor) and the block holds no other objects
            unsafe { b.reset() };
            b.reused();
            self.state(j).kind.store(Kind::Small as u32, Ordering::Release);
            Stack::on(&self.sup().available).push(j, &self.state(j).next);
            empty.push(j);
        }
    }

    /// Give empty blocks (indexes sorted) back to the system: one hole per run of adjacent blocks. The block metadata is zeroed too, and all zeros is exactly a valid empty block (as [`Block::reset`]);
    /// the block state table is not inside the blocks and is unaffected, so they stay on the available stack and fault in zero pages when taken again.
    fn punch_blocks(&self, sorted: &[u32]) {
        for run in sorted.chunk_by(|a, b| a + 1 == *b) {
            let (first, n) = (run[0] as usize, run.len());
            self.backing.punch(self.window.as_ptr(), self.layout.blocks + first * BLOCK, n * BLOCK);
        }
    }

    /// The birth stamp of objects this heap allocates in epoch `epoch`: the heap identity and the epoch mixed together. Two objects occupying the same place one after the other were allocated in different epochs
    /// (a place frees up only after a sweep, and the epoch always grows after a sweep), and a reused heap number has a different identity, so stamps collide only with probability about 2^-[`STAMP_BITS`].
    pub fn stamp(&self, epoch: u32) -> u32 {
        let mut z = self.incarnation() ^ u64::from(epoch).wrapping_mul(SPLITMIX_GAMMA);
        z = (z ^ (z >> SPLITMIX_SHIFTS[0])).wrapping_mul(SPLITMIX_MULS[0]);
        z = (z ^ (z >> SPLITMIX_SHIFTS[1])).wrapping_mul(SPLITMIX_MULS[1]);
        u32::try_from((z ^ (z >> SPLITMIX_SHIFTS[2])) >> u32::BITS).expect("the high 32 bits")
    }

    /// When a weak reference word points at a live object of this heap (an object header at that place and a matching birth stamp), give its data address. Safe even for words sent by other processes:
    /// it reads only table entries and blocks that have memory, and out-of-range offsets, unopened blocks, the middle of an object and the following blocks of a large object are all None.
    /// Call only inside this heap's mutate (collections wait for mutators to leave, so the object cannot be collected between the check and its use).
    pub fn locate(&self, word: WeakWord) -> Option<*mut u8> {
        let off = word.offset();
        if off == 0 || !self.owns(off) {
            return None;
        }
        let local = usize::try_from(off).ok()? - self.index * WINDOW;
        let rel = local.checked_sub(self.layout.blocks)?;
        let i = rel / BLOCK;
        let sup = self.sup();
        if i >= sup.blocks.load(Ordering::Acquire) as usize {
            return None;
        }
        let states = usize::try_from(sup.committed[Segment::States.index()].load(Ordering::Acquire)).ok()?;
        if self.layout.states + (i + 1) * size_of::<BlockState>() > states {
            return None;
        }
        let i = u32::try_from(i).ok()?;
        let b = self.block(i);
        let data = self.at::<u8>(local);
        let header = data.wrapping_sub(PHEADER);
        let at_object = match Kind::from(self.state(i).kind.load(Ordering::Acquire)) {
            Kind::Small if header.addr() >= b.line(Block::FIRST_LINE).addr() => {
                let (word, bit) = b.start_bit(header);
                // SAFETY: the start bitmap is in the block's metadata, and the block was opened (it has memory)
                unsafe { &*word }.load(Ordering::Acquire) & bit != 0
            }
            Kind::LargeHead => core::ptr::eq(header, b.line(0)),
            _ => false,
        };
        // SAFETY: checked above that header is a fully written object header
        (at_object && unsafe { (*header.cast::<PHeader>()).stamp } & STAMP_MASK == word.stamp()).then_some(data)
    }

    /// The type of the object at `data` (checked by [`Inner::locate`]) is `T`: first look at this process's cached type number, then compare type keys if it does not match.
    pub fn is_type<T: Shared>(&self, data: *const u8) -> bool {
        // SAFETY: see above
        let ty = unsafe { (*PHeader::of(data)).ty };
        let tag = type_name::<T>().as_ptr().addr();
        self.types.iter().any(|&(t, k)| t == tag && k == ty) || self.same_type::<T>(ty)
    }

    /// The type key of the object at `data`.
    fn key_of(&self, data: *const u8) -> [u64; 2] {
        // SAFETY: data is a live object of this heap, and its type number comes from a registered type
        let ty = unsafe { (*PHeader::of(data)).ty };
        self.type_entry(ty).key.each_ref().map(|k| k.load(Ordering::Relaxed))
    }
}

#[cfg(crossheap)]
impl Drop for Inner {
    fn drop(&mut self) {
        // Every handle is dropped (entries returned) and there are no mutators: free the attachment slot
        if self.slot < ATTACH_CAP {
            let a = self.attachment(self.slot);
            a.start.store(0, Ordering::Relaxed);
            a.pid.store(0, Ordering::Release);
        }
        // The creating handle (the owner) lets go without persist: the heap dies with it — mark it dead (processes still mapping it notice) and delete the file and marker.
        // The superblock must be read and written before unmapping
        if let Backing::File { path: Some(path), owner: true, .. } = &self.backing
            && self.sup().persist.load(Ordering::Acquire) == 0
        {
            self.sup().dead.store(1, Ordering::Release);
            Registry::of(path).bury(path, self.index, self.incarnation());
        }
        // SAFETY: the last reference: no mutators, every handle dropped
        unsafe { self.space.region().unmap(self.index) };
        self.space.release(self.index);
        let spare = *self.spare.get_mut();
        if !spare.is_null() {
            // SAFETY: from Box::into_raw
            drop(unsafe { Box::from_raw(spare) });
        }
    }
}

/// A public heap. Several threads can [`CrossHeap::mutate`] at once; when the same heap is also mapped elsewhere (another region, another process), the handshake still excludes them mutually.
#[cfg(crossheap)]
pub struct CrossHeap(Arc<Inner>);

#[cfg(crossheap)]
impl From<Inner> for CrossHeap {
    fn from(inner: Inner) -> Self {
        Self(Arc::new(inner))
    }
}

/// The heap is dead (its owner is gone).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(crossheap)]
pub struct HeapDead;

#[cfg(crossheap)]
impl core::fmt::Display for HeapDead {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("the public heap is dead (the process that created it is gone)")
    }
}

#[cfg(crossheap)]
impl std::error::Error for HeapDead {}

/// Public heap statistics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg(crossheap)]
pub struct CrossHeapStats {
    /// Live bytes after the last collection (including headers)
    pub live: usize,
    /// Bytes newly taken since the last collection
    pub allocated: usize,
    /// Blocks opened
    pub blocks: usize,
    pub collections: u64,
    /// Times a collector was killed (half way through) and someone else took over
    pub takeovers: u64,
}

#[cfg(crossheap)]
impl CrossHeap {
    /// Heap number (= window number).
    pub fn id(&self) -> usize {
        self.0.index
    }

    /// Keep the heap file and heap-number marker when the creating handle is dropped (after the main process exits or restarts, other processes can still [`crate::CrossHeapSpace::open`] it and continue).
    pub fn persist(&self) {
        self.0.sup().persist.store(1, Ordering::Release);
    }

    /// The heap is alive: its owner (the creating process) is still there, or [`CrossHeap::persist`] was called. Once the owner is gone the heap is dead: on finding that, mark it dead and delete the heap file and marker;
    /// this process should then let go of its handles (the mapping goes and memory returns only when the last one is dropped).
    pub fn is_alive(&self) -> bool {
        self.0.is_alive()
    }

    /// Work in this heap and `other` at once (two different public heaps): always enters the lower heap number first. Nesting into several public heaps in this order everywhere,
    /// group collections (holding back and waiting for each heap in heap-number order) never wait on mutators; reverse nesting is not wrong either, only slower as collections give up and retry.
    pub fn mutate_with<R>(&self, other: &CrossHeap, f: impl for<'a, 'b> FnOnce(&CrossHeapMutator<'a>, &CrossHeapMutator<'b>) -> R) -> R {
        assert!(self.id() != other.id(), "two different public heaps");
        if self.id() < other.id() { self.mutate(|ma| other.mutate(|mb| f(ma, mb))) } else { other.mutate(|mb| self.mutate(|ma| f(ma, mb))) }
    }

    /// As [`CrossHeap::mutate`], but does not enter a dead heap (owner gone) and returns [`HeapDead`]. Use it for heaps created by other processes.
    pub fn try_mutate<R>(&self, f: impl for<'p> FnOnce(&CrossHeapMutator<'p>) -> R) -> Result<R, HeapDead> {
        if !self.0.alive_cached() {
            return Err(HeapDead);
        }
        Ok(self.mutate(f))
    }

    /// Slots taken in the attachment table (places mapping this heap: processes, or several regions in one process; including dead ones not yet cleaned up).
    pub fn attached(&self) -> usize {
        (0..ATTACH_CAP).filter(|&i| self.0.attachment(i).pid.load(Ordering::Acquire) != 0).count()
    }

    /// Work on the heap; can be called on several threads at once. Objects allocated in the closure survive the next collection only if they hang from a [`CrossHeapRoot`] (directly or indirectly).
    pub fn mutate<R>(&self, f: impl for<'p> FnOnce(&CrossHeapMutator<'p>) -> R) -> R {
        let spare = self.0.spare.swap(null_mut(), Ordering::AcqRel);
        let mut tlab = if spare.is_null() {
            Box::default()
        } else {
            // SAFETY: spare only ever holds Tlabs from Box::into_raw, and after the swap it is ours alone
            unsafe { Box::from_raw(spare) }
        };
        let r = {
            let _gate = Gate::enter(&self.0);
            tlab.adopt((Arc::as_ptr(&self.0).addr(), self.0.sup().collections.load(Ordering::Acquire)));
            // Collection happens only with no mutators, so the epoch does not change within one mutate
            let epoch = self.0.sup().epoch.load(Ordering::Acquire);
            f(&CrossHeapMutator { heap: &self.0, tlab: &tlab, epoch, stamp: self.0.stamp(epoch), _brand: PhantomData })
        };
        let tlab = Box::into_raw(tlab);
        if self.0.spare.compare_exchange(null_mut(), tlab, Ordering::AcqRel, Ordering::Relaxed).is_err() {
            // SAFETY: it was not put back, so it is still ours alone
            drop(unsafe { Box::from_raw(tlab) });
        }
        r
    }

    /// Stop-the-world collection: set the request, wait until every place attached to this heap (including other processes) has left mutate, then mark in parallel + sweep in parallel.
    /// If someone else is collecting, wait for them to finish and return; take over if the collector was killed. Dead processes are not waited for, and the entries they hold are returned here.
    /// Do not call it inside this heap's mutate (it would wait forever for itself to leave).
    pub fn collect(&self) {
        self.0.collect();
    }

    /// As [`CrossHeap::collect`], and every empty block found by sweeping goes back to the system (ordinary collections keep some spares according to the allocation since the last one).
    /// Allocating into returned blocks later faults the pages in again.
    pub fn shrink(&self) {
        self.0.collect_with(true);
    }

    /// The heap file's descriptor (anonymous heaps have none): pass it to another process through `SCM_RIGHTS`, and the receiver opens it with [`crate::CrossHeapSpace::open_fd`]
    /// without needing access to the heap file's path.
    pub fn fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        use std::os::fd::AsFd;
        match &self.0.backing {
            Backing::File { file, .. } => Some(file.as_fd()),
            Backing::Anonymous => None,
        }
    }

    /// Mail received so far (see [`CrossHeapMutator::post`]).
    pub fn mail(&self) -> u32 {
        self.0.sup().mail.load(Ordering::SeqCst)
    }

    /// Sleep while the mail count is still `seen`, at most `timeout` (waking does not guarantee new mail; check [`CrossHeap::mail`] again).
    /// Do not wait inside this heap's mutate (collections would wait forever for you to leave).
    pub fn wait_mail(&self, seen: u32, timeout: Duration) {
        self.0.sup().mail.wait(seen, timeout);
    }

    /// Collect several public heaps (all mapped in this process) together, reclaiming cycles across heaps: [`crate::CrossHeapRef`] registrations held by heaps of the group are not roots,
    /// tracing decides their liveness; after marking, the cross-heap registrations inside the group are replaced by the ones traced. The same heap must not appear twice.
    pub fn collect_group(heaps: &[&CrossHeap]) {
        // Dead heaps do not take part: the registrations they hold are returned by each heap's collection according to liveness. Heaps collected only by their owner (attached to a private heap as old generation) do not take part either
        let mut members: Vec<&Inner> = heaps.iter().map(|h| &*h.0).filter(|m| m.is_alive() && m.sup().owner_only.load(Ordering::Acquire) == 0).collect();
        members.sort_by_key(|m| m.index);
        assert!(members.windows(2).all(|w| w[0].index != w[1].index), "the same public heap must not appear twice in a group");
        if !members.is_empty() {
            Inner::collect_group(&members);
        }
    }

    /// Heap identity (chosen at random at creation; heap numbers are reused, identities are not).
    pub fn incarnation(&self) -> u64 {
        self.0.incarnation()
    }

    pub fn stats(&self) -> CrossHeapStats {
        let sup = self.0.sup();
        CrossHeapStats {
            live: usize::try_from(sup.live.load(Ordering::Relaxed)).unwrap_or(usize::MAX),
            allocated: usize::try_from(sup.allocated.load(Ordering::Relaxed)).unwrap_or(usize::MAX),
            blocks: sup.blocks.load(Ordering::Relaxed) as usize,
            collections: sup.collections.load(Ordering::Relaxed),
            takeovers: sup.takeovers.load(Ordering::Relaxed),
        }
    }
}

/// One collector worker's promoter in a young collection: copies shareable young objects into the owner's cross-heap (with its own allocation buffer, so workers do not interfere).
/// The collector is the cross-heap's owner and only the owner collects it, so the cross-heap is not collected while copying.
#[cfg(crossheap)]
pub struct Promoter<'h> {
    heap: &'h Inner,
    tlab: Tlab,
    /// The cross-heap's last completed epoch (to judge free lines) and the birth stamp of this batch of objects
    epoch: u32,
    stamp: u32,
}

#[cfg(crossheap)]
impl<'h> Promoter<'h> {
    pub fn new(heap: &'h Inner) -> Self {
        let mut tlab = Tlab::default();
        tlab.adopt((core::ptr::from_ref(heap).addr(), heap.sup().collections.load(Ordering::Acquire)));
        let epoch = heap.sup().epoch.load(Ordering::Acquire);
        Self { heap, tlab, epoch, stamp: heap.stamp(epoch) }
    }

    /// The number of `T` in the cross-heap's type table (registered if missing).
    pub fn type_of<T: Shared>(&self) -> u32 {
        self.heap.type_of::<T>(&self.tlab)
    }

    /// Copy the data of private heap object `from` into the cross-heap (type number `ty`) and return the new data address.
    /// SAFETY (caller): `from` is a live object this thread claimed, and the type number is its own answer.
    pub unsafe fn copy(&self, from: *const Header, ty: u32) -> *mut u8 {
        // SAFETY: see above
        let data = unsafe { &*from }.bytes() - HEADER;
        let bytes = (PHEADER + data).next_multiple_of(GRANULE);
        let large = bytes > LARGE;
        let (p, run) = if large {
            let (p, first, n) = self.heap.alloc_large(bytes);
            (p, Some((first, n)))
        } else {
            (self.tlab.bump(bytes, self.epoch, || self.heap.acquire(), |_| self.heap.acquire()), None)
        };
        // SAFETY: p is new memory of bytes bytes, granule aligned; the data is copied bytewise (shareable types have no destructor and their edges are absolute addresses)
        unsafe {
            p.cast::<PHeader>().write(PHeader::new(bytes, large, ty, self.stamp));
            PHeader::data(p.cast()).copy_from_nonoverlapping(Header::data(from), data);
        }
        match run {
            Some((first, n)) => self.heap.publish_large(first, n),
            None => Block::containing(p).set_start(p),
        }
        PHeader::data(p.cast())
    }
}

/// Operations for this crate only: the trait lives in a private module and is not exported, so it is unreachable outside the crate.
#[cfg(crossheap)]
pub trait CrossHeapInternal {
    fn is(&self, inner: &Arc<Inner>) -> bool;
}

#[cfg(crossheap)]
impl CrossHeapInternal for CrossHeap {
    fn is(&self, inner: &Arc<Inner>) -> bool {
        Arc::ptr_eq(&self.0, inner)
    }
}


/// Inside mutate: increment this attachment's count on the way in and decrement on the way out (also on panic).
///
/// Entering increments the count before checking the collection request, and a collector sets the request before checking the counts (all SeqCst): at least one side sees the other, so no
/// mutator slips in during a collection. Seeing a request, back out and wait; if the collector died (the request stays set), take over and finish the collection.
#[cfg(crossheap)]
struct Gate<'h> {
    inside: &'h AtomicU32,
    request: &'h AtomicU32,
}

#[cfg(crossheap)]
impl<'h> Gate<'h> {
    fn enter(heap: &'h Inner) -> Self {
        let request = &heap.sup().request;
        let a = heap.attachment(heap.slot);
        let (inside, waiting) = (&a.inside, &a.waiting);
        // Record it while held at the door and cancel it once inside (see Attachment::waiting)
        let mut queued = false;
        let leave_queue = |queued: &mut bool| {
            if core::mem::take(queued) && waiting.fetch_sub(1, Ordering::SeqCst) == 1 {
                waiting.wake_all();
            }
        };
        loop {
            let owner = request.load(Ordering::SeqCst);
            if owner != 0 {
                if heap.alive(owner as usize - 1) {
                    if !queued {
                        waiting.fetch_add(1, Ordering::SeqCst);
                        queued = true;
                    }
                    request.wait(owner, POLL);
                } else {
                    // The collector died: take over (cancel the queueing first, since the collector waits for those queued)
                    leave_queue(&mut queued);
                    heap.collect();
                }
                continue;
            }
            inside.fetch_add(1, Ordering::SeqCst);
            if request.load(Ordering::SeqCst) == 0 {
                leave_queue(&mut queued);
                return Self { inside, request };
            }
            inside.fetch_sub(1, Ordering::SeqCst);
            inside.wake_all();
        }
    }
}

#[cfg(crossheap)]
impl Drop for Gate<'_> {
    fn drop(&mut self) {
        self.inside.fetch_sub(1, Ordering::SeqCst);
        if self.request.load(Ordering::SeqCst) != 0 {
            self.inside.wake_all();
        }
    }
}

/// The allocator and brand of one public heap mutate; used only on the calling thread.
#[cfg(crossheap)]
pub struct CrossHeapMutator<'p> {
    heap: &'p Arc<Inner>,
    tlab: &'p Tlab,
    /// The last completed epoch (to judge free lines)
    epoch: u32,
    /// The birth stamp of objects allocated in this mutate ([`Inner::stamp`])
    stamp: u32,
    _brand: Invariant<'p>,
}

#[cfg(crossheap)]
impl<'p> CrossHeapMutator<'p> {

    /// Allocate an object. The type must not have a destructor (checked at compile time).
    pub fn alloc<T: Shared>(&self, value: T) -> CrossHeapGc<'p, T> {
        const {
            assert!(!core::mem::needs_drop::<T>(), "public heap objects must not have destructors");
            assert!(align_of::<T>() <= GRANULE, "object data alignment must not exceed 8 bytes");
        };
        let bytes = (PHEADER + size_of::<T>()).next_multiple_of(GRANULE);
        let ty = self.heap.type_of::<T>(self.tlab);
        let large = bytes > LARGE;
        let (p, run) = if large {
            let (p, first, n) = self.heap.alloc_large(bytes);
            (p, Some((first, n)))
        } else {
            (self.alloc_small(bytes), None)
        };
        // SAFETY: p is new memory of bytes bytes, granule aligned, owned by this allocation
        let gc = unsafe {
            p.cast::<PHeader>().write(PHeader::new(bytes, large, ty, self.stamp));
            let data = PHeader::data(p.cast());
            data.cast::<T>().write(value);
            CrossHeapGc::from_data(data)
        };
        // Register only after writing (the start bit of a small object, the block kind of a large one): if the process dies in between, sweeping never reads a half-written object
        match run {
            Some((first, n)) => self.heap.publish_large(first, n),
            None => Block::containing(p).set_start(p),
        }
        gc
    }

    /// Make a handle (entry) for an object.
    pub fn root<B: CrossHeapBrand>(&self, gc: CrossHeapGc<'p, B::Of<'p>>) -> CrossHeapRoot<B> {
        let entry = self.heap.claim_entry(gc.offset());
        // SAFETY: a Gc's data pointer is non-null
        CrossHeapRoot::new(self.heap.clone(), entry, unsafe { NonNull::new_unchecked(gc.as_ptr().cast_mut().cast()) })
    }

    /// Get a new handle to the object of an entry number ([`CrossHeapRoot::index`], possibly from another mapping or process). None when the entry is empty
    /// or the object's type (type name + size + pointer map) does not match `B`.
    pub fn find<B: CrossHeapBrand>(&self, index: u32) -> Option<CrossHeapRoot<B>> {
        let sup = self.heap.sup();
        if index >= sup.entries.load(Ordering::Acquire) {
            return None;
        }
        let off = self.heap.entry(index).ptr.load(Ordering::Acquire);
        if off == 0 || !self.heap.owns(off) {
            return None;
        }
        // Inside mutate: a non-empty entry ⇒ the object is a root right now and does not die before the next collection; take our own entry below before leaving
        let data = self.heap.ptr(off);
        // SAFETY: the entry points at a live object of this heap
        let ty = unsafe { (*PHeader::of(data)).ty };
        if !self.heap.same_type::<B::Of<'p>>(ty) {
            return None;
        }
        let entry = self.heap.claim_entry(off);
        Some(CrossHeapRoot::new(self.heap.clone(), entry, NonNull::new(data).expect("the object address is non-null")))
    }

    /// Return an entry owned by nobody (left by [`CrossHeapRoot::persist`]); an empty entry or one still held by a process / heap is left alone and false returned.
    pub fn release(&self, index: u32) -> bool {
        let heap = self.heap;
        if index >= heap.sup().entries.load(Ordering::Acquire) {
            return false;
        }
        let e = heap.entry(index);
        if e.ptr.load(Ordering::Acquire) == 0 || e.holder.load(Ordering::Acquire) != 0 {
            return false;
        }
        heap.release_entry(index);
        true
    }

    /// Turn a private heap [`Gc`] pointing at an object of this heap (promoted by a young collection, see [`Heap::attach_cross_heap`]) into an object of this heap, to sign it or put it into
    /// this heap's slots; None if it does not point into this heap or the type is not `B`.
    pub fn adopt<B: CrossHeapBrand, T>(&self, gc: Gc<'_, T>) -> Option<CrossHeapGc<'p, B::Of<'p>>> {
        let off = self.heap.offset_of(gc.as_ptr().addr()).filter(|&off| self.heap.owns(off))?;
        let data = self.heap.ptr(off);
        // SAFETY: an object of this heap (promoted here) with a matching type; it is not collected within this mutate
        self.heap.is_type::<B::Of<'p>>(data).then(|| unsafe { CrossHeapGc::from_data(data) })
    }

    /// The signature of the object a private heap [`Gc`] points at, when it is an object of this heap (promoted here, see [`Heap::attach_cross_heap`]); None otherwise.
    /// Needs no brand: the type key is read from the object's header.
    pub fn sign_promoted<T>(&self, gc: Gc<'_, T>) -> Option<Signature> {
        let off = self.heap.offset_of(gc.as_ptr().addr()).filter(|&off| self.heap.owns(off))?;
        let data = self.heap.ptr(off);
        // SAFETY: a live Gc pointing into this heap is the data of a promoted object, with its header in front
        let stamp = unsafe { (*PHeader::of(data)).stamp };
        Some((WeakWord::from((off, stamp)), self.heap.incarnation(), self.heap.key_of(data)).into())
    }

    /// The object's signature: send it to another process, which opens this heap and finds the object with it ([`CrossHeapMutator::resolve`]). A signature keeps nothing alive.
    pub fn sign<T: Shared>(&self, gc: CrossHeapGc<'p, T>) -> Signature {
        (WeakWord::from(gc), self.heap.incarnation(), self.heap.key_of(gc.as_ptr().cast())).into()
    }

    /// The signature points at a live object of this heap with a matching type key (no Rust type needed; signatures may come from untrusted parties and checking only reads).
    pub fn verify(&self, sig: &Signature) -> bool {
        self.find_signed(sig).is_some()
    }

    /// Get an object by signature: when the signature points at a live object of this heap whose type is `B`, give it; otherwise None (the object was collected, the heap number changed owner, the type is wrong, or it is forged).
    /// The result is valid only within this mutate; to keep it, hang it from a [`CrossHeapRoot`] or write it into another object.
    pub fn resolve<B: CrossHeapBrand>(&self, sig: &Signature) -> Option<CrossHeapGc<'p, B::Of<'p>>> {
        let data = self.find_signed(sig)?;
        // SAFETY: find_signed checked a live object of this heap right now; it is not collected within this mutate
        self.heap.is_type::<B::Of<'p>>(data).then(|| unsafe { CrossHeapGc::from_data(data) })
    }

    fn find_signed(&self, sig: &Signature) -> Option<*mut u8> {
        if sig.incarnation() != self.heap.incarnation() {
            return None;
        }
        self.heap.locate(sig.into()).filter(|&data| self.heap.key_of(data) == sig.key())
    }

    /// Post an object to this heap's mailbox for the heap's owner to handle (other processes asking the owner to act for them): take an entry owned by no process pointing at it (so the letter stays even if the poster exits or is killed),
    /// push it into the mailbox and wake the owner waiting for mail. Returns the entry number. The owner takes it with [`CrossHeapMutator::take_mail`] and, when done,
    /// returns the entry with [`CrossHeapMutator::release`].
    pub fn post<T: Shared>(&self, gc: CrossHeapGc<'p, T>) -> u32 {
        let i = self.heap.claim_entry_for(gc.offset(), 0, 0);
        let sup = self.heap.sup();
        Stack::on(&sup.mailbox).push(i, &self.heap.entry(i).next);
        sup.mail.fetch_add(1, Ordering::SeqCst);
        sup.mail.wake_all();
        i
    }

    /// Take all mail from the mailbox (entry numbers, earliest post first). Get the objects by type with [`CrossHeapMutator::find`] and [`CrossHeapMutator::release`] them when done.
    pub fn take_mail(&self) -> Vec<u32> {
        Stack::on(&self.heap.sup().mailbox).take_all(|i| &self.heap.entry(i).next)
    }

    fn alloc_small(&self, bytes: usize) -> *mut u8 {
        self.tlab.bump(bytes, self.epoch, || self.heap.acquire(), |_| self.heap.acquire())
    }
}

/// Operations for this crate only: the trait lives in a private module and is not exported, so it is unreachable outside the crate.
#[cfg(crossheap)]
pub trait CrossHeapMutatorInternal {
    fn is(&self, inner: &Arc<Inner>) -> bool;

    /// The address of a public offset in this process.
    fn at(&self, off: u64) -> *mut u8;

    /// Heap number.
    fn index(&self) -> usize;

    /// Register an entry in this heap for a reference held by heap `holder`.
    fn register(&self, off: u64, holder: &CrossHeapMutator<'_>);

    /// Heap identity.
    fn incarnation(&self) -> u64;

    /// See [`Inner::locate`].
    fn locate(&self, word: WeakWord) -> Option<*mut u8>;

    /// See [`Inner::is_type`].
    fn is_type<T: Shared>(&self, data: *const u8) -> bool;
}

#[cfg(crossheap)]
impl CrossHeapMutatorInternal for CrossHeapMutator<'_> {
    fn is(&self, inner: &Arc<Inner>) -> bool {
        Arc::ptr_eq(self.heap, inner)
    }

    fn at(&self, off: u64) -> *mut u8 {
        self.heap.ptr(off)
    }

    fn index(&self) -> usize {
        self.heap.index
    }

    fn register(&self, off: u64, holder: &CrossHeapMutator<'_>) {
        self.heap.claim_entry_for(off, holder.heap.holder_word(self.heap), holder.heap.incarnation());
    }

    fn incarnation(&self) -> u64 {
        self.heap.incarnation()
    }

    fn locate(&self, word: WeakWord) -> Option<*mut u8> {
        self.heap.locate(word)
    }

    fn is_type<T: Shared>(&self, data: *const u8) -> bool {
        self.heap.is_type::<T>(data)
    }
}
