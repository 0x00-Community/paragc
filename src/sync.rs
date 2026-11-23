//! Concurrency primitives: the collector thread [`Pool`], the append-only segmented vector [`SegVec`] and push-only list [`PushList`], the lock-free [`Stack`] inside public heap files, and futex waits across processes.
//!
//! A resident thread pool for collection: the heap's first collection starts `threads − 1` worker threads that live until the heap is dropped; the calling thread itself is worker 0.
//!
//! - Dispatch: write the job and the caller first, then advance one atomic word ("generation + number of participants this round") by one generation, waking only the participating workers
//! - Idle workers spin with backoff (crossbeam `Backoff`) and then `thread::park` (a futex, not a lock), so no CPU is wasted
//! - When every participant is done, the last one wakes the caller; a panic in a worker is re-raised in the caller
//!
//! Two lock-free structures used inside the heap: the append-only segmented vector [`SegVec`] and the push-only / take-all list [`PushList`].
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::num::NonZeroUsize;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
#[cfg(crossheap)]
use core::sync::atomic::AtomicU32;
#[cfg(crossheap)]
use core::time::Duration;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread::{self, JoinHandle, Thread};

use crossbeam_utils::{Backoff, CachePadded};

use crate::layout::UNITS_PER_WORKER;

/// Low bits of the job word: the number of participants this round (including the caller)
const ACTIVE_BITS: u32 = 16;
const ACTIVE_MASK: u64 = (1 << ACTIVE_BITS) - 1;
/// The job word grows by this much per generation
const GENERATION: u64 = 1 << ACTIVE_BITS;

/// One round's job: a type-erased `&dyn Fn(worker id)`, valid only while [`Pool::run`] waits.
type Job = *const (dyn Fn(usize) + Sync);

struct Shared {
    /// Generation in the high bits, participants in the low bits; workers read both at once, so they agree
    task: CachePadded<AtomicU64>,
    /// Workers not yet done this round (excluding the caller)
    remaining: CachePadded<AtomicUsize>,
    job: UnsafeCell<Job>,
    /// This round's caller (the last worker to finish wakes it)
    caller: UnsafeCell<Option<Thread>>,
    /// A worker panicked this round
    panicked: AtomicBool,
    /// A round is running (guards against reentry: concurrent marking calls in through &Heap)
    busy: AtomicBool,
    stop: AtomicBool,
}

// SAFETY: job / caller are written only when no worker reads them (the previous round is fully done and the next generation not yet published); the rest are atomics
unsafe impl Sync for Shared {}
// SAFETY: as above
unsafe impl Send for Shared {}

impl Shared {
    fn task(&self) -> (u64, usize) {
        let word = self.task.load(Ordering::Acquire);
        (word & !ACTIVE_MASK, usize::try_from(word & ACTIVE_MASK).unwrap_or(usize::MAX))
    }

    /// Main loop of worker `me`.
    fn serve(&self, me: usize) {
        // Count from generation 0: a thread that starts late, after the first round was published, does not miss it
        let mut seen = 0;
        loop {
            let (generation, active) = self.wait(seen);
            if self.stop.load(Ordering::Acquire) {
                return;
            }
            seen = generation;
            if me >= active {
                continue;
            }
            // SAFETY: the caller waits until every worker of this round is done, so the closure behind job is alive and unchanged
            let job = unsafe { &**self.job.get() };
            if catch_unwind(AssertUnwindSafe(|| job(me))).is_err() {
                self.panicked.store(true, Ordering::Release);
            }
            // Copy the caller before decrementing: the moment the count reaches 0 the caller may return, start the next round and overwrite caller
            // SAFETY: this worker has not decremented yet, the round is not done, the caller does not change caller
            let caller = unsafe { (*self.caller.get()).clone() };
            if self.remaining.fetch_sub(1, Ordering::AcqRel) == 1
                && let Some(caller) = caller
            {
                // The caller may already be gone; an extra unpark is harmless
                caller.unpark();
            }
        }
    }

    /// Wait until the generation is not `seen` (or we must stop).
    fn wait(&self, seen: u64) -> (u64, usize) {
        let backoff = Backoff::new();
        loop {
            let task = self.task();
            if task.0 != seen || self.stop.load(Ordering::Acquire) {
                return task;
            }
            if backoff.is_completed() {
                thread::park();
            } else {
                backoff.snooze();
            }
        }
    }
}

