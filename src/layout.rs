//! Memory layout: size constants, object headers (the private heap's [`Header`] and the public heap's [`PHeader`] share [`Core`]), blocks and lines, and the tables in a public heap window.
//!
//! Heap size constants: block / line / granule / large object threshold.
//!
//! Object headers: one in front of every heap object, recording how to visit it (trace / relocate), how to finalize it, how big it is and which round marked it.
//!
//! The mark word normally holds an epoch; objects moved during evacuation use its top two bits: [`BUSY`] (a collector thread is copying it) and [`FORWARDED`] (moved;
//! the new address is written in the word that held the destructor). Epochs never use these two bits.
//!
//! Blocks: memory aligned to their own size, starting with metadata (line marks + the object start bitmap) followed by lines handed to small objects.
//!
//! - line marks hold epochs: during marking every live object writes this round's epoch into the lines it spans, so "line mark ≠ current epoch" means a free line
//! - the start bitmap has one bit per granule: sweeping uses it to find objects one by one and finalize the dead ones
//! - between two collections a block belongs to a single allocation buffer ([`crate::Tlab`]), so writing metadata during allocation is uncontended
//!
//! Layout of a public heap window: every position is fixed, so any process finds everything at the window start + a constant offset.
//!
//! ```text
//! 0          superblock + attachment table (1 page)
//! types      type table: one entry per type (type key, size, where its pointer map is)
//! map        pointer maps: offsets of the pointer slots of each type, packed one after another
//! entries    entry table: the only one; every reference into this heap from elsewhere takes an entry
//! states     block state table: one entry per block (kind, next on the available stack)
//! blocks     block area: taken on demand from here on
//! ```
//!
//! A new file has no pages; each part extends the file only as far as it is used ([`crate::shm::Region::commit`]). On tmpfs the extended part takes memory right away.
use core::mem::needs_drop;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::alloc::{Layout, handle_alloc_error};
#[cfg(any(not(target_os = "linux"), miri))]
use std::alloc::{alloc, dealloc};

#[cfg(all(target_os = "linux", not(miri)))]
use crossbeam_deque::{Injector, Steal};
use crossbeam_utils::Backoff;

use crate::Trace;
use crate::mark::{Tracer, TracerInternal};
#[cfg(crossheap)]
use crate::shm::WINDOW;

/// Block size: blocks are aligned to their own size, so masking the low bits of an object address gives its block's start
pub const BLOCK: usize = 32 * 1024;
/// Line size: the smallest unit reclaimed; holes are found line by line
pub const LINE: usize = 256;
/// Lines per block
pub const LINES: usize = BLOCK / LINE;
/// Granule: object starts and sizes are aligned to it; also the largest alignment allowed for object data
pub const GRANULE: usize = 8;
/// Granules per block (bits in the object start bitmap)
pub const GRANULES: usize = BLOCK / GRANULE;
/// Bits per word of the start bitmap
pub const BITS: usize = u64::BITS as usize;
/// Large object threshold (bytes including the header): above it an object is allocated on its own, outside blocks
pub const LARGE: usize = BLOCK / 2;
/// Large objects of at least this size get their own mmap (Linux): munmap returns them to the system immediately (large blocks freed to the global allocator are often kept)
#[cfg(all(target_os = "linux", not(miri)))]
pub const LARGE_MMAP: usize = 64 * 1024;

/// Estimating collector threads: one more thread is worthwhile only if each gets at least this many blocks / large objects
pub const UNITS_PER_WORKER: usize = 8;

/// Collection trigger floor: no collection while fewer bytes than this were taken since the last one
pub const MIN_TRIGGER: usize = 4 * 1024 * 1024;
/// Collection trigger factor: collect once newly taken bytes exceed "last live bytes × this"
pub const GROWTH: usize = 2;
/// collect_if_needed: while reference counts are untrustworthy, one full collection after this many young ones (reclaiming dead old objects and recounting)
pub const YOUNG_PER_FULL: usize = 4;
/// collect_if_needed: while reference counts are trustworthy (dead old objects are reclaimed by count), only one full collection per this many young ones (for cycles and stuck counts)
pub const YOUNG_PER_FULL_RC: usize = 16;
/// Evacuation candidates: blocks with less than this many live bytes at the last sweep (half a block)
pub const EVACUATE_LIVE: usize = BLOCK / 2;
/// Evacuate only when there are at least this many candidates
pub const EVACUATE_MIN: usize = 4;
/// Evacuate at most this many blocks at once (to bound this pause)
pub const EVACUATE_MAX: usize = usize::MAX;
/// collect_if_needed: when blocks take more than FRAG_NUM / FRAG_DEN times "live + what one round may allocate", the heap is fragmented and gets a full collection (with evacuation)
pub const FRAG_NUM: usize = 3;
pub const FRAG_DEN: usize = 2;
/// Empty blocks always kept for reuse after a collection
pub const MIN_SPARE_BLOCKS: usize = 4;
/// When the last young collection kept more than NUM / DEN of what it took, this one does not move young survivors (they are promoted in place)
pub const YOUNG_COPY_NUM: usize = 3;
pub const YOUNG_COPY_DEN: usize = 5;
/// The warm pool of [`Pages`] is capped by what the last few rounds took
pub const WARM_CYCLES: usize = 4;
/// Overflow allocation looks at most at this many available blocks (for a hole that fits a medium object) before opening a new block
pub const OVERFLOW_TRIES: usize = 8;

/// Layout of `info` in the header: the low [`SIZE_BITS`] bits are the granule count, the next [`RC_BITS`] the reference count, then one bit of count parity, and the top bit marks a large object
const SIZE_BITS: u32 = 24;
const SIZE_MASK: u32 = (1 << SIZE_BITS) - 1;
const RC_BITS: u32 = 6;
const RC_SHIFT: u32 = SIZE_BITS;
/// Reference count ceiling: once reached the count sticks (no more increments or decrements) and is left to tracing
const RC_MAX: u32 = (1 << RC_BITS) - 1;
const RC_MASK: u32 = RC_MAX << RC_SHIFT;
/// Which "generation" a count belongs to: flipped when a full collection recounts, invalidating old counts all at once
const PARITY_BIT: u32 = 1 << (RC_SHIFT + RC_BITS);
/// Large object (allocated on its own, not in a block)
const LARGE_BIT: u32 = 1 << 31;
/// Mark word: moved; the new address is in the `drop` word
const FORWARDED: u32 = 1 << 31;
/// Mark word: a collector thread claimed it and is copying it
const BUSY: u32 = 1 << 30;
/// Epoch ceiling (must not reach the BUSY / FORWARDED bits)
pub const MAX_EPOCH: u32 = BUSY - 1;

