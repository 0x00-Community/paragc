//! 弱引用：逐条跑 `cargo test --test weak -- --exact <名字>`
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use paragc::{Far, Gc, Heap, Root, Slot, Trace, Tracer, Weak, YoungEvacuation, brand};

/// 每个堆的回收线程数；Miri 下减到 2
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

#[derive(Trace)]
struct Item {
    value: u64,
    tag: Counted,
}

brand!(ItemB = for<'gc> Item);

/// 强引用一串对象（在一个对象里）
#[derive(Trace)]
struct Keep<'gc> {
    items: Vec<Gc<'gc, Item>>,
}

brand!(KeepB = for<'gc> Keep<'gc>);

/// 一个可改的强引用，外加放在堆对象里的弱引用
#[derive(Trace)]
struct Holder<'gc> {
    item: Slot<'gc, Item>,
    weak: Vec<Weak<ItemB>>,
}

brand!(HolderB = for<'gc> Holder<'gc>);

/// 建 `n` 个对象（值 0..n），留下它们的弱引用；`keep` 为真的挂到一个根上
fn items(heap: &Heap, drops: &Arc<AtomicUsize>, n: u64, keep: impl Fn(u64) -> bool) -> (Root<KeepB>, Vec<Weak<ItemB>>) {
    heap.mutate(|m| {
        let mut kept = Vec::new();
        let mut weak = Vec::new();
        for value in 0..n {
            let x = m.alloc(Item { value, tag: Counted(drops.clone()) });
            weak.push(m.weak::<ItemB>(x));
            if keep(value) {
                kept.push(x);
            }
        }
        (m.root(m.alloc(Keep { items: kept })), weak)
    })
}

/// 每个弱引用现在的值（被清空的是 None）
fn values(heap: &Heap, weak: &[Weak<ItemB>]) -> Vec<Option<u64>> {
    heap.mutate(|m| weak.iter().map(|w| m.upgrade(w).map(|x| x.value)).collect())
}

fn count(n: u64) -> usize {
    usize::try_from(n).expect("小")
}

#[test]
fn weak_does_not_keep_alive() {
    const N: u64 = if cfg!(miri) { 200 } else { 20_000 };
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new(THREADS);
    let (keep, weak) = items(&heap, &drops, N, |v| v % 2 == 0);
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), count(N / 2));
    let got = values(&heap, &weak);
    assert!(got.iter().enumerate().all(|(i, v)| *v == (i % 2 == 0).then_some(i as u64)), "活的取得到、死的清空");
    assert!(weak.iter().enumerate().all(|(i, w)| w.is_cleared() == (i % 2 == 1)));
    drop(keep);
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), count(N));
    assert!(weak.iter().all(Weak::is_cleared));
}

/// 疏散把活对象搬走：弱引用跟着改到新地址，死的清空，旧副本不析构
#[test]
fn weak_follows_evacuation() {
    const N: u64 = if cfg!(miri) { 4_000 } else { 64_000 };
    const KEEP_EVERY: u64 = 16;
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new(THREADS);
    let (keep, weak) = items(&heap, &drops, N, |v| v.is_multiple_of(KEEP_EVERY));
    heap.collect();
    let discarded = count(N - N / KEEP_EVERY);
    assert_eq!(drops.load(Ordering::Relaxed), discarded);
    let addr = |heap: &Heap| -> Vec<usize> { heap.mutate(|m| weak.iter().step_by(count(KEEP_EVERY)).map(|w| m.upgrade(w).expect("活着").as_ptr().addr()).collect()) };
    let before = addr(&heap);
    heap.collect();
    assert!(heap.stats().evacuated_blocks > 0, "稀疏块没有被疏散：{:?}", heap.stats());
    let after = addr(&heap);
    assert!(before.iter().zip(&after).any(|(b, a)| b != a), "没有对象换地址");
    // 弱引用与强引用看到同一个（新）对象
    let strong: Vec<usize> = heap.mutate(|m| m.get(&keep).items.iter().map(|x| x.as_ptr().addr()).collect());
    assert_eq!(after, strong);
    let got = values(&heap, &weak);
    assert!(got.iter().enumerate().all(|(i, v)| *v == (i as u64).is_multiple_of(KEEP_EVERY).then_some(i as u64)));
    assert_eq!(drops.load(Ordering::Relaxed), discarded, "搬走的旧副本被析构了");
    drop(keep);
    heap.collect();
    assert!(weak.iter().all(Weak::is_cleared));
    assert_eq!(drops.load(Ordering::Relaxed), count(N));
}

