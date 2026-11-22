//! cppgc (V8's Oilpan, C++), used through rusty_v8's bindings. Two modes (environment variable `CPPGC_HEAP`):
//!
//! - `attached` (default, reported as `cppgc`): the heap is attached to an isolate (as Blink uses it), and collection goes through V8's full GC
//!   (`request_garbage_collection_for_testing(Full)`, precise, no stack scanning), with parallel marking on the platform's worker threads;
//!   in gcbench the timing of collections is left to V8's own scheduling
//! - `detached` (reported as `cppgc-detached`): the heap is not attached to an isolate and collection uses `collect_garbage_for_testing`.
//!   Measured, this path is a single-threaded atomic collection unaffected by the thread count or `--cppheap-concurrent-marking`; gcbench collects by hand with paragc's trigger policy
//!
//! Common: 1 thread uses the single-threaded platform (no worker threads), more than 1 the default platform with that many worker threads; `V8_FLAGS` passes extra V8 options.
//! Every allocation, every `Member` construction and every edge during marking crosses the Rust ⇄ C++ FFI once, and that cost is charged to cppgc.
//! A heap can be used by one thread only, so there is no alloc-mt.
use std::cell::UnsafeCell;
use std::ffi::CStr;
use std::num::NonZeroUsize;
use std::str::FromStr;

use bench_common::{Args, Bench, Clock, FRAG_PER_ROUND, FRAG_ROUNDS, Footprint, Frag, GcBench, LONG_LIVED_DEPTH, PAUSE_DEPTH, STRETCH_DEPTH, Size, Took, Tree};
use v8::cppgc::{EmbedderStackState, GarbageCollected, Heap, HeapCreateParams, Member, Persistent, UnsafePtr, Visitor, make_garbage_collected};
use v8::{GarbageCollectionType, SharedRef, UniqueRef};

/// Collection trigger floor (the same as paragc; detached only)
const MIN_TRIGGER: usize = 4 * 1024 * 1024;
/// Collect once new allocation exceeds "live × this" (the same as paragc; detached only)
const GROWTH: usize = 2;
/// Approximate bytes per node on the cppgc heap: object header 8 + RustObj header and dyn pointer 24 + two Members and a u64 24
const NODE_BYTES: usize = 56;
/// V8's full GC needs this option to be triggered by the embedder
const EXPOSE_GC: &str = "--expose-gc";
/// The single-threaded platform needs this option (so V8 starts no parallel / concurrent tasks), otherwise attaching to an isolate crashes
const SINGLE_THREADED: &str = "--single-threaded";

struct Node {
    left: Member<Node>,
    right: Member<Node>,
    #[expect(dead_code, reason = "payload: as large as the other libraries' nodes")]
    value: u64,
}

// SAFETY: both Members are traced
unsafe impl GarbageCollected for Node {
    fn trace(&self, visitor: &mut Visitor) {
        visitor.trace(&self.left);
        visitor.trace(&self.right);
    }

    fn get_name(&self) -> &'static CStr {
        c"Node"
    }
}

/// An object of the fragmentation load: `N` u64s of data
struct Blob<const N: usize> {
    _data: [u64; N],
}

// SAFETY: no Members
unsafe impl<const N: usize> GarbageCollected for Blob<N> {
    fn trace(&self, _: &mut Visitor) {}

    fn get_name(&self) -> &'static CStr {
        c"Blob"
    }
}

/// A chunk of slots: [`CHUNK`] Members on the cppgc heap (Members may only live in heap objects: the write barrier finds the page from the slot's address).
/// Changed only on the mutator thread.
struct Chunk<const N: usize> {
    slots: UnsafeCell<[Member<Blob<N>>; CHUNK]>,
}

/// Slots per chunk
const CHUNK: usize = 64;

// SAFETY: every slot is traced
unsafe impl<const N: usize> GarbageCollected for Chunk<N> {
    fn trace(&self, visitor: &mut Visitor) {
        // SAFETY: no mutator changes this chunk while marking (incremental marking runs at the mutator's safepoints; slot changes go through Member's write barrier)
        unsafe { (*self.slots.get()).iter().for_each(|m| visitor.trace(m)) };
    }

