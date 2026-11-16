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
fn collect_all<T: Send>(heaps: &mut [Heap], dropping: Vec<T>) {
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
        collect_all(&mut heaps, Vec::<()>::new());
    }
    assert_eq!(ledger.dropped.load(Ordering::Relaxed), ledger.made.load(Ordering::Relaxed), "有对象没被回收");
    assert!(heaps.iter().all(|h| h.stats().live == 0 && h.stats().large == 0), "堆里还有存活对象");
}

/// 深树测试的节点：两个可改的孩子，带析构计数。
#[derive(Trace)]
struct Bin<'gc> {
    left: Slot<'gc, Bin<'gc>>,
    right: Slot<'gc, Bin<'gc>>,
    tag: Counted,
}

brand!(BinB = for<'gc> Bin<'gc>);

impl Bin<'_> {
    /// 从 `self` 出发可达的节点数（显式栈，不递归）。
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

/// 满二叉树（`depth` 层，2^depth − 1 个节点），自底向上逐层造。
fn build_tree(heap: &Heap, drops: &Arc<AtomicUsize>, depth: u32) -> Root<BinB> {
    heap.mutate(|m| {
        let tag = || Counted(drops.clone());
        let mut level: Vec<_> = (0..1usize << (depth - 1)).map(|_| m.alloc(Bin { left: Slot::default(), right: Slot::default(), tag: tag() })).collect();
        while level.len() > 1 {
            level = level.chunks(2).map(|pair| m.alloc(Bin { left: pair[0].into(), right: pair[1].into(), tag: tag() })).collect();
        }
        m.root(level[0])
    })
}

#[test]
fn deep_binary_tree() {
    const DEPTH: u32 = if cfg!(miri) { 7 } else { 20 };
    const ALL: usize = (1 << DEPTH) - 1;
    const HALF: usize = (1 << (DEPTH - 1)) - 1;
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new(THREADS);
    let root = build_tree(&heap, &drops, DEPTH);
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    assert_eq!(heap.mutate(|m| m.get(&root).count()), ALL);
    // 剪掉右子树：恰好 2^(DEPTH-1) − 1 个节点析构
    heap.mutate(|m| m.get(&root).right.set(None));
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), HALF);
    assert_eq!(heap.mutate(|m| m.get(&root).count()), ALL - HALF);
    drop(root);
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), ALL);
    assert_eq!(heap.stats().live, 0);
}

#[test]
fn deep_chain() {
    const LEN: usize = if cfg!(miri) { 500 } else { 1_000_000 };
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new(THREADS);
    let root: Root<BinB> = heap.mutate(|m| {
        let mut top: Option<Gc<'_, Bin<'_>>> = None;
        for _ in 0..LEN {
            top = Some(m.alloc(Bin { left: top.into(), right: Slot::default(), tag: Counted(drops.clone()) }));
        }
        m.root(top.expect("LEN > 0"))
    });
    heap.collect();
    assert_eq!(heap.mutate(|m| m.get(&root).count()), LEN);
    // 从中间剪断：后一半析构
    heap.mutate(|m| {
        let mut cur = m.get(&root);
        for _ in 1..LEN / 2 {
            cur = cur.left.get().expect("链够长");
        }
        cur.left.set(None);
    });
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), LEN - LEN / 2);
    assert_eq!(heap.mutate(|m| m.get(&root).count()), LEN / 2);
    drop(root);
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), LEN);
}

/// 交叉深树的节点：树节点用两个孩子，链节点只用 left；树叶带着别的堆上深链中某个节点的根。
#[derive(Trace)]
struct XNode<'gc> {
    left: Slot<'gc, XNode<'gc>>,
    right: Slot<'gc, XNode<'gc>>,
    /// (堆号, 根)：别的堆上深链里的一个节点
    guest: Option<(usize, Root<XB>)>,
    tag: Counted,
}

brand!(XB = for<'gc> XNode<'gc>);

/// 每个堆一棵满二叉树的层数
const X_DEPTH: u32 = if cfg!(miri) { 4 } else { 15 };
const X_LEAVES: usize = 1 << (X_DEPTH - 1);
const X_TREE: usize = (1 << X_DEPTH) - 1;
/// 树叶依次锚在深链的 X_OFFSET、X_OFFSET + X_STEP、… 处
const X_OFFSET: usize = if cfg!(miri) { 3 } else { 1000 };
const X_STEP: usize = if cfg!(miri) { 2 } else { 4 };
/// 每个堆一条深链的长度
const X_CHAIN: usize = X_OFFSET + X_LEAVES * X_STEP;

impl XNode<'_> {
    /// 从这里可达的本堆节点数（不跨堆，显式栈）。
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
    /// 第 `h` 个堆上一条 X_CHAIN 长的深链。
    fn x_chain(&self, heap: &Heap) -> Root<XB> {
        heap.mutate(|m| {
            let mut top: Option<Gc<'_, XNode<'_>>> = None;
            for _ in 0..X_CHAIN {
                top = Some(m.alloc(XNode { left: top.into(), right: Slot::default(), guest: None, tag: self.tag() }));
            }
            m.root(top.expect("X_CHAIN > 0"))
        })
    }

    /// 在 `heap` 上造满二叉树，第 i 片叶子带 `guests[i]`。
    fn x_tree(&self, heap: &Heap, guests: Vec<(usize, Root<XB>)>) -> Root<XB> {
        heap.mutate(|m| {
            let mut guests = guests.into_iter();
            let mut level: Vec<_> = (0..X_LEAVES)
                .map(|_| m.alloc(XNode { left: Slot::default(), right: Slot::default(), guest: guests.next(), tag: self.tag() }))
                .collect();
            while level.len() > 1 {
                level = level
                    .chunks(2)
                    .map(|pair| m.alloc(XNode { left: pair[0].into(), right: pair[1].into(), guest: None, tag: self.tag() }))
                    .collect();
            }
            m.root(level[0])
        })
    }
}

