//! Run one at a time: `cargo test --test gc -- --exact <name>`
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use paragc::{Gc, Heap, Global, GcSlot, Trace, Tracer, YoungEvacuation, brand};

#[derive(Trace)]
struct Node<'scope> {
    value: u64,
    next: Option<Gc<'scope, Node<'scope>>>,
}

brand!(NodeB = for<'scope> Node<'scope>);

/// Counts on drop (no Gc, so it may have Drop).
struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: no Gc
unsafe impl Trace for Counted {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut Tracer<'_>) {}
}

#[derive(Trace)]
enum Tree<'scope> {
    Leaf(Counted),
    Branch { left: GcSlot<'scope, Tree<'scope>>, right: GcSlot<'scope, Tree<'scope>> },
}

brand!(TreeB = for<'scope> Tree<'scope>);

/// Collector threads per heap; under Miri the idle workers' backoff spinning costs a lot of interpretation time, so 2 (still parallel marking and a thread pool)
const THREADS: NonZeroUsize = NonZeroUsize::new(if cfg!(miri) { 2 } else { 4 }).unwrap();
/// Smaller sizes under Miri (each test must still finish within 15 s)
const LIST_LEN: u64 = if cfg!(miri) { 200 } else { 20_000 };

fn build_list<'h>(heap: &Heap<'h>, len: u64) -> Global<'h, NodeB> {
    heap.scope(|m| {
        let mut top = None;
        for value in 0..len {
            top = Some(m.alloc(Node { value, next: top }));
        }
        m.global(top.expect("len > 0"))
    })
}

fn sum_list<'h>(heap: &Heap<'h>, root: &Global<'h, NodeB>) -> u64 {
    heap.scope(|m| {
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
    let mut heap = Heap::new_static(THREADS);
    let root = build_list(&heap, LIST_LEN);
    // Garbage: not rooted
    heap.scope(|m| {
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
    let mut heap = Heap::new_static(THREADS);
    let kept: Global<'_, TreeB> = heap.scope(|m| {
        let leaf = || GcSlot::from(m.alloc(Tree::Leaf(Counted(drops.clone()))));
        let keep = m.alloc(Tree::Branch { left: leaf(), right: leaf() });
        for _ in 0..1000 {
            m.alloc(Tree::Branch { left: leaf(), right: leaf() });
        }
        m.global(keep)
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
    let heap = Heap::new_static(THREADS);
    let root: Global<'_, TreeB> = heap.scope(|m| m.global(m.alloc(Tree::Leaf(Counted(drops.clone())))));
    drop(heap);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    drop(root);
}

#[test]
fn threads_mutate_at_once() {
    const WRITERS: u64 = 4;
    let mut heap = Heap::new_static(THREADS);
    let roots: Vec<Global<'_, NodeB>> = std::thread::scope(|s| {
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
    let mut heap = Heap::new_static(THREADS);
    let root = build_list(&heap, LIST_LEN);
    let mut peak = 0;
    for round in 0..ROUNDS {
        drop(build_list(&heap, LIST_LEN));
        heap.collect();
        // From the second round on the block count stops growing: lines freed by dead lists are reused
        if round == 1 {
            peak = heap.stats().blocks;
        }
        assert!(round <= 1 || heap.stats().blocks <= peak, "round {round}: {} blocks > {peak}", heap.stats().blocks);
    }
    assert_eq!(sum_list(&heap, &root), LIST_LEN * (LIST_LEN - 1) / 2);
}

#[test]
#[allow(clippy::large_stack_arrays)]
fn large_objects() {
    /// The size class with its own mmap (≥ 64 KiB)
    const BIG: usize = 64 * 1024;
    /// The size class using the global allocator (half a block up to 64 KiB)
    const MID: usize = 20 * 1024;
    let mut heap = Heap::new_static(THREADS);
    let root: Global<'_, TreeB> = heap.scope(|m| {
        let big = m.alloc(vec![7u8; BIG]);
        let _garbage = m.alloc([0u64; BIG / 8]);
        let mid = m.alloc([3u64; MID / 8]);
        assert_eq!(big.len(), BIG);
        assert!(mid.iter().all(|&x| x == 3));
        m.global(m.alloc(Tree::Leaf(Counted(Arc::default()))))
    });
    assert_eq!(heap.stats().large, 2);
    heap.collect();
    assert_eq!(heap.stats().large, 0);
    drop(root);
}

/// A chain node of the cross test: carries a counted tag; the chain head holds the root of a chain on another heap; every few nodes hold a large object (pointing back into the chain, forming a cycle).
#[derive(Trace)]
struct Item<'scope> {
    value: u64,
    next: Option<Gc<'scope, Item<'scope>>>,
    /// (heap id, root): a chain on another heap, released when this object is finalized
    guest: Option<(usize, Global<'static, ItemB>)>,
    ballast: Option<Gc<'scope, Ballast<'scope>>>,
    tag: Counted,
}

/// A large object (above half a block, allocated on its own), pointing back at the node holding it (a cycle).
#[derive(Trace)]
struct Ballast<'scope> {
    words: [u64; BALLAST_WORDS],
    owner: GcSlot<'scope, Item<'scope>>,
}

brand!(ItemB = for<'scope> Item<'scope>);

const BALLAST_WORDS: usize = 2100;
/// Under Miri 8 × 8 takes more than 15 s per test, so 4 × 4
const HEAPS: usize = if cfg!(miri) { 4 } else { 8 };
const WORKERS: usize = HEAPS;
const ITEMS: u64 = if cfg!(miri) { 6 } else { 4_000 };
/// Even under Miri, each heap gets at least 16 large objects per round (enough for 2 marking workers)
const BALLAST_EVERY: u64 = if cfg!(miri) { 2 } else { 10 };
const CROSS_ROUNDS: usize = if cfg!(miri) { 1 } else { 8 };
const ITEMS_SUM: u64 = ITEMS * (ITEMS - 1) / 2;

/// Tags made and tags finalized.
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

    /// Make a chain on heap `h` whose head carries `guest`.
    #[allow(clippy::large_stack_arrays, reason = "a large object has to exceed half a block")]
    fn build(&self, heaps: &[Heap<'static>], h: usize, guest: Option<(usize, Global<'static, ItemB>)>) -> Global<'static, ItemB> {
        heaps[h].scope(|m| {
            let mut guest = guest;
            let mut top = None;
            for value in 0..ITEMS {
                let ballast = value.is_multiple_of(BALLAST_EVERY).then(|| m.alloc(Ballast { words: [value; BALLAST_WORDS], owner: GcSlot::default() }));
                let guest = if value == ITEMS - 1 { guest.take() } else { None };
                let item = m.alloc(Item { value, next: top, guest, ballast, tag: self.tag() });
                if let Some(b) = ballast {
                    b.owner.set(m, Some(item));
                }
                top = Some(item);
            }
            m.global(top.expect("ITEMS > 0"))
        })
    }
}

