//! cppgc（V8 的 Oilpan，C++），经 rusty_v8 的绑定使用。两种用法（环境变量 `CPPGC_HEAP`）：
//!
//! - `attached`（缺省，报告名 `cppgc`）：堆挂在 isolate 上（Blink 的用法），回收走 V8 的完整 GC
//!   （`request_garbage_collection_for_testing(Full)`，精确、不扫栈），并行标记用平台的工作线程；
//!   gcbench 里回收时机交给 V8 自己调度
//! - `detached`（报告名 `cppgc-detached`）：堆不挂 isolate，回收用 `collect_garbage_for_testing`。
//!   实测这条路径是单线程的原子回收，线程数和 `--cppheap-concurrent-marking` 都不影响；gcbench 按 paragc 的触发策略手动回收
//!
//! 共同：线程数 1 用单线程平台（没有工作线程），大于 1 用带这么多工作线程的默认平台；`V8_FLAGS` 传额外的 V8 选项。
//! 每次分配、每个 `Member` 的构造、标记时每条边都要过一次 Rust ⇄ C++ 的 FFI，这部分开销算在 cppgc 头上。
//! 只能一个线程用一个堆，没有 alloc-mt。
use std::cell::UnsafeCell;
use std::ffi::CStr;
use std::num::NonZeroUsize;
use std::str::FromStr;

use bench_common::{Args, Bench, Clock, FRAG_PER_ROUND, FRAG_ROUNDS, Footprint, Frag, GcBench, LONG_LIVED_DEPTH, PAUSE_DEPTH, STRETCH_DEPTH, Size, Took, Tree};
use v8::cppgc::{EmbedderStackState, GarbageCollected, Heap, HeapCreateParams, Member, Persistent, UnsafePtr, Visitor, make_garbage_collected};
use v8::{GarbageCollectionType, SharedRef, UniqueRef};

/// 触发回收的下限（与 paragc 相同；只用于 detached）
const MIN_TRIGGER: usize = 4 * 1024 * 1024;
/// 新分配超过「存活 × 它」就回收（与 paragc 相同；只用于 detached）
const GROWTH: usize = 2;
/// 每个节点在 cppgc 堆上大约占的字节数：对象头 8 + RustObj 头与 dyn 指针 24 + 两个 Member 与 u64 24
const NODE_BYTES: usize = 56;
/// V8 的完整 GC 要这个选项才能从嵌入方触发
const EXPOSE_GC: &str = "--expose-gc";
/// 单线程平台必须配这个选项（V8 不再起并行 / 并发任务），否则挂 isolate 时崩溃
const SINGLE_THREADED: &str = "--single-threaded";

struct Node {
    left: Member<Node>,
    right: Member<Node>,
    #[expect(dead_code, reason = "负载：和其他库的节点一样大")]
    value: u64,
}

// SAFETY: 两个 Member 都追踪了
unsafe impl GarbageCollected for Node {
    fn trace(&self, visitor: &mut Visitor) {
        visitor.trace(&self.left);
        visitor.trace(&self.right);
    }

    fn get_name(&self) -> &'static CStr {
        c"Node"
    }
}

/// 碎片负载的对象：`N` 个 u64 的数据
struct Blob<const N: usize> {
    _data: [u64; N],
}

// SAFETY: 没有 Member
unsafe impl<const N: usize> GarbageCollected for Blob<N> {
    fn trace(&self, _: &mut Visitor) {}

    fn get_name(&self) -> &'static CStr {
        c"Blob"
    }
}

/// 一块槽：[`CHUNK`] 个 Member，放在 cppgc 堆上（Member 只能在堆对象里：写屏障按槽地址找所在页）。
/// 只在 mutator 线程上改。
struct Chunk<const N: usize> {
    slots: UnsafeCell<[Member<Blob<N>>; CHUNK]>,
}

/// 每块几个槽
const CHUNK: usize = 64;

// SAFETY: 槽都追踪了
unsafe impl<const N: usize> GarbageCollected for Chunk<N> {
    fn trace(&self, visitor: &mut Visitor) {
        // SAFETY: 标记时 mutator 不在改这一块（增量标记在 mutator 的安全点上跑；改槽走 Member 的写屏障）
        unsafe { (*self.slots.get()).iter().for_each(|m| visitor.trace(m)) };
    }

    fn get_name(&self) -> &'static CStr {
        c"Chunk"
    }
}

/// 一种大小的全部槽（按块挂在 Persistent 上）。
struct Slots<const N: usize>(Vec<Persistent<Chunk<N>>>);

impl<const N: usize> Slots<N> {
    fn new(heap: &Heap, size: Size) -> Self {
        let chunk = || {
            // SAFETY: 马上放进 Persistent
            Persistent::new(&unsafe { make_garbage_collected(heap, Chunk { slots: UnsafeCell::new(std::array::from_fn(|_| Member::empty())) }) })
        };
        Self((0..size.slots().div_ceil(CHUNK)).map(|_| chunk()).collect())
    }