pub struct Pool {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

impl Pool {
    /// Start `threads − 1` workers.
    pub fn new(threads: NonZeroUsize) -> Self {
        let shared = Arc::new(Shared {
            task: CachePadded::new(AtomicU64::new(0)),
            remaining: CachePadded::new(AtomicUsize::new(0)),
            job: UnsafeCell::new(core::ptr::null::<fn(usize)>() as Job),
            caller: UnsafeCell::new(None),
            panicked: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let max = u64::try_from(threads.get()).unwrap_or(ACTIVE_MASK).min(ACTIVE_MASK);
        let workers = (1..usize::try_from(max).unwrap_or(1))
            .map(|me| {
                let shared = shared.clone();
                thread::Builder::new().name(format!("paragc-{me}")).spawn(move || shared.serve(me)).expect("start a collector thread")
            })
            .collect();
        Self { shared, workers }
    }

    /// Total threads (including the caller).
    pub fn threads(&self) -> usize {
        self.workers.len() + 1
    }

    /// How many threads this much work (blocks + large objects) deserves: fewer when there is little.
    pub fn sized(&self, units: usize) -> usize {
        (units / UNITS_PER_WORKER).clamp(1, self.threads())
    }

    /// Run `f(worker id)` once on each of `n` threads (including the caller, ids 0..n) and collect the results by id.
    pub fn run<R: Send>(&self, n: usize, f: impl Fn(usize) -> R + Sync) -> Vec<R> {
        let n = n.clamp(1, self.threads());
        if n == 1 {
            return vec![f(0)];
        }
        let _busy = Busy::enter(&self.shared.busy);
        let slots: Vec<UnsafeCell<Option<R>>> = (0..n).map(|_| UnsafeCell::new(None)).collect();
        let slots = Slots(&slots);
        let job = |me: usize| slots.put(me, f(me));
        let job: &(dyn Fn(usize) + Sync) = &job;
        let shared = &*self.shared;
        // SAFETY: the previous round is fully done (Busy keeps run from reentering), so no worker reads job / caller now;
        // the erased lifetime is upheld by "return only after everyone is done" below
        #[expect(clippy::transmute_ptr_to_ptr, reason = "erasing the lifetime of a dyn cannot be done with as")]
        unsafe {
            *shared.job.get() = core::mem::transmute::<*const (dyn Fn(usize) + Sync + '_), Job>(job);
            *shared.caller.get() = Some(thread::current());
        }
        shared.panicked.store(false, Ordering::Relaxed);
        shared.remaining.store(n - 1, Ordering::Relaxed);
        let active = u64::try_from(n).unwrap_or(ACTIVE_MASK);
        let word = shared.task.load(Ordering::Relaxed);
        shared.task.store(((word & !ACTIVE_MASK) + GENERATION) | active, Ordering::Release);
        for w in &self.workers[..n - 1] {
            w.thread().unpark();
        }
        let mine = catch_unwind(AssertUnwindSafe(|| job(0)));
        let backoff = Backoff::new();
        while shared.remaining.load(Ordering::Acquire) != 0 {
            if backoff.is_completed() {
                thread::park();
            } else {
                backoff.snooze();
            }
        }
        if let Err(e) = mine {
            std::panic::resume_unwind(e);
        }
        assert!(!shared.panicked.load(Ordering::Acquire), "a collector thread panicked");
        slots.take()
    }
}

/// Marks a round as in progress: one round at a time.
struct Busy<'a>(&'a AtomicBool);

impl<'a> Busy<'a> {
    /// Wait until no other round is running (concurrent marking and concurrent sweeping may arrive from different threads at once).
    fn enter(flag: &'a AtomicBool) -> Self {
        let backoff = Backoff::new();
        while flag.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            backoff.snooze();
        }
        Self(flag)
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Result slots: one per worker id; each thread writes only its own.
struct Slots<'a, R>(&'a [UnsafeCell<Option<R>>]);

// SAFETY: each slot is written by one thread only, and read after all writes are done
unsafe impl<R: Send> Sync for Slots<'_, R> {}

impl<R> Slots<'_, R> {
    fn put(&self, me: usize, r: R) {
        // SAFETY: each worker id has one thread writing its own slot, read after everyone is done
        unsafe { *self.0[me].get() = Some(r) };
    }

    /// Take the results once everyone is done.
    fn take(&self) -> Vec<R> {
        // SAFETY: every worker is done, no concurrent writes
        self.0.iter().map(|s| unsafe { (*s.get()).take() }.expect("every worker id wrote a result")).collect()
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.task.fetch_add(GENERATION, Ordering::AcqRel);
        for w in self.workers.drain(..) {
            w.thread().unpark();
            // A panic in a worker was already reported in run
            let _ = w.join();
        }
    }
}

/// Capacity of the first segment (a power of two); each later segment doubles
const FIRST: usize = 32;
/// Segment limit: the capacities together cover all of usize
const SEGMENTS: usize = usize::BITS as usize - FIRST.trailing_zeros() as usize;

/// A cell: `ready` is set only after the value is written; readers only accept ready cells.
struct Cell<T> {
    ready: AtomicBool,
    value: UnsafeCell<MaybeUninit<T>>,
}

/// An append-only segmented vector: segments never move once allocated, so element addresses are stable and `push` can run alongside reads without locks.
pub struct SegVec<T> {
    segments: [AtomicPtr<Cell<T>>; SEGMENTS],
    /// Cells handed out (including ones not yet written)
    len: AtomicUsize,
}

// SAFETY: a cell becomes visible to others only once written (ready's Release / Acquire); after that only &T is given out
unsafe impl<T: Send + Sync> Sync for SegVec<T> {}
// SAFETY: the elements move with the container
unsafe impl<T: Send> Send for SegVec<T> {}

impl<T> Default for SegVec<T> {
    fn default() -> Self {
        Self { segments: core::array::from_fn(|_| AtomicPtr::new(null_mut())), len: AtomicUsize::new(0) }
    }
}

impl<T> SegVec<T> {
    /// Capacity of segment `k`.
    fn capacity(k: usize) -> usize {
        FIRST << k
    }

