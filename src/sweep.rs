//! Freeing dead objects: the [`Sweeper`] (after marking, concurrent with mutators, lazy) and the background [`Releaser`] for objects whose reference count drops to 0.
//!
//! Sweeping: at the end of marking (with the heap held exclusively) all blocks and large objects go to the [`Sweeper`]; mutators then run as usual and sweeping proceeds from both ends:
//!
//! - a mutator that needs a block takes a swept one first; if there is none it takes an unswept block and sweeps it on the spot (finalizing dead objects), using it if it can (lazy sweeping)
//! - collector threads ([`crate::Heap::sweep_concurrently`]) take and sweep the remaining blocks and large objects
//!
//! Taking is an atomic cursor `fetch_add`, so each block is swept exactly once by one thread; it is handed out only after sweeping, so no mutator uses it meanwhile.
//! Death is "mark epoch ≠ this round": sweeping must finish before the next marking starts (otherwise live objects marked with a newer epoch would look dead).
//!
//! Reference count decrements are processed concurrently on the thread pool (mutators keep running): a young collection's pause only queues them.
//!
//! Safety: an object whose count is 0 at the pause has no counted references, every root was incremented and every young object was promoted or died ⇒ mutators cannot reach it,
//! and the same holds down the chain. Releasing only finalizes, clears start bits and records dirty blocks; their lines are recomputed at the start of the next pause (an allocation buffer may be allocating into them right now).
//! Lines of released objects stay marked as used until recomputed, so they are never handed out early.
//!
//! Termination is decided as in marking: a worker whose own queue is empty and that cannot steal adds 1 to the idle count; all idle ⇒ done (only workers push to the queues).
//!
//! Finalizing only on the caller's thread ([`crate::Heap::set_parallel_drop`] off): other workers still judge death, clear start bits and compute lines in parallel,
//! but put dead objects, together with the swept blocks, into a pending list; the caller (worker 0 of the pool) finalizes them before handing the blocks out / back to the system.
//! Memory freed in destructors was mostly allocated on mutator threads, and freeing it across threads makes the allocator contend (glibc's arena locks).
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};

use crossbeam_deque::{Injector, Steal, Worker};
use crossbeam_utils::{Backoff, CachePadded};

#[cfg(all(target_os = "linux", not(miri)))]
use crate::layout::LARGE_MMAP;
use crate::layout::{Block, Core, Finalize, GRANULE, Header, State};
use crate::mark::{Tracer, TracerInternal};
use crate::sync::Pool;

/// A separately allocated large object (header pointer).
#[derive(Clone, Copy)]
pub struct Large(pub NonNull<Header>);

// SAFETY: separate heap memory; the object's contents across threads are covered by Trace: Send + Sync
unsafe impl Send for Large {}
// SAFETY: as above
unsafe impl Sync for Large {}

impl Large {
    pub fn layout(bytes: usize) -> Layout {
        Layout::from_size_align(bytes, GRANULE).expect("object too large")
    }

    /// Allocate `bytes` bytes (at least a header): on Linux, sizes of at least [`LARGE_MMAP`] get their own mapping (returned to the system when freed); the rest use the global allocator.
    pub fn alloc(bytes: usize) -> *mut u8 {
        #[cfg(all(target_os = "linux", not(miri)))]
        if bytes >= LARGE_MMAP {
            // SAFETY: an anonymous private mapping with valid arguments
            let p = unsafe { libc::mmap(core::ptr::null_mut(), bytes, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0) };
            if p == libc::MAP_FAILED {
                handle_alloc_error(Self::layout(bytes));
            }
            return p.cast();
        }
        let layout = Self::layout(bytes);
        // SAFETY: the size is non-zero (at least a header)
        let p = unsafe { alloc(layout) };
        if p.is_null() {
            handle_alloc_error(layout);
        }
        p
    }

    fn header(&self) -> &Header {
        // SAFETY: the header stays valid while the large object lives
        unsafe { self.0.as_ref() }
    }