    fn get_name(&self) -> &'static CStr {
        c"Chunk"
    }
}

/// All slots of one size (chunks held by Persistents).
struct Slots<const N: usize>(Vec<Persistent<Chunk<N>>>);

impl<const N: usize> Slots<N> {
    fn new(heap: &Heap, size: Size) -> Self {
        let chunk = || {
            // SAFETY: put into a Persistent right away
            Persistent::new(&unsafe { make_garbage_collected(heap, Chunk { slots: UnsafeCell::new(std::array::from_fn(|_| Member::empty())) }) })
        };
        Self((0..size.slots().div_ceil(CHUNK)).map(|_| chunk()).collect())
    }

    /// Allocate an object and put it in slot `slot` if needed; returns roughly how many bytes it takes on the cppgc heap.
    fn put(&self, heap: &Heap, slot: Option<usize>) -> usize {
        // SAFETY: the new object goes into a slot or is dropped right away, with no allocation in between
        let blob = unsafe { make_garbage_collected(heap, Blob { _data: [0; N] }) };
        if let Some(i) = slot {
            let chunk = self.0[i / CHUNK].get().expect("the chunk is held by a Persistent");
            // SAFETY: only this thread changes slots
            unsafe { (&mut *chunk.slots.get())[i % CHUNK].set(&blob) };
        }
        BLOB_OVERHEAD + N * size_of::<u64>()
    }
}

/// The holder of the fragmentation load: one group of slots per size
struct Holder {
    small: Slots<3>,
    medium: Slots<30>,
    large: Slots<200>,
    huge: Slots<1500>,
}

impl Holder {
    fn new(heap: &Heap) -> Self {
        Self { small: Slots::new(heap, Size::Small), medium: Slots::new(heap, Size::Medium), large: Slots::new(heap, Size::Large), huge: Slots::new(heap, Size::Huge) }
    }

    /// Allocate an object and put it in a slot if needed; returns roughly how many bytes it takes on the cppgc heap (header 8 + RustObj wrapper 24 + data).
    fn place(&self, heap: &Heap, frag: &mut Frag) -> usize {
        let pick = frag.next();
        match pick.size {
            Size::Small => self.small.put(heap, pick.slot),
            Size::Medium => self.medium.put(heap, pick.slot),
            Size::Large => self.large.put(heap, pick.slot),
            Size::Huge => self.huge.put(heap, pick.slot),
        }
    }

    /// Live bytes when every slot is full (an estimate, for detached's trigger)
    fn live() -> usize {
        [(Size::Small, 3), (Size::Medium, 30), (Size::Large, 200), (Size::Huge, 1500)].iter().map(|&(s, n)| s.slots() * (BLOB_OVERHEAD + n * size_of::<u64>())).sum()
    }
}

/// Bytes besides data per object of the fragmentation load on the cppgc heap: object header 8 + RustObj header and dyn pointer 24
const BLOB_OVERHEAD: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq)]
enum HeapMode {
    Attached,
    Detached,
}

impl FromStr for HeapMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "attached" => Ok(Self::Attached),
            "detached" => Ok(Self::Detached),
            _ => Err(format!("CPPGC_HEAP accepts only attached / detached, not {s}")),
        }
    }
}

/// The process-wide V8 platform (initialized only once).
struct V8 {
    platform: SharedRef<v8::Platform>,
}

impl From<NonZeroUsize> for V8 {
    fn from(threads: NonZeroUsize) -> Self {
        let platform = if threads.get() == 1 {
            v8::new_single_threaded_default_platform(false)
        } else {
            v8::new_default_platform(u32::try_from(threads.get()).expect("too many threads"), false)
        }
        .make_shared();
        v8::V8::set_flags_from_string(EXPOSE_GC);
        if threads.get() == 1 {
            v8::V8::set_flags_from_string(SINGLE_THREADED);
        }
        if let Ok(flags) = std::env::var("V8_FLAGS") {
            v8::V8::set_flags_from_string(&flags);
        }
        v8::V8::initialize_platform(platform.clone());
        v8::V8::initialize();
        Self { platform }
    }
}

