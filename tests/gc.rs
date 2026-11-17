//! 逐条跑：`cargo test --test gc -- --exact <名字>`
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use paragc::{Gc, Heap, Root, Slot, Trace, Tracer, YoungEvacuation, brand};

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

/// 每个堆的回收线程数；Miri 下空闲工人的退避自旋很费解释时间，减到 2（仍有并行标记与线程池）
const THREADS: NonZeroUsize = NonZeroUsize::new(if cfg!(miri) { 2 } else { 4 }).unwrap();
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
const ITEMS: u64 = if cfg!(miri) { 6 } else { 4_000 };
/// Miri 下也要保证每个堆每轮至少 16 个大对象（够开 2 个标记工人）
const BALLAST_EVERY: u64 = if cfg!(miri) { 2 } else { 10 };
const CROSS_ROUNDS: usize = if cfg!(miri) { 1 } else { 8 };
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
                    b.owner.set(m, Some(item));
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
    heap.mutate(|m| m.get(&root).right.set(m, None));
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
        cur.left.set(m, None);
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
const X_STEP: usize = if cfg!(miri) { 1 } else { 4 };
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
                heaps[h].mutate(|m| m.get(&trees[h]).left.set(m, None));
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

/// 并发标记测试的节点：链上的下一个是可改的槽。
#[derive(Trace)]
struct Link<'gc> {
    next: Slot<'gc, Link<'gc>>,
    tag: Counted,
}

/// 一排槽，每个槽挂一条链。
#[derive(Trace)]
struct Board<'gc> {
    cells: Vec<Slot<'gc, Link<'gc>>>,
}

brand!(BoardB = for<'gc> Board<'gc>);

/// 槽数是 MOVERS 的倍数（每个线程分到同样多的槽）
const CELLS: usize = if cfg!(miri) { 4 } else { 16 };
const MOVERS: usize = if cfg!(miri) { 2 } else { 4 };
/// 每个 mutator 每轮做几次 mutate、每次做几步
const MUTATES: usize = if cfg!(miri) { 6 } else { 400 };
const STEPS: usize = if cfg!(miri) { 4 } else { 32 };
const MARK_ROUNDS: usize = if cfg!(miri) { 2 } else { 6 };

/// 每个线程自己的伪随机数（xorshift）。
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        usize::try_from(self.0 % u64::try_from(n).expect("n 不大")).expect("比 n 小")
    }
}

/// 造了几个、扔了几个（扔 = 从链上摘下来不再挂回去）。
#[derive(Default)]
struct Moves {
    ledger: Ledger,
    discarded: AtomicUsize,
    /// 落在并发标记期间的 mutate 次数（确认真的和标记重叠了）
    overlapped: AtomicUsize,
    /// 落在并发清扫期间的 mutate 次数
    overlapped_sweep: AtomicUsize,
}

