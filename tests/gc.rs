//! 逐条跑：`cargo test --test gc -- --exact <名字>`
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use paragc::{Gc, Heap, Root, Slot, Trace, Tracer, brand};

#[derive(Trace)]
struct Node<'gc> {
    value: u64,
    next: Option<Gc<'gc, Node<'gc>>>,
}

brand!(NodeB = for<'gc> Node<'gc>);

/// 析构时计数（没有 Gc，可以有 Drop）。
struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: 没有 Gc
unsafe impl Trace for Counted {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut Tracer<'_>) {}
}

#[derive(Trace)]
enum Tree<'gc> {
    Leaf(Counted),
    Branch { left: Slot<'gc, Tree<'gc>>, right: Slot<'gc, Tree<'gc>> },
}

brand!(TreeB = for<'gc> Tree<'gc>);

const THREADS: NonZeroUsize = NonZeroUsize::new(4).unwrap();
const LIST_LEN: u64 = 20_000;

fn build_list(heap: &Heap, len: u64) -> Root<NodeB> {
    heap.mutate(|m| {
        let mut top = None;
        for value in 0..len {
            top = Some(m.alloc(Node { value, next: top }));
        }
        m.root(top.expect("len > 0"))
    })
}

fn sum_list(heap: &Heap, root: &Root<NodeB>) -> u64 {
    heap.mutate(|m| {
        let mut sum = 0;
        let mut cur = Some(m.get(root));
        while let Some(n) = cur {
            sum += n.value;
            cur = n.next;
        }
        sum
    })
}

#[test]
fn rooted_list_survives() {
    let mut heap = Heap::new(THREADS);
    let root = build_list(&heap, LIST_LEN);
    // 垃圾：不挂根
    heap.mutate(|m| {
        for value in 0..LIST_LEN {
            m.alloc(Node { value, next: None });
        }
    });
    heap.collect();
    heap.collect();
    assert_eq!(sum_list(&heap, &root), LIST_LEN * (LIST_LEN - 1) / 2);
}

#[test]
fn garbage_is_finalized() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new(THREADS);
    let kept: Root<TreeB> = heap.mutate(|m| {
        let leaf = || Slot::from(m.alloc(Tree::Leaf(Counted(drops.clone()))));
        let keep = m.alloc(Tree::Branch { left: leaf(), right: leaf() });
        for _ in 0..1000 {
            m.alloc(Tree::Branch { left: leaf(), right: leaf() });
        }
        m.root(keep)
    });
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), 2000);
    drop(kept);
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), 2002);
}

#[test]
fn heap_drop_finalizes_everything() {
    let drops = Arc::new(AtomicUsize::new(0));
    let heap = Heap::new(THREADS);
    let root: Root<TreeB> = heap.mutate(|m| m.root(m.alloc(Tree::Leaf(Counted(drops.clone())))));
    drop(heap);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    drop(root);
}

#[test]
fn threads_mutate_at_once() {
    const WRITERS: u64 = 4;
    let mut heap = Heap::new(THREADS);
    let roots: Vec<Root<NodeB>> = std::thread::scope(|s| {
        let heap = &heap;
        let handles: Vec<_> = (0..WRITERS).map(|_| s.spawn(move || build_list(heap, LIST_LEN))).collect();
        handles.into_iter().map(|h| h.join().expect("mutator panicked")).collect()
    });
    heap.collect();
    for root in &roots {
        assert_eq!(sum_list(&heap, root), LIST_LEN * (LIST_LEN - 1) / 2);
    }
}

#[test]
fn blocks_are_reused() {
    const ROUNDS: usize = 20;
    let mut heap = Heap::new(THREADS);
    let root = build_list(&heap, LIST_LEN);
    let mut peak = 0;
    for round in 0..ROUNDS {
        drop(build_list(&heap, LIST_LEN));
        heap.collect();
        // 第二轮之后块数不再增长：死掉的列表腾出的行被复用
        if round == 1 {
            peak = heap.stats().blocks;
        }
        assert!(round <= 1 || heap.stats().blocks <= peak, "第 {round} 轮块数 {} > {peak}", heap.stats().blocks);
    }
    assert_eq!(sum_list(&heap, &root), LIST_LEN * (LIST_LEN - 1) / 2);
}

#[test]
#[allow(clippy::large_stack_arrays)]
fn large_objects() {
    const BIG: usize = 64 * 1024;
    let mut heap = Heap::new(THREADS);
    let root: Root<TreeB> = heap.mutate(|m| {
        let big = m.alloc(vec![7u8; BIG]);
        let _garbage = m.alloc([0u64; BIG / 8]);
        assert_eq!(big.len(), BIG);
        m.root(m.alloc(Tree::Leaf(Counted(Arc::default()))))
    });
    assert_eq!(heap.stats().large, 1);
    heap.collect();
    assert_eq!(heap.stats().large, 0);
    drop(root);
}