/// 第 `h` 个堆深链上锚点的根（第 i 个在深度 X_OFFSET + i·X_STEP）。
fn x_anchors(heap: &Heap, h: usize, chain: &Root<XB>) -> Vec<(usize, Root<XB>)> {
    heap.mutate(|m| {
        let mut out = Vec::with_capacity(X_LEAVES);
        let mut cur = Some(m.get(chain));
        let mut depth = 0;
        while let Some(n) = cur {
            if depth >= X_OFFSET && (depth - X_OFFSET).is_multiple_of(X_STEP) && out.len() < X_LEAVES {
                out.push((h, m.root(n)));
            }
            depth += 1;
            cur = n.left.get();
        }
        out
    })
}

/// 树 `h` 最左（或最右）那片叶子锚着的深链，从锚点往下还有多长；树和链在不同的堆，嵌套 mutate。
fn x_edge_reach(heaps: &[Heap], h: usize, tree: &Root<XB>, leftmost: bool) -> usize {
    heaps[h].mutate(|m| {
        let mut cur = m.get(tree);
        while let Some(next) = if leftmost { cur.left.get().or(cur.right.get()) } else { cur.right.get().or(cur.left.get()) } {
            cur = next;
        }
        let (g, root) = cur.guest.as_ref().expect("叶子都带锚");
        heaps[*g].mutate(|gm| gm.get(root).reach())
    })
}

/// 8 线程 × 8 堆的交叉深树：每个堆一条深链 + 一棵满二叉树，树叶锚在下一个堆的深链上（跨堆持有）；
/// 链根、剪枝、树根都由别的线程在别的时刻释放，全部堆并行回收；每一步核对析构数恰好等于理论值。
#[test]
fn crossed_deep_trees() {
    let ledger = Ledger::default();
    let dropped = || ledger.dropped.load(Ordering::Relaxed);
    let mut heaps: Vec<Heap> = (0..HEAPS).map(|_| Heap::new(THREADS)).collect();
    // 1. 每个线程在自己的堆上造深链
    let chains: Vec<Root<XB>> = std::thread::scope(|s| {
        let (heaps, ledger) = (&heaps, &ledger);
        let hs: Vec<_> = (0..WORKERS).map(|t| s.spawn(move || ledger.x_chain(&heaps[t]))).collect();
        hs.into_iter().map(|h| h.join().expect("造链 panic")).collect()
    });
    // 2. 线程 t 在下一个堆的深链上取锚点（那个堆同时正被它的线程造树），在自己的堆上造树
    let trees: Vec<Root<XB>> = std::thread::scope(|s| {
        let (heaps, ledger, chains) = (&heaps, &ledger, &chains);
        let hs: Vec<_> = (0..WORKERS)
            .map(|t| {
                s.spawn(move || {
                    let next = (t + 1) % HEAPS;
                    ledger.x_tree(&heaps[t], x_anchors(&heaps[next], next, &chains[next]))
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().expect("造树 panic")).collect()
    });
    assert_eq!(dropped(), 0);
    // 3. 链根交给另一个线程释放，同时全部堆并行回收；链头到第一个锚点之前的 X_OFFSET 个节点死掉
    collect_all(&mut heaps, chains);
    collect_all(&mut heaps, Vec::<()>::new());
    assert_eq!(dropped(), HEAPS * X_OFFSET);
    for (h, tree) in trees.iter().enumerate() {
        assert_eq!(heaps[h].mutate(|m| m.get(tree).reach()), X_TREE);
        assert_eq!(x_edge_reach(&heaps, h, tree, true), X_CHAIN - X_OFFSET);
        assert_eq!(x_edge_reach(&heaps, h, tree, false), X_CHAIN - X_OFFSET - (X_LEAVES - 1) * X_STEP);
    }
    // 4. 线程 t 剪掉堆 (t + 2) 那棵树的左半边：左子树死掉，它的叶子带的锚随之释放，
    //    下一个堆深链上前一半锚点之间的节点再下一轮死掉
    std::thread::scope(|s| {
        let (heaps, trees) = (&heaps, &trees);
        for t in 0..WORKERS {
            s.spawn(move || {
                let h = (t + 2) % HEAPS;
                heaps[h].mutate(|m| m.get(&trees[h]).left.set(None));
            });
        }
    });
    collect_all(&mut heaps, Vec::<()>::new());
    collect_all(&mut heaps, Vec::<()>::new());
    assert_eq!(dropped(), HEAPS * (X_OFFSET + X_TREE / 2 + X_LEAVES / 2 * X_STEP));
    for (h, tree) in trees.iter().enumerate() {
        assert_eq!(heaps[h].mutate(|m| m.get(tree).reach()), X_TREE - X_TREE / 2);
        assert_eq!(x_edge_reach(&heaps, h, tree, true), X_CHAIN - X_OFFSET - X_LEAVES / 2 * X_STEP);
    }
    // 5. 树根交给另一个线程、在第一轮回收的同时释放：释放可能晚于那个堆的标记，树要到第二轮才死；
    //    树死时锚才释放，链要到第三轮才死
    collect_all(&mut heaps, trees);
    collect_all(&mut heaps, Vec::<()>::new());
    collect_all(&mut heaps, Vec::<()>::new());
    assert_eq!(dropped(), ledger.made.load(Ordering::Relaxed));
    assert!(heaps.iter().all(|h| h.stats().live == 0), "堆里还有存活对象");
}
