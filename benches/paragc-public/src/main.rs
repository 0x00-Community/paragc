//! paragc 的公开堆（共享内存文件上）：pause / gcbench 与私有堆同样的树，看共享内存、偏移指针、按指针图追踪的代价。
//! 公开堆没有年轻回收与引用计数，gcbench 照私有堆的触发阈值手动回收。线程数是回收线程数。
use std::num::NonZeroUsize;
use std::path::PathBuf;

use bench_common::{Args, Bench, Clock, GcBench, LONG_LIVED_DEPTH, PAUSE_DEPTH, STRETCH_DEPTH, Took, Tree};
use paragc::{CrossHeapRoot, CrossHeapGc, CrossHeap, CrossHeapMutator, CrossHeapSlot, CrossHeapSpace, Shared, cross_heap_brand};

#[repr(C)]
#[derive(Shared)]
struct Node<'p> {
    left: CrossHeapSlot<'p, Node<'p>>,
    right: CrossHeapSlot<'p, Node<'p>>,
    value: u64,
}

cross_heap_brand!(NodeB = for<'p> Node<'p>);

/// 回收触发的下限与倍数（同私有堆）：新领用的超过「上次存活 × GROWTH」且不少于 MIN_TRIGGER 就回收
const MIN_TRIGGER: usize = 4 * 1024 * 1024;
const GROWTH: usize = 2;

struct ParagcPublic {
    threads: NonZeroUsize,
    space: CrossHeapSpace,
}

impl ParagcPublic {
    fn heap(&self) -> CrossHeap {
        let path = PathBuf::from(format!("/dev/shm/paragc-bench-{}", std::process::id()));
        self.space.create(path, self.threads).expect("建公开堆")
    }

    fn tree<'p>(m: &CrossHeapMutator<'p>, depth: u32) -> CrossHeapGc<'p, Node<'p>> {
        let (left, right) = if depth > 1 { (Some(Self::tree(m, depth - 1)), Some(Self::tree(m, depth - 1))) } else { (None, None) };
        m.alloc(Node { left: left.into(), right: right.into(), value: depth.into() })
    }

    fn rooted(heap: &CrossHeap, depth: u32) -> CrossHeapRoot<NodeB> {
        heap.mutate(|m| m.root(Self::tree(m, depth)))
    }

    fn count(heap: &CrossHeap, root: &CrossHeapRoot<NodeB>) -> usize {
        heap.mutate(|m| {
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
        heap.mutate(|m| {
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
        heap.mutate(|m| {
            Self::tree(m, STRETCH_DEPTH);
        });
        Self::collect_if_needed(&heap);
        let long = Self::rooted(&heap, LONG_LIVED_DEPTH);
        for (depth, iters) in GcBench::rounds() {
            for _ in 0..iters {
                heap.mutate(|m| {
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
    args.report(&ParagcPublic { threads: args.threads, space: CrossHeapSpace::reserve().expect("保留公开地址区") });
}