    /// Index → (segment, index within the segment).
    fn locate(i: usize) -> (usize, usize) {
        let j = i + FIRST;
        let k = (usize::BITS - 1 - j.leading_zeros()) as usize - FIRST.trailing_zeros() as usize;
        (k, j - Self::capacity(k))
    }

    /// Segment `k`, allocating it if missing (when two threads allocate at once the loser frees its own).
    fn segment(&self, k: usize) -> *mut Cell<T> {
        let seg = self.segments[k].load(Ordering::Acquire);
        if !seg.is_null() {
            return seg;
        }
        let fresh: Box<[Cell<T>]> = (0..Self::capacity(k)).map(|_| Cell { ready: AtomicBool::new(false), value: UnsafeCell::new(MaybeUninit::uninit()) }).collect();
        let fresh = Box::into_raw(fresh).cast::<Cell<T>>();
        match self.segments[k].compare_exchange(null_mut(), fresh, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => fresh,
            Err(won) => {
                // SAFETY: just allocated and never handed out
                drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(fresh, Self::capacity(k))) });
                won
            }
        }
    }

    fn cell(&self, i: usize) -> Option<&Cell<T>> {
        let (k, at) = Self::locate(i);
        let seg = self.segments.get(k)?.load(Ordering::Acquire);
        // SAFETY: a non-null segment has capacity(k) cells and at is in range; segments are not freed (except with &mut self)
        (!seg.is_null()).then(|| unsafe { &*seg.add(at) })
    }

    /// Append and return the index.
    pub fn push(&self, value: T) -> usize {
        let i = self.len.fetch_add(1, Ordering::AcqRel);
        let (k, at) = Self::locate(i);
        // SAFETY: the segment has capacity(k) cells; index i is written only by this thread
        let cell = unsafe { &*self.segment(k).add(at) };
        // SAFETY: as above; ready is not set yet, nobody reads it
        unsafe { (*cell.value.get()).write(value) };
        cell.ready.store(true, Ordering::Release);
        i
    }

    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Element `i`; None if not fully written yet.
    pub fn get(&self, i: usize) -> Option<&T> {
        let cell = self.cell(i)?;
        // SAFETY: after ready the value is initialized and never changes
        cell.ready.load(Ordering::Acquire).then(|| unsafe { (*cell.value.get()).assume_init_ref() })
    }

    /// Elements already written (skipping ones still being written).
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        (0..self.len()).filter_map(|i| self.get(i))
    }

    /// With exclusive access, take all elements and leave the container empty (segments are kept for reuse).
    pub fn take(&mut self) -> Vec<T> {
        let n = core::mem::replace(self.len.get_mut(), 0);
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let (k, at) = Self::locate(i);
            let seg = *self.segments[k].get_mut();
            // SAFETY: exclusive: every push has returned and the first n cells are written
            let cell = unsafe { &mut *seg.add(at) };
            *cell.ready.get_mut() = false;
            // SAFETY: it was written; once taken here ready is cleared, so it is not read or dropped again
            out.push(unsafe { cell.value.get_mut().assume_init_read() });
        }
        out
    }
}

