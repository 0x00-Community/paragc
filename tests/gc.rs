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
/// Miri 下缩小规模（单条仍要在 15 s 内）
const LIST_LEN: u64 = if cfg!(miri) { 200 } else { 20_000 };

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
    const ROUNDS: usize = if cfg!(miri) { 3 } else { 20 };
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

/// 交叉测试的链节点：带计数标签；链头挂着别的堆上一条链的根；隔几个挂一个大对象（回指本链，成环）。
#[derive(Trace)]
struct Item<'gc> {
    value: u64,
    next: Option<Gc<'gc, Item<'gc>>>,
    /// (堆号, 根)：别的堆上的链，随本对象析构而释放
    guest: Option<(usize, Root<ItemB>)>,
    ballast: Option<Gc<'gc, Ballast<'gc>>>,
    tag: Counted,
}

/// 大对象（超过 8 KiB 单独分配），回指挂着它的节点（成环）。
#[derive(Trace)]
struct Ballast<'gc> {
    words: [u64; BALLAST_WORDS],
    owner: Slot<'gc, Item<'gc>>,
}

brand!(ItemB = for<'gc> Item<'gc>);

const BALLAST_WORDS: usize = 1100;
/// Miri 下 8 × 8 超出单条 15 s，缩到 4 × 4
const HEAPS: usize = if cfg!(miri) { 4 } else { 8 };
const WORKERS: usize = HEAPS;
const ITEMS: u64 = if cfg!(miri) { 8 } else { 4_000 };
/// Miri 下也要保证每个堆每轮至少 16 个大对象（够开 2 个标记工人）
const BALLAST_EVERY: u64 = if cfg!(miri) { 2 } else { 10 };
const CROSS_ROUNDS: usize = if cfg!(miri) { 2 } else { 8 };
const ITEMS_SUM: u64 = ITEMS * (ITEMS - 1) / 2;

/// 造了几个标签、析构了几个。
#[derive(Default)]
struct Ledger {
    made: AtomicUsize,
    dropped: Arc<AtomicUsize>,
}

impl Ledger {
    fn tag(&self) -> Counted {
        self.made.fetch_add(1, Ordering::Relaxed);
        Counted(self.dropped.clone())
    }

    /// 在第 `h` 个堆上造一条链，链头带着 `guest`。
    fn build(&self, heaps: &[Heap], h: usize, guest: Option<(usize, Root<ItemB>)>) -> Root<ItemB> {
        heaps[h].mutate(|m| {
            let mut guest = guest;
            let mut top = None;
            for value in 0..ITEMS {
                let ballast = value.is_multiple_of(BALLAST_EVERY).then(|| m.alloc(Ballast { words: [value; BALLAST_WORDS], owner: Slot::default() }));
                let guest = if value == ITEMS - 1 { guest.take() } else { None };
                let item = m.alloc(Item { value, next: top, guest, ballast, tag: self.tag() });
                if let Some(b) = ballast {
                    b.owner.set(Some(item));
                }
                top = Some(item);
            }
            m.root(top.expect("ITEMS > 0"))
        })
    }
}

/// 检查一条链完整；`depth > 0` 时连同链头挂着的别的堆上的链（每条链本身也都单独查，所以只往下一层）。
fn check(heaps: &[Heap], h: usize, root: &Root<ItemB>, depth: usize) {
    heaps[h].mutate(|m| {
        let mut sum = 0;
        let mut cur = Some(m.get(root));
        while let Some(n) = cur {
            sum += n.value;
            if let Some(b) = n.ballast {
                assert!(b.words[0] == n.value && b.words[BALLAST_WORDS - 1] == n.value, "大对象内容坏了");
                assert!(b.owner.get().is_some_and(|o| Gc::ptr_eq(o, n)), "大对象回指坏了");
            }
            if let Some((g, guest)) = &n.guest
                && depth > 0
            {
                check(heaps, *g, guest, depth - 1);
            }
            cur = n.next;
        }
        assert_eq!(sum, ITEMS_SUM, "堆 {h} 上的链坏了");
    });
}