    /// Finalize and free. SAFETY (caller): it is dead and nobody touches it again.
    unsafe fn free(self) {
        let bytes = self.header().bytes();
        // Objects promoted into a cross-heap (moved away) have no destructor; the destructor word holds the new address
        if !self.header().is_forwarded() {
            // SAFETY: not marked ⇒ unreachable
            unsafe { Header::finalize(self.0.as_ptr()) };
        }
        #[cfg(all(target_os = "linux", not(miri)))]
        if bytes >= LARGE_MMAP {
            // SAFETY: mapped by Large::alloc with the same byte count
            unsafe { libc::munmap(self.0.as_ptr().cast(), bytes) };
            return;
        }
        // SAFETY: the memory comes from Large::alloc with the same layout
        unsafe { dealloc(self.0.as_ptr().cast(), Self::layout(bytes)) };
    }
}

/// Items swept by other workers and waiting for the caller to finalize.
enum Pending {
    /// A swept block, its state and the dead objects in it waiting to be finalized (header addresses)
    Block(Block, State, Vec<usize>),
    /// A dead large object
    Large(Large),
}

/// Where a block goes after sweeping.
pub enum Outcome {
    /// Has free lines (including fully empty): can be allocated from
    Usable(Block),
    /// Full
    Full(Block),
    /// Empty and enough spares are kept: return it to the system (dead objects are finalized, no live objects remain)
    Release(Block),
}

/// Who receives an item once it is swept.
pub trait Sink {
    fn block(&self, outcome: Outcome);
    fn large(&self, survivor: Large);
    fn large_freed(&self, bytes: usize);
}

#[derive(Default)]
pub struct Sweeper {
    /// The epoch that counts as alive this round
    epoch: u32,
    blocks: Vec<Block>,
    next_block: AtomicUsize,
    large: Vec<Large>,
    next_large: AtomicUsize,
    /// Items not yet swept (taken but unfinished ones included)
    unfinished: AtomicUsize,
    /// How many more empty blocks may be kept as spares
    spare: AtomicUsize,
    /// Live bytes this round (including headers)
    live: AtomicUsize,
    /// Only when finalizing on the caller's thread: items swept by other workers waiting for the caller to finalize (they count as swept only once handled)
    pending: Injector<Pending>,
    /// This round, blocks with free lines give whole pages of free lines back to the system
    discard: bool,
}

impl Sweeper {
    /// Start a round (with the heap held exclusively). The previous round must be finished.
    /// `discard`: blocks with free lines give whole pages of free lines back to the system ([`crate::Heap::shrink`]).
    pub fn start(&mut self, epoch: u32, blocks: Vec<Block>, large: Vec<Large>, spare: usize, discard: bool) {
        debug_assert!(self.is_done());
        self.discard = discard;
        *self.unfinished.get_mut() = blocks.len() + large.len();
        *self.next_block.get_mut() = 0;
        *self.next_large.get_mut() = 0;
        *self.spare.get_mut() = spare;
        *self.live.get_mut() = 0;
        self.epoch = epoch;
        self.blocks = blocks;
        self.large = large;
    }

    pub fn is_done(&self) -> bool {
        self.unfinished.load(Ordering::Acquire) == 0
    }

