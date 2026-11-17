//! paragc：pause / gcbench 的线程数是回收线程数；alloc-mt 是 mutator 线程数（回收线程同数）。
use std::num::NonZeroUsize;

use bench_common::{Args, Bench, Clock, FRAG_PER_ROUND, FRAG_ROUNDS, Footprint, Frag, GcBench, LONG_LIVED_DEPTH, MT_DEPTH, MT_ROUNDS, MT_TREES, PAUSE_DEPTH, STRETCH_DEPTH, Size, Took, Tree};
use paragc::{Gc, Heap, Mutator, Root, Slot, Tlab, Trace, brand};

#[derive(Trace)]
struct Node<'gc> {
    left: Option<Gc<'gc, Node<'gc>>>,
    right: Option<Gc<'gc, Node<'gc>>>,
    value: u64,
}

brand!(NodeB = for<'gc> Node<'gc>);

/// 碎片负载的对象：`N` 个 u64 的数据
#[derive(Trace)]
struct Blob<const N: usize> {
    _data: [u64; N],
}

/// 碎片负载的持有者：每种大小一组槽
#[derive(Trace)]
struct Holder<'gc> {
    small: Vec<Slot<'gc, Blob<3>>>,
    medium: Vec<Slot<'gc, Blob<30>>>,
    large: Vec<Slot<'gc, Blob<200>>>,
    huge: Vec<Slot<'gc, Blob<1500>>>,
}

brand!(HolderB = for<'gc> Holder<'gc>);

impl<'gc> Holder<'gc> {
    fn new() -> Self {
        fn slots<'gc, const N: usize>(size: Size) -> Vec<Slot<'gc, Blob<N>>> {
            (0..size.slots()).map(|_| Slot::default()).collect()
        }
        Self { small: slots(Size::Small), medium: slots(Size::Medium), large: slots(Size::Large), huge: slots(Size::Huge) }
    }

    /// 分配一个对象，需要的话放进槽里（覆盖掉的旧对象成为垃圾）。
    fn place(&self, m: &Mutator<'gc>, frag: &mut Frag) {
        fn put<'gc, const N: usize>(m: &Mutator<'gc>, slots: &[Slot<'gc, Blob<N>>], slot: Option<usize>) {
            let blob = m.alloc(Blob { _data: [0; N] });
            if let Some(i) = slot {
                slots[i].set(m, Some(blob));
            }
        }
        let pick = frag.next();
        match pick.size {
            Size::Small => put(m, &self.small, pick.slot),
            Size::Medium => put(m, &self.medium, pick.slot),
            Size::Large => put(m, &self.large, pick.slot),
            Size::Huge => put(m, &self.huge, pick.slot),
        }
    }
}

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

    /// 一棵活树 + 一棵垃圾树，只量停住 mutator 的那段：
    /// `concurrent_mark` 先并发标记（不计时）；`concurrent_sweep` 只量 `finish_marking`，清扫随后并发做（不计时）。
    fn timed_pause(&self, concurrent_mark: bool, concurrent_sweep: bool) -> Took {
        let mut heap = Heap::new(self.threads);
        let live = Self::rooted(&heap, PAUSE_DEPTH);
        heap.mutate(|m| {
            Self::tree(m, PAUSE_DEPTH);
        });
        if concurrent_mark {
            assert!(heap.mark_concurrently());
        }
        let start = Clock::start();
        if concurrent_sweep {
            heap.finish_marking();
        } else {
            heap.collect();
        }
        let took = start.stop();
        heap.sweep_concurrently();
        assert_eq!(Self::count(&heap, &live), Tree::size(PAUSE_DEPTH));
        took
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

    fn frag(&self) -> Option<(Took, Footprint)> {
        let base = Footprint::rss();
        let start = Clock::start();
        let mut heap = Heap::new(self.threads);
        let holder: Root<HolderB> = heap.mutate(|m| m.root(m.alloc(Holder::new())));
        let mut frag = Frag::default();
        let mut peak = 0;
        for _ in 0..FRAG_ROUNDS {
            heap.mutate(|m| {
                let h = m.get(&holder);
                for _ in 0..FRAG_PER_ROUND {
                    h.place(m, &mut frag);
                }
            });
            heap.collect_if_needed();
            peak = peak.max(Footprint::rss().saturating_sub(base));
        }
        heap.collect();
        let took = start.stop();
        let after = Footprint::rss().saturating_sub(base);
        drop(holder);
        Some((took, Footprint { after, peak: peak.max(after) }))
    }

    fn pause_concurrent(&self) -> Option<Took> {
        Some(self.timed_pause(true, false))
    }

    fn pause_sweep_concurrent(&self) -> Option<Took> {
        Some(self.timed_pause(false, true))
    }

    fn pause_all_concurrent(&self) -> Option<Took> {
        Some(self.timed_pause(true, true))
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