/// The header prefix shared by both heaps: the mark word and the info word. Blocks, lines and sweeping look only at this part, so private and public heaps share the block code.
#[repr(C)]
pub struct Core {
    /// The epoch of the last mark, or BUSY / FORWARDED
    mark: AtomicU32,
    /// Granule count (including the header), reference count, count parity, large-object bit (see [`SIZE_BITS`] and the rest)
    info: AtomicU32,
}

#[repr(C)]
pub struct Header {
    core: Core,
    /// Visits the edges in the object's data: tracing (read only) or relocating (exclusive rewrite), depending on the Tracer's mode
    visit: unsafe fn(*mut u8, &mut Tracer<'_>),
    /// Finalizes the object's data; None when the type needs no destructor. Once moved, this word holds the new address
    drop: Option<unsafe fn(*mut u8)>,
}

/// Header size in bytes; the object data follows it
pub const HEADER: usize = size_of::<Header>();

const _: () = assert!(HEADER.is_multiple_of(GRANULE), "object data must be granule aligned");
const _: () = assert!(size_of::<Option<unsafe fn(*mut u8)>>() == size_of::<*const Header>(), "the forwarding address must fit in the drop word");

/// Result of claiming an object for evacuation.
pub enum Claim {
    /// Mine to copy
    Mine,
    /// Someone already moved it; the new header is here
    Moved(*const Header),
}

impl core::ops::Deref for Header {
    type Target = Core;

    fn deref(&self) -> &Core {
        &self.core
    }
}

impl Header {
    /// `mark`: the initial mark epoch.
    pub fn new<T: Trace>(bytes: usize, large: bool, mark: u32) -> Self {
        Self {
            core: Core::new(bytes, large, mark),
            visit: Self::visit_as::<T>,
            drop: needs_drop::<T>().then_some(Self::drop_as::<T> as unsafe fn(*mut u8)),
        }
    }

    /// A shareable object (its type is also [`crate::Shared`]): when a private heap with a cross-heap attached runs a young collection, it is promoted into the [`crate::CrossHeap`].
    /// The type must not have a destructor (checked at compile time).
    #[cfg(crossheap)]
    pub fn new_shared<T: Trace + crate::Shared>(bytes: usize, large: bool, mark: u32) -> Self {
        const { assert!(!needs_drop::<T>(), "shareable objects must not have destructors") };
        Self { core: Core::new(bytes, large, mark), visit: Self::visit_shared_as::<T>, drop: None }
    }

    /// As [`Header::visit_as`]; when the collector asks for the type ([`Tracer::asking`]) it answers `T`. SAFETY as for [`Header::visit_as`]
    #[cfg(crossheap)]
    unsafe fn visit_shared_as<T: Trace + crate::Shared>(data: *mut u8, t: &mut Tracer<'_>) {
        if t.asking() {
            t.answer::<T>();
            return;
        }
        // SAFETY: see above
        unsafe { Self::visit_as::<T>(data, t) };
    }

    /// SAFETY (caller): `data` points at a live `T`; in relocating mode the caller holds it exclusively (no mutators, no other collector thread touches its data)
    unsafe fn visit_as<T: Trace>(data: *mut u8, t: &mut Tracer<'_>) {
        // The collector asks whether it is shareable: it is not (shareable ones go through visit_shared_as)
        #[cfg(crossheap)]
        if t.asking() {
            return;
        }
        if t.relocating() {
            // SAFETY: see above
            unsafe { &mut *data.cast::<T>() }.relocate(t);
        } else {
            // SAFETY: see above
            unsafe { &*data.cast::<T>() }.trace(t);
        }
    }

    /// SAFETY (caller): `data` points at a `T` that is never used again
    unsafe fn drop_as<T>(data: *mut u8) {
        // SAFETY: see above
        unsafe { data.cast::<T>().drop_in_place() };
    }

    /// The data pointer (after the header).
    pub fn data(this: *const Self) -> *mut u8 {
        this.cast::<u8>().cast_mut().wrapping_add(HEADER)
    }

    /// Find the header from a data pointer.
    pub fn of(data: *const u8) -> *const Self {
        data.wrapping_sub(HEADER).cast()
    }
}

impl Core {
    pub fn new(bytes: usize, large: bool, mark: u32) -> Self {
        let granules = u32::try_from(bytes / GRANULE).expect("object too large");
        assert!(granules <= SIZE_MASK, "object too large (at most 128 MiB per object)");
        Self { mark: AtomicU32::new(mark), info: AtomicU32::new(granules | if large { LARGE_BIT } else { 0 }) }
    }

    pub fn bytes(&self) -> usize {
        (self.info.load(Ordering::Relaxed) & SIZE_MASK) as usize * GRANULE
    }

    pub fn is_large(&self) -> bool {
        self.info.load(Ordering::Relaxed) & LARGE_BIT != 0
    }

    /// Returns true the first time it is marked this round (only one thread gets true).
    pub fn mark(&self, epoch: u32) -> bool {
        self.mark.load(Ordering::Relaxed) != epoch && self.mark.swap(epoch, Ordering::AcqRel) != epoch
    }

    pub fn is_marked(&self, epoch: u32) -> bool {
        self.mark.load(Ordering::Acquire) == epoch
    }

    /// Increment the reference count (`parity` is the current count's parity: a mismatch means an old count from the previous generation, so it starts from 1). Unchanged once stuck.
    pub fn rc_inc(&self, parity: bool) {
        let parity = if parity { PARITY_BIT } else { 0 };
        let mut word = self.info.load(Ordering::Relaxed);
        loop {
            let rc = if word & PARITY_BIT == parity { (word & RC_MASK) >> RC_SHIFT } else { 0 };
            if rc == RC_MAX {
                return;
            }
            let new = (word & !(RC_MASK | PARITY_BIT)) | parity | ((rc + 1) << RC_SHIFT);
            match self.info.compare_exchange_weak(word, new, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return,
                Err(now) => word = now,
            }
        }
    }

