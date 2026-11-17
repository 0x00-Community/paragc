//! 跨堆直接引用 [`Far`]。逐条跑：`cargo test --test far -- --exact <名字>`
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use paragc::{Far, Gc, Heap, Root, Slot, Trace, Tracer, brand};

/// 每个堆的回收线程数（Miri 下减少）
const THREADS: NonZeroUsize = NonZeroUsize::new(if cfg!(miri) { 2 } else { 4 }).unwrap();

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

/// 被别的堆指着的对象
#[derive(Trace)]
struct Item {
    value: u64,
    _counted: Counted,
}

brand!(ItemB = for<'gc> Item);

/// 持有方堆里的对象：直接指着另一个堆的 Item
#[derive(Trace)]
struct Holder<'gc> {
    value: u64,
    far: Far<'gc, ItemB>,
}

brand!(HolderB = for<'gc> Holder<'gc>);

/// 环上的一环：`out` 指向本堆的 Hop，Hop 经 Far 指向下一个堆的 Link（Far 建好后不变，所以要先建 Link 再补 Hop）
#[derive(Trace)]
struct Link<'gc> {
    heap: usize,
    _counted: Counted,
    out: Slot<'gc, Hop<'gc>>,
}

#[derive(Trace)]
struct Hop<'gc> {
    far: Far<'gc, LinkB>,
}

brand!(LinkB = for<'gc> Link<'gc>);

fn heaps(n: usize) -> Vec<Heap> {
    (0..n).map(|_| Heap::new(THREADS)).collect()
}

/// 建一个 Item，返回它的根。
fn item(heap: &Heap, value: u64, drops: &Arc<AtomicUsize>) -> Root<ItemB> {
    heap.mutate(|m| m.root(m.alloc(Item { value, _counted: Counted(drops.clone()) })))
}

/// 在 `holder` 堆建一个 Holder，直接指着 `target` 堆里 `root` 的对象。
fn hold(holder: &Heap, target: &Heap, root: &Root<ItemB>, value: u64) -> Root<HolderB> {
    holder.mutate(|ma| target.mutate(|mb| ma.root(ma.alloc(Holder { value, far: Far::new(ma, mb, mb.get(root)) }))))
}

/// 经 Holder 的 Far 读目标 Item 的值。
fn far_value(holder: &Heap, target: &Heap, root: &Root<HolderB>) -> u64 {
    holder.mutate(|ma| target.mutate(|mb| ma.get(root).far.get(mb).value))
}

#[test]
fn far_targets_are_pinned_during_evacuation() {
    // 前四分之一的 Item 被 A 直接指着（不能搬），其余只有根（可以搬）。每 KEEP 个留一个，其余死掉 ⇒ 块都很稀疏、都是疏散候选
    const ITEMS: u64 = if cfg!(miri) { 5_600 } else { 40_000 };
    const KEEP: u64 = 40;
    let mut a = Heap::new(THREADS);
    let mut b = Heap::new(THREADS);
    let drops = Arc::new(AtomicUsize::new(0));
    let roots: Vec<(u64, Root<ItemB>)> = b.mutate(|m| {
        let mut kept = Vec::new();
        for value in 0..ITEMS {
            let gc = m.alloc(Item { value, _counted: Counted(drops.clone()) });
            if value % KEEP == 0 {
                kept.push((value, m.root(gc)));
            }
        }
        kept
    });
    let held: Vec<(u64, Root<HolderB>)> = roots.iter().filter(|(v, _)| *v < ITEMS / 4).map(|(v, r)| (*v, hold(&a, &b, r, *v))).collect();
    // 第一次：清扫后记下每块的存活量；第二次：挑稀疏块疏散
    b.collect();
    b.collect();
    assert!(b.stats().evacuated_blocks > 0, "只有根的那些块应该被疏散");
    a.collect();
    b.mutate(|mb| {
        a.mutate(|ma| {
            for (value, h) in &held {
                let via_far = ma.get(h).far.get(mb);
                assert_eq!(via_far.value, *value);
                let root = &roots.iter().find(|(v, _)| v == value).expect("同一个值").1;
                // 根会跟着搬走的对象改地址；被 Far 指着的必须原地不动，两条路径是同一个对象
                assert!(Gc::ptr_eq(mb.get(root), via_far), "被 Far 指着的对象被搬走了");
            }
        });
    });
    for (value, r) in &roots {
        assert_eq!(b.mutate(|m| m.get(r).value), *value);
    }
    let dead = usize::try_from(ITEMS - ITEMS.div_ceil(KEEP)).unwrap();
    assert_eq!(drops.load(Ordering::Relaxed), dead);
}