    /// Live bytes found so far this round (complete only once sweeping is done).
    pub fn live(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    /// Items not yet swept (to estimate the number of collector threads).
    pub fn unfinished(&self) -> usize {
        self.unfinished.load(Ordering::Relaxed)
    }

    /// Take an unswept block and sweep it. Returns None when there is nothing left to take. Call [`Sweeper::finished`] after handing out the result.
    pub fn sweep_next_block(&self) -> Option<Outcome> {
        let (b, state) = self.sweep_claimed(None)?;
        Some(self.settle(b, state))
    }

    /// Take an unswept block and sweep it (`deferred`: see [`Block::sweep`]); returns the block and its state.
    fn sweep_claimed(&self, deferred: Option<&mut Vec<usize>>) -> Option<(Block, State)> {
        let i = self.next_block.fetch_add(1, Ordering::AcqRel);
        let b = *self.blocks.get(i)?;
        // SAFETY: this round's marking is complete; the block was given to this taker only and nobody allocates into it before it is swept
        let (state, live) = unsafe { b.sweep::<Header>(self.epoch, deferred) };
        self.live.fetch_add(live, Ordering::Relaxed);
        Some((b, state))
    }

    /// Where a swept block goes (its dead objects are all finalized).
    fn settle(&self, b: Block, state: State) -> Outcome {
        match state {
            State::Full => Outcome::Full(b),
            State::Recyclable => {
                if self.discard {
                    // SAFETY: dead objects are finalized and nothing in the free lines is needed; the block has not been handed out
                    unsafe { b.discard_free_lines(self.epoch) };
                }
                Outcome::Usable(b)
            }
            State::Empty if self.take_spare() => Outcome::Usable(b),
            State::Empty => Outcome::Release(b),
        }
    }

    /// Take one spare slot if another empty block may still be kept.
    fn take_spare(&self) -> bool {
        let mut left = self.spare.load(Ordering::Acquire);
        while left > 0 {
            match self.spare.compare_exchange_weak(left, left - 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(now) => left = now,
            }
        }
        false
    }

    /// An item is done (call after handing out the result).
    pub fn finished(&self) {
        self.unfinished.fetch_sub(1, Ordering::AcqRel);
    }

    /// Take an unswept large object and sweep it; a live one goes to `sink`. Returns false when there is nothing left. `defer`: a dead one is not finalized but put on the pending list.
    fn sweep_next_large(&self, sink: &impl Sink, defer: bool) -> bool {
        let i = self.next_large.fetch_add(1, Ordering::AcqRel);
        let Some(&l) = self.large.get(i) else { return false };
        if l.header().is_marked(self.epoch) {
            self.live.fetch_add(l.header().bytes(), Ordering::Relaxed);
            sink.large(l);
        } else if defer {
            self.pending.push(Pending::Large(l));
            return true;
        } else {
            sink.large_freed(l.header().bytes());
            // SAFETY: this round's marking is complete and it is not marked ⇒ unreachable; this large object was given to this taker only
            unsafe { l.free() };
        }
        self.finished();
        true
    }

    /// Collector thread: sweep until nothing is left to take. `defer`: dead objects are not finalized but put on the pending list with their blocks (the other workers when finalizing only on the caller's thread);
    /// the caller finalizes on the spot and handles a batch of pending items after each block.
    fn work(&self, sink: &impl Sink, defer: bool, caller: bool) {
        let mut dead = Vec::new();
        while let Some((b, state)) = self.sweep_claimed(defer.then_some(&mut dead)) {
            if defer {
                self.pending.push(Pending::Block(b, state, core::mem::take(&mut dead)));
                continue;
            }
            sink.block(self.settle(b, state));
            self.finished();
            if caller {
                self.drain(sink);
            }
        }
        while self.sweep_next_large(sink, defer) {}
    }

    /// Caller: finalize the dead objects in the pending list, then hand the blocks out.
    fn drain(&self, sink: &impl Sink) {
        loop {
            match self.pending.steal() {
                Steal::Success(Pending::Block(b, state, dead)) => {
                    for addr in dead {
                        // SAFETY: objects judged dead while sweeping, not yet finalized; nobody touches the block before it is handed out
                        unsafe { <Header as Finalize>::finalize(core::ptr::with_exposed_provenance::<Core>(addr)) };
                    }
                    sink.block(self.settle(b, state));
                }
                Steal::Success(Pending::Large(l)) => {
                    sink.large_freed(l.header().bytes());
                    // SAFETY: a large object judged dead, only on the pending list
                    unsafe { l.free() }
                }
                Steal::Retry => continue,
                Steal::Empty => return,
            }
            self.finished();
        }
    }

    /// Finish this round on the collector threads, and wait for blocks taken by others (mutators sweeping lazily) to be finished too.
    /// With `parallel_drop` false dead objects are finalized only on the caller's thread (see the module docs).
    pub fn finish(&self, pool: &Pool, sink: &(impl Sink + Sync), parallel_drop: bool) {
        if self.is_done() {
            return;
        }
        pool.run(pool.sized(self.unfinished()), |me| self.work(sink, !parallel_drop && me != 0, me == 0));
        let backoff = Backoff::new();
        loop {
            self.drain(sink);
            if self.is_done() {
                return;
            }
            backoff.snooze();
        }
    }
}

pub struct Releaser<'h> {
    /// Objects to decrement (header addresses)
    decs: &'h Injector<usize>,
    /// Blocks in which objects were released (lines recomputed at the next pause)
    dirty: &'h Injector<Block>,
    workers: usize,
    idle: CachePadded<AtomicUsize>,
    /// Old means: mark epoch equals this
    epoch: u32,
    /// Parity of the current count
    parity: bool,
    /// Only when finalizing on the caller's thread: objects other workers brought to zero but did not finalize (header addresses)
    pending: Option<Injector<usize>>,
}