    /// As [`Core::rc_inc`], but the caller guarantees it is the only one changing counts right now (single-threaded marking: mutators do not touch counts and the decrement queue is empty):
    /// a plain read, compute and write instead of a locked atomic read-modify-write (once per edge while recounting; that is what single-threaded marking saves).
    pub fn rc_inc_exclusive(&self, parity: bool) {
        let parity = if parity { PARITY_BIT } else { 0 };
        let word = self.info.load(Ordering::Relaxed);
        let rc = if word & PARITY_BIT == parity { (word & RC_MASK) >> RC_SHIFT } else { 0 };
        if rc < RC_MAX {
            self.info.store((word & !(RC_MASK | PARITY_BIT)) | parity | ((rc + 1) << RC_SHIFT), Ordering::Relaxed);
        }
    }

    /// Decrement the reference count; true when it reaches 0. Stuck counts, counts from the old generation and counts already 0 stay as they are and return false.
    pub fn rc_dec(&self, parity: bool) -> bool {
        let parity = if parity { PARITY_BIT } else { 0 };
        let mut word = self.info.load(Ordering::Relaxed);
        loop {
            if word & PARITY_BIT != parity {
                return false;
            }
            let rc = (word & RC_MASK) >> RC_SHIFT;
            if rc == RC_MAX || rc == 0 {
                return false;
            }
            let new = (word & !RC_MASK) | ((rc - 1) << RC_SHIFT);
            match self.info.compare_exchange_weak(word, new, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return rc == 1,
                Err(now) => word = now,
            }
        }
    }

    /// Make the reference count stick (`parity` is the current count's parity): later increments and decrements change nothing, and only tracing can reclaim it.
    /// Used for targets of weak references: release by count runs in the background, concurrently with mutators, and could not clear the weak references first.
    pub fn rc_stick(&self, parity: bool) {
        let parity = if parity { PARITY_BIT } else { 0 };
        self.info.fetch_update(Ordering::AcqRel, Ordering::Relaxed, |word| Some((word & !(RC_MASK | PARITY_BIT)) | parity | RC_MASK)).ok();
    }

    /// Already moved (not finalized and not counted live when sweeping).
    pub fn is_forwarded(&self) -> bool {
        self.mark.load(Ordering::Acquire) & FORWARDED != 0
    }

    /// A newly copied header: marked with this round.
    pub fn set_mark(&self, epoch: u32) {
        self.mark.store(epoch, Ordering::Release);
    }
}

impl Header {
    /// Evacuation: claim the right to copy; wait while someone else is copying, and get the new address once someone else has moved it.
    pub fn claim(&self) -> Claim {
        let backoff = Backoff::new();
        loop {
            let word = self.mark.load(Ordering::Acquire);
            if word & FORWARDED != 0 {
                return Claim::Moved(self.forwardee());
            }
            if word & BUSY != 0 {
                backoff.snooze();
                continue;
            }
            if self.mark.compare_exchange_weak(word, BUSY, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return Claim::Mine;
            }
        }
    }

    /// Copy done: write the new address and publish it. SAFETY (caller): this thread claimed it ([`Claim::Mine`]).
    pub unsafe fn forward(this: *const Self, to: *const Self) {
        // SAFETY: after the claim only this thread writes this word; other threads do not read it before seeing FORWARDED (Release / Acquire)
        unsafe { (&raw const (*this).drop).cast_mut().cast::<*const Self>().write(to) };
        // SAFETY: this is valid
        unsafe { &*this }.core.mark.store(FORWARDED, Ordering::Release);
    }

    /// After marking: the current header if alive (the new header if moved), None if not marked with `epoch`.
    pub fn survivor(this: *const Self, epoch: u32) -> Option<*const Self> {
        // SAFETY: the caller passes a header of this heap that has not been swept yet
        let h = unsafe { &*this };
        if h.is_forwarded() {
            Some(h.forwardee())
        } else {
            h.is_marked(epoch).then_some(this)
        }
    }

    pub fn forwardee(&self) -> *const Self {
        // SAFETY: called only after seeing FORWARDED, so the new address is written
        unsafe { (&raw const self.drop).cast::<*const Self>().read() }
    }

    /// Visit the object's data (trace or relocate). SAFETY (caller): the object is alive; when relocating the caller holds it exclusively.
    pub unsafe fn visit(this: *const Self, t: &mut Tracer<'_>) {
        // SAFETY: see above; visit was generated for the object's own type
        unsafe { ((*this).visit)(Self::data(this), t) };
    }

    /// Visit another copy `data` with `this`'s type (the copy moved into the cross-heap by promotion). SAFETY (caller): `data` is a live object of the same type; otherwise as for [`Header::visit`].
    #[cfg(crossheap)]
    pub unsafe fn visit_data(this: *const Self, data: *mut u8, t: &mut Tracer<'_>) {
        // SAFETY: see above
        unsafe { ((*this).visit)(data, t) };
    }

    /// Finalize the object's data. SAFETY (caller): the object is dead, never used again, and was not moved.
    pub unsafe fn finalize(this: *const Self) {
        // SAFETY: see above; drop was generated for the object's own type
        if let Some(drop) = unsafe { (*this).drop } {
            unsafe { drop(Self::data(this)) };
        }
    }
}

#[repr(C)]
struct Meta {
    /// The epoch at which each line was last occupied by a live object
    lines: [AtomicU32; LINES],
    /// Object starts: one bit per granule
    starts: [AtomicU64; GRANULES / BITS],
    /// Live bytes at the last sweep; [`REUSED`] once handed to an allocation buffer (or just opened) (unknown, not an evacuation candidate)
    live: AtomicU32,
    /// Evacuate this round: live objects in it move when traced
    evacuate: AtomicBool,
    /// On the available queue (keeps a block from being queued twice)
    listed: AtomicBool,
}

/// How sweeping finalizes dead objects: the private heap calls the object's own destructor, public heap objects have none.
pub trait Finalize {
    /// SAFETY (caller): `object` is the header of a dead object that is never used again.
    unsafe fn finalize(object: *const Core);
}

impl Finalize for Header {
    unsafe fn finalize(object: *const Core) {
        // SAFETY: every object header in a private heap block is a Header (with Core at its start); see above
        unsafe { Header::finalize(object.cast()) };
    }
}

/// [`Meta::live`]: the block was taken for allocation again, so the live bytes from the last sweep no longer count
const REUSED: u32 = u32::MAX;

/// Result of sweeping a block.
#[derive(Clone, Copy)]
pub enum State {
    /// No live objects
    Empty,
    /// Still has free lines
    Recyclable,
    /// No free lines
    Full,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Block(NonNull<Meta>);

// SAFETY: a block is a separate piece of heap memory; the metadata is all atomics, and cross-thread access to object data is covered by Trace: Send + Sync
unsafe impl Send for Block {}
// SAFETY: as above
unsafe impl Sync for Block {}

impl Block {
    const LAYOUT: Layout = match Layout::from_size_align(BLOCK, BLOCK) {
        Ok(layout) => layout,
        Err(_) => panic!("invalid block size / alignment"),
    };
    /// The first line after the metadata that can hold objects
    pub const FIRST_LINE: usize = size_of::<Meta>().div_ceil(LINE);