impl V8 {
    fn heap(&self) -> UniqueRef<Heap> {
        Heap::create(self.platform.clone(), HeapCreateParams::default())
    }

    /// Build a tree; the returned pointer lives only on the stack and goes into a Persistent / Member or is dropped right away.
    fn tree(heap: &Heap, depth: u32) -> UnsafePtr<Node> {
        let (left, right) = if depth > 1 {
            (Member::new(&Self::tree(heap, depth - 1)), Member::new(&Self::tree(heap, depth - 1)))
        } else {
            (Member::empty(), Member::empty())
        };
        // SAFETY: see above
        unsafe { make_garbage_collected(heap, Node { left, right, value: depth.into() }) }
    }

    fn count(root: &Persistent<Node>) -> usize {
        let mut stack: Vec<&Node> = root.get().into_iter().collect();
        let mut n = 0;
        while let Some(x) = stack.pop() {
            n += 1;
            // SAFETY: the tree is held by a Persistent and is not collected while traversed
            stack.extend(unsafe { x.left.get() });
            stack.extend(unsafe { x.right.get() });
        }
        n
    }
}

/// A heap attached to an isolate.
struct Attached(V8);

impl Attached {
    /// The isolate and its cppgc heap. The heap belongs to the isolate, and the pointer is valid while the isolate lives.
    fn isolate(&self) -> (v8::OwnedIsolate, *const Heap) {
        let mut isolate = v8::Isolate::new(v8::CreateParams::default().cpp_heap(self.0.heap()));
        let heap: *const Heap = isolate.get_cpp_heap().expect("the cppgc heap just attached");
        (isolate, heap)
    }
}

impl Bench for Attached {
    const NAME: &str = "cppgc";

    fn pause(&self) -> Option<Took> {
        let (mut isolate, heap) = self.isolate();
        // SAFETY: the heap belongs to the isolate, which lives until the end of the function
        let heap = unsafe { &*heap };
        // Baseline: the time of a full GC with the cppgc heap empty (V8's own share), printed to stderr for comparison
        let start = Clock::start();
        isolate.request_garbage_collection_for_testing(GarbageCollectionType::Full);
        eprintln!("cppgc empty-heap full GC baseline {:.2} ms", start.stop().wall.as_secs_f64() * 1e3);
        let live = Persistent::new(&V8::tree(heap, PAUSE_DEPTH));
        V8::tree(heap, PAUSE_DEPTH);
        let start = Clock::start();
        isolate.request_garbage_collection_for_testing(GarbageCollectionType::Full);
        let took = start.stop();
        assert_eq!(V8::count(&live), Tree::size(PAUSE_DEPTH));
        drop(live);
        Some(took)
    }

    fn gcbench(&self) -> Option<Took> {
        let (isolate, heap) = self.isolate();
        // SAFETY: as above
        let heap = unsafe { &*heap };
        let start = Clock::start();
        V8::tree(heap, STRETCH_DEPTH);
        let long = Persistent::new(&V8::tree(heap, LONG_LIVED_DEPTH));
        for (depth, iters) in GcBench::rounds() {
            for _ in 0..iters {
                V8::tree(heap, depth);
            }
        }
        let took = start.stop();
        assert_eq!(V8::count(&long), Tree::size(LONG_LIVED_DEPTH));
        drop(long);
        drop(isolate);
        Some(took)
    }

    /// Collection timing is left to V8's own scheduling; RSS is measured after the last full GC.
    fn frag(&self) -> Option<(Took, Footprint)> {
        let base = Footprint::rss();
        let start = Clock::start();
        let (mut isolate, heap) = self.isolate();
        // SAFETY: as above
        let heap = unsafe { &*heap };
        let h = Holder::new(heap);
        let mut frag = Frag::default();
        let mut peak = 0;
        for _ in 0..FRAG_ROUNDS {
            for _ in 0..FRAG_PER_ROUND {
                h.place(heap, &mut frag);
            }
            peak = peak.max(Footprint::rss().saturating_sub(base));
        }
        isolate.request_garbage_collection_for_testing(GarbageCollectionType::Full);
        let took = start.stop();
        let after = Footprint::rss().saturating_sub(base);
        drop(h);
        drop(isolate);
        Some((took, Footprint { after, peak: peak.max(after) }))
    }
}

