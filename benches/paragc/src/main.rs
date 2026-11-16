//! paragc：pause / gcbench 的线程数是回收线程数；alloc-mt 是 mutator 线程数（回收线程同数）。
use std::num::NonZeroUsize;

use bench_common::{Args, Bench, Clock, Took, GcBench, LONG_LIVED_DEPTH, MT_DEPTH, MT_ROUNDS, MT_TREES, PAUSE_DEPTH, STRETCH_DEPTH, Tree};
use paragc::{Gc, Heap, Mutator, Root, Tlab, Trace, brand};

#[derive(Trace)]
struct Node<'gc> {
    left: Option<Gc<'gc, Node<'gc>>>,
    right: Option<Gc<'gc, Node<'gc>>>,
    value: u64,
}

brand!(NodeB = for<'gc> Node<'gc>);

struct Paragc {
    threads: NonZeroUsize,
}

impl Paragc {
    fn tree<'gc>(m: &Mutator<'gc>, depth: u32) -> Gc<'gc, Node<'gc>> {
        let (left, right) = if depth > 1 { (Some(Self::tree(m, depth - 1)), Some(Self::tree(m, depth - 1))) } else { (None, None) };
        m.alloc(Node { left, right, value: depth.into() })
    }

    fn rooted(heap: &Heap, depth: u32) -> Root<NodeB> {
        heap.mutate(|m| m.root(Self::tree(m, depth)))
    }

    fn count(heap: &Heap, root: &Root<NodeB>) -> usize {
        heap.mutate(|m| {
            let mut stack = vec![m.get(root)];
            let mut n = 0;
            while let Some(x) = stack.pop() {
                n += 1;
                stack.extend(x.left);
                stack.extend(x.right);
            }
            n
        })
    }
}

impl Bench for Paragc {
    const NAME: &str = "paragc";

    fn pause(&self) -> Option<Took> {
        let mut heap = Heap::new(self.threads);
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
        let mut heap = Heap::new(self.threads);
        heap.mutate(|m| {
            Self::tree(m, STRETCH_DEPTH);
        });
        heap.collect_if_needed();
        let long = Self::rooted(&heap, LONG_LIVED_DEPTH);
        for (depth, iters) in GcBench::rounds() {
            for _ in 0..iters {
                heap.mutate(|m| {
                    Self::tree(m, depth);
                });
                heap.collect_if_needed();
            }
        }
        let took = start.stop();
        assert_eq!(Self::count(&heap, &long), Tree::size(LONG_LIVED_DEPTH));
        Some(took)
    }

    fn alloc_mt(&self) -> Option<Took> {
        let mut heap = Heap::new(self.threads);
        let long = Self::rooted(&heap, LONG_LIVED_DEPTH);
        let per_thread = MT_TREES / (MT_ROUNDS * self.threads.get());
        let start = Clock::start();
        for _ in 0..MT_ROUNDS {
            std::thread::scope(|s| {
                let heap = &heap;
                for _ in 0..self.threads.get() {
                    s.spawn(move || {
                        let mut tlab = Tlab::default();
                        for _ in 0..per_thread {
                            heap.mutate_in(&mut tlab, |m| {
                                Self::tree(m, MT_DEPTH);
                            });
                        }
                    });
                }
            });
            heap.collect_if_needed();
        }
        let took = start.stop();
        assert_eq!(Self::count(&heap, &long), Tree::size(LONG_LIVED_DEPTH));
        Some(took)
    }
}

fn main() {
    let args = Args::parse();
    args.report(&Paragc { threads: args.threads });
}