#[test]
fn far_keeps_target_until_holder_dies() {
    let mut a = Heap::new(THREADS);
    let mut b = Heap::new(THREADS);
    let drops = Arc::new(AtomicUsize::new(0));
    let target = item(&b, 7, &drops);
    let holder = hold(&a, &b, &target, 1);
    drop(target);
    // 目标的根没了，但 A 里的 Far 还指着它：B 单独回收（完整、年轻）都不能收
    b.collect();
    b.collect_young();
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    assert_eq!(far_value(&a, &b, &holder), 7);
    // 持有方死了：A 回收时析构 Holder、还掉入口；之后 B 回收才收掉目标
    drop(holder);
    b.collect();
    assert_eq!(drops.load(Ordering::Relaxed), 0, "A 还没清扫到 Holder，入口还在");
    a.collect();
    b.collect();
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[test]
fn far_survives_holder_heap_drop_order() {
    // 目标堆先析构：Holder 之后析构时还槽，根表靠 Arc 还在
    let a = Heap::new(THREADS);
    let b = Heap::new(THREADS);
    let drops = Arc::new(AtomicUsize::new(0));
    let target = item(&b, 3, &drops);
    let holder = hold(&a, &b, &target, 1);
    drop(target);
    drop(b);
    assert_eq!(drops.load(Ordering::Relaxed), 1, "目标堆析构时全部对象析构");
    drop(holder);
    drop(a);
}

/// 建一个环：`heaps` 里依次每个堆一个 Link，第 i 个经 Hop 直接指向第 i+1 个，最后一个指回第一个。返回第一个 Link 的根。
fn ring(heaps: &[Heap], drops: &Arc<AtomicUsize>) -> Root<LinkB> {
    let links: Vec<Root<LinkB>> = heaps.iter().enumerate().map(|(i, h)| h.mutate(|m| m.root(m.alloc(Link { heap: i, _counted: Counted(drops.clone()), out: Slot::default() })))).collect();
    for i in 0..heaps.len() {
        let j = (i + 1) % heaps.len();
        heaps[i].mutate(|ma| {
            heaps[j].mutate(|mb| {
                let hop = ma.alloc(Hop { far: Far::new(ma, mb, mb.get(&links[j])) });
                ma.get(&links[i]).out.set(ma, Some(hop));
            });
        });
    }
    links.into_iter().next().expect("至少一个堆")
}

/// 从第一个 Link 出发沿环走一圈，返回经过的堆号。
fn walk(heaps: &[Heap], first: &Root<LinkB>) -> Vec<usize> {
    fn step<'a>(heaps: &[Heap], at: usize, link: Gc<'a, Link<'a>>, seen: &mut Vec<usize>) {
        seen.push(link.heap);
        let next = (at + 1) % heaps.len();
        if next == 0 {
            return;
        }
        let hop = link.out.get().expect("环已连好");
        heaps[next].mutate(|mb| step(heaps, next, hop.far.get(mb), seen));
    }
    let mut seen = Vec::new();
    heaps[0].mutate(|m| step(heaps, 0, m.get(first), &mut seen));
    seen
}

#[test]
fn cross_heap_cycle_needs_group_collection() {
    let mut hs = heaps(2);
    let drops = Arc::new(AtomicUsize::new(0));
    let first = ring(&hs, &drops);
    assert_eq!(walk(&hs, &first), [0, 1]);
    drop(first);
    // 各自回收：彼此的入口都是根，收不掉
    for _ in 0..2 {
        for h in &mut hs {
            h.collect();
        }
    }
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    let mut group: Vec<&mut Heap> = hs.iter_mut().collect();
    Heap::collect_group(&mut group);
    assert_eq!(drops.load(Ordering::Relaxed), 2);
    // 被清空的入口与之后析构的 Far 都不碰已回收的对象
    for h in &mut hs {
        h.collect();
    }
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

#[test]
fn rings_over_eight_heaps() {
    const RINGS: usize = if cfg!(miri) { 2 } else { 64 };
    let mut hs = heaps(8);
    let drops = Arc::new(AtomicUsize::new(0));
    let rings: Vec<Root<LinkB>> = (0..RINGS).map(|_| ring(&hs, &drops)).collect();
    // 留一半
    let kept: Vec<Root<LinkB>> = rings.into_iter().step_by(2).collect();
    let dead = (RINGS - kept.len()) * hs.len();
    // 只把前 4 个堆放进一组：环跨 8 个堆，组外持有的入口是根 ⇒ 一个都不收
    {
        let mut group: Vec<&mut Heap> = hs.iter_mut().take(4).collect();
        Heap::collect_group(&mut group);
    }
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    let mut group: Vec<&mut Heap> = hs.iter_mut().collect();
    Heap::collect_group(&mut group);
    assert_eq!(drops.load(Ordering::Relaxed), dead);
    for first in &kept {
        assert_eq!(walk(&hs, first), (0..8).collect::<Vec<_>>());
    }
    // 留下的环在单独回收、年轻回收、再一起回收之后依然完整
    for h in &mut hs {
        h.collect();
        h.collect_young();
    }
    let mut group: Vec<&mut Heap> = hs.iter_mut().collect();
    Heap::collect_group(&mut group);
    for first in &kept {
        assert_eq!(walk(&hs, first), (0..8).collect::<Vec<_>>());
    }
    drop(kept);
    let mut group: Vec<&mut Heap> = hs.iter_mut().collect();
    Heap::collect_group(&mut group);
    assert_eq!(drops.load(Ordering::Relaxed), RINGS * 8);
}

/// 每个堆一块板：一排槽，每个槽放一个 Holder（直接指着别的堆的 Item）
#[derive(Trace)]
struct Board<'gc> {
    cells: Vec<Slot<'gc, Holder<'gc>>>,
}