    /// A new block (from `pages`): only the metadata is zeroed (line mark 0 predates every epoch, so every line is free); the object area is not cleared — allocation never relies on zeroed memory,
    /// so untouched pages never fault in.
    pub fn new(pages: &Pages) -> Self {
        let b = Self(pages.take());
        // SAFETY: just taken and owned; the metadata is all atomics, and all zeros is a valid value
        unsafe { b.0.as_ptr().write_bytes(0, 1) };
        b.reused();
        b
    }

    #[cfg(crossheap)]
    /// A block in a public heap window. SAFETY (caller): `p` is block aligned with [`BLOCK`] readable and writable bytes;
    /// on first use the memory is zero (newly extended file pages) or was [`Block::reset`].
    pub unsafe fn at(p: *mut u8) -> Self {
        // SAFETY: see above
        Self(unsafe { NonNull::new_unchecked(p.cast()) })
    }

    #[cfg(crossheap)]
    /// Zero the metadata (when blocks used by a large object come back). SAFETY (caller): the block holds no objects and nobody uses it.
    pub unsafe fn reset(self) {
        // SAFETY: see above; the metadata is all atomics, and all zeros is a valid value
        unsafe { self.0.as_ptr().write_bytes(0, 1) };
    }

    /// Handed to an allocation buffer: the live bytes from the last sweep no longer count.
    pub fn reused(self) {
        self.meta().live.store(REUSED, Ordering::Relaxed);
    }

    /// Live bytes as of the last sweep, if nothing was allocated since.
    pub fn live_since_sweep(self) -> Option<usize> {
        let live = self.meta().live.load(Ordering::Relaxed);
        (live != REUSED).then_some(live as usize)
    }

    /// About to go on the available queue: true only if it was not there already.
    pub fn list(self) -> bool {
        !self.meta().listed.swap(true, Ordering::AcqRel)
    }

    /// Taken off the available queue.
    pub fn unlist(self) {
        self.meta().listed.store(false, Ordering::Release);
    }

    /// Clear the object start at `p` (reference counting released it).
    pub fn clear_start(self, p: *const u8) {
        let g = self.offset(p) / GRANULE;
        self.meta().starts[g / BITS].fetch_and(!(1 << (g % BITS)), Ordering::Relaxed);
    }

    /// Recompute line marks from the old objects still present (mark epoch = `epoch`) (blocks in which reference counting released objects): their lines get `epoch` and the rest become free;
    /// young objects' lines are not marked (the young trace in the same pause marks the survivors, and sweeping reclaims the dead).
    /// If the block was taken for allocation after its last sweep (it may hold unswept dead young objects): keep that state and return false, leaving it to the young sweep to classify;
    /// otherwise record the live bytes and return whether it has free lines (so it can go straight back on the available queue).
    /// SAFETY (caller): no mutators (heap held exclusively), and the objects marked in the start bitmap are not finalized yet.
    pub unsafe fn recompute_lines(self, epoch: u32) -> bool {
        let meta = self.meta();
        let reused = meta.live.load(Ordering::Relaxed) == REUSED;
        for line in &meta.lines {
            line.store(0, Ordering::Relaxed);
        }
        let mut live = 0;
        for (w, word) in meta.starts.iter().enumerate() {
            let mut bits = word.load(Ordering::Relaxed);
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let p = self.base().wrapping_add((w * BITS + bit) * GRANULE);
                // SAFETY: the places marked in the start bitmap hold objects not yet finalized
                let h = unsafe { &*p.cast::<Core>() };
                if h.is_marked(epoch) {
                    self.mark_lines(p, h.bytes(), epoch);
                    live += h.bytes();
                }
            }
        }
        if reused {
            return false;
        }
        meta.live.store(u32::try_from(live).unwrap_or(REUSED - 1), Ordering::Relaxed);
        meta.lines[Self::FIRST_LINE..].iter().any(|l| l.load(Ordering::Relaxed) != epoch)
    }

    pub fn set_evacuate(self) {
        self.meta().evacuate.store(true, Ordering::Relaxed);
    }

    /// Do not evacuate this block (it holds objects pointed to directly from another heap); returns true if it was going to be evacuated.
    pub fn pin(self) -> bool {
        self.meta().evacuate.swap(false, Ordering::Relaxed)
    }

    pub fn is_evacuating(self) -> bool {
        self.meta().evacuate.load(Ordering::Relaxed)
    }

    /// Give it back to `pages` (physical memory returns to the system). SAFETY (caller): the block comes from the same `pages`, holds no live objects (the dead are finalized) and is never used again.
    pub unsafe fn release(self, pages: &Pages) {
        // SAFETY: see above
        unsafe { pages.give(self.0) };
    }

    /// The block containing an address (the address must be in some block).
    pub fn containing(p: *const u8) -> Self {
        let base = p.map_addr(|a| a & !(BLOCK - 1)).cast_mut().cast::<Meta>();
        // SAFETY: a block start is never 0
        Self(unsafe { NonNull::new_unchecked(base) })
    }

    fn meta(&self) -> &Meta {
        // SAFETY: the metadata stays valid while the block lives
        unsafe { self.0.as_ref() }
    }

    fn base(self) -> *mut u8 {
        self.0.as_ptr().cast()
    }

    /// Start of line `i` (`i == LINES` is the end of the block).
    pub fn line(self, i: usize) -> *mut u8 {
        self.base().wrapping_add(i * LINE)
    }

    fn offset(self, p: *const u8) -> usize {
        p.addr() - self.base().addr()
    }

