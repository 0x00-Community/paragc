//! paragc public heaps (on a shared-memory file): the same pause / gcbench trees as the private heap, measuring the cost of shared memory, offset pointers and tracing by pointer map.
//! Public heaps have no young collections or reference counting, so gcbench collects by hand at the private heap's trigger. The thread count is the number of collector threads.
use std::num::NonZeroUsize;
use std::path::PathBuf;

use bench_common::{Args, Bench, Clock, GcBench, LONG_LIVED_DEPTH, PAUSE_DEPTH, STRETCH_DEPTH, Took, Tree};
use paragc::{CrossHeapGlobal, CrossHeapGc, CrossHeap, CrossHeapScope, CrossHeapSlot, CrossHeapSpace, Shared, cross_heap_brand};

#[repr(C)]
#[derive(Shared)]
struct Node<'scope> {
    left: CrossHeapSlot<'scope, Node<'scope>>,
    right: CrossHeapSlot<'scope, Node<'scope>>,
    value: u64,
}

cross_heap_brand!(NodeB = for<'scope> Node<'scope>);

/// Collection trigger floor and factor (as for the private heap): collect once newly taken bytes exceed "last live × GROWTH" and at least MIN_TRIGGER
const MIN_TRIGGER: usize = 4 * 1024 * 1024;
const GROWTH: usize = 2;

struct ParagcPublic {
    threads: NonZeroUsize,
    space: CrossHeapSpace,
}

impl ParagcPublic {
    fn heap(&self) -> CrossHeap {
        let path = PathBuf::from(format!("/dev/shm/paragc-bench-{}", std::process::id()));
        self.space.create(path, self.threads).expect("create a public heap")
    }

    fn tree<'scope>(m: &CrossHeapScope<'scope>, depth: u32) -> CrossHeapGc<'scope, Node<'scope>> {
        let (left, right) = if depth > 1 { (Some(Self::tree(m, depth - 1)), Some(Self::tree(m, depth - 1))) } else { (None, None) };
        m.alloc(Node { left: left.into(), right: right.into(), value: depth.into() })
    }

    fn rooted(heap: &CrossHeap, depth: u32) -> CrossHeapGlobal<NodeB> {
        heap.scope(|m| m.global(Self::tree(m, depth)))
    }

    fn count(heap: &CrossHeap, root: &CrossHeapGlobal<NodeB>) -> usize {
        heap.scope(|m| {
            let mut stack = vec![root.get(m)];
            let mut n = 0;
            while let Some(x) = stack.pop() {
                n += 1;
                stack.extend(x.left.get(m));
                stack.extend(x.right.get(m));
            }
            n
        })
    }

    fn collect_if_needed(heap: &CrossHeap) {
        let s = heap.stats();
        if s.allocated >= s.live.max(MIN_TRIGGER) * GROWTH {
            heap.collect();
        }
    }
}

impl Bench for ParagcPublic {
    const NAME: &str = "paragc-public";

    fn pause(&self) -> Option<Took> {
        let heap = self.heap();
        let live = Self::rooted(&heap, PAUSE_DEPTH);
        heap.scope(|m| {
            Self::tree(m, PAUSE_DEPTH);
        });
        let start = Clock::start();
        heap.collect();
        let took = start.stop();
        assert_eq!(Self::count(&heap, &live), Tree::size(PAUSE_DEPTH));
        Some(took)
    }

    fn gcbench(&self) -> Option<Took> {
        let start = Clock::start();
        let heap = self.heap();
        heap.scope(|m| {
            Self::tree(m, STRETCH_DEPTH);
        });
        Self::collect_if_needed(&heap);
        let long = Self::rooted(&heap, LONG_LIVED_DEPTH);
        for (depth, iters) in GcBench::rounds() {
            for _ in 0..iters {
                heap.scope(|m| {
                    Self::tree(m, depth);
                });
                Self::collect_if_needed(&heap);
            }
        }
        let took = start.stop();
        assert_eq!(Self::count(&heap, &long), Tree::size(LONG_LIVED_DEPTH));
        Some(took)
    }
}

fn main() {
    let args = Args::parse();
    args.report(&ParagcPublic { threads: args.threads, space: CrossHeapSpace::reserve().expect("reserve the public region") });
}