impl Moves {
    /// 一步：随机地 挪（从自己的槽摘下链头、在栈上拿着、再挂到任意槽）/ 新建 / 扔。
    /// 槽 i 只归线程 i % MOVERS 摘（多生产者单消费者的 Treiber 栈没有 ABA），挂谁都能挂。
    fn step<'gc>(&self, m: &paragc::Mutator<'gc>, board: &Board<'gc>, me: usize, rng: &mut Rng) {
        let (a, b) = (rng.below(CELLS / MOVERS) * MOVERS + me, rng.below(CELLS));
        match rng.below(4) {
            0 | 1 => {
                if let Some(x) = Self::pop(m, &board.cells[a]) {
                    Self::push(m, &board.cells[b], x);
                }
            }
            2 => {
                let x = m.alloc(Link { next: Slot::default(), tag: self.ledger.tag() });
                Self::push(m, &board.cells[b], x);
            }
            _ => {
                if Self::pop(m, &board.cells[a]).is_some() {
                    self.discarded.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// 摘下链头（只有这个槽的主人摘；CAS 只和挂的线程抢）；摘下来的节点此后只在栈上。
    fn pop<'gc>(m: &paragc::Mutator<'gc>, cell: &Slot<'gc, Link<'gc>>) -> Option<Gc<'gc, Link<'gc>>> {
        loop {
            let x = cell.get()?;
            if cell.compare_exchange(m, Some(x), x.next.get()).is_ok() {
                return Some(x);
            }
        }
    }

    /// 挂到链头（CAS）。
    fn push<'gc>(m: &paragc::Mutator<'gc>, cell: &Slot<'gc, Link<'gc>>, x: Gc<'gc, Link<'gc>>) {
        loop {
            let head = cell.get();
            x.next.set(m, head);
            if cell.compare_exchange(m, head, Some(x)).is_ok() {
                return;
            }
        }
    }

    /// 全部链上的节点数（显式栈）。
    fn reachable(heap: &Heap, board: &Root<BoardB>) -> usize {
        heap.mutate(|m| {
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
    /// 一轮：MOVERS 个 mutator 不停地挪 / 新建 / 扔（每次 mutate 做 STEPS 步），做满 MUTATES 次后、并且直到
    /// 后台的 `background` 做完才停；后台线程等 mutator 做了一阵才开始（保证二者重叠）。
    fn round(&self, heap: &Heap, board: &Root<BoardB>, round: usize, background: impl FnOnce(&Heap) + Send) {
        let started = std::sync::atomic::AtomicBool::new(false);
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            let (started, done) = (&started, &done);
            for t in 0..MOVERS {
                s.spawn(move || {
                    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ u64::try_from((round << 8) | t).expect("小"));
                    let mut tlab = paragc::Tlab::default();
                    let mut i = 0;
                    while i < MUTATES || !done.load(Ordering::Acquire) {
                        if i == MUTATES / 4 {
                            started.store(true, Ordering::Release);
                        }
                        i += 1;
                        heap.mutate_in(&mut tlab, |m| {
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

    /// 每轮之后：没析构还挂在链上的节点，链上节点数恰好 = 造的 − 扔的。
    fn check(&self, heap: &Heap, board: &Root<BoardB>, round: usize) {
        let (dropped, discarded) = (self.ledger.dropped.load(Ordering::Relaxed), self.discarded.load(Ordering::Relaxed));
        assert!(dropped <= discarded, "第 {round} 轮析构了还挂在链上的节点：析构 {dropped} > 扔掉 {discarded}");
        let live = self.ledger.made.load(Ordering::Relaxed) - discarded;
        assert_eq!(Self::reachable(heap, board), live, "第 {round} 轮链上丢了节点");
    }

    /// 收尾：再完整回收一次清掉浮动垃圾，析构数恰好 = 扔掉的；放掉板子后全部析构。
    fn finish(&self, mut heap: Heap, board: Root<BoardB>) {
        heap.collect();
        assert_eq!(self.ledger.dropped.load(Ordering::Relaxed), self.discarded.load(Ordering::Relaxed));
        drop(board);
        heap.collect();
        assert_eq!(self.ledger.dropped.load(Ordering::Relaxed), self.ledger.made.load(Ordering::Relaxed));
    }
}

fn board(heap: &Heap) -> Root<BoardB> {
    heap.mutate(|m| m.root(m.alloc(Board { cells: (0..CELLS).map(|_| Slot::default()).collect() })))
}

/// 并发标记期间，多个 mutator 不停地把节点从一个槽摘下（只剩栈上引用）再挂到另一个槽、新建、扔掉；
/// 另一个线程同时并发标记。每轮结束回收后：链上节点数 = 造的 − 扔的（初始快照没漏掉被挪动的节点）；
/// 最后再完整回收一次：析构数恰好 = 扔掉的个数。
#[test]
fn concurrent_marking_while_relinking() {
    let moves = Moves::default();
    let mut heap = Heap::new(THREADS);
    let board = board(&heap);
    for round in 0..MARK_ROUNDS {
        moves.round(&heap, &board, round, |h| assert!(h.mark_concurrently(), "这一轮还没有在标记"));
        heap.collect();
        moves.check(&heap, &board, round);
    }
    assert!(moves.overlapped.load(Ordering::Relaxed) > 0, "mutate 一次也没落在并发标记期间");
    moves.finish(heap, board);
}

/// 并发标记 → 标记收尾（停顿）→ 并发清扫：清扫期间 mutator 照常挪 / 新建 / 扔，要块时顺手扫（惰性清扫），
/// 另一个线程同时 `sweep_concurrently`。每轮扫完后链上节点数恰好、没有析构还挂着的节点；最后析构数恰好。
#[test]
fn concurrent_sweeping_while_relinking() {
    let moves = Moves::default();
    let mut heap = Heap::new(THREADS);
    let board = board(&heap);
    for round in 0..MARK_ROUNDS {
        moves.round(&heap, &board, round, |h| assert!(h.mark_concurrently(), "这一轮还没有在标记"));
        heap.finish_marking();
        moves.round(&heap, &board, round, Heap::sweep_concurrently);
        assert!(!heap.is_sweeping());
        moves.check(&heap, &board, round);
    }
    assert!(moves.overlapped.load(Ordering::Relaxed) > 0, "mutate 一次也没落在并发标记期间");
    assert!(moves.overlapped_sweep.load(Ordering::Relaxed) > 0, "mutate 一次也没落在并发清扫期间");
    moves.finish(heap, board);
}

/// 疏散测试的元素：只有值和析构计数（没有边）。
#[derive(Trace)]
struct Cell8 {
    value: u64,
    tag: Counted,
}

/// 根：留下来的元素。
#[derive(Trace)]
struct Keep<'gc> {
    items: Vec<Gc<'gc, Cell8>>,
}

brand!(KeepB = for<'gc> Keep<'gc>);

/// 分配大量元素、只留 1/16：第一次回收后各块只剩 ~6% 存活（稀疏），第二次回收应把它们疏散走：
/// 块数下降、留下的元素值不变且地址变了、析构数恰好等于扔掉的个数。
#[test]
fn evacuation_compacts_sparse_blocks() {
    const ITEMS: u64 = if cfg!(miri) { 4_000 } else { 64_000 };
    const KEEP_EVERY: u64 = 16;
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new(THREADS);
    let keep: Root<KeepB> = heap.mutate(|m| {
        let mut items = Vec::new();
        for value in 0..ITEMS {
            let x = m.alloc(Cell8 { value, tag: Counted(drops.clone()) });
            if value.is_multiple_of(KEEP_EVERY) {
                items.push(x);
            }
        }
        m.root(m.alloc(Keep { items }))
    });
    let kept = ITEMS / KEEP_EVERY;
    let discarded = usize::try_from(ITEMS - kept).expect("小");
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), discarded);
    let before_blocks = heap.stats().blocks;
    let before: Vec<usize> = heap.mutate(|m| m.get(&keep).items.iter().map(|x| x.as_ptr().addr()).collect());
    heap.collect();
    let stats = heap.stats();
    assert!(stats.evacuated_blocks > 0, "稀疏块没有被疏散：{stats:?}");
    assert!(stats.blocks < before_blocks, "疏散后块数没降：{before_blocks} ⇒ {}", stats.blocks);
    let after: Vec<(usize, u64)> = heap.mutate(|m| m.get(&keep).items.iter().map(|x| (x.as_ptr().addr(), x.value)).collect());
    assert!(after.iter().enumerate().all(|(i, &(_, v))| v == u64::try_from(i).expect("小") * KEEP_EVERY), "搬过的元素值不对");
    let moved = before.iter().zip(&after).filter(|(b, (a, _))| *b != a).count();
    assert!(moved > 0, "没有元素换地址");
    assert_eq!(drops.load(Ordering::Relaxed), discarded, "搬走的旧副本被析构了");
    drop(keep);
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), usize::try_from(ITEMS).expect("小"));
}

/// 多父共享子节点：父节点各有两个指向共享子节点的槽。
#[derive(Trace)]
struct Parent<'gc> {
    a: Slot<'gc, Cell8>,
    b: Gc<'gc, Cell8>,
    tag: Counted,
}

#[derive(Trace)]
struct Family<'gc> {
    parents: Vec<Gc<'gc, Parent<'gc>>>,
}

brand!(FamilyB = for<'gc> Family<'gc>);

/// 很多父节点指向同一小撮子节点（槽与普通字段各一条），父与子都在稀疏块里：疏散时多个回收线程会同时追到同一个子节点、
/// 争着认领复制。之后每个父节点的两条边必须指向同一个（新）子节点，值不变，子节点只被复制一次（不重复析构、不丢）。
#[test]
fn evacuation_with_shared_children() {
    const PARENTS: u64 = if cfg!(miri) { 1_500 } else { 40_000 };
    const CHILDREN: u64 = 64;
    const KEEP_EVERY: u64 = 8;
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new(THREADS);
    let family: Root<FamilyB> = heap.mutate(|m| {
        let children: Vec<_> = (0..CHILDREN).map(|value| m.alloc(Cell8 { value, tag: Counted(drops.clone()) })).collect();
        let mut parents = Vec::new();
        for i in 0..PARENTS {
            // 留下来的父节点（i 是 KEEP_EVERY 的倍数）依次指向第 0、1、2 … 个子节点，全部子节点都被引用
            let c = children[usize::try_from((i / KEEP_EVERY) % CHILDREN).expect("小")];
            let p = m.alloc(Parent { a: c.into(), b: c, tag: Counted(drops.clone()) });
            // 垃圾穿插在中间，让块变稀疏
            m.alloc(Cell8 { value: u64::MAX, tag: Counted(drops.clone()) });
            if i.is_multiple_of(KEEP_EVERY) {
                parents.push(p);
            }
        }
        m.root(m.alloc(Family { parents }))
    });
    // 留下的父节点：i = 0, KEEP_EVERY, 2·KEEP_EVERY … 共 ⌈PARENTS / KEEP_EVERY⌉ 个
    let garbage = usize::try_from(PARENTS * 2 - PARENTS.div_ceil(KEEP_EVERY)).expect("小");
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), garbage);
    heap.collect();
    assert!(heap.stats().evacuated_blocks > 0, "没有疏散：{:?}", heap.stats());
    heap.mutate(|m| {
        for (i, p) in m.get(&family).parents.iter().enumerate() {
            let a = p.a.get().expect("槽不空");
            assert!(Gc::ptr_eq(a, p.b), "同一个子节点的两条边指向不同地址");
            assert_eq!(a.value, u64::try_from(i).expect("小") % CHILDREN);
        }
    });
    assert_eq!(drops.load(Ordering::Relaxed), garbage, "疏散时多析构了");
    drop(family);
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), usize::try_from(PARENTS * 2 + CHILDREN).expect("小"));
}