/// 年轻回收（三种搬法）：没有强引用的年轻对象清空，活着的（可能搬走）取得到原值；老对象不受影响
#[test]
fn young_collection_clears_young_targets() {
    const N: u64 = if cfg!(miri) { 300 } else { 30_000 };
    for mode in [YoungEvacuation::Off, YoungEvacuation::SkipRemembered, YoungEvacuation::Full] {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut heap = Heap::new(THREADS);
        heap.set_young_evacuation(mode);
        let (old_keep, old_weak) = items(&heap, &drops, N, |_| true);
        heap.collect();
        let (keep, weak) = items(&heap, &drops, N, |v| v % 3 == 0);
        heap.collect_young();
        assert_eq!(drops.load(Ordering::Relaxed), count(N - N.div_ceil(3)), "{mode:?}");
        let got = values(&heap, &weak);
        assert!(got.iter().enumerate().all(|(i, v)| *v == (i % 3 == 0).then_some(i as u64)), "{mode:?}");
        assert!(values(&heap, &old_weak).iter().enumerate().all(|(i, v)| *v == Some(i as u64)), "{mode:?}");
        drop((keep, old_keep));
        heap.collect();
        assert!(weak.iter().chain(&old_weak).all(Weak::is_cleared), "{mode:?}");
        assert_eq!(drops.load(Ordering::Relaxed), count(2 * N), "{mode:?}");
    }
}

/// 老对象最后一个强引用没了：没有弱引用的由年轻回收按计数释放；有弱引用的计数粘住、留到完整回收（按计数释放与
/// mutator 并发，来不及先清空弱引用）
#[test]
fn weak_targets_wait_for_tracing() {
    let drops = Arc::new(AtomicUsize::new(0));
    let other = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new(THREADS);
    let holders: [Root<HolderB>; 2] = heap.mutate(|m| {
        [&drops, &other].map(|d| {
            let item = m.alloc(Item { value: 7, tag: Counted(d.clone()) });
            m.root(m.alloc(Holder { item: Some(item).into(), weak: Vec::new() }))
        })
    });
    heap.collect();
    let weak = heap.mutate(|m| m.weak::<ItemB>(m.get(&holders[0]).item.get().expect("挂着")));
    heap.mutate(|m| {
        for h in &holders {
            m.get(h).item.set(m, None);
        }
    });
    heap.collect_young();
    assert_eq!(other.load(Ordering::Relaxed), 1, "没有弱引用的老对象应按计数释放");
    assert_eq!(drops.load(Ordering::Relaxed), 0, "有弱引用的不能按计数释放");
    assert_eq!(values(&heap, std::slice::from_ref(&weak)), [Some(7)]);
    heap.collect();
    assert!(weak.is_cleared());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    drop(holders);
}

/// 并发标记开始时只有弱引用指着的对象：标记期间 upgrade 出来、挂进别的对象，收尾后它必须活着（读屏障）；
/// 没被 upgrade 的照常清空
#[test]
fn upgrade_during_concurrent_marking_keeps_target() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new(THREADS);
    let (keep, weak) = items(&heap, &drops, 2, |_| true);
    let holder: Root<HolderB> = heap.mutate(|m| m.root(m.alloc(Holder { item: Slot::default(), weak: Vec::new() })));
    heap.collect();
    drop(keep);
    assert!(heap.mark_concurrently());
    heap.mutate(|m| {
        assert!(m.is_marking());
        let x = m.upgrade(&weak[0]).expect("还没回收");
        m.get(&holder).item.set(m, Some(x));
    });
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), 1, "只有没被取出的那个死");
    assert_eq!(values(&heap, &weak), [Some(0), None]);
    assert_eq!(heap.mutate(|m| m.get(&holder).item.get().map(|x| x.value)), Some(0));
    heap.mutate(|m| m.get(&holder).item.set(m, None));
    heap.collect();
    assert!(weak[0].is_cleared());
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

/// 放在堆对象里的弱引用：持有者活着时照常用；持有者死了，弱引用随它析构（还槽），目标照常回收
#[test]
fn weak_inside_heap_objects() {
    const N: u64 = if cfg!(miri) { 100 } else { 5_000 };
    const ROUNDS: usize = if cfg!(miri) { 2 } else { 10 };
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new(THREADS);
    let (keep, outside) = items(&heap, &drops, N, |v| v % 2 == 0);
    let total = || -> u64 { (0..N).filter(|v| v % 2 == 0).sum() };
    for _ in 0..ROUNDS {
        let holder: Root<HolderB> = heap.mutate(|m| {
            let weak = m.get(&keep).items.iter().map(|&x| m.weak::<ItemB>(x)).collect();
            m.root(m.alloc(Holder { item: Slot::default(), weak }))
        });
        heap.collect();
        let sum: u64 = heap.mutate(|m| m.get(&holder).weak.iter().map(|w| m.upgrade(w).expect("挂着").value).sum());
        assert_eq!(sum, total());
        drop(holder);
        heap.collect();
    }
    assert_eq!(drops.load(Ordering::Relaxed), count(N / 2));
    drop(keep);
    heap.collect();
    assert!(outside.iter().all(Weak::is_cleared));
    assert_eq!(drops.load(Ordering::Relaxed), count(N));
}