/// Check that a chain is intact; with `depth > 0` also the chain on another heap held by the chain head (every chain is also checked on its own, so only one level down).
fn check(heaps: &[Heap<'static>], h: usize, root: &Global<'static, ItemB>, depth: usize) {
    heaps[h].scope(|m| {
        let mut sum = 0;
        let mut cur = Some(m.get(root));
        while let Some(n) = cur {
            sum += n.value;
            if let Some(b) = n.ballast {
                assert!(b.words[0] == n.value && b.words[BALLAST_WORDS - 1] == n.value, "the large object's contents are corrupted");
                assert!(b.owner.get().is_some_and(|o| Gc::ptr_eq(o, n)), "the large object's back pointer is corrupted");
            }
            if let Some((g, guest)) = &n.guest
                && depth > 0
            {
                check(heaps, *g, guest, depth - 1);
            }
            cur = n.next;
        }
        assert_eq!(sum, ITEMS_SUM, "the chain on heap {h} is corrupted");
    });
}

/// Collect all heaps in parallel (one thread per heap) while another thread releases the roots in `dropping`.
fn collect_all<T: Send>(heaps: &mut [Heap], dropping: Vec<T>) {
    std::thread::scope(|s| {
        s.spawn(move || {
            // Release every other one, backwards, to shuffle the order
            let (odd, even): (Vec<_>, Vec<_>) = dropping.into_iter().rev().enumerate().partition(|(i, _)| i % 2 == 1);
            drop(odd);
            drop(even);
        });
        for heap in heaps.iter_mut() {
            s.spawn(move || heap.collect());
        }
    });
}

/// 8 threads × 8 heaps: each thread makes chains on every heap in turn, each chain head holding the root of its own chain on the previous heap (cross-heap holding);
/// the finished roots go to another thread to check; half are released on the spot (released across threads), half kept until the next round and released by another thread while all heaps collect in parallel.
#[test]
fn eight_threads_eight_heaps_crossed() {
    let ledger = Ledger::default();
    let mut heaps: Vec<Heap<'static>> = (0..HEAPS).map(|_| Heap::new_static(THREADS)).collect();
    let mut kept_prev: Vec<(usize, Global<'_, ItemB>)> = Vec::new();
    for round in 0..CROSS_ROUNDS {
        // Make: thread t makes chains on heap (t + k + round) % HEAPS in turn
        let made: Vec<Vec<(usize, Global<'_, ItemB>)>> = std::thread::scope(|s| {
            let (heaps, ledger) = (&heaps, &ledger);
            let handles: Vec<_> = (0..WORKERS)
                .map(|t| {
                    s.spawn(move || {
                        let mut carry: Option<(usize, Global<'_, ItemB>)> = None;
                        let mut out = Vec::new();
                        for k in 0..HEAPS {
                            let h = (t + k + round) % HEAPS;
                            let root = ledger.build(heaps, h, carry.take());
                            carry = Some((h, heaps[h].scope(|m| m.global(m.get(&root)))));
                            out.push((h, root));
                        }
                        out
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("a chain-making thread panicked")).collect()
        });
        // Check: thread t checks the chains made by thread t + 1 and releases half on the spot
        let kept_now: Vec<(usize, Global<'_, ItemB>)> = std::thread::scope(|s| {
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
            handles.into_iter().flat_map(|h| h.join().expect("a checking thread panicked")).collect()
        });
        // Collect all heaps while releasing what the previous round kept
        collect_all(&mut heaps, std::mem::take(&mut kept_prev));
        for (h, root) in &kept_now {
            check(&heaps, *h, root, 1);
        }
        kept_prev = kept_now;
    }
    // Collect repeatedly after releasing everything: a cross-heap root is released only when the object holding it is collected, at most HEAPS levels deep
    collect_all(&mut heaps, kept_prev);
    for _ in 0..=HEAPS {
        if ledger.dropped.load(Ordering::Relaxed) == ledger.made.load(Ordering::Relaxed) {
            break;
        }
        collect_all(&mut heaps, Vec::<()>::new());
    }
    assert_eq!(ledger.dropped.load(Ordering::Relaxed), ledger.made.load(Ordering::Relaxed), "some objects were not collected");
    assert!(heaps.iter().all(|h| h.stats().live == 0 && h.stats().large == 0), "the heaps still hold live objects");
}

/// A node of the deep tree test: two mutable children, with a drop counter.
#[derive(Trace)]
struct Bin<'scope> {
    left: GcSlot<'scope, Bin<'scope>>,
    right: GcSlot<'scope, Bin<'scope>>,
    tag: Counted,
}

brand!(BinB = for<'scope> Bin<'scope>);

impl Bin<'_> {
    /// Nodes reachable from `self` (explicit stack, no recursion).
    fn count(&self) -> usize {
        let mut stack = vec![self];
        let mut n = 0;
        while let Some(b) = stack.pop() {
            n += 1;
            stack.extend(b.left.get().map(Gc::get));
            stack.extend(b.right.get().map(Gc::get));
        }
        n
    }
}

/// A full binary tree (`depth` levels, 2^depth − 1 nodes), built bottom up level by level.
fn build_tree<'h>(heap: &Heap<'h>, drops: &Arc<AtomicUsize>, depth: u32) -> Global<'h, BinB> {
    heap.scope(|m| {
        let tag = || Counted(drops.clone());
        let mut level: Vec<_> = (0..1usize << (depth - 1)).map(|_| m.alloc(Bin { left: GcSlot::default(), right: GcSlot::default(), tag: tag() })).collect();
        while level.len() > 1 {
            level = level.chunks(2).map(|pair| m.alloc(Bin { left: pair[0].into(), right: pair[1].into(), tag: tag() })).collect();
        }
        m.global(level[0])
    })
}

#[test]
fn deep_binary_tree() {
    const DEPTH: u32 = if cfg!(miri) { 7 } else { 20 };
    const ALL: usize = (1 << DEPTH) - 1;
    const HALF: usize = (1 << (DEPTH - 1)) - 1;
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new_static(THREADS);
    let root = build_tree(&heap, &drops, DEPTH);
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    assert_eq!(heap.scope(|m| m.get(&root).count()), ALL);
    // Prune the right subtree: exactly 2^(DEPTH-1) − 1 nodes are finalized
    heap.scope(|m| m.get(&root).right.set(m, None));
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), HALF);
    assert_eq!(heap.scope(|m| m.get(&root).count()), ALL - HALF);
    drop(root);
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), ALL);
    assert_eq!(heap.stats().live, 0);
}

#[test]
fn deep_chain() {
    const LEN: usize = if cfg!(miri) { 500 } else { 1_000_000 };
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new_static(THREADS);
    let root: Global<'_, BinB> = heap.scope(|m| {
        let mut top: Option<Gc<'_, Bin<'_>>> = None;
        for _ in 0..LEN {
            top = Some(m.alloc(Bin { left: top.into(), right: GcSlot::default(), tag: Counted(drops.clone()) }));
        }
        m.global(top.expect("LEN > 0"))
    });
    heap.collect();
    assert_eq!(heap.scope(|m| m.get(&root).count()), LEN);
    // Cut in the middle: the second half is finalized
    heap.scope(|m| {
        let mut cur = m.get(&root);
        for _ in 1..LEN / 2 {
            cur = cur.left.get().expect("the chain is long enough");
        }
        cur.left.set(m, None);
    });
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), LEN - LEN / 2);
    assert_eq!(heap.scope(|m| m.get(&root).count()), LEN / 2);
    drop(root);
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), LEN);
}

/// A node of the cross deep tree: tree nodes use both children, chain nodes only left; tree leaves hold the root of a node in a deep chain on another heap.
#[derive(Trace)]
struct XNode<'scope> {
    left: GcSlot<'scope, XNode<'scope>>,
    right: GcSlot<'scope, XNode<'scope>>,
    /// (heap id, root): a node in a deep chain on another heap
    guest: Option<(usize, Global<'static, XB>)>,
    tag: Counted,
}

brand!(XB = for<'scope> XNode<'scope>);

/// Levels of the full binary tree on each heap
const X_DEPTH: u32 = if cfg!(miri) { 4 } else { 15 };
const X_LEAVES: usize = 1 << (X_DEPTH - 1);
const X_TREE: usize = (1 << X_DEPTH) - 1;
/// Leaves anchor at X_OFFSET, X_OFFSET + X_STEP, … in the deep chain
const X_OFFSET: usize = if cfg!(miri) { 3 } else { 1000 };
const X_STEP: usize = if cfg!(miri) { 1 } else { 4 };
/// Length of the deep chain on each heap
const X_CHAIN: usize = X_OFFSET + X_LEAVES * X_STEP;

impl XNode<'_> {
    /// Nodes of this heap reachable from here (not crossing heaps, explicit stack).
    fn reach(&self) -> usize {
        let mut stack = vec![self];
        let mut n = 0;
        while let Some(x) = stack.pop() {
            n += 1;
            stack.extend(x.left.get().map(Gc::get));
            stack.extend(x.right.get().map(Gc::get));
        }
        n
    }
}