/// 年轻回收的精确账目：老节点（经过一次完整回收）挂在板子的 0 号、2 号槽；然后新建年轻节点，三种方式挂上：
/// 直接挂到老板子的 1 号槽（老对象的槽 ⇒ 记忆集）、挂到 0 号槽链头老节点的 next（老对象的槽）、挂到另一个年轻节点底下（年轻 → 年轻）；
/// 其余年轻节点扔掉。再从 2 号槽摘掉一些老节点（老垃圾）。年轻回收应恰好析构扔掉的年轻节点（追踪），
/// 以及扔掉的老节点（引用计数归零），链上节点一个不少；之后的完整回收不再多析构。
#[test]
fn young_collection_frees_only_young_garbage() {
    const OLD: usize = if cfg!(miri) { 60 } else { 2_000 };
    const YOUNG: usize = if cfg!(miri) { 120 } else { 20_000 };
    const OLD_DISCARD: usize = OLD / 4;
    let moves = Moves::default();
    let mut heap = Heap::new(THREADS);
    let board = board(&heap);
    // 老节点：0 号、2 号槽各一条链，完整回收后都是老的
    heap.mutate(|m| {
        let b = m.get(&board);
        for i in 0..OLD {
            Moves::push(m, &b.cells[if i % 2 == 0 { 0 } else { 2 }], m.alloc(Link { next: Slot::default(), tag: moves.ledger.tag() }));
        }
    });
    heap.collect();
    // 年轻节点：每 4 个里 1 个挂到 1 号槽、1 个插到 0 号槽链头老节点之后、1 个插到 1 号槽链头（年轻）之后，1 个扔掉
    let young_garbage = heap.mutate(|m| {
        let b = m.get(&board);
        let old_head = b.cells[0].get().expect("老链不空");
        let mut garbage = 0;
        for i in 0..YOUNG {
            let x = m.alloc(Link { next: Slot::default(), tag: moves.ledger.tag() });
            match i % 4 {
                0 => Moves::push(m, &b.cells[1], x),
                1 => {
                    x.next.set(m, old_head.next.get());
                    old_head.next.set(m, Some(x));
                }
                2 => {
                    let host = b.cells[1].get().expect("刚挂过");
                    x.next.set(m, host.next.get());
                    host.next.set(m, Some(x));
                }
                _ => garbage += 1,
            }
        }
        garbage
    });
    // 老垃圾：从 2 号槽（全是老节点）摘掉 OLD_DISCARD 个
    heap.mutate(|m| {
        let b = m.get(&board);
        for _ in 0..OLD_DISCARD {
            Moves::pop(m, &b.cells[2]).expect("老链够长");
            moves.discarded.fetch_add(1, Ordering::Relaxed);
        }
    });
    let before = moves.ledger.dropped.load(Ordering::Relaxed);
    heap.collect_young();
    assert_eq!(heap.stats().young_collections, 1);
    assert_eq!(moves.ledger.dropped.load(Ordering::Relaxed) - before, young_garbage + OLD_DISCARD, "年轻回收析构的不是恰好扔掉的年轻节点 + 老节点");
    assert_eq!(Moves::reachable(&heap, &board), OLD - OLD_DISCARD + (YOUNG - young_garbage), "年轻回收后链上丢了节点");
    // 完整回收没有剩下的垃圾
    heap.collect();
    assert_eq!(moves.ledger.dropped.load(Ordering::Relaxed) - before, young_garbage + OLD_DISCARD);
    assert_eq!(Moves::reachable(&heap, &board), OLD - OLD_DISCARD + (YOUNG - young_garbage));
    moves.discarded.fetch_add(young_garbage, Ordering::Relaxed);
    moves.finish(heap, board);
}