impl<T> Extend<T> for SegVec<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for v in iter {
            self.push(v);
        }
    }
}

impl<T> Drop for SegVec<T> {
    fn drop(&mut self) {
        drop(self.take());
        for (k, seg) in self.segments.iter_mut().enumerate() {
            let p = *seg.get_mut();
            if !p.is_null() {
                // SAFETY: the segment is the Box<[Cell<T>]> from segment(), and its elements were all taken
                drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(p, Self::capacity(k))) });
            }
        }
    }
}

struct Node<T> {
    value: T,
    next: *mut Node<T>,
}

/// A lock-free push-only, take-all list (the Treiber stack head is only ever swapped to empty, so there is no ABA).
pub struct PushList<T> {
    head: AtomicPtr<Node<T>>,
}

// SAFETY: nodes pass as a whole from pushers to the taker
unsafe impl<T: Send> Sync for PushList<T> {}
// SAFETY: as above
unsafe impl<T: Send> Send for PushList<T> {}

impl<T> Default for PushList<T> {
    fn default() -> Self {
        Self { head: AtomicPtr::new(null_mut()) }
    }
}

impl<T> PushList<T> {
    pub fn push(&self, value: T) {
        let node = Box::into_raw(Box::new(Node { value, next: null_mut() }));
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            // SAFETY: the node is not published yet, only this thread touches it
            unsafe { (*node).next = head };
            match self.head.compare_exchange_weak(head, node, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(now) => head = now,
            }
        }
    }

    /// Take everything (latest push first).
    pub fn take(&self) -> Vec<T> {
        let mut p = self.head.swap(null_mut(), Ordering::AcqRel);
        let mut out = Vec::new();
        while !p.is_null() {
            // SAFETY: the whole chain was just swapped out and belongs to this thread; nodes come from push's Box
            let node = unsafe { Box::from_raw(p) };
            p = node.next;
            out.push(node.value);
        }
        out
    }
}

impl<T> Drop for PushList<T> {
    fn drop(&mut self) {
        drop(self.take());
    }
}

/// Low 32 bits of a stack head: top index + 1 (0 = empty); the high 32 bits count changes (against ABA)
#[cfg(crossheap)]
const STACK_TOP: u64 = u32::MAX as u64;
/// Shift of the change count in a stack head
#[cfg(crossheap)]
const STACK_TAG_SHIFT: u32 = u32::BITS;

/// A lock-free stack inside the window (free blocks, returned entries): the head lives in the superblock and each element's "next" lives in its own table entry.
#[cfg(crossheap)]
pub struct Stack<'a> {
    head: &'a AtomicU64,
}

#[cfg(crossheap)]
impl<'a> Stack<'a> {
    /// A stack whose head is `head`.
    pub fn on(head: &'a AtomicU64) -> Self {
        Self { head }
    }