impl Ledger {
    /// A deep chain of length X_CHAIN on heap `h`.
    fn x_chain<'h>(&self, heap: &Heap<'h>) -> Global<'h, XB> {
        heap.scope(|m| {
            let mut top: Option<Gc<'_, XNode<'_>>> = None;
            for _ in 0..X_CHAIN {
                top = Some(m.alloc(XNode { left: top.into(), right: GcSlot::default(), guest: None, tag: self.tag() }));
            }
            m.global(top.expect("X_CHAIN > 0"))
        })
    }

    /// Build a full binary tree on `heap` whose i-th leaf carries `guests[i]`.
    fn x_tree<'h>(&self, heap: &Heap<'h>, guests: Vec<(usize, Global<'static, XB>)>) -> Global<'h, XB> {
        heap.scope(|m| {
            let mut guests = guests.into_iter();
            let mut level: Vec<_> = (0..X_LEAVES)
                .map(|_| m.alloc(XNode { left: GcSlot::default(), right: GcSlot::default(), guest: guests.next(), tag: self.tag() }))
                .collect();
            while level.len() > 1 {
                level = level
                    .chunks(2)
                    .map(|pair| m.alloc(XNode { left: pair[0].into(), right: pair[1].into(), guest: None, tag: self.tag() }))
                    .collect();
            }
            m.global(level[0])
        })
    }
}

/// Roots of the anchors in the deep chain of heap `h` (the i-th at depth X_OFFSET + i·X_STEP).
fn x_anchors<'h>(heap: &Heap<'h>, h: usize, chain: &Global<'h, XB>) -> Vec<(usize, Global<'h, XB>)> {
    heap.scope(|m| {
        let mut out = Vec::with_capacity(X_LEAVES);
        let mut cur = Some(m.get(chain));
        let mut depth = 0;
        while let Some(n) = cur {
            if depth >= X_OFFSET && (depth - X_OFFSET).is_multiple_of(X_STEP) && out.len() < X_LEAVES {
                out.push((h, m.global(n)));
            }
            depth += 1;
            cur = n.left.get();
        }
        out
    })
}

/// How long the deep chain anchored by the leftmost (or rightmost) leaf of tree `h` is from the anchor down; tree and chain are on different heaps, so the scopes nest.
fn x_edge_reach(heaps: &[Heap<'static>], h: usize, tree: &Global<'static, XB>, leftmost: bool) -> usize {
    heaps[h].scope(|m| {
        let mut cur = m.get(tree);
        while let Some(next) = if leftmost { cur.left.get().or(cur.right.get()) } else { cur.right.get().or(cur.left.get()) } {
            cur = next;
        }
        let (g, root) = cur.guest.as_ref().expect("every leaf carries an anchor");
        heaps[*g].scope(|gm| gm.get(root).reach())
    })
}

/// Cross deep trees with 8 threads × 8 heaps: each heap has a deep chain + a full binary tree whose leaves anchor in the next heap's deep chain (cross-heap holding);
/// chain roots, pruning and tree roots are released by other threads at other times while all heaps collect in parallel; every step checks that the finalized count is exactly as predicted.
#[test]
fn crossed_deep_trees() {
    let ledger = Ledger::default();
    let dropped = || ledger.dropped.load(Ordering::Relaxed);
    let mut heaps: Vec<Heap<'static>> = (0..HEAPS).map(|_| Heap::new_static(THREADS)).collect();
    // 1. Each thread makes a deep chain on its own heap
    let chains: Vec<Global<'_, XB>> = std::thread::scope(|s| {
        let (heaps, ledger) = (&heaps, &ledger);
        let hs: Vec<_> = (0..WORKERS).map(|t| s.spawn(move || ledger.x_chain(&heaps[t]))).collect();
        hs.into_iter().map(|h| h.join().expect("making a chain panicked")).collect()
    });
    // 2. Thread t takes anchors in the next heap's deep chain (while that heap's thread builds its tree) and builds a tree on its own heap
    let trees: Vec<Global<'_, XB>> = std::thread::scope(|s| {
        let (heaps, ledger, chains) = (&heaps, &ledger, &chains);
        let hs: Vec<_> = (0..WORKERS)
            .map(|t| {
                s.spawn(move || {
                    let next = (t + 1) % HEAPS;
                    ledger.x_tree(&heaps[t], x_anchors(&heaps[next], next, &chains[next]))
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().expect("building a tree panicked")).collect()
    });
    assert_eq!(dropped(), 0);
    // 3. Chain roots go to another thread to release while all heaps collect in parallel; the X_OFFSET nodes from the chain head to the first anchor die
    collect_all(&mut heaps, chains);
    collect_all(&mut heaps, Vec::<()>::new());
    assert_eq!(dropped(), HEAPS * X_OFFSET);
    for (h, tree) in trees.iter().enumerate() {
        assert_eq!(heaps[h].scope(|m| m.get(tree).reach()), X_TREE);
        assert_eq!(x_edge_reach(&heaps, h, tree, true), X_CHAIN - X_OFFSET);
        assert_eq!(x_edge_reach(&heaps, h, tree, false), X_CHAIN - X_OFFSET - (X_LEAVES - 1) * X_STEP);
    }
    // 4. Thread t prunes the left half of heap (t + 2)'s tree: the left subtree dies and the anchors its leaves carry are released with it,
    //    so the nodes between the first half of the anchors in the next heap's deep chain die a round later
    std::thread::scope(|s| {
        let (heaps, trees) = (&heaps, &trees);
        for t in 0..WORKERS {
            s.spawn(move || {
                let h = (t + 2) % HEAPS;
                heaps[h].scope(|m| m.get(&trees[h]).left.set(m, None));
            });
        }
    });
    collect_all(&mut heaps, Vec::<()>::new());
    collect_all(&mut heaps, Vec::<()>::new());
    assert_eq!(dropped(), HEAPS * (X_OFFSET + X_TREE / 2 + X_LEAVES / 2 * X_STEP));
    for (h, tree) in trees.iter().enumerate() {
        assert_eq!(heaps[h].scope(|m| m.get(tree).reach()), X_TREE - X_TREE / 2);
        assert_eq!(x_edge_reach(&heaps, h, tree, true), X_CHAIN - X_OFFSET - X_LEAVES / 2 * X_STEP);
    }
    // 5. Tree roots go to another thread and are released during the first round of collection: the release may come after that heap's marking, so the tree dies only in the second round;
    //    the anchors are released only when the tree dies, so the chains die only in the third round
    collect_all(&mut heaps, trees);
    collect_all(&mut heaps, Vec::<()>::new());
    collect_all(&mut heaps, Vec::<()>::new());
    assert_eq!(dropped(), ledger.made.load(Ordering::Relaxed));
    assert!(heaps.iter().all(|h| h.stats().live == 0), "the heaps still hold live objects");
}

/// A node of the concurrent marking test: the next one in the chain is a mutable slot.
#[derive(Trace)]
struct Link<'scope> {
    next: GcSlot<'scope, Link<'scope>>,
    tag: Counted,
}

/// A row of slots, each holding a chain.
#[derive(Trace)]
struct Board<'scope> {
    cells: Vec<GcSlot<'scope, Link<'scope>>>,
}

brand!(BoardB = for<'scope> Board<'scope>);

/// The slot count is a multiple of MOVERS (every thread gets as many slots)
const CELLS: usize = if cfg!(miri) { 4 } else { 16 };
const MOVERS: usize = if cfg!(miri) { 2 } else { 4 };
/// Mutates per mutator per round, and steps per scope
const MUTATES: usize = if cfg!(miri) { 6 } else { 400 };
const STEPS: usize = if cfg!(miri) { 4 } else { 32 };
const MARK_ROUNDS: usize = if cfg!(miri) { 2 } else { 6 };

/// Each thread's own pseudo-random numbers (xorshift).
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        usize::try_from(self.0 % u64::try_from(n).expect("n is small")).expect("less than n")
    }
}

/// Made and discarded (discarded = taken off a chain and never hung back).
#[derive(Default)]
struct Moves {
    ledger: Ledger,
    discarded: AtomicUsize,
    /// Mutates that fell within concurrent marking (confirming they really overlapped marking)
    overlapped: AtomicUsize,
    /// Mutates that fell within concurrent sweeping
    overlapped_sweep: AtomicUsize,
}