/// 多个 mutator 不停地挪 / 新建 / 扔（挪动会把年轻节点挂到老节点的槽上 ⇒ 记忆集），每轮之后回收：
/// 轮流做年轻回收、并发标记后用年轻回收收尾（等于完整回收）、完整回收。每轮：没有析构还挂着的节点、链上节点数恰好。
#[test]
fn young_collections_while_relinking() {
    let moves = Moves::default();
    let mut heap = Heap::new(THREADS);
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

/// 环测试：持有者链上每个持有者挂一个两节点的环。
#[derive(Trace)]
struct Holder<'gc> {
    ring: Slot<'gc, Link<'gc>>,
    next: Slot<'gc, Holder<'gc>>,
    tag: Counted,
}

#[derive(Trace)]
struct Holders<'gc> {
    head: Slot<'gc, Holder<'gc>>,
    plain: Slot<'gc, Link<'gc>>,
}

brand!(HoldersB = for<'gc> Holders<'gc>);

/// 引用计数收不了环：PAIRS 个持有者各挂一个两节点环（a.next = c、c.next = a），另有 PAIRS 个普通节点；
/// 完整回收后都是老的。然后把持有者链和普通链都摘掉：持有者和普通节点计数归零，年轻回收（引用计数）就收掉；
/// 环内互相计数不为 0，年轻回收不析构它们，要等完整回收（追踪）。
#[test]
fn reference_counting_leaves_cycles_to_tracing() {
    const PAIRS: usize = if cfg!(miri) { 20 } else { 500 };
    let moves = Moves::default();
    let mut heap = Heap::new(THREADS);
    let root: Root<HoldersB> = heap.mutate(|m| m.root(m.alloc(Holders { head: Slot::default(), plain: Slot::default() })));
    heap.mutate(|m| {
        let holders = m.get(&root);
        for _ in 0..PAIRS {
            let first = m.alloc(Link { next: Slot::default(), tag: moves.ledger.tag() });
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
    heap.mutate(|m| {
        let holders = m.get(&root);
        holders.head.set(m, None);
        holders.plain.set(m, None);
    });
    heap.collect_young();
    assert_eq!(moves.ledger.dropped.load(Ordering::Relaxed) - before, PAIRS * 2, "引用计数应收掉持有者与普通节点，环留着");
    heap.collect();
    assert_eq!(moves.ledger.dropped.load(Ordering::Relaxed) - before, PAIRS * 4, "完整回收应收掉环");
    drop(root);
}

/// 引用计数的减量移出停顿、在后台与 mutator 并发处理：
/// 老链整体扔掉后 `finish_young`（停顿）返回时还一个都没析构；之后 mutator 照常挪 / 新建 / 扔，另一个线程同时
/// `sweep_concurrently` 连锁释放——不析构还挂着的节点、链上计数恰好；下一次停顿重算行，空出的行被复用（块数不涨）。
#[test]
fn background_release_while_mutating() {
    const CHAIN: usize = if cfg!(miri) { 60 } else { 20_000 };
    let moves = Moves::default();
    let mut heap = Heap::new(THREADS);
    let board = board(&heap);
    let spare: Root<BoardB> = heap.mutate(|m| m.root(m.alloc(Board { cells: vec![Slot::default()] })));
    // 一条长的老链挂在另一个板子上，完整回收后计数可信
    heap.mutate(|m| {
        let cell = &m.get(&spare).cells[0];
        for _ in 0..CHAIN {
            Moves::push(m, cell, m.alloc(Link { next: Slot::default(), tag: moves.ledger.tag() }));
        }
    });
    heap.collect();
    // 整条扔掉：只改了板子上的一个槽
    heap.mutate(|m| m.get(&spare).cells[0].set(m, None));
    moves.discarded.fetch_add(CHAIN, Ordering::Relaxed);
    let before = moves.ledger.dropped.load(Ordering::Relaxed);
    heap.finish_young();
    assert_eq!(moves.ledger.dropped.load(Ordering::Relaxed), before, "停顿里不该做减量");
    // mutator 跑着，后台连锁释放
    moves.round(&heap, &board, 0, Heap::sweep_concurrently);
    assert!(moves.ledger.dropped.load(Ordering::Relaxed) - before >= CHAIN, "后台没把扔掉的链释放完");
    moves.check(&heap, &board, 0);
    // 下一次停顿重算行；再分配同样多的节点，空出的行被复用（块数基本不涨）
    heap.collect_young();
    let blocks_after_release = heap.stats().blocks;
    heap.mutate(|m| {
        let cell = &m.get(&spare).cells[0];
        for _ in 0..CHAIN {
            Moves::push(m, cell, m.alloc(Link { next: Slot::default(), tag: moves.ledger.tag() }));
        }
    });
    heap.collect_young();
    let slack = 16;
    assert!(heap.stats().blocks <= blocks_after_release + slack, "释放的行没被复用：{} ⇒ {}", blocks_after_release, heap.stats().blocks);
    drop(spare);
    heap.collect();
    moves.discarded.fetch_add(CHAIN, Ordering::Relaxed);
    moves.finish(heap, board);
}

/// 并发标记顺带重新计数：一轮并发标记（期间 mutator 挪 / 新建 / 扔，黑分配的节点把别的节点挂到自己底下）收尾后，
/// 引用计数立刻可信——整条扔掉的老链在下一次年轻回收就按计数释放，不用等停顿式完整回收；之后几轮年轻回收里按计数释放
/// 也不碰还挂着的节点（漏算黑对象的边或标记期间被改写的槽，这里会析构活节点）。
#[test]
fn concurrent_marking_recounts() {
    const CHAIN: usize = if cfg!(miri) { 40 } else { 10_000 };
    let moves = Moves::default();
    // 老链单独记账（不在主板子上，不进 moves 的账）
    let chain = Ledger::default();
    let mut heap = Heap::new(THREADS);
    let board = board(&heap);
    let spare: Root<BoardB> = heap.mutate(|m| m.root(m.alloc(Board { cells: vec![Slot::default()] })));
    heap.mutate(|m| {
        let cell = &m.get(&spare).cells[0];
        for _ in 0..CHAIN {
            Moves::push(m, cell, m.alloc(Link { next: Slot::default(), tag: chain.tag() }));
        }
    });
    // 一轮并发标记（不经过停顿式完整回收），收尾
    moves.round(&heap, &board, 0, |h| assert!(h.mark_concurrently()));
    heap.collect();
    moves.check(&heap, &board, 0);
    assert_eq!(chain.dropped.load(Ordering::Relaxed), 0);
    // 计数应已可信：扔掉整条链，年轻回收按计数释放
    heap.mutate(|m| m.get(&spare).cells[0].set(m, None));
    heap.collect_young();
    assert_eq!(chain.dropped.load(Ordering::Relaxed), CHAIN, "并发标记之后计数不可信：扔掉的链没在年轻回收里释放");
    // 再几轮：mutator 挪动、年轻回收按计数释放，不碰还挂着的
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
    // 布局：一个指针大小，Option 也是（repr(transparent) + NonNull）
    assert_eq!(size_of::<Gc<'static, u64>>(), size_of::<*const u64>());
    assert_eq!(size_of::<Option<Gc<'static, u64>>>(), size_of::<*const u64>());
    let heap = Heap::new(THREADS);
    heap.mutate(|m| {
        let a = m.alloc(Node { value: 7, next: None });
        let b = m.alloc(Node { value: 8, next: Some(a) });
        // 拆成裸地址再取回（同一次 mutate 里）
        let raw = b.as_ptr();
        // SAFETY: raw 是这次 mutate 里 b 的数据指针，b 活着
        let back: Gc<'_, Node<'_>> = unsafe { Gc::from_raw(raw) };
        assert!(Gc::ptr_eq(back, b));
        assert_eq!(back.value, 8);
        assert_eq!(back.next.map(|n| n.value), Some(7));
    });
}

/// 年轻对象里的槽写屏障不记（年轻回收从对象本身追到）；被年轻回收晋升后就要记：
/// 第一次完整回收之前（引用计数还不可信，晋升不重算计数）晋升的持有者，之后写进去的年轻叶子在下一次年轻回收后还活着。
#[test]
fn promoted_slots_remember_young_values() {
    let heap = &mut Heap::new(THREADS);
    let drops = Arc::new(AtomicUsize::new(0));
    let holder: Root<TreeB> = heap.mutate(|m| m.root(m.alloc(Tree::Branch { left: Slot::default(), right: Slot::default() })));
    heap.collect_young();
    heap.mutate(|m| {
        let Tree::Branch { left, .. } = &*m.get(&holder) else { unreachable!() };
        left.set(m, Some(m.alloc(Tree::Leaf(Counted(drops.clone())))));
    });
    heap.collect_young();
    assert_eq!(drops.load(Ordering::Relaxed), 0, "晋升的持有者指着的年轻叶子被回收了");
    heap.mutate(|m| {
        let Tree::Branch { left, .. } = &*m.get(&holder) else { unreachable!() };
        assert!(matches!(left.get().as_deref(), Some(Tree::Leaf(_))));
    });
    drop(holder);
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

/// 析构时记下是不是在建堆的线程上（没有 Gc，可以有 Drop）。
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

// SAFETY: 没有 Gc
unsafe impl Trace for Witness {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut Tracer<'_>) {}
}

#[derive(Trace)]
struct Twig<'gc> {
    left: Slot<'gc, Twig<'gc>>,
    right: Slot<'gc, Twig<'gc>>,
    witness: Witness,
}

brand!(TwigB = for<'gc> Twig<'gc>);

/// 关掉并行析构：引用计数的后台释放、并行清扫（年轻 / 完整）、堆析构，死对象都只在调用方线程上析构，一个不少。
#[test]
fn serial_drop_stays_on_the_caller() {
    const DEPTH: u32 = if cfg!(miri) { 6 } else { 15 };
    let nodes = (1usize << DEPTH) - 1;
    let ledger = Arc::new((std::thread::current().id(), AtomicUsize::new(0), AtomicUsize::new(0)));
    let mut heap = Heap::new(THREADS);
    heap.set_parallel_drop(false);
    let tree = |heap: &Heap| -> Root<TwigB> {
        heap.mutate(|m| {
            let mut level: Vec<_> = (0..1usize << (DEPTH - 1)).map(|_| m.alloc(Twig { left: Slot::default(), right: Slot::default(), witness: Witness(ledger.clone()) })).collect();
            while level.len() > 1 {
                level = level.chunks(2).map(|pair| m.alloc(Twig { left: pair[0].into(), right: pair[1].into(), witness: Witness(ledger.clone()) })).collect();
            }
            m.root(level[0])
        })
    };
    let holder: Root<TwigB> = heap.mutate(|m| m.root(m.alloc(Twig { left: Slot::default(), right: Slot::default(), witness: Witness(ledger.clone()) })));
    let first = tree(&heap);
    heap.mutate(|m| m.get(&holder).left.set(m, Some(m.get(&first))));
    drop(first);
    heap.collect();
    heap.collect();
    assert_eq!(ledger.1.load(Ordering::Relaxed), 0);
    // 老树摘掉：引用计数归零，年轻回收后的后台释放析构
    heap.mutate(|m| m.get(&holder).left.set(m, None));
    heap.collect_young();
    assert_eq!(ledger.1.load(Ordering::Relaxed), nodes, "引用计数应收掉整棵老树");
    // 年轻垃圾：年轻清扫；再一棵变老后扔掉：完整回收的清扫
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
    assert_eq!(ledger.2.load(Ordering::Relaxed), 0, "有对象在别的线程上析构");
}

/// 一行大（232 字节数据 + 24 字节头 = 256）
#[derive(Trace)]
struct Line {
    fill: [u64; 29],
}

/// 五个正好填满一行（48 字节）
#[derive(Trace)]
struct Small {
    _pad: [u64; 3],
}

/// 三行多一点（584 字节）
#[derive(Trace)]
struct Medium {
    _pad: [u64; 70],
}

#[derive(Trace)]
struct Lines<'gc> {
    kept: Vec<Slot<'gc, Line>>,
}

brand!(LinesB = for<'gc> Lines<'gc>);

/// 溢出分配：块里只剩单行的空洞时，交替分配小对象与中对象（放不进单行空洞），小对象照样填进旧空洞，
/// 中对象另开溢出块——新开的块数只够放中对象（中对象把主游标带走的话，旧空洞全部作废，小对象也要开新块）。
#[test]
fn medium_objects_overflow_without_wasting_holes() {
    const HOLES: usize = if cfg!(miri) { 300 } else { 6000 };
    /// 块的大小（paragc::layout::BLOCK）
    const BLOCK: usize = 32 * 1024;
    let mut heap = Heap::new(THREADS);
    // 连续分配一行大的对象，每三个扔一个：完整回收后每个死的都是单行空洞（块里存活 2/3，不会被疏散）
    let lines: Root<LinesB> = heap.mutate(|m| {
        let all: Vec<_> = (0..3 * HOLES).map(|_| m.alloc(Line { fill: [0; 29] })).collect();
        m.root(m.alloc(Lines { kept: all.iter().enumerate().filter(|(i, _)| i % 3 != 2).map(|(_, &l)| Slot::from(Some(l))).collect() }))
    });
    heap.collect();
    heap.collect();
    let before = heap.stats().blocks;
    heap.mutate(|m| {
        for _ in 0..HOLES {
            for _ in 0..5 {
                m.alloc(Small { _pad: [1; 3] });
            }
            m.alloc(Medium { _pad: [2; 70] });
        }
    });
    let grown = heap.stats().blocks - before;
    let medium_blocks = (HOLES * size_of::<Medium>()).div_ceil(BLOCK * 9 / 10);
    assert!(grown <= medium_blocks + 2, "新开 {grown} 块，只该够放中对象（约 {medium_blocks} 块）：小对象没填进旧空洞");
    drop(lines);
}

/// 持有者（会变老）与它挂着的年轻链。
#[derive(Trace)]
struct Anchor<'gc> {
    head: Slot<'gc, Bead<'gc>>,
}

brand!(AnchorB = for<'gc> Anchor<'gc>);

#[derive(Trace)]
struct Bead<'gc> {
    value: u64,
    next: Option<Gc<'gc, Bead<'gc>>>,
    _counted: Counted,
}

brand!(BeadB = for<'gc> Bead<'gc>);

/// 链上每个节点的（地址, 值）。
fn beads<'gc>(head: Option<Gc<'gc, Bead<'gc>>>) -> Vec<(usize, u64)> {
    let mut out = Vec::new();
    let mut cur = head;
    while let Some(b) = cur {
        out.push((b.as_ptr().addr(), b.value));
        cur = b.next;
    }
    out
}

/// 造一条 `len` 个节点的链（值从 `from` 起），节点之间夹着垃圾（存活对象散在块里）。
fn chain<'gc>(m: &paragc::Mutator<'gc>, drops: &Arc<AtomicUsize>, from: u64, len: u64) -> Gc<'gc, Bead<'gc>> {
    let mut top = None;
    for value in from..from + len {
        m.alloc(Bead { value: u64::MAX, next: None, _counted: Counted(drops.clone()) });
        top = Some(m.alloc(Bead { value, next: top, _counted: Counted(drops.clone()) }));
    }
    top.expect("len > 0")
}

