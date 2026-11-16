//! dumpster：引用计数 + 环检测。pause / gcbench 用单线程的 unsync（串行，线程数必须是 1）；
//! alloc-mt 用线程安全的 sync，线程数是 mutator 线程数。
//! 树里没有环，死对象在最后一个引用析构时就释放了，pause 量的是「扔掉垃圾树 + collect」。
use std::num::NonZeroUsize;

use bench_common::{Args, Bench, Clock, Took, GcBench, LONG_LIVED_DEPTH, MT_DEPTH, MT_ROUNDS, MT_TREES, PAUSE_DEPTH, STRETCH_DEPTH, Tree};
use dumpster::Trace;

#[derive(Trace)]
struct Node {
    left: Option<dumpster::unsync::Gc<Node>>,
    right: Option<dumpster::unsync::Gc<Node>>,
    value: u64,
}

#[derive(Trace)]
struct SyncNode {
    left: Option<dumpster::sync::Gc<SyncNode>>,
    right: Option<dumpster::sync::Gc<SyncNode>>,
    value: u64,
}

struct Dumpster {
    threads: NonZeroUsize,
}

impl Dumpster {
    fn tree(depth: u32) -> dumpster::unsync::Gc<Node> {
        let (left, right) = if depth > 1 { (Some(Self::tree(depth - 1)), Some(Self::tree(depth - 1))) } else { (None, None) };
        dumpster::unsync::Gc::new(Node { left, right, value: depth.into() })
    }

    fn sync_tree(depth: u32) -> dumpster::sync::Gc<SyncNode> {
        let (left, right) = if depth > 1 { (Some(Self::sync_tree(depth - 1)), Some(Self::sync_tree(depth - 1))) } else { (None, None) };
        dumpster::sync::Gc::new(SyncNode { left, right, value: depth.into() })
    }

    fn count(root: &dumpster::unsync::Gc<Node>) -> usize {
        let mut stack = vec![root.clone()];
        let mut n = 0;
        while let Some(x) = stack.pop() {
            n += 1;
            stack.extend(x.left.clone());
            stack.extend(x.right.clone());
        }
        n
    }

    fn sync_count(root: &dumpster::sync::Gc<SyncNode>) -> usize {
        let mut stack = vec![root.clone()];
        let mut n = 0;
        while let Some(x) = stack.pop() {
            n += 1;
            stack.extend(x.left.clone());
            stack.extend(x.right.clone());
        }
        n
    }
}

impl Bench for Dumpster {
    const NAME: &str = "dumpster";

    fn pause(&self) -> Option<Took> {
        if self.threads.get() != 1 {
            return None;
        }
        let live = Self::tree(PAUSE_DEPTH);
        let garbage = Self::tree(PAUSE_DEPTH);
        let start = Clock::start();
        drop(garbage);
        dumpster::unsync::collect();
        let took = start.stop();
        assert_eq!(Self::count(&live), Tree::size(PAUSE_DEPTH));
        Some(took)
    }

    fn gcbench(&self) -> Option<Took> {
        if self.threads.get() != 1 {
            return None;
        }
        let start = Clock::start();
        drop(Self::tree(STRETCH_DEPTH));
        let long = Self::tree(LONG_LIVED_DEPTH);
        for (depth, iters) in GcBench::rounds() {
            for _ in 0..iters {
                drop(Self::tree(depth));
            }
        }
        let took = start.stop();
        assert_eq!(Self::count(&long), Tree::size(LONG_LIVED_DEPTH));
        Some(took)
    }

    fn alloc_mt(&self) -> Option<Took> {
        let long = Self::sync_tree(LONG_LIVED_DEPTH);
        let per_thread = MT_TREES / (MT_ROUNDS * self.threads.get());
        let start = Clock::start();
        for _ in 0..MT_ROUNDS {
            std::thread::scope(|s| {
                for _ in 0..self.threads.get() {
                    s.spawn(move || {
                        for _ in 0..per_thread {
                            drop(Self::sync_tree(MT_DEPTH));
                        }
                    });
                }
            });
        }
        let took = start.stop();
        assert_eq!(Self::sync_count(&long), Tree::size(LONG_LIVED_DEPTH));
        Some(took)
    }
}

fn main() {
    let args = Args::parse();
    args.report(&Dumpster { threads: args.threads });
}