impl Moves {
    /// One step, chosen at random: move (take a chain head off its own slot, hold it on the stack, hang it on any slot) / make / discard.
    /// Slot i is taken from only by thread i % MOVERS (a multi-producer single-consumer Treiber stack has no ABA); anyone can hang onto it.
    fn step<'scope>(&self, m: &paragc::Scope<'_, 'scope>, board: &Board<'scope>, me: usize, rng: &mut Rng) {
        let (a, b) = (rng.below(CELLS / MOVERS) * MOVERS + me, rng.below(CELLS));
        match rng.below(4) {
            0 | 1 => {
                if let Some(x) = Self::pop(m, &board.cells[a]) {
                    Self::push(m, &board.cells[b], x);
                }
            }
            2 => {
                let x = m.alloc(Link { next: GcSlot::default(), tag: self.ledger.tag() });
                Self::push(m, &board.cells[b], x);
            }
            _ => {
                if Self::pop(m, &board.cells[a]).is_some() {
                    self.discarded.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Take off the chain head (only this slot's owner takes; the CAS races only with hangers); the node taken lives only on the stack afterwards.
    fn pop<'scope>(m: &paragc::Scope<'_, 'scope>, cell: &GcSlot<'scope, Link<'scope>>) -> Option<Gc<'scope, Link<'scope>>> {
        loop {
            let x = cell.get()?;
            if cell.compare_exchange(m, Some(x), x.next.get()).is_ok() {
                return Some(x);
            }
        }
    }

    /// Hang onto the chain head (CAS).
    fn push<'scope>(m: &paragc::Scope<'_, 'scope>, cell: &GcSlot<'scope, Link<'scope>>, x: Gc<'scope, Link<'scope>>) {
        loop {
            let head = cell.get();
            x.next.set(m, head);
            if cell.compare_exchange(m, head, Some(x)).is_ok() {
                return;
            }
        }
    }

    /// Nodes on all chains (explicit stack).
    fn reachable<'h>(heap: &Heap<'h>, board: &Global<'h, BoardB>) -> usize {
        heap.scope(|m| {
            let mut n = 0;
            for cell in &m.get(board).cells {
                let mut cur = cell.get();
                while let Some(x) = cur {
                    n += 1;
                    cur = x.next.get();
                }
            }
            n
        })
    }
}

impl Moves {
    /// One round: MOVERS mutators keep moving / making / discarding (STEPS steps per scope), stopping after MUTATES scopes and only once
    /// `background` has finished; the background thread starts after the mutators have run a while (so the two overlap).
    fn round<'h>(&self, heap: &Heap<'h>, board: &Global<'h, BoardB>, round: usize, background: impl FnOnce(&Heap<'h>) + Send) {
        let started = std::sync::atomic::AtomicBool::new(false);
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            let (started, done) = (&started, &done);
            for t in 0..MOVERS {
                s.spawn(move || {
                    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ u64::try_from((round << 8) | t).expect("small"));
                    let mut tlab = paragc::Tlab::default();
                    let mut i = 0;
                    while i < MUTATES || !done.load(Ordering::Acquire) {
                        if i == MUTATES / 4 {
                            started.store(true, Ordering::Release);
                        }
                        i += 1;
                        heap.scope_in(&mut tlab, |m| {
                            if m.is_marking() {
                                self.overlapped.fetch_add(1, Ordering::Relaxed);
                            }
                            if heap.is_sweeping() {
                                self.overlapped_sweep.fetch_add(1, Ordering::Relaxed);
                            }
                            let board = m.get(board);
                            for _ in 0..STEPS {
                                self.step(m, &board, t, &mut rng);
                            }
                        });
                    }
                });
            }
            s.spawn(move || {
                while !started.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                background(heap);
                done.store(true, Ordering::Release);
            });
        });
    }

    /// After each round: no node still on a chain was finalized, and the nodes on chains are exactly made − discarded.
    fn check<'h>(&self, heap: &Heap<'h>, board: &Global<'h, BoardB>, round: usize) {
        let (dropped, discarded) = (self.ledger.dropped.load(Ordering::Relaxed), self.discarded.load(Ordering::Relaxed));
        assert!(dropped <= discarded, "round {round} finalized nodes still on a chain: {dropped} finalized > {discarded} discarded");
        let live = self.ledger.made.load(Ordering::Relaxed) - discarded;
        assert_eq!(Self::reachable(heap, board), live, "round {round} lost nodes from the chains");
    }

    /// Finish: one more full collection clears floating garbage, the finalized count is exactly the discarded count; after dropping the board everything is finalized.
    fn finish(&self, mut heap: Heap, board: Global<'_, BoardB>) {
        heap.collect();
        assert_eq!(self.ledger.dropped.load(Ordering::Relaxed), self.discarded.load(Ordering::Relaxed));
        drop(board);
        heap.collect();
        assert_eq!(self.ledger.dropped.load(Ordering::Relaxed), self.ledger.made.load(Ordering::Relaxed));
    }
}

fn board<'h>(heap: &Heap<'h>) -> Global<'h, BoardB> {
    heap.scope(|m| m.global(m.alloc(Board { cells: (0..CELLS).map(|_| GcSlot::default()).collect() })))
}

/// During concurrent marking, several mutators keep taking nodes off one slot (leaving only a stack reference), hanging them on another slot, making and discarding;
/// another thread marks concurrently. After each round's collection: nodes on chains = made − discarded (the snapshot missed no moved node);
/// one final full collection: the finalized count is exactly the discarded count.
#[test]
fn concurrent_marking_while_relinking() {
    let moves = Moves::default();
    let mut heap = Heap::new_static(THREADS);
    let board = board(&heap);
    for round in 0..MARK_ROUNDS {
        moves.round(&heap, &board, round, |h| assert!(h.mark_concurrently(), "this round is not marking yet"));
        heap.collect();
        moves.check(&heap, &board, round);
    }
    assert!(moves.overlapped.load(Ordering::Relaxed) > 0, "no scope fell within concurrent marking");
    moves.finish(heap, board);
}

/// Concurrent marking → end of marking (pause) → concurrent sweeping: during sweeping mutators keep moving / making / discarding and sweep along when they need blocks (lazy sweeping),
/// while another thread runs `sweep_concurrently`. After each round's sweep the nodes on chains are exact and no node still hanging was finalized; the final finalized count is exact.
#[test]
fn concurrent_sweeping_while_relinking() {
    let moves = Moves::default();
    let mut heap = Heap::new_static(THREADS);
    let board = board(&heap);
    for round in 0..MARK_ROUNDS {
        moves.round(&heap, &board, round, |h| assert!(h.mark_concurrently(), "this round is not marking yet"));
        heap.finish_marking();
        moves.round(&heap, &board, round, Heap::sweep_concurrently);
        assert!(!heap.is_sweeping());
        moves.check(&heap, &board, round);
    }
    assert!(moves.overlapped.load(Ordering::Relaxed) > 0, "no scope fell within concurrent marking");
    assert!(moves.overlapped_sweep.load(Ordering::Relaxed) > 0, "no scope fell within concurrent sweeping");
    moves.finish(heap, board);
}

/// An element of the evacuation test: only a value and a drop counter (no edges).
#[derive(Trace)]
struct Cell8 {
    value: u64,
    tag: Counted,
}

/// Global: the elements kept.
#[derive(Trace)]
struct Keep<'scope> {
    items: Vec<Gc<'scope, Cell8>>,
}

brand!(KeepB = for<'scope> Keep<'scope>);