/// 年轻疏散：老持有者的槽挂一条年轻链、根也挂一条，年轻回收后——
/// `Full`：两条链都搬了（地址全变）、值不变、槽与根都指到新地址；
/// `SkipRemembered`：槽直接指着的头原地不动，链上其余的搬了；
/// `Off`：都不动。之后摘掉链，引用计数照样收掉（析构数恰好）。
#[test]
fn young_evacuation_moves_survivors() {
    const LEN: u64 = if cfg!(miri) { 50 } else { 5000 };
    let n = usize::try_from(LEN).expect("链长");
    for mode in [YoungEvacuation::Full, YoungEvacuation::SkipRemembered, YoungEvacuation::Off] {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut heap = Heap::new(THREADS);
        heap.set_young_evacuation(mode);
        let anchor: Root<AnchorB> = heap.mutate(|m| m.root(m.alloc(Anchor { head: Slot::default() })));
        heap.collect();
        heap.collect();
        let (rooted, before_slot, before_root): (Root<BeadB>, _, _) = heap.mutate(|m| {
            let a = m.get(&anchor);
            a.head.set(m, Some(chain(m, &drops, 0, LEN)));
            let r = chain(m, &drops, LEN, LEN);
            (m.root(r), beads(a.head.get()), beads(Some(r)))
        });
        heap.collect_young();
        assert_eq!(drops.load(Ordering::Relaxed), 2 * n, "{mode:?}: 夹着的垃圾应该都析构了");
        let (after_slot, after_root) = heap.mutate(|m| (beads(m.get(&anchor).head.get()), beads(Some(m.get(&rooted)))));
        let values = |v: &[(usize, u64)]| v.iter().map(|&(_, x)| x).collect::<Vec<_>>();
        assert_eq!(values(&after_slot), values(&before_slot), "{mode:?}: 槽挂的链值变了");
        assert_eq!(values(&after_root), values(&before_root), "{mode:?}: 根挂的链值变了");
        let moved = |a: &[(usize, u64)], b: &[(usize, u64)]| a.iter().zip(b).filter(|(x, y)| x.0 != y.0).count();
        let (slot_moved, root_moved) = (moved(&before_slot, &after_slot), moved(&before_root, &after_root));
        match mode {
            YoungEvacuation::Full => {
                assert_eq!((slot_moved, root_moved), (n, n), "Full：全都该搬");
            }
            YoungEvacuation::SkipRemembered => {
                assert_eq!(after_slot[0].0, before_slot[0].0, "SkipRemembered：老对象直接指着的不该搬");
                assert_eq!((slot_moved, root_moved), (n - 1, n), "SkipRemembered：其余的该搬");
            }
            YoungEvacuation::Off => assert_eq!((slot_moved, root_moved), (0, 0), "Off：不该搬"),
        }
        // 摘掉两条链：引用计数（槽那条）与年轻 / 完整回收（根那条）收掉，析构数恰好
        heap.mutate(|m| m.get(&anchor).head.set(m, None));
        drop(rooted);
        heap.collect_young();
        heap.collect();
        assert_eq!(drops.load(Ordering::Relaxed), 4 * n, "{mode:?}: 摘掉的链没收干净");
        drop(anchor);
    }
}