    /// Mark the lines spanned by `[p, p + bytes)`.
    pub fn mark_lines(self, p: *const u8, bytes: usize, epoch: u32) {
        let first = self.offset(p) / LINE;
        let last = (self.offset(p) + bytes - 1) / LINE;
        for line in &self.meta().lines[first..=last] {
            line.store(epoch, Ordering::Relaxed);
        }
    }

    /// Record that an object starts at `p`.
    pub fn set_start(self, p: *const u8) {
        let g = self.offset(p) / GRANULE;
        self.meta().starts[g / BITS].fetch_or(1 << (g % BITS), Ordering::Relaxed);
    }

    /// The start bit of the object at `p`: its word (valid as long as the block lives) and bit.
    pub fn start_bit(self, p: *const u8) -> (*const AtomicU64, u64) {
        let g = self.offset(p) / GRANULE;
        (&raw const self.meta().starts[g / BITS], 1 << (g % BITS))
    }

    /// Give whole pages of free lines (line mark older than `epoch`) back to the system (`madvise(MADV_DONTNEED)`; page 0 holds the metadata and is left alone).
    /// Allocating there later faults pages in again (zeroed; allocation does not rely on contents). Linux only (not under Miri).
    /// SAFETY (caller): nothing in the free lines is still needed (dead objects are finalized) and no mutator uses this block.
    pub unsafe fn discard_free_lines(self, epoch: u32) {
        #[cfg(all(target_os = "linux", not(miri)))]
        {
            /// Lines per page
            const PER_PAGE: usize = PAGE / LINE;
            let mut from = PER_PAGE.max(Self::FIRST_LINE);
            while let Some((start, end)) = self.hole(from, epoch) {
                let first = start.next_multiple_of(PER_PAGE);
                let last = end / PER_PAGE * PER_PAGE;
                if first < last {
                    // SAFETY: the whole pages lie in this block's free lines; see above
                    unsafe { libc::madvise(self.line(first).cast(), (last - first) * LINE, libc::MADV_DONTNEED) };
                }
                from = end;
            }
        }
        #[cfg(not(all(target_os = "linux", not(miri))))]
        let _ = (self, epoch);
    }

    /// Whether there is a hole (run of free lines) that fits `bytes` bytes.
    pub fn has_hole(self, bytes: usize, epoch: u32) -> bool {
        let need = bytes.div_ceil(LINE);
        let mut from = Self::FIRST_LINE;
        while let Some((start, end)) = self.hole(from, epoch) {
            if end - start >= need {
                return true;
            }
            from = end;
        }
        false
    }

    /// Prefetch the cache lines holding each object header in the block (sweeping reads headers one by one and the block is usually not cached: without prefetching every object is a miss,
    /// and the next header's address depends on the next bit in the bitmap, so out-of-order execution cannot overlap them).
    #[cfg_attr(not(target_arch = "x86_64"), expect(clippy::unused_self, reason = "prefetches only on x86-64"))]
    fn prefetch_headers(self) {
        #[cfg(target_arch = "x86_64")]
        {
            /// Cache line
            const LINE_BYTES: usize = 64;
            let mut last = usize::MAX;
            for (w, word) in self.meta().starts.iter().enumerate() {
                let mut bits = word.load(Ordering::Relaxed);
                while bits != 0 {
                    let bit = bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    let at = (w * BITS + bit) * GRANULE / LINE_BYTES;
                    if at != last {
                        last = at;
                        // SAFETY: prefetching does not access memory; the address is in the block
                        unsafe { core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T0 }>(self.base().wrapping_add(at * LINE_BYTES).cast()) };
                    }
                }
            }
        }
    }

    /// The first hole (a run of free lines) from line `from` on, as the line range `[start, end)`.
    /// Free line = line mark older than the last completed epoch `epoch`: during concurrent marking the marker writes newer epochs into live lines, and those are not free either.
    pub fn hole(self, from: usize, epoch: u32) -> Option<(usize, usize)> {
        let lines = &self.meta().lines;
        let free = |i: usize| lines[i].load(Ordering::Relaxed) < epoch;
        let start = (from..LINES).find(|&i| free(i))?;
        let end = (start..LINES).find(|&i| !free(i)).unwrap_or(LINES);
        Some((start, end))
    }

    /// Sweep: finalize (through `K`) the objects not marked with this round's epoch (moved ones are not finalized), clear their start bits, and return the block's state and live bytes.
    /// With `deferred`, dead objects are not finalized on the spot but their header addresses are put there (the caller must finalize them before handing the block out).
    /// SAFETY (caller): marking is done and no mutator uses this block; the block's object headers are of the kind `K` knows.
    pub unsafe fn sweep<K: Finalize>(self, epoch: u32, mut deferred: Option<&mut Vec<usize>>) -> (State, usize) {
        let mut live = 0;
        self.prefetch_headers();
        for (w, word) in self.meta().starts.iter().enumerate() {
            let mut bits = word.load(Ordering::Relaxed);
            let mut dead = 0u64;
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let header = self.base().wrapping_add((w * BITS + bit) * GRANULE).cast::<Core>().cast_const();
                // SAFETY: every place marked in the start bitmap is an initialized object
                let h = unsafe { &*header };
                if h.is_forwarded() {
                    // The live one is the moved copy; the old one only has its start bit cleared
                    dead |= 1 << bit;
                } else if h.is_marked(epoch) {
                    live += h.bytes();
                } else {
                    match deferred.as_deref_mut() {
                        Some(list) => list.push(header.expose_provenance()),
                        // SAFETY: not marked ⇒ unreachable from the roots, nobody uses it again
                        None => unsafe { K::finalize(header) },
                    }
                    dead |= 1 << bit;
                }
            }
            if dead != 0 {
                word.fetch_and(!dead, Ordering::Relaxed);
            }
        }
        let lines = &self.meta().lines[Self::FIRST_LINE..];
        let used = lines.iter().filter(|l| l.load(Ordering::Relaxed) == epoch).count();
        let state = match used {
            0 => State::Empty,
            n if n == lines.len() => State::Full,
            _ => State::Recyclable,
        };
        self.meta().live.store(u32::try_from(live).unwrap_or(REUSED - 1), Ordering::Relaxed);
        self.meta().evacuate.store(false, Ordering::Relaxed);
        (state, live)
    }
}

/// How much to map from the system at once and cut into blocks (Linux)
#[cfg(all(target_os = "linux", not(miri)))]
const CHUNK: usize = 2 * 1024 * 1024;