impl<'h> Releaser<'h> {
    /// With `parallel_drop` false, objects brought to zero are finalized only on the caller's thread.
    pub fn new(decs: &'h Injector<usize>, dirty: &'h Injector<Block>, workers: usize, epoch: u32, parity: bool, parallel_drop: bool) -> Self {
        let pending = (!parallel_drop).then(Injector::new);
        Self { decs, dirty, workers: workers.max(1), idle: CachePadded::new(AtomicUsize::new(0)), epoch, parity, pending }
    }

    pub fn run(&self, pool: &Pool) {
        pool.run(self.workers, |me| self.work(if me == 0 { None } else { self.pending.as_ref() }));
        let Some(pending) = &self.pending else { return };
        loop {
            match pending.steal() {
                Steal::Success(addr) => self.free(core::ptr::with_exposed_provenance::<Header>(addr)),
                Steal::Retry => {}
                Steal::Empty => return,
            }
        }
    }

    fn work(&self, defer: Option<&Injector<usize>>) {
        let local = Worker::new_lifo();
        loop {
            while let Some(addr) = local.pop() {
                self.one(addr, &local, defer);
            }
            if let Some(addr) = self.steal(&local) {
                self.one(addr, &local, defer);
                continue;
            }
            self.idle.fetch_add(1, Ordering::AcqRel);
            let backoff = Backoff::new();
            loop {
                if self.idle.load(Ordering::Acquire) == self.workers {
                    return;
                }
                if let Some(addr) = self.steal(&local) {
                    self.idle.fetch_sub(1, Ordering::AcqRel);
                    self.one(addr, &local, defer);
                    break;
                }
                backoff.snooze();
            }
        }
    }

    fn steal(&self, local: &Worker<usize>) -> Option<usize> {
        loop {
            match self.decs.steal_batch_and_pop(local) {
                Steal::Success(addr) => return Some(addr),
                Steal::Empty => return None,
                Steal::Retry => {}
            }
        }
    }

    /// Decrement an object; an old in-block object that reaches 0 is released: the target of each of its edges is decremented in turn (pushed on the local queue), then it is finalized and its start bit cleared.
    /// Large, stuck and young objects are not released here (left to tracing / young sweeping). `defer`: after the edges, put it on the pending list for the caller instead of finalizing.
    fn one(&self, addr: usize, local: &Worker<usize>, defer: Option<&Injector<usize>>) {
        let header = core::ptr::with_exposed_provenance::<Header>(addr);
        // SAFETY: the decrement comes from a counted edge; the target stays alive until its count reaches zero
        let h = unsafe { &*header };
        if !h.rc_dec(self.parity) || h.is_large() || !h.is_marked(self.epoch) {
            return;
        }
        let mut children = Vec::new();
        let seed = Worker::new_lifo();
        let mut t = Tracer::releasing(&seed, &mut children);
        // SAFETY: not yet finalized; release mode only reads its edges; once the count is zero only the thread that got true touches it
        unsafe { Header::visit(header, &mut t) };
        for child in children {
            local.push(child);
        }
        match defer {
            Some(pending) => pending.push(addr),
            None => self.free(header),
        }
    }

    /// Finalize an object brought to zero, clear its start bit and record the dirty block.
    fn free(&self, header: *const Header) {
        // SAFETY: count zero ⇒ no counted references and unreachable for mutators (see the module docs); only the thread that brought it to zero (or the caller it handed it to) touches it
        unsafe { Header::finalize(header) };
        let block = Block::containing(header.cast());
        block.clear_start(header.cast());
        self.dirty.push(block);
    }
}