/// Allocate many elements and keep only 1/16: after the first collection each block is only ~6% live (sparse), and the second collection should evacuate them:
/// the block count drops, the kept elements keep their values but change address, and the finalized count is exactly the discarded count.
#[test]
fn evacuation_compacts_sparse_blocks() {
    const ITEMS: u64 = if cfg!(miri) { 4_000 } else { 64_000 };
    const KEEP_EVERY: u64 = 16;
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new_static(THREADS);
    let keep: Global<'_, KeepB> = heap.scope(|m| {
        let mut items = Vec::new();
        for value in 0..ITEMS {
            let x = m.alloc(Cell8 { value, tag: Counted(drops.clone()) });
            if value.is_multiple_of(KEEP_EVERY) {
                items.push(x);
            }
        }
        m.global(m.alloc(Keep { items }))
    });
    let kept = ITEMS / KEEP_EVERY;
    let discarded = usize::try_from(ITEMS - kept).expect("small");
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), discarded);
    let before_blocks = heap.stats().blocks;
    let before: Vec<usize> = heap.scope(|m| m.get(&keep).items.iter().map(|x| x.as_ptr().addr()).collect());
    heap.collect();
    let stats = heap.stats();
    assert!(stats.evacuated_blocks > 0, "sparse blocks were not evacuated: {stats:?}");
    assert!(stats.blocks < before_blocks, "the block count did not drop after evacuation: {before_blocks} ⇒ {}", stats.blocks);
    let after: Vec<(usize, u64)> = heap.scope(|m| m.get(&keep).items.iter().map(|x| (x.as_ptr().addr(), x.value)).collect());
    assert!(after.iter().enumerate().all(|(i, &(_, v))| v == u64::try_from(i).expect("small") * KEEP_EVERY), "moved elements have wrong values");
    let moved = before.iter().zip(&after).filter(|(b, (a, _))| *b != a).count();
    assert!(moved > 0, "no element changed address");
    assert_eq!(drops.load(Ordering::Relaxed), discarded, "a moved old copy was finalized");
    drop(keep);
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), usize::try_from(ITEMS).expect("small"));
}

/// Children shared by many parents: each parent has two slots pointing at a shared child.
#[derive(Trace)]
struct Parent<'scope> {
    a: GcSlot<'scope, Cell8>,
    b: Gc<'scope, Cell8>,
    tag: Counted,
}

#[derive(Trace)]
struct Family<'scope> {
    parents: Vec<Gc<'scope, Parent<'scope>>>,
}

brand!(FamilyB = for<'scope> Family<'scope>);

/// Many parents point at the same few children (one slot and one plain field each), parents and children all in sparse blocks: during evacuation several collector threads reach the same child at once
/// and race to claim and copy it. Afterwards both edges of each parent must point at the same (new) child with its value intact, and each child is copied once (never finalized twice, never lost).
#[test]
fn evacuation_with_shared_children() {
    const PARENTS: u64 = if cfg!(miri) { 1_500 } else { 40_000 };
    const CHILDREN: u64 = 64;
    const KEEP_EVERY: u64 = 8;
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new_static(THREADS);
    let family: Global<'_, FamilyB> = heap.scope(|m| {
        let children: Vec<_> = (0..CHILDREN).map(|value| m.alloc(Cell8 { value, tag: Counted(drops.clone()) })).collect();
        let mut parents = Vec::new();
        for i in 0..PARENTS {
            // The kept parents (i a multiple of KEEP_EVERY) point at children 0, 1, 2 … in turn, so every child is referenced
            let c = children[usize::try_from((i / KEEP_EVERY) % CHILDREN).expect("small")];
            let p = m.alloc(Parent { a: c.into(), b: c, tag: Counted(drops.clone()) });
            // Garbage interleaved, making the blocks sparse
            m.alloc(Cell8 { value: u64::MAX, tag: Counted(drops.clone()) });
            if i.is_multiple_of(KEEP_EVERY) {
                parents.push(p);
            }
        }
        m.global(m.alloc(Family { parents }))
    });
    // Kept parents: i = 0, KEEP_EVERY, 2·KEEP_EVERY … ⌈PARENTS / KEEP_EVERY⌉ in all
    let garbage = usize::try_from(PARENTS * 2 - PARENTS.div_ceil(KEEP_EVERY)).expect("small");
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), garbage);
    heap.collect();
    assert!(heap.stats().evacuated_blocks > 0, "no evacuation: {:?}", heap.stats());
    heap.scope(|m| {
        for (i, p) in m.get(&family).parents.iter().enumerate() {
            let a = p.a.get().expect("the slot is not empty");
            assert!(Gc::ptr_eq(a, p.b), "the two edges to the same child point at different addresses");
            assert_eq!(a.value, u64::try_from(i).expect("small") % CHILDREN);
        }
    });
    assert_eq!(drops.load(Ordering::Relaxed), garbage, "evacuation finalized too much");
    drop(family);
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), usize::try_from(PARENTS * 2 + CHILDREN).expect("small"));
}

/// Exact accounting for young collections: old nodes (through one full collection) hang on slots 0 and 2 of the board; then young nodes are made and hung three ways:
/// directly on the old board's slot 1 (an old object's slot ⇒ remembered set), on the next of the old node heading slot 0 (an old object's slot), and under another young node (young → young);
/// the other young nodes are discarded. Then some old nodes are taken off slot 2 (old garbage). The young collection must finalize exactly the discarded young nodes (by tracing)
/// and the discarded old nodes (count reaching zero), with no node lost from the chains; the full collection afterwards finalizes nothing more.
#[test]
fn young_collection_frees_only_young_garbage() {
    const OLD: usize = if cfg!(miri) { 60 } else { 2_000 };
    const YOUNG: usize = if cfg!(miri) { 120 } else { 20_000 };
    const OLD_DISCARD: usize = OLD / 4;
    let moves = Moves::default();
    let mut heap = Heap::new_static(THREADS);
    let board = board(&heap);
    // Old nodes: one chain each on slots 0 and 2, all old after a full collection
    heap.scope(|m| {
        let b = m.get(&board);
        for i in 0..OLD {
            Moves::push(m, &b.cells[if i % 2 == 0 { 0 } else { 2 }], m.alloc(Link { next: GcSlot::default(), tag: moves.ledger.tag() }));
        }
    });
    heap.collect();
    // Young nodes: of every 4, 1 hangs on slot 1, 1 goes after the old node heading slot 0, 1 goes after the (young) node heading slot 1, and 1 is discarded
    let young_garbage = heap.scope(|m| {
        let b = m.get(&board);
        let old_head = b.cells[0].get().expect("the old chain is not empty");
        let mut garbage = 0;
        for i in 0..YOUNG {
            let x = m.alloc(Link { next: GcSlot::default(), tag: moves.ledger.tag() });
            match i % 4 {
                0 => Moves::push(m, &b.cells[1], x),
                1 => {
                    x.next.set(m, old_head.next.get());
                    old_head.next.set(m, Some(x));
                }
                2 => {
                    let host = b.cells[1].get().expect("just hung");
                    x.next.set(m, host.next.get());
                    host.next.set(m, Some(x));
                }
                _ => garbage += 1,
            }
        }
        garbage
    });
    // Old garbage: take OLD_DISCARD off slot 2 (all old nodes)
    heap.scope(|m| {
        let b = m.get(&board);
        for _ in 0..OLD_DISCARD {
            Moves::pop(m, &b.cells[2]).expect("the old chain is long enough");
            moves.discarded.fetch_add(1, Ordering::Relaxed);
        }
    });
    let before = moves.ledger.dropped.load(Ordering::Relaxed);
    heap.collect_young();
    assert_eq!(heap.stats().young_collections, 1);
    assert_eq!(moves.ledger.dropped.load(Ordering::Relaxed) - before, young_garbage + OLD_DISCARD, "the young collection did not finalize exactly the discarded young nodes + old nodes");
    assert_eq!(Moves::reachable(&heap, &board), OLD - OLD_DISCARD + (YOUNG - young_garbage), "nodes were lost from the chains after the young collection");
    // The full collection finds no garbage left
    heap.collect();
    assert_eq!(moves.ledger.dropped.load(Ordering::Relaxed) - before, young_garbage + OLD_DISCARD);
    assert_eq!(Moves::reachable(&heap, &board), OLD - OLD_DISCARD + (YOUNG - young_garbage));
    moves.discarded.fetch_add(young_garbage, Ordering::Relaxed);
    moves.finish(heap, board);
}