/// Where private heap blocks come from and go back to (one per heap).
///
/// Linux: mmap chunks of our own ([`CHUNK`]) and cut them into [`BLOCK`]-aligned blocks for a free pool (bypassing glibc: aligned allocation costs extra, and memory freed to it is
/// not returned to the system). Returned blocks go to the warm pool first and keep their memory (the next round probably needs as many blocks, so `madvise` and faulting back in would be wasted); only when the warm pool is full (cap: see
/// [`Pages::set_warm_cap`]) does `madvise(MADV_DONTNEED)` give the physical memory back, with the address going to the cold pool for reuse. Warm blocks are handed out first.
/// The mapped chunks are unmapped together when the heap is dropped. Other platforms and Miri use the global allocator.
#[derive(Default)]
pub struct Pages {
    /// Free blocks (addresses) without physical memory (newly cut or `madvise`d)
    #[cfg(all(target_os = "linux", not(miri)))]
    free: Injector<usize>,
    /// Free blocks (addresses) whose memory is kept
    #[cfg(all(target_os = "linux", not(miri)))]
    warm: Injector<usize>,
    /// Blocks in the warm pool (may briefly overcount: incremented before pushing)
    #[cfg(all(target_os = "linux", not(miri)))]
    warm_len: core::sync::atomic::AtomicUsize,
    /// At most this many blocks kept warm
    #[cfg(all(target_os = "linux", not(miri)))]
    warm_cap: core::sync::atomic::AtomicUsize,
    /// Mapped chunks (start addresses), unmapped on drop
    #[cfg(all(target_os = "linux", not(miri)))]
    chunks: Injector<usize>,
}

impl Pages {
    #[cfg(all(target_os = "linux", not(miri)))]
    fn steal(pool: &Injector<usize>) -> Option<NonNull<Meta>> {
        loop {
            match pool.steal() {
                // SAFETY: the pools hold only non-zero block addresses cut by this object
                Steal::Success(addr) => {
                    return Some(unsafe { NonNull::new_unchecked(core::ptr::with_exposed_provenance_mut(addr)) });
                }
                Steal::Retry => {}
                Steal::Empty => return None,
            }
        }
    }

    /// Keep at most `cap` blocks warm; the excess goes back to the system now. `usize::MAX`: give nothing back (when the whole chunk is about to be unmapped).
    #[cfg(all(target_os = "linux", not(miri)))]
    pub fn set_warm_cap(&self, cap: usize) {
        self.warm_cap.store(cap, Ordering::Relaxed);
        while self.warm_len.load(Ordering::Relaxed) > cap {
            let Some(p) = Self::steal(&self.warm) else {
                break;
            };
            self.warm_len.fetch_sub(1, Ordering::Relaxed);
            // SAFETY: blocks in the warm pool are unused
            unsafe { self.discard(p) };
        }
    }

    #[cfg(any(not(target_os = "linux"), miri))]
    #[expect(clippy::unused_self, reason = "same signature as the Linux version")]
    pub fn set_warm_cap(&self, _cap: usize) {}

    #[cfg(all(target_os = "linux", not(miri)))]
    fn take(&self) -> NonNull<Meta> {
        if let Some(p) = Self::steal(&self.warm) {
            self.warm_len.fetch_sub(1, Ordering::Relaxed);
            return p;
        }
        if let Some(p) = Self::steal(&self.free) {
            return p;
        }
        let base = self.map_chunk();
        // Keep the first block, pool the rest
        for i in 1..CHUNK / BLOCK {
            self.free.push(base.wrapping_add(i * BLOCK).expose_provenance());
        }
        // SAFETY: a successful mmap returns a non-zero address
        unsafe { NonNull::new_unchecked(base.cast()) }
    }

    /// Map [`CHUNK`] bytes aligned to [`BLOCK`] (map one extra block, then unmap the excess at both ends after aligning).
    #[cfg(all(target_os = "linux", not(miri)))]
    fn map_chunk(&self) -> *mut u8 {
        let len = CHUNK + BLOCK;
        // SAFETY: an anonymous private mapping with valid arguments
        let raw = unsafe { libc::mmap(core::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE, -1, 0) };
        if raw == libc::MAP_FAILED {
            handle_alloc_error(Block::LAYOUT);
        }
        let raw = raw.cast::<u8>();
        let head = raw.align_offset(BLOCK);
        let base = raw.wrapping_add(head);
        // SAFETY: both pieces are inside the mapping just made and do not overlap; not called for length 0
        unsafe {
            if head > 0 {
                libc::munmap(raw.cast(), head);
            }
            let tail = BLOCK - head;
            if tail > 0 {
                libc::munmap(base.wrapping_add(CHUNK).cast(), tail);
            }
        }
        self.chunks.push(base.expose_provenance());
        base
    }

    /// SAFETY (caller): `p` comes from this object's [`Pages::take`] and is never used again.
    #[cfg(all(target_os = "linux", not(miri)))]
    unsafe fn give(&self, p: NonNull<Meta>) {
        if self.warm_len.fetch_add(1, Ordering::Relaxed) < self.warm_cap.load(Ordering::Relaxed) {
            self.warm.push(p.as_ptr().expose_provenance());
            return;
        }
        self.warm_len.fetch_sub(1, Ordering::Relaxed);
        // SAFETY: see above
        unsafe { self.discard(p) };
    }

    /// Give the physical memory back to the system and put the block in the cold pool. SAFETY (caller): as for [`Pages::give`].
    #[cfg(all(target_os = "linux", not(miri)))]
    unsafe fn discard(&self, p: NonNull<Meta>) {
        // SAFETY: the block lies in a chunk mapped by this object; DONTNEED only drops the contents, the address stays usable
        unsafe { libc::madvise(p.as_ptr().cast(), BLOCK, libc::MADV_DONTNEED) };
        self.free.push(p.as_ptr().expose_provenance());
    }

    #[cfg(any(not(target_os = "linux"), miri))]
    #[expect(clippy::unused_self, reason = "same signature as the Linux version")]
    fn take(&self) -> NonNull<Meta> {
        // SAFETY: LAYOUT has a non-zero size
        let p = unsafe { alloc(Block::LAYOUT) };
        NonNull::new(p.cast()).unwrap_or_else(|| handle_alloc_error(Block::LAYOUT))
    }