brand!(BoardB = for<'gc> Board<'gc>);

/// 简单的线性同余随机数（测试可复现）
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        usize::try_from((self.0 >> 33) % n as u64).unwrap()
    }
}

#[test]
fn eight_threads_relink_far_holders() {
    const HEAPS: usize = if cfg!(miri) { 4 } else { 8 };
    const CELLS: usize = if cfg!(miri) { 4 } else { 64 };
    const ROUNDS: usize = if cfg!(miri) { 3 } else { 12 };
    const STEPS: usize = if cfg!(miri) { 2 } else { 400 };
    let mut hs = heaps(HEAPS);
    let drops = Arc::new(AtomicUsize::new(0));
    let made = AtomicUsize::new(0);
    let boards: Vec<Root<BoardB>> = hs.iter().map(|h| h.mutate(|m| m.root(m.alloc(Board { cells: (0..CELLS).map(|_| Slot::default()).collect() })))).collect();
    for round in 0..ROUNDS {
        std::thread::scope(|s| {
            for t in 0..HEAPS {
                let (hs, boards, drops, made) = (&hs, &boards, &drops, &made);
                s.spawn(move || {
                    let mut rng = Rng((round * HEAPS + t) as u64 + 1);
                    for step in 0..STEPS {
                        let i = rng.below(HEAPS);
                        let j = (i + 1 + rng.below(HEAPS - 1)) % HEAPS;
                        let cell = rng.below(CELLS);
                        // Holder 的值 = Item 的值：读回来对得上说明 Far 指着的还是原来那个对象
                        let value = ((round * HEAPS + t) * STEPS + step) as u64;
                        hs[i].mutate(|ma| {
                            hs[j].mutate(|mb| {
                                let it = mb.alloc(Item { value, _counted: Counted(drops.clone()) });
                                made.fetch_add(1, Ordering::Relaxed);
                                let h = ma.alloc(Holder { value, far: Far::new(ma, mb, it) });
                                ma.get(&boards[i]).cells[cell].set(ma, Some(h));
                            });
                        });
                        // 顺手读一个别处的格子
                        let (k, c) = (rng.below(HEAPS), rng.below(CELLS));
                        hs[k].mutate(|mk| {
                            if let Some(h) = mk.get(&boards[k]).cells[c].get() {
                                let (_, target) = hs.iter().enumerate().find(|&(x, heap)| x != k && h.far.points_into(heap)).expect("Far 指着某个别的堆");
                                target.mutate(|mt| assert_eq!(h.far.get(mt).value, h.value));
                            }
                        });
                    }
                });
            }
            // 并发标记与清扫和 mutator 一起跑（标记期间新建的 Far 也要保住目标）
            s.spawn(|| {
                for h in &hs {
                    h.mark_concurrently();
                }
            });
        });
        match round % 3 {
            0 => hs.iter_mut().for_each(Heap::collect),
            1 => hs.iter_mut().for_each(Heap::collect_young),
            _ => {
                let mut group: Vec<&mut Heap> = hs.iter_mut().collect();
                Heap::collect_group(&mut group);
            }
        }
    }
    drop(boards);
    // 板没了：持有方先回收（析构 Holder、还入口），再一起回收一次收尾
    hs.iter_mut().for_each(Heap::collect);
    let mut group: Vec<&mut Heap> = hs.iter_mut().collect();
    Heap::collect_group(&mut group);
    assert_eq!(drops.load(Ordering::Relaxed), made.load(Ordering::Relaxed), "所有 Item 都应该被回收（没有漏掉的入口）");
}
