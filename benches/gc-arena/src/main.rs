//! gc-arena: single-threaded, incremental marking, serial only; the thread count argument must be 1.

use bench_common::{Args, Bench, Clock, Took, GcBench, LONG_LIVED_DEPTH, PAUSE_DEPTH, STRETCH_DEPTH, Tree};
use gc_arena::{Arena, Collect, Gc, Mutation, Rootable};

#[derive(Collect)]
#[collect(no_drop)]
struct Node<'gc> {
    left: Option<Gc<'gc, Node<'gc>>>,
    right: Option<Gc<'gc, Node<'gc>>>,
    value: u64,
}

#[derive(Collect)]
#[collect(no_drop)]
struct Roots<'gc> {
    live: Option<Gc<'gc, Node<'gc>>>,
}

type Heap = Arena<Rootable![Roots<'_>]>;

struct GcArena;

impl GcArena {
    fn tree<'gc>(mc: &Mutation<'gc>, depth: u32) -> Gc<'gc, Node<'gc>> {
        let (left, right) = if depth > 1 { (Some(Self::tree(mc, depth - 1)), Some(Self::tree(mc, depth - 1))) } else { (None, None) };
        Gc::new(mc, Node { left, right, value: depth.into() })
    }

    fn count(heap: &Heap) -> usize {
        heap.mutate(|_, roots| {
            let mut stack: Vec<_> = roots.live.into_iter().collect();
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

impl Bench for GcArena {
    const NAME: &str = "gc-arena";

    fn pause(&self) -> Option<Took> {
        let mut heap = Heap::new(|_| Roots { live: None });
        heap.mutate_root(|mc, roots| roots.live = Some(Self::tree(mc, PAUSE_DEPTH)));
        heap.mutate(|mc, _| {
            Self::tree(mc, PAUSE_DEPTH);
        });
        let start = Clock::start();
        heap.finish_cycle();
        let took = start.stop();
        assert_eq!(Self::count(&heap), Tree::size(PAUSE_DEPTH));
        Some(took)
    }

    fn gcbench(&self) -> Option<Took> {
        let start = Clock::start();
        let mut heap = Heap::new(|_| Roots { live: None });
        heap.mutate(|mc, _| {
            Self::tree(mc, STRETCH_DEPTH);
        });
        heap.collect_debt();
        heap.mutate_root(|mc, roots| roots.live = Some(Self::tree(mc, LONG_LIVED_DEPTH)));
        for (depth, iters) in GcBench::rounds() {
            for _ in 0..iters {
                heap.mutate(|mc, _| {
                    Self::tree(mc, depth);
                });
                heap.collect_debt();
            }
        }
        let took = start.stop();
        assert_eq!(Self::count(&heap), Tree::size(LONG_LIVED_DEPTH));
        Some(took)
    }
}

fn main() {
    let args = Args::parse();
    assert_eq!(args.threads.get(), 1, "gc-arena is serial only");
    args.report(&GcArena);
}