/// 只经另一个堆的 Far 可达的对象：所在堆单独回收时 Far 的入口是根，弱引用不清；持有者也死了以后一起回收才清
#[test]
fn group_collection_clears_weak() {
    #[derive(Trace)]
    struct Pin<'a> {
        far: Far<'a, ItemB>,
    }
    brand!(PinB = for<'a> Pin<'a>);
    let drops = Arc::new(AtomicUsize::new(0));
    let mut a = Heap::new(THREADS);
    let mut b = Heap::new(THREADS);
    let (weak, pin): (Weak<ItemB>, Root<PinB>) = a.mutate(|ma| {
        let x = ma.alloc(Item { value: 5, tag: Counted(drops.clone()) });
        let pin = b.mutate(|mb| mb.root(mb.alloc(Pin { far: Far::new(mb, ma, x) })));
        (ma.weak(x), pin)
    });
    a.collect();
    assert_eq!(values(&a, std::slice::from_ref(&weak)), [Some(5)]);
    drop(pin);
    a.collect();
    assert!(!weak.is_cleared(), "b 还没回收，入口还在");
    Heap::collect_group(&mut [&mut a, &mut b]);
    assert!(weak.is_cleared());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

/// 四个线程不停地建对象、建弱引用、从弱引用取对象挂进自己的槽、扔掉旧的，另一个线程同时并发标记；每轮收尾后：
/// 挂着的都取得到原值，取不到的都已经析构，析构数恰好等于建的减活着的
#[test]
fn weak_churn_while_marking_concurrently() {
    const THREADS_M: usize = 4;
    const PER_ROUND: u64 = if cfg!(miri) { 50 } else { 3_000 };
    const ROUNDS: usize = if cfg!(miri) { 2 } else { 8 };
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new(THREADS);
    let holders: Vec<Root<HolderB>> = heap.mutate(|m| (0..THREADS_M).map(|_| m.root(m.alloc(Holder { item: Slot::default(), weak: Vec::new() }))).collect());
    let mut weak: Vec<Vec<(u64, Weak<ItemB>)>> = (0..THREADS_M).map(|_| Vec::new()).collect();
    let mut made = 0usize;
    for round in 0..ROUNDS {
        std::thread::scope(|s| {
            let heap = &heap;
            s.spawn(move || {
                heap.mark_concurrently();
            });
            for (t, (holder, mine)) in holders.iter().zip(weak.iter_mut()).enumerate() {
                let drops = &drops;
                s.spawn(move || {
                    for k in 0..PER_ROUND {
                        heap.mutate(|m| {
                            let value = ((round * THREADS_M + t) as u64) << 32 | k;
                            let x = m.alloc(Item { value, tag: Counted(drops.clone()) });
                            mine.push((value, m.weak(x)));
                            // 隔一个挂一个从前面某个弱引用里取出的（可能是更早的轮次），没取到就挂新的
                            let pick = mine.len() / 2;
                            let got = m.upgrade(&mine[pick].1);
                            if let Some(y) = got {
                                assert_eq!(y.value, mine[pick].0, "取出的值不对");
                            }
                            m.get(holder).item.set(m, Some(got.unwrap_or(x)));
                        });
                    }
                });
            }
        });
        made += THREADS_M * count(PER_ROUND);
        // 并发标记收尾：标记期间新建的是黑的、都活过这一轮；没清空的弱引用都取得到原值
        let check = |heap: &Heap| -> usize {
            heap.mutate(|m| weak.iter().flatten().filter(|(value, w)| m.upgrade(w).inspect(|x| assert_eq!(x.value, *value)).is_some()).count())
        };
        heap.collect();
        check(&heap);
        // 再一次暂停式回收：只剩挂着的
        heap.collect();
        let alive = check(&heap);
        assert_eq!(alive, THREADS_M, "每个线程恰好挂着一个");
        assert_eq!(drops.load(Ordering::Relaxed), made - alive, "第 {round} 轮");
        for mine in &mut weak {
            mine.retain(|(_, w)| !w.is_cleared());
        }
    }
    drop(holders);
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), made);
}