/// Several mutators keep moving / making / discarding (moves hang young nodes on old nodes' slots ⇒ remembered set), collecting after each round:
/// alternating a young collection, concurrent marking finished by a young collection (equivalent to a full one), and a full collection. Each round: no node still hanging is finalized and the nodes on chains are exact.
#[test]
fn young_collections_while_relinking() {
    let moves = Moves::default();
    let mut heap = Heap::new_static(THREADS);
    let board = board(&heap);
    for round in 0..MARK_ROUNDS * 2 {
        match round % 3 {
            0 => {
                moves.round(&heap, &board, round, |_| ());
                heap.collect_young();
            }
            1 => {
                moves.round(&heap, &board, round, |h| assert!(h.mark_concurrently()));
                heap.collect_young();
            }
            _ => {
                moves.round(&heap, &board, round, |_| ());
                heap.collect();
            }
        }
        moves.check(&heap, &board, round);
    }
    assert!(heap.stats().young_collections > 0);
    moves.finish(heap, board);
}

/// Cycle test: every holder on the holder chain holds a two-node cycle.
#[derive(Trace)]
struct Holder<'scope> {
    ring: GcSlot<'scope, Link<'scope>>,
    next: GcSlot<'scope, Holder<'scope>>,
    tag: Counted,
}

#[derive(Trace)]
struct Holders<'scope> {
    head: GcSlot<'scope, Holder<'scope>>,
    plain: GcSlot<'scope, Link<'scope>>,
}

brand!(HoldersB = for<'scope> Holders<'scope>);

/// Reference counting cannot reclaim cycles: PAIRS holders each hold a two-node cycle (a.next = c, c.next = a), plus PAIRS plain nodes;
/// all old after a full collection. Then both the holder chain and the plain chain are taken off: holders and plain nodes drop to zero and the young collection (reference counting) reclaims them;
/// within a cycle the counts stay above 0, so the young collection does not finalize them; that waits for a full collection (tracing).
#[test]
fn reference_counting_leaves_cycles_to_tracing() {
    const PAIRS: usize = if cfg!(miri) { 20 } else { 500 };
    let moves = Moves::default();
    let mut heap = Heap::new_static(THREADS);
    let root: Global<'_, HoldersB> = heap.scope(|m| m.global(m.alloc(Holders { head: GcSlot::default(), plain: GcSlot::default() })));
    heap.scope(|m| {
        let holders = m.get(&root);
        for _ in 0..PAIRS {
            let first = m.alloc(Link { next: GcSlot::default(), tag: moves.ledger.tag() });
            let second = m.alloc(Link { next: Some(first).into(), tag: moves.ledger.tag() });
            first.next.set(m, Some(second));
            let holder = m.alloc(Holder { ring: Some(first).into(), next: holders.head.get().into(), tag: moves.ledger.tag() });
            holders.head.set(m, Some(holder));
            let plain = m.alloc(Link { next: holders.plain.get().into(), tag: moves.ledger.tag() });
            holders.plain.set(m, Some(plain));
        }
    });
    heap.collect();
    let before = moves.ledger.dropped.load(Ordering::Relaxed);
    assert_eq!(before, 0);
    heap.scope(|m| {
        let holders = m.get(&root);
        holders.head.set(m, None);
        holders.plain.set(m, None);
    });
    heap.collect_young();
    assert_eq!(moves.ledger.dropped.load(Ordering::Relaxed) - before, PAIRS * 2, "reference counting should reclaim holders and plain nodes and leave the cycles");
    heap.collect();
    assert_eq!(moves.ledger.dropped.load(Ordering::Relaxed) - before, PAIRS * 4, "the full collection should reclaim the cycles");
    drop(root);
}

/// Reference count decrements move out of the pause and run in the background concurrently with mutators:
/// after a whole old chain is discarded, nothing is finalized yet when `finish_young` (the pause) returns; then mutators keep moving / making / discarding while another thread
/// runs `sweep_concurrently` and releases in cascade — nodes still hanging are not finalized and chain counts are exact; the next pause recomputes lines and the freed lines are reused (the block count does not grow).
#[test]
fn background_release_while_mutating() {
    const CHAIN: usize = if cfg!(miri) { 60 } else { 20_000 };
    let moves = Moves::default();
    let mut heap = Heap::new_static(THREADS);
    let board = board(&heap);
    let spare: Global<'_, BoardB> = heap.scope(|m| m.global(m.alloc(Board { cells: vec![GcSlot::default()] })));
    // A long old chain hangs on another board, with trustworthy counts after a full collection
    heap.scope(|m| {
        let cell = &m.get(&spare).cells[0];
        for _ in 0..CHAIN {
            Moves::push(m, cell, m.alloc(Link { next: GcSlot::default(), tag: moves.ledger.tag() }));
        }
    });
    heap.collect();
    // Discard it whole: only one slot on the board changes
    heap.scope(|m| m.get(&spare).cells[0].set(m, None));
    moves.discarded.fetch_add(CHAIN, Ordering::Relaxed);
    let before = moves.ledger.dropped.load(Ordering::Relaxed);
    heap.finish_young();
    assert_eq!(moves.ledger.dropped.load(Ordering::Relaxed), before, "the pause must not process decrements");
    // Mutators run while the background releases in cascade
    moves.round(&heap, &board, 0, Heap::sweep_concurrently);
    assert!(moves.ledger.dropped.load(Ordering::Relaxed) - before >= CHAIN, "the background did not finish releasing the discarded chain");
    moves.check(&heap, &board, 0);
    // The next pause recomputes lines; allocating as many nodes again reuses the freed lines (the block count barely grows)
    heap.collect_young();
    let blocks_after_release = heap.stats().blocks;
    heap.scope(|m| {
        let cell = &m.get(&spare).cells[0];
        for _ in 0..CHAIN {
            Moves::push(m, cell, m.alloc(Link { next: GcSlot::default(), tag: moves.ledger.tag() }));
        }
    });
    heap.collect_young();
    let slack = 16;
    assert!(heap.stats().blocks <= blocks_after_release + slack, "released lines were not reused: {} ⇒ {}", blocks_after_release, heap.stats().blocks);
    drop(spare);
    heap.collect();
    moves.discarded.fetch_add(CHAIN, Ordering::Relaxed);
    moves.finish(heap, board);
}

/// Concurrent marking recounts along the way: after a round of concurrent marking (with mutators moving / making / discarding, black-allocated nodes hanging others under themselves) finishes,
/// reference counts are trustworthy right away — an old chain discarded whole is released by count at the next young collection without waiting for a stop-the-world full collection; and in the following young collections release by count
/// never touches nodes still hanging (missing the edges of black objects or slots rewritten during marking would finalize live nodes here).
#[test]
fn concurrent_marking_recounts() {
    const CHAIN: usize = if cfg!(miri) { 40 } else { 10_000 };
    let moves = Moves::default();
    // The old chain is accounted separately (not on the main board, not in moves' ledger)
    let chain = Ledger::default();
    let mut heap = Heap::new_static(THREADS);
    let board = board(&heap);
    let spare: Global<'_, BoardB> = heap.scope(|m| m.global(m.alloc(Board { cells: vec![GcSlot::default()] })));
    heap.scope(|m| {
        let cell = &m.get(&spare).cells[0];
        for _ in 0..CHAIN {
            Moves::push(m, cell, m.alloc(Link { next: GcSlot::default(), tag: chain.tag() }));
        }
    });
    // One round of concurrent marking (no stop-the-world full collection), finished
    moves.round(&heap, &board, 0, |h| assert!(h.mark_concurrently()));
    heap.collect();
    moves.check(&heap, &board, 0);
    assert_eq!(chain.dropped.load(Ordering::Relaxed), 0);
    // Counts should be trustworthy: discard the whole chain and the young collection releases it by count
    heap.scope(|m| m.get(&spare).cells[0].set(m, None));
    heap.collect_young();
    assert_eq!(chain.dropped.load(Ordering::Relaxed), CHAIN, "counts untrustworthy after concurrent marking: the discarded chain was not released by the young collection");
    // A few more rounds: mutators move and young collections release by count, never touching what still hangs
    for round in 1..=MARK_ROUNDS {
        moves.round(&heap, &board, round, |_| ());
        heap.collect_young();
        moves.check(&heap, &board, round);
    }
    drop(spare);
    moves.finish(heap, board);
}