/// A heap not attached to an isolate.
struct Detached(V8);

impl Detached {
    fn heap(&self) -> UniqueRef<Heap> {
        let heap = self.0.heap();
        heap.enable_detached_garbage_collections_for_testing();
        heap
    }

    fn collect(heap: &Heap) {
        // SAFETY: when called no pointer into the heap is on the stack (the trees are held by Persistents or already dropped)
        unsafe { heap.collect_garbage_for_testing(EmbedderStackState::NoHeapPointers) };
    }
}

/// Decide when to collect with paragc's policy (estimating bytes from node counts).
#[derive(Default)]
struct Trigger {
    allocated: usize,
    live: usize,
}

impl Trigger {
    fn allocated(&mut self, heap: &Heap, nodes: usize) {
        self.allocated += nodes * NODE_BYTES;
        if self.allocated >= self.live.max(MIN_TRIGGER) * GROWTH {
            Detached::collect(heap);
            self.allocated = 0;
        }
    }
}

impl Bench for Detached {
    const NAME: &str = "cppgc-detached";

    fn pause(&self) -> Option<Took> {
        let heap = self.heap();
        let live = Persistent::new(&V8::tree(&heap, PAUSE_DEPTH));
        V8::tree(&heap, PAUSE_DEPTH);
        let start = Clock::start();
        Self::collect(&heap);
        let took = start.stop();
        assert_eq!(V8::count(&live), Tree::size(PAUSE_DEPTH));
        drop(live);
        Some(took)
    }

    fn gcbench(&self) -> Option<Took> {
        let heap = self.heap();
        let start = Clock::start();
        let mut trigger = Trigger::default();
        V8::tree(&heap, STRETCH_DEPTH);
        trigger.allocated(&heap, Tree::size(STRETCH_DEPTH));
        let long = Persistent::new(&V8::tree(&heap, LONG_LIVED_DEPTH));
        trigger.live = Tree::size(LONG_LIVED_DEPTH) * NODE_BYTES;
        for (depth, iters) in GcBench::rounds() {
            for _ in 0..iters {
                V8::tree(&heap, depth);
                trigger.allocated(&heap, Tree::size(depth));
            }
        }
        let took = start.stop();
        assert_eq!(V8::count(&long), Tree::size(LONG_LIVED_DEPTH));
        drop(long);
        Some(took)
    }

    /// Collect by hand with paragc's trigger policy (by estimated bytes).
    fn frag(&self) -> Option<(Took, Footprint)> {
        let base = Footprint::rss();
        let start = Clock::start();
        let heap = self.heap();
        let h = Holder::new(&heap);
        let mut frag = Frag::default();
        let (mut allocated, mut live, mut peak) = (0, 0, 0);
        for _ in 0..FRAG_ROUNDS {
            for _ in 0..FRAG_PER_ROUND {
                allocated += h.place(&heap, &mut frag);
                if allocated >= live.max(MIN_TRIGGER) * GROWTH {
                    Self::collect(&heap);
                    live = Holder::live();
                    allocated = 0;
                }
            }
            peak = peak.max(Footprint::rss().saturating_sub(base));
        }
        Self::collect(&heap);
        let took = start.stop();
        let after = Footprint::rss().saturating_sub(base);
        drop(h);
        Some((took, Footprint { after, peak: peak.max(after) }))
    }
}

fn main() {
    let args = Args::parse();
    let mode = std::env::var("CPPGC_HEAP").map_or(Ok(HeapMode::Attached), |s| s.parse()).unwrap_or_else(|e| panic!("{e}"));
    let v8 = V8::from(args.threads);
    match mode {
        HeapMode::Attached => args.report(&Attached(v8)),
        HeapMode::Detached => args.report(&Detached(v8)),
    }
}