    pub fn push(&self, i: u32, next: &AtomicU32) {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            next.store((head & STACK_TOP) as u32, Ordering::Relaxed);
            let new = ((head >> STACK_TAG_SHIFT).wrapping_add(1) << STACK_TAG_SHIFT) | u64::from(i + 1);
            match self.head.compare_exchange_weak(head, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(now) => head = now,
            }
        }
    }

    pub fn pop<'n>(&self, next_of: impl Fn(u32) -> &'n AtomicU32) -> Option<u32> {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            let top = (head & STACK_TOP) as u32;
            let i = top.checked_sub(1)?;
            // The "next" read may belong to an element someone just popped and changed: then the head's change count changed too and the CAS below fails
            let next = next_of(i).load(Ordering::Relaxed);
            let new = ((head >> STACK_TAG_SHIFT).wrapping_add(1) << STACK_TAG_SHIFT) | u64::from(next);
            match self.head.compare_exchange_weak(head, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(i),
                Err(now) => head = now,
            }
        }
    }

    /// Take the whole stack at once, earliest push first. The taken elements belong to the caller only.
    pub fn take_all<'n>(&self, next_of: impl Fn(u32) -> &'n AtomicU32) -> Vec<u32> {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            if head & STACK_TOP == 0 {
                return Vec::new();
            }
            let new = (head >> STACK_TAG_SHIFT).wrapping_add(1) << STACK_TAG_SHIFT;
            match self.head.compare_exchange_weak(head, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(now) => head = now,
            }
        }
        let mut out = Vec::new();
        let mut top = (head & STACK_TOP) as u32;
        while let Some(i) = top.checked_sub(1) {
            out.push(i);
            top = next_of(i).load(Ordering::Relaxed);
        }
        out.reverse();
        out
    }
}

#[cfg(crossheap)]
/// A 32-bit word in shared memory used as a futex (without FUTEX_PRIVATE_FLAG, so it works across processes). Under Miri it degrades to yielding.
pub trait FutexWord {
    /// Sleep while the word still equals `expected`, at most `timeout` (waking does not guarantee the word changed; the caller checks again).
    fn wait(&self, expected: u32, timeout: Duration);
    /// Wake every thread sleeping on this word (in any process).
    fn wake_all(&self);
}

#[cfg(all(crossheap, not(miri)))]
impl FutexWord for AtomicU32 {
    fn wait(&self, expected: u32, timeout: Duration) {
        let ts = libc::timespec {
            tv_sec: libc::time_t::try_from(timeout.as_secs()).unwrap_or(libc::time_t::MAX),
            tv_nsec: timeout.subsec_nanos().into(),
        };
        // SAFETY: the word stays valid in shared memory; FUTEX_WAIT only reads it
        unsafe { libc::syscall(libc::SYS_futex, self.as_ptr(), libc::FUTEX_WAIT, expected, &raw const ts) };
    }

    fn wake_all(&self) {
        // SAFETY: as above
        unsafe { libc::syscall(libc::SYS_futex, self.as_ptr(), libc::FUTEX_WAKE, i32::MAX) };
    }
}

#[cfg(all(crossheap, miri))]
impl FutexWord for AtomicU32 {
    fn wait(&self, _: u32, _: Duration) {
        std::thread::yield_now();
    }

    fn wake_all(&self) {}
}

/// Backoff before trying again after a collector gave up its claim: a random duration (to stagger collectors contending at the same time so they do not collide every time),
/// with a cap that doubles each time, up to [`Retry::MAX`].
#[cfg(crossheap)]
pub struct Retry {
    round: u32,
    /// Each collector's own random source (std's random seed), no global state
    seed: std::collections::hash_map::RandomState,
}

#[cfg(crossheap)]
impl Retry {
    /// Cap of the first backoff
    const BASE: Duration = Duration::from_millis(2);
    /// Ceiling of the backoff cap
    const MAX: Duration = Duration::from_millis(200);

    pub fn new() -> Self {
        Self { round: 0, seed: std::collections::hash_map::RandomState::new() }
    }

    pub fn pause(&mut self) {
        let cap = Self::BASE.saturating_mul(1 << self.round.min(16)).min(Self::MAX);
        let nanos = u64::try_from(cap.as_nanos()).unwrap_or(u64::MAX).max(1);
        let jitter = std::hash::BuildHasher::hash_one(&self.seed, self.round) % nanos;
        std::thread::sleep(Duration::from_nanos(jitter));
        self.round += 1;
    }
}