#[test]
fn gc_from_raw_round_trip() {
    // Layout: one pointer in size, and so is the Option (repr(transparent) + NonNull)
    assert_eq!(size_of::<Gc<'static, u64>>(), size_of::<*const u64>());
    assert_eq!(size_of::<Option<Gc<'static, u64>>>(), size_of::<*const u64>());
    let heap = Heap::new_static(THREADS);
    heap.scope(|m| {
        let a = m.alloc(Node { value: 7, next: None });
        let b = m.alloc(Node { value: 8, next: Some(a) });
        // Turn it into a raw address and back (within the same scope)
        let raw = b.as_ptr();
        // SAFETY: raw is b's data pointer from this scope, and b is alive
        let back: Gc<'_, Node<'_>> = unsafe { Gc::from_raw(raw) };
        assert!(Gc::ptr_eq(back, b));
        assert_eq!(back.value, 8);
        assert_eq!(back.next.map(|n| n.value), Some(7));
    });
}

/// The write barrier does not record slots in young objects (young collections reach them from the object); once promoted by a young collection they must be recorded:
/// a holder promoted before the first full collection (counts still untrustworthy, promotion does not recount) keeps the young leaves written into it alive through the next young collection.
#[test]
fn promoted_slots_remember_young_values() {
    let heap = &mut Heap::new_static(THREADS);
    let drops = Arc::new(AtomicUsize::new(0));
    let holder: Global<'_, TreeB> = heap.scope(|m| m.global(m.alloc(Tree::Branch { left: GcSlot::default(), right: GcSlot::default() })));
    heap.collect_young();
    heap.scope(|m| {
        let Tree::Branch { left, .. } = &*m.get(&holder) else { unreachable!() };
        left.set(m, Some(m.alloc(Tree::Leaf(Counted(drops.clone())))));
    });
    heap.collect_young();
    assert_eq!(drops.load(Ordering::Relaxed), 0, "a young leaf held by a promoted holder was collected");
    heap.scope(|m| {
        let Tree::Branch { left, .. } = &*m.get(&holder) else { unreachable!() };
        assert!(matches!(left.get().as_deref(), Some(Tree::Leaf(_))));
    });
    drop(holder);
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

/// Records on drop whether it ran on the thread that created the heap (no Gc, so it may have Drop).
struct Witness(Arc<(std::thread::ThreadId, AtomicUsize, AtomicUsize)>);

impl Drop for Witness {
    fn drop(&mut self) {
        let (home, dropped, foreign) = &*self.0;
        dropped.fetch_add(1, Ordering::Relaxed);
        if std::thread::current().id() != *home {
            foreign.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// SAFETY: no Gc
unsafe impl Trace for Witness {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut Tracer<'_>) {}
}

#[derive(Trace)]
struct Twig<'scope> {
    left: GcSlot<'scope, Twig<'scope>>,
    right: GcSlot<'scope, Twig<'scope>>,
    witness: Witness,
}

brand!(TwigB = for<'scope> Twig<'scope>);

/// With parallel finalization off: background release by reference counting, parallel sweeping (young / full) and dropping the heap all finalize dead objects on the caller's thread only, none missing.
#[test]
fn serial_drop_stays_on_the_caller() {
    const DEPTH: u32 = if cfg!(miri) { 6 } else { 15 };
    let nodes = (1usize << DEPTH) - 1;
    let ledger = Arc::new((std::thread::current().id(), AtomicUsize::new(0), AtomicUsize::new(0)));
    let mut heap = Heap::new_static(THREADS);
    heap.set_parallel_drop(false);
    let tree = |heap: &Heap<'static>| -> Global<'static, TwigB> {
        heap.scope(|m| {
            let mut level: Vec<_> = (0..1usize << (DEPTH - 1)).map(|_| m.alloc(Twig { left: GcSlot::default(), right: GcSlot::default(), witness: Witness(ledger.clone()) })).collect();
            while level.len() > 1 {
                level = level.chunks(2).map(|pair| m.alloc(Twig { left: pair[0].into(), right: pair[1].into(), witness: Witness(ledger.clone()) })).collect();
            }
            m.global(level[0])
        })
    };
    let holder: Global<'_, TwigB> = heap.scope(|m| m.global(m.alloc(Twig { left: GcSlot::default(), right: GcSlot::default(), witness: Witness(ledger.clone()) })));
    let first = tree(&heap);
    heap.scope(|m| m.get(&holder).left.set(m, Some(m.get(&first))));
    drop(first);
    heap.collect();
    heap.collect();
    assert_eq!(ledger.1.load(Ordering::Relaxed), 0);
    // The old tree is taken off: its count drops to zero and the background release after the young collection finalizes it
    heap.scope(|m| m.get(&holder).left.set(m, None));
    heap.collect_young();
    assert_eq!(ledger.1.load(Ordering::Relaxed), nodes, "reference counting should reclaim the whole old tree");
    // Young garbage: the young sweep; another tree made old and then discarded: the full collection's sweep
    drop(tree(&heap));
    heap.collect_young();
    assert_eq!(ledger.1.load(Ordering::Relaxed), 2 * nodes);
    let third = tree(&heap);
    heap.collect();
    drop(third);
    heap.collect();
    heap.collect();
    assert_eq!(ledger.1.load(Ordering::Relaxed), 3 * nodes);
    drop(holder);
    drop(heap);
    assert_eq!(ledger.1.load(Ordering::Relaxed), 3 * nodes + 1);
    assert_eq!(ledger.2.load(Ordering::Relaxed), 0, "some objects were finalized on another thread");
}

/// One line in size (232 data bytes + 24 header bytes = 256)
#[derive(Trace)]
struct Line {
    fill: [u64; 29],
}

/// Five of them fill a line exactly (48 bytes)
#[derive(Trace)]
struct Small {
    _pad: [u64; 3],
}

/// A bit over three lines (584 bytes)
#[derive(Trace)]
struct Medium {
    _pad: [u64; 70],
}

#[derive(Trace)]
struct Lines<'scope> {
    kept: Vec<GcSlot<'scope, Line>>,
}

brand!(LinesB = for<'scope> Lines<'scope>);

/// Overflow allocation: when blocks have only single-line holes left, alternating small and medium objects (medium ones do not fit a single-line hole) keeps filling the old holes with small objects
/// while medium objects get overflow blocks — only enough new blocks for the medium objects (if medium objects took the main cursor away, every old hole would be wasted and small objects would need new blocks too).
#[test]
fn medium_objects_overflow_without_wasting_holes() {
    const HOLES: usize = if cfg!(miri) { 300 } else { 6000 };
    /// Block size (paragc::layout::BLOCK)
    const BLOCK: usize = 32 * 1024;
    let mut heap = Heap::new_static(THREADS);
    // Allocate line-sized objects back to back and discard one in three: after a full collection every dead one is a single-line hole (blocks 2/3 live, not evacuated)
    let lines: Global<'_, LinesB> = heap.scope(|m| {
        let all: Vec<_> = (0..3 * HOLES).map(|_| m.alloc(Line { fill: [0; 29] })).collect();
        m.global(m.alloc(Lines { kept: all.iter().enumerate().filter(|(i, _)| i % 3 != 2).map(|(_, &l)| GcSlot::from(Some(l))).collect() }))
    });
    heap.collect();
    heap.collect();
    let before = heap.stats().blocks;
    heap.scope(|m| {
        for _ in 0..HOLES {
            for _ in 0..5 {
                m.alloc(Small { _pad: [1; 3] });
            }
            m.alloc(Medium { _pad: [2; 70] });
        }
    });
    let grown = heap.stats().blocks - before;
    let medium_blocks = (HOLES * size_of::<Medium>()).div_ceil(BLOCK * 9 / 10);
    assert!(grown <= medium_blocks + 2, "opened {grown} blocks, only enough for the medium objects was expected (about {medium_blocks}): small objects did not fill the old holes");
    drop(lines);
}

/// A holder (which becomes old) and the young chain it holds.
#[derive(Trace)]
struct Anchor<'scope> {
    head: GcSlot<'scope, Bead<'scope>>,
}

brand!(AnchorB = for<'scope> Anchor<'scope>);

#[derive(Trace)]
struct Bead<'scope> {
    value: u64,
    next: Option<Gc<'scope, Bead<'scope>>>,
    _counted: Counted,
}