/// `shrink`：空行还给系统后对象完好，之后接着往那些空行里分配（重新缺页、内容是零）也对。
#[test]
fn shrink_keeps_objects_and_reuses_lines() {
    const HOLES: usize = if cfg!(miri) { 100 } else { 3000 };
    let mut heap = Heap::new(THREADS);
    let lines: Root<LinesB> = heap.mutate(|m| {
        let all: Vec<_> = (0..3 * HOLES).map(|i| m.alloc(Line { fill: [i as u64; 29] })).collect();
        m.root(m.alloc(Lines { kept: all.iter().enumerate().filter(|(i, _)| i % 3 == 0).map(|(_, &l)| Slot::from(Some(l))).collect() }))
    });
    heap.shrink();
    let check = |heap: &Heap| heap.mutate(|m| m.get(&lines).kept.iter().enumerate().all(|(k, s)| s.get().is_some_and(|l| l.fill.iter().all(|&x| x == 3 * k as u64))));
    assert!(check(&heap), "shrink 之后存活对象的内容变了");
    let drops = Arc::new(AtomicUsize::new(0));
    let fresh = build_tree(&heap, &drops, if cfg!(miri) { 8 } else { 14 });
    assert!(check(&heap));
    assert_eq!(heap.mutate(|m| m.get(&fresh).count()), (1 << if cfg!(miri) { 8 } else { 14 }) - 1);
    drop(fresh);
    heap.shrink();
    assert!(check(&heap));
}
