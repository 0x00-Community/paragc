//! gc（rust-gc）：线程局部堆、按分配量自动回收，只有串行；线程数参数必须是 1。
#![allow(non_local_definitions, reason = "gc 0.5 的 derive 宏生成的写法，不是本仓库的代码")]

use bench_common::{Args, Bench, Clock, Took, GcBench, LONG_LIVED_DEPTH, PAUSE_DEPTH, STRETCH_DEPTH, Tree};
use gc::{Finalize, Gc, Trace, force_collect};

#[derive(Trace, Finalize)]
struct Node {
    left: Option<Gc<Node>>,
    right: Option<Gc<Node>>,
    value: u64,
}

struct RustGc;

impl RustGc {
    fn tree(depth: u32) -> Gc<Node> {
        let (left, right) = if depth > 1 { (Some(Self::tree(depth - 1)), Some(Self::tree(depth - 1))) } else { (None, None) };
        Gc::new(Node { left, right, value: depth.into() })
    }

    fn count(root: &Gc<Node>) -> usize {
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

impl Bench for RustGc {
    const NAME: &str = "rust-gc";

    fn pause(&self) -> Option<Took> {
        let live = Self::tree(PAUSE_DEPTH);
        drop(Self::tree(PAUSE_DEPTH));
        let start = Clock::start();
        force_collect();
        let took = start.stop();
        assert_eq!(Self::count(&live), Tree::size(PAUSE_DEPTH));
        Some(took)
    }

    fn gcbench(&self) -> Option<Took> {
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
}

fn main() {
    let args = Args::parse();
    assert_eq!(args.threads.get(), 1, "rust-gc 只有串行");
    args.report(&RustGc);
}