    #[cfg(any(not(target_os = "linux"), miri))]
    #[expect(clippy::unused_self, reason = "same signature as the Linux version")]
    unsafe fn give(&self, p: NonNull<Meta>) {
        // SAFETY: from take, with the same LAYOUT
        unsafe { dealloc(p.as_ptr().cast(), Block::LAYOUT) };
    }
}

#[cfg(all(target_os = "linux", not(miri)))]
impl Drop for Pages {
    fn drop(&mut self) {
        loop {
            match self.chunks.steal() {
                // SAFETY: the chunk is CHUNK bytes mapped by map_chunk; every block was returned when the heap was dropped, nobody uses them
                Steal::Success(addr) => unsafe {
                    libc::munmap(core::ptr::with_exposed_provenance_mut::<libc::c_void>(addr), CHUNK);
                },
                Steal::Retry => {}
                Steal::Empty => return,
            }
        }
    }
}

/// Superblock magic: recognizes a paragc public heap with a matching layout version
#[cfg(crossheap)]
pub const MAGIC: u64 = u64::from_le_bytes(*b"paragcP5");
/// Page size: public heap parts are page aligned and the file grows by pages; the private heap gives free lines back by whole pages
#[cfg(target_os = "linux")]
pub const PAGE: usize = 4096;
/// Type table capacity
#[cfg(crossheap)]
pub const TYPE_CAP: usize = 1024;
/// Pointer map capacity (the pointer slot offsets of all types together)
#[cfg(crossheap)]
pub const MAP_CAP: usize = 64 * 1024;
/// One entry per this many bytes of the window
#[cfg(crossheap)]
pub const BYTES_PER_ENTRY: usize = 16 * 1024;
/// Attachment table capacity: at most this many places (processes, or several regions in one process) map the heap at once
#[cfg(crossheap)]
pub const ATTACH_CAP: usize = 32;
/// Offset of the attachment table within the superblock page
#[cfg(crossheap)]
pub const ATTACH_OFF: usize = 1024;

/// The superblock (window offset 0). Shared across processes: everything except the fields written once at creation is atomic.
#[repr(C)]
#[cfg(crossheap)]
pub struct Super {
    pub magic: AtomicU64,
    /// Window size (the layout is computed from it)
    pub window: AtomicU64,
    /// Heap number = window number
    pub heap: AtomicU64,
    /// Collection request (a futex word): 0 = nobody is collecting, otherwise the collector's attachment slot + 1; while non-zero new mutates wait first
    pub request: AtomicU32,
    /// The epoch of the most recently started collection (written when it starts): if the collector is killed, the one taking over marks from scratch with a newer epoch
    pub marking: AtomicU32,
    /// The epoch of the last completed marking
    pub epoch: AtomicU32,
    /// Blocks opened so far (the index of the next new block in the block area)
    pub blocks: AtomicU32,
    pub collections: AtomicU64,
    /// Times a collector was killed and someone else took over
    pub takeovers: AtomicU64,
    /// Head of the available block stack (see [`crate::sync::Stack`])
    pub available: AtomicU64,
    /// Next entry of the type table
    pub types: AtomicU32,
    /// Next free place in the pointer maps
    pub map: AtomicU32,
    /// Next never-used entry of the entry table
    pub entries: AtomicU32,
    /// Mail received so far (a futex word): incremented on every post, waking the owner
    pub mail: AtomicU32,
    /// Mailbox: head of the stack of posted entries not yet taken (see [`crate::sync::Stack`]; "next" uses the entry's `next`)
    pub mailbox: AtomicU64,
    /// Head of the returned entry stack
    pub free_entries: AtomicU64,
    /// How far the file has been extended (window offsets): one each for the type table, pointer maps, entry table and block state table
    pub committed: [AtomicU64; Segment::COUNT],
    /// Live bytes after the last collection
    pub live: AtomicU64,
    /// Bytes newly taken since the last collection
    pub allocated: AtomicU64,
    /// Heap identity: a random 64-bit number chosen at creation. Heap numbers are reused, identities are not: references and registrations into this heap carry it, and a mismatch means an old heap that already died
    pub incarnation: AtomicU64,
    /// The owner's (creating process's) pid and start time (see [`crate::shm::Process`]): when the owner is gone and there is no [`Super::persist`], the heap is dead
    pub owner_pid: AtomicU32,
    /// Non-zero: the heap stays after its owner is gone ([`crate::CrossHeap::persist`])
    pub persist: AtomicU32,
    pub owner_start: AtomicU64,
    /// Non-zero: the heap is dead (its owner left) and the file is deleted or being deleted; processes still mapping it can only let go
    pub dead: AtomicU32,
    /// Non-zero: only the owner collects it (attached to the owner's private heap as its old generation; the owner's private heap points at its objects through roots nobody else can see)
    pub owner_only: AtomicU32,
}

#[cfg(crossheap)]
impl Super {
    /// A new heap's superblock: epochs start at 1 (mark 0 of new objects and line mark 0 of new blocks both predate it).
    pub fn new(heap: u64, incarnation: u64, owner: crate::shm::Process) -> Self {
        Self {
            magic: AtomicU64::new(MAGIC),
            window: AtomicU64::new(WINDOW as u64),
            heap: AtomicU64::new(heap),
            request: AtomicU32::new(0),
            marking: AtomicU32::new(0),
            epoch: AtomicU32::new(1),
            blocks: AtomicU32::new(0),
            collections: AtomicU64::new(0),
            takeovers: AtomicU64::new(0),
            available: AtomicU64::new(0),
            types: AtomicU32::new(0),
            map: AtomicU32::new(0),
            entries: AtomicU32::new(0),
            mail: AtomicU32::new(0),
            mailbox: AtomicU64::new(0),
            free_entries: AtomicU64::new(0),
            committed: core::array::from_fn(|_| AtomicU64::new(0)),
            live: AtomicU64::new(0),
            allocated: AtomicU64::new(0),
            incarnation: AtomicU64::new(incarnation),
            owner_pid: AtomicU32::new(owner.pid),
            persist: AtomicU32::new(0),
            owner_start: AtomicU64::new(owner.start),
            dead: AtomicU32::new(0),
            owner_only: AtomicU32::new(0),
        }
    }
}