brand!(BeadB = for<'scope> Bead<'scope>);

/// (address, value) of every node on the chain.
fn beads<'scope>(head: Option<Gc<'scope, Bead<'scope>>>) -> Vec<(usize, u64)> {
    let mut out = Vec::new();
    let mut cur = head;
    while let Some(b) = cur {
        out.push((b.as_ptr().addr(), b.value));
        cur = b.next;
    }
    out
}

/// Make a chain of `len` nodes (values from `from` on) with garbage between the nodes (survivors scattered across blocks).
fn chain<'scope>(m: &paragc::Scope<'_, 'scope>, drops: &Arc<AtomicUsize>, from: u64, len: u64) -> Gc<'scope, Bead<'scope>> {
    let mut top = None;
    for value in from..from + len {
        m.alloc(Bead { value: u64::MAX, next: None, _counted: Counted(drops.clone()) });
        top = Some(m.alloc(Bead { value, next: top, _counted: Counted(drops.clone()) }));
    }
    top.expect("len > 0")
}

/// Young evacuation: an old holder's slot holds a young chain and a root holds another; after a young collection —
/// `Full`: both chains moved (every address changed) with values intact, and the slot and root point at the new addresses;
/// `SkipRemembered`: the head the slot points at directly stays put and the rest of that chain moves;
/// `Off`: nothing moves. Taking the chains off afterwards, reference counting still reclaims them (exact finalized count).
#[test]
fn young_evacuation_moves_survivors() {
    const LEN: u64 = if cfg!(miri) { 50 } else { 5000 };
    let n = usize::try_from(LEN).expect("chain length");
    for mode in [YoungEvacuation::Full, YoungEvacuation::SkipRemembered, YoungEvacuation::Off] {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut heap = Heap::new_static(THREADS);
        heap.set_young_evacuation(mode);
        let anchor: Global<'_, AnchorB> = heap.scope(|m| m.global(m.alloc(Anchor { head: GcSlot::default() })));
        heap.collect();
        heap.collect();
        let (rooted, before_slot, before_root): (Global<'_, BeadB>, _, _) = heap.scope(|m| {
            let a = m.get(&anchor);
            a.head.set(m, Some(chain(m, &drops, 0, LEN)));
            let r = chain(m, &drops, LEN, LEN);
            (m.global(r), beads(a.head.get()), beads(Some(r)))
        });
        heap.collect_young();
        assert_eq!(drops.load(Ordering::Relaxed), 2 * n, "{mode:?}: the garbage in between should all be finalized");
        let (after_slot, after_root) = heap.scope(|m| (beads(m.get(&anchor).head.get()), beads(Some(m.get(&rooted)))));
        let values = |v: &[(usize, u64)]| v.iter().map(|&(_, x)| x).collect::<Vec<_>>();
        assert_eq!(values(&after_slot), values(&before_slot), "{mode:?}: the values of the chain on the slot changed");
        assert_eq!(values(&after_root), values(&before_root), "{mode:?}: the values of the chain on the root changed");
        let moved = |a: &[(usize, u64)], b: &[(usize, u64)]| a.iter().zip(b).filter(|(x, y)| x.0 != y.0).count();
        let (slot_moved, root_moved) = (moved(&before_slot, &after_slot), moved(&before_root, &after_root));
        match mode {
            YoungEvacuation::Full => {
                assert_eq!((slot_moved, root_moved), (n, n), "Full: everything should move");
            }
            YoungEvacuation::SkipRemembered => {
                assert_eq!(after_slot[0].0, before_slot[0].0, "SkipRemembered: what old objects point at directly should not move");
                assert_eq!((slot_moved, root_moved), (n - 1, n), "SkipRemembered: the rest should move");
            }
            YoungEvacuation::Off => assert_eq!((slot_moved, root_moved), (0, 0), "Off: nothing should move"),
        }
        // Take both chains off: reference counting (the slot's chain) and young / full collections (the root's chain) reclaim them, with an exact finalized count
        heap.scope(|m| m.get(&anchor).head.set(m, None));
        drop(rooted);
        heap.collect_young();
        heap.collect();
        assert_eq!(drops.load(Ordering::Relaxed), 4 * n, "{mode:?}: the chains taken off were not fully reclaimed");
        drop(anchor);
    }
}

/// `shrink`: objects stay intact after free lines go back to the system, and allocating into those lines again (faulting in zeroed pages) works too.
#[test]
fn shrink_keeps_objects_and_reuses_lines() {
    const HOLES: usize = if cfg!(miri) { 100 } else { 3000 };
    let mut heap = Heap::new_static(THREADS);
    let lines: Global<'_, LinesB> = heap.scope(|m| {
        let all: Vec<_> = (0..3 * HOLES).map(|i| m.alloc(Line { fill: [i as u64; 29] })).collect();
        m.global(m.alloc(Lines { kept: all.iter().enumerate().filter(|(i, _)| i % 3 == 0).map(|(_, &l)| GcSlot::from(Some(l))).collect() }))
    });
    heap.shrink();
    let check = |heap: &Heap<'static>| heap.scope(|m| m.get(&lines).kept.iter().enumerate().all(|(k, s)| s.get().is_some_and(|l| l.fill.iter().all(|&x| x == 3 * k as u64))));
    assert!(check(&heap), "live objects' contents changed after shrink");
    let drops = Arc::new(AtomicUsize::new(0));
    let fresh = build_tree(&heap, &drops, if cfg!(miri) { 8 } else { 14 });
    assert!(check(&heap));
    assert_eq!(heap.scope(|m| m.get(&fresh).count()), (1 << if cfg!(miri) { 8 } else { 14 }) - 1);
    drop(fresh);
    heap.shrink();
    assert!(check(&heap));
}

fn list_addrs<'h>(heap: &Heap<'h>, root: &Global<'h, NodeB>) -> Vec<usize> {
    heap.scope(|m| {
        let mut out = Vec::new();
        let mut cur = Some(m.get(root));
        while let Some(n) = cur {
            out.push(n.as_ptr().addr());
            cur = n.next;
        }
        out
    })
}

/// When the last young collection kept nearly everything, this one does not move young objects (promotion in place); with low survival they move as usual.
#[test]
fn young_collection_promotes_in_place_when_most_survive() {
    let mut heap = Heap::new_static(THREADS);
    let first = build_list(&heap, LIST_LEN);
    heap.collect_young();
    // All survived: the next one does not move
    let second = build_list(&heap, LIST_LEN);
    let before = list_addrs(&heap, &second);
    heap.collect_young();
    assert_eq!(list_addrs(&heap, &second), before, "the last round all survived, yet this round still moves");
    assert_eq!(sum_list(&heap, &first), LIST_LEN * (LIST_LEN - 1) / 2);
    assert_eq!(sum_list(&heap, &second), LIST_LEN * (LIST_LEN - 1) / 2);
    // This round is nearly all garbage: low survival, so the round after moves as usual
    heap.scope(|m| {
        for value in 0..LIST_LEN * 8 {
            m.alloc(Node { value, next: None });
        }
    });
    heap.collect_young();
    let third = build_list(&heap, LIST_LEN / 8);
    let before = list_addrs(&heap, &third);
    heap.collect_young();
    assert_ne!(list_addrs(&heap, &third), before, "the last round was nearly all garbage, yet this round did not move");
    assert_eq!(sum_list(&heap, &third), (LIST_LEN / 8) * (LIST_LEN / 8 - 1) / 2);
}