/// 全部堆并行回收（每个堆一个线程），同时另一个线程释放 `dropping` 里的根。
fn collect_all(heaps: &mut [Heap], dropping: Vec<(usize, Root<ItemB>)>) {
    std::thread::scope(|s| {
        s.spawn(move || {
            // 倒着、隔一个地释放，打乱次序
            let (odd, even): (Vec<_>, Vec<_>) = dropping.into_iter().rev().enumerate().partition(|(i, _)| i % 2 == 1);
            drop(odd);
            drop(even);
        });
        for heap in heaps.iter_mut() {
            s.spawn(move || heap.collect());
        }
    });
}

/// 8 线程 × 8 堆：每个线程轮流在每个堆上造链、链头挂着自己在上一个堆的链的根（跨堆持有）；
/// 造好的根交给另一个线程检查，一半当场释放（跨线程释放），一半留到下一轮、在全部堆并行回收的同时由另一个线程释放。
#[test]
fn eight_threads_eight_heaps_crossed() {
    let ledger = Ledger::default();
    let mut heaps: Vec<Heap> = (0..HEAPS).map(|_| Heap::new(THREADS)).collect();
    let mut kept_prev: Vec<(usize, Root<ItemB>)> = Vec::new();
    for round in 0..CROSS_ROUNDS {
        // 造：线程 t 依次在堆 (t + k + round) % HEAPS 上造链
        let made: Vec<Vec<(usize, Root<ItemB>)>> = std::thread::scope(|s| {
            let (heaps, ledger) = (&heaps, &ledger);
            let handles: Vec<_> = (0..WORKERS)
                .map(|t| {
                    s.spawn(move || {
                        let mut carry: Option<(usize, Root<ItemB>)> = None;
                        let mut out = Vec::new();
                        for k in 0..HEAPS {
                            let h = (t + k + round) % HEAPS;
                            let root = ledger.build(heaps, h, carry.take());
                            carry = Some((h, heaps[h].mutate(|m| m.root(m.get(&root)))));
                            out.push((h, root));
                        }
                        out
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("造链的线程 panic")).collect()
        });
        // 查：线程 t 检查线程 t + 1 造的链，一半当场释放
        let kept_now: Vec<(usize, Root<ItemB>)> = std::thread::scope(|s| {
            let heaps = &heaps;
            let mut made = made;
            made.rotate_left(1);
            let handles: Vec<_> = made
                .into_iter()
                .map(|roots| {
                    s.spawn(move || {
                        let mut keep = Vec::new();
                        for (i, (h, root)) in roots.into_iter().enumerate() {
                            check(heaps, h, &root, 1);
                            if i % 2 == 1 {
                                keep.push((h, root));
                            }
                        }
                        keep
                    })
                })
                .collect();
            handles.into_iter().flat_map(|h| h.join().expect("检查的线程 panic")).collect()
        });
        // 回收全部堆，同时释放上一轮留下的
        collect_all(&mut heaps, std::mem::take(&mut kept_prev));
        for (h, root) in &kept_now {
            check(&heaps, *h, root, 1);
        }
        kept_prev = kept_now;
    }
    // 全部释放后反复回收：跨堆的根要等持有它的对象被回收才释放，最多传 HEAPS 层
    collect_all(&mut heaps, kept_prev);
    for _ in 0..=HEAPS {
        if ledger.dropped.load(Ordering::Relaxed) == ledger.made.load(Ordering::Relaxed) {
            break;
        }
        collect_all(&mut heaps, Vec::new());
    }
    assert_eq!(ledger.dropped.load(Ordering::Relaxed), ledger.made.load(Ordering::Relaxed), "有对象没被回收");
    assert!(heaps.iter().all(|h| h.stats().live == 0 && h.stats().large == 0), "堆里还有存活对象");
}