/// An entry of the attachment table (one cache line, so the processes' counters do not contend).
#[repr(C, align(64))]
#[cfg(crossheap)]
pub struct Attachment {
    /// pid; 0 = free slot
    pub pid: AtomicU32,
    /// Threads of this attachment inside a mutate (a futex word: the collector waits for it to reach zero)
    pub inside: AtomicU32,
    /// Process start time (see [`crate::shm::Process`]); 0 = still registering
    pub start: AtomicU64,
    /// Threads of this attachment held at the door by a collection request and not yet inside (a futex word): the collector lets them in before claiming again, so mutators do not starve
    pub waiting: AtomicU32,
}

#[cfg(crossheap)]
const _: () = assert!(size_of::<Super>() <= ATTACH_OFF && ATTACH_OFF + ATTACH_CAP * size_of::<Attachment>() <= PAGE, "the attachment table lives in the superblock page");

/// The parts of the file extended on demand.
#[derive(Clone, Copy)]
#[cfg(crossheap)]
pub enum Segment {
    Types,
    Map,
    Entries,
    States,
}

#[cfg(crossheap)]
impl Segment {
    pub const COUNT: usize = 4;

    pub fn index(self) -> usize {
        self as usize
    }
}

/// An entry of the type table.
#[repr(C)]
#[cfg(crossheap)]
pub struct TypeEntry {
    /// Set to 1 after the other fields are written (only then can it be looked up and traced by)
    pub ready: AtomicU32,
    /// Number of pointer slots
    pub edges: AtomicU32,
    /// Index of its first offset in the pointer maps
    pub map: AtomicU32,
    /// Bytes of object data
    pub size: AtomicU32,
    /// Type key: a 128-bit hash of the type name, size and pointer map
    pub key: [AtomicU64; 2],
}

#[cfg(crossheap)]
impl TypeEntry {
    /// Registered, with type key `key`.
    pub fn has_key(&self, key: [u64; 2]) -> bool {
        self.ready.load(core::sync::atomic::Ordering::Acquire) == 1 && self.key.iter().zip(key).all(|(k, v)| k.load(core::sync::atomic::Ordering::Relaxed) == v)
    }
}

/// An entry of the entry table.
#[repr(C)]
#[cfg(crossheap)]
pub struct Entry {
    /// Public offset of the target object's data; 0 = empty
    pub ptr: AtomicU64,
    /// Holder (0 = nobody; attachment slot + 1; or [`crate::heap::HEAP_HOLDER`] | heap number)
    pub holder: AtomicU32,
    /// On the returned entry stack: the next one (see [`crate::sync::Stack`])
    pub next: AtomicU32,
    /// When the holder is another public heap, that heap's identity ([`Super::incarnation`]): recognizes that it died and its number went to someone else
    pub holder_incarnation: AtomicU64,
}

/// An entry of the block state table.
#[repr(C)]
#[cfg(crossheap)]
pub struct BlockState {
    /// [`Kind`]
    pub kind: AtomicU32,
    /// On the available block stack: the next one
    pub next: AtomicU32,
}

/// Kind of block.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg(crossheap)]
pub enum Kind {
    /// Never opened
    Unused = 0,
    /// Cut into lines for small objects
    Small = 1,
    /// First block of a large object (the object header is at the block start)
    LargeHead = 2,
    /// A following block of a large object
    LargeTail = 3,
}

#[cfg(crossheap)]
impl From<u32> for Kind {
    fn from(v: u32) -> Self {
        match v {
            1 => Self::Small,
            2 => Self::LargeHead,
            3 => Self::LargeTail,
            _ => Self::Unused,
        }
    }
}

/// Offsets of the parts within a window (computed from the window size, so every attached process gets the same).
#[derive(Clone, Copy)]
#[cfg(crossheap)]
pub struct WindowLayout {
    pub types: usize,
    pub map: usize,
    pub entries: usize,
    pub entry_cap: usize,
    pub states: usize,
    pub blocks: usize,
    pub max_blocks: usize,
}

#[cfg(crossheap)]
impl WindowLayout {
    pub fn new() -> Self {
        let types = size_of::<Super>().next_multiple_of(PAGE);
        let map = types + TYPE_CAP * size_of::<TypeEntry>();
        let entries = (map + MAP_CAP * size_of::<u32>()).next_multiple_of(PAGE);
        let entry_cap = WINDOW / BYTES_PER_ENTRY;
        let states = (entries + entry_cap * size_of::<Entry>()).next_multiple_of(PAGE);
        let blocks = (states + WINDOW / BLOCK * size_of::<BlockState>()).next_multiple_of(BLOCK);
        let max_blocks = (WINDOW - blocks) / BLOCK;
        assert!(max_blocks > 0, "the window cannot hold a single block");
        Self { types, map, entries, entry_cap, states, blocks, max_blocks }
    }

    /// Start and end of a part (window offsets).
    pub fn segment(&self, s: Segment) -> (usize, usize) {
        match s {
            Segment::Types => (self.types, self.map),
            Segment::Map => (self.map, self.entries),
            Segment::Entries => (self.entries, self.states),
            Segment::States => (self.states, self.blocks),
        }
    }
}

/// A public heap object header (16 B): the mark / info words shared with the private heap, plus the type number.
#[repr(C)]
#[cfg(crossheap)]
pub struct PHeader {
    pub core: Core,
    /// Index in the type table
    pub ty: u32,
    /// Birth stamp: computed from the heap identity and the allocation epoch (see [`crate::heap::Inner::stamp`]). Two objects that occupy the same place one after the other have different stamps (probabilistically);
    /// cross-heap weak references and signatures use it to tell that the original object died and its place was taken
    pub stamp: u32,
}

/// Bytes in a public heap object header; the data follows it
#[cfg(crossheap)]
pub const PHEADER: usize = size_of::<PHeader>();

#[cfg(crossheap)]
const _: () = assert!(PHEADER == 16, "a public heap object header is 16 bytes");

#[cfg(crossheap)]
impl PHeader {
    pub fn new(bytes: usize, large: bool, ty: u32, stamp: u32) -> Self {
        // A new object's mark 0 equals no epoch (epochs start at 1)
        Self { core: Core::new(bytes, large, 0), ty, stamp }
    }

    pub fn data(this: *const Self) -> *mut u8 {
        this.cast::<u8>().cast_mut().wrapping_add(PHEADER)
    }

    pub fn of(data: *const u8) -> *const Self {
        data.wrapping_sub(PHEADER).cast()
    }
}

// Public heap objects have no destructor
#[cfg(crossheap)]
impl Finalize for PHeader {
    unsafe fn finalize(_: *const Core) {}
}