    /// 分配一个对象，需要的话放进第 `slot` 个槽；返回它在 cppgc 堆上大约占的字节。
    fn put(&self, heap: &Heap, slot: Option<usize>) -> usize {
        // SAFETY: 新对象马上放进槽或丢掉，期间不分配
        let blob = unsafe { make_garbage_collected(heap, Blob { _data: [0; N] }) };
        if let Some(i) = slot {
            let chunk = self.0[i / CHUNK].get().expect("块挂在 Persistent 上");
            // SAFETY: 只有这个线程改槽
            unsafe { (&mut *chunk.slots.get())[i % CHUNK].set(&blob) };
        }
        BLOB_OVERHEAD + N * size_of::<u64>()
    }
}

/// 碎片负载的持有者：每种大小一组槽
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

    /// 分配一个对象，需要的话放进槽里；返回它在 cppgc 堆上大约占的字节（头 8 + RustObj 包装 24 + 数据）。
    fn place(&self, heap: &Heap, frag: &mut Frag) -> usize {
        let pick = frag.next();
        match pick.size {
            Size::Small => self.small.put(heap, pick.slot),
            Size::Medium => self.medium.put(heap, pick.slot),
            Size::Large => self.large.put(heap, pick.slot),
            Size::Huge => self.huge.put(heap, pick.slot),
        }
    }

    /// 槽全满时的存活字节（估算，detached 的触发用）
    fn live() -> usize {
        [(Size::Small, 3), (Size::Medium, 30), (Size::Large, 200), (Size::Huge, 1500)].iter().map(|&(s, n)| s.slots() * (BLOB_OVERHEAD + n * size_of::<u64>())).sum()
    }
}

/// 碎片负载里每个对象在 cppgc 堆上除数据外的字节：对象头 8 + RustObj 头与 dyn 指针 24
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
            _ => Err(format!("CPPGC_HEAP 只认 attached / detached，不认 {s}")),
        }
    }
}

/// 进程级的 V8 平台（只能初始化一次）。
struct V8 {
    platform: SharedRef<v8::Platform>,
}

impl From<NonZeroUsize> for V8 {
    fn from(threads: NonZeroUsize) -> Self {
        let platform = if threads.get() == 1 {
            v8::new_single_threaded_default_platform(false)
        } else {
            v8::new_default_platform(u32::try_from(threads.get()).expect("线程数太大"), false)
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

    /// 造树；返回的指针只在栈上，马上放进 Persistent / Member 或者丢掉。
    fn tree(heap: &Heap, depth: u32) -> UnsafePtr<Node> {
        let (left, right) = if depth > 1 {
            (Member::new(&Self::tree(heap, depth - 1)), Member::new(&Self::tree(heap, depth - 1)))
        } else {
            (Member::empty(), Member::empty())
        };
        // SAFETY: 见上
        unsafe { make_garbage_collected(heap, Node { left, right, value: depth.into() }) }
    }

    fn count(root: &Persistent<Node>) -> usize {
        let mut stack: Vec<&Node> = root.get().into_iter().collect();
        let mut n = 0;
        while let Some(x) = stack.pop() {
            n += 1;
            // SAFETY: 树挂在 Persistent 上，遍历期间不回收
            stack.extend(unsafe { x.left.get() });
            stack.extend(unsafe { x.right.get() });
        }
        n
    }
}

/// 挂在 isolate 上的堆。
struct Attached(V8);

impl Attached {
    /// isolate 与它的 cppgc 堆。堆归 isolate 所有，指针在 isolate 活着时有效。
    fn isolate(&self) -> (v8::OwnedIsolate, *const Heap) {
        let mut isolate = v8::Isolate::new(v8::CreateParams::default().cpp_heap(self.0.heap()));
        let heap: *const Heap = isolate.get_cpp_heap().expect("刚挂上的 cppgc 堆");
        (isolate, heap)
    }
}

impl Bench for Attached {
    const NAME: &str = "cppgc";

    fn pause(&self) -> Option<Took> {
        let (mut isolate, heap) = self.isolate();
        // SAFETY: 堆归 isolate 所有，isolate 活到函数末尾
        let heap = unsafe { &*heap };
        // 基线：cppgc 堆是空的时候一次完整 GC 的耗时（V8 自己那一份），打到 stderr 供对照
        let start = Clock::start();
        isolate.request_garbage_collection_for_testing(GarbageCollectionType::Full);
        eprintln!("cppgc 空堆完整 GC 基线 {:.2} ms", start.stop().wall.as_secs_f64() * 1e3);
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
        // SAFETY: 同上
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

    /// 回收时机交给 V8 自己调度；最后一次完整 GC 后量 RSS。
    fn frag(&self) -> Option<(Took, Footprint)> {
        let base = Footprint::rss();
        let start = Clock::start();
        let (mut isolate, heap) = self.isolate();
        // SAFETY: 同上
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

/// 不挂 isolate 的堆。
struct Detached(V8);

impl Detached {
    fn heap(&self) -> UniqueRef<Heap> {
        let heap = self.0.heap();
        heap.enable_detached_garbage_collections_for_testing();
        heap
    }

    fn collect(heap: &Heap) {
        // SAFETY: 调用时栈上没有指向堆的指针（树都挂在 Persistent 上或已丢弃）
        unsafe { heap.collect_garbage_for_testing(EmbedderStackState::NoHeapPointers) };
    }
}

/// 照 paragc 的策略决定什么时候回收（按节点数估算字节）。
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

    /// 照 paragc 的触发策略手动回收（按估算字节）。
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
