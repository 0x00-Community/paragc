//! Weak references. Run one at a time: `cargo test --test weak -- --exact <name>`
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use paragc::{Far, Gc, Heap, Root, Slot, Trace, Tracer, Weak, YoungEvacuation, brand};

/// Collector threads per heap; 2 under Miri
const THREADS: NonZeroUsize = NonZeroUsize::new(if cfg!(miri) { 2 } else { 4 }).unwrap();

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
struct Item {
    value: u64,
    tag: Counted,
}

brand!(ItemB = for<'gc> Item);

/// Strong references to a run of objects (inside one object)
#[derive(Trace)]
struct Keep<'gc> {
    items: Vec<Gc<'gc, Item>>,
}

brand!(KeepB = for<'gc> Keep<'gc>);

/// A mutable strong reference, plus a weak reference stored in a heap object
#[derive(Trace)]
struct Holder<'gc> {
    item: Slot<'gc, Item>,
    weak: Vec<Weak<ItemB>>,
}

brand!(HolderB = for<'gc> Holder<'gc>);

/// Make `n` objects (values 0..n) and keep weak references to them; with `keep` they also hang from a root
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

/// The current value of every weak reference (None once cleared)
fn values(heap: &Heap, weak: &[Weak<ItemB>]) -> Vec<Option<u64>> {
    heap.mutate(|m| weak.iter().map(|w| m.upgrade(w).map(|x| x.value)).collect())
}

fn count(n: u64) -> usize {
    usize::try_from(n).expect("small")
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
    assert!(got.iter().enumerate().all(|(i, v)| *v == (i % 2 == 0).then_some(i as u64)), "live ones upgrade, dead ones are cleared");
    assert!(weak.iter().enumerate().all(|(i, w)| w.is_cleared() == (i % 2 == 1)));
    drop(keep);
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), count(N));
    assert!(weak.iter().all(Weak::is_cleared));
}

/// Evacuation moves live objects: weak references follow to the new address, dead ones are cleared, old copies are not finalized
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
    let addr = |heap: &Heap| -> Vec<usize> { heap.mutate(|m| weak.iter().step_by(count(KEEP_EVERY)).map(|w| m.upgrade(w).expect("alive").as_ptr().addr()).collect()) };
    let before = addr(&heap);
    heap.collect();
    assert!(heap.stats().evacuated_blocks > 0, "sparse blocks were not evacuated: {:?}", heap.stats());
    let after = addr(&heap);
    assert!(before.iter().zip(&after).any(|(b, a)| b != a), "no object changed address");
    // The weak and the strong reference see the same (new) object
    let strong: Vec<usize> = heap.mutate(|m| m.get(&keep).items.iter().map(|x| x.as_ptr().addr()).collect());
    assert_eq!(after, strong);
    let got = values(&heap, &weak);
    assert!(got.iter().enumerate().all(|(i, v)| *v == (i as u64).is_multiple_of(KEEP_EVERY).then_some(i as u64)));
    assert_eq!(drops.load(Ordering::Relaxed), discarded, "a moved old copy was finalized");
    drop(keep);
    heap.collect();
    assert!(weak.iter().all(Weak::is_cleared));
    assert_eq!(drops.load(Ordering::Relaxed), count(N));
}

/// Young collections (all three modes): young objects without strong references are cleared, live ones (possibly moved) upgrade to their values; old objects are unaffected
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

/// An old object loses its last strong reference: without weak references a young collection frees it by count; with one the count sticks until a full collection (release by count
/// runs concurrently with mutators and could not clear the weak reference first)
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
    let weak = heap.mutate(|m| m.weak::<ItemB>(m.get(&holders[0]).item.get().expect("attached")));
    heap.mutate(|m| {
        for h in &holders {
            m.get(h).item.set(m, None);
        }
    });
    heap.collect_young();
    assert_eq!(other.load(Ordering::Relaxed), 1, "an old object without weak references should be freed by count");
    assert_eq!(drops.load(Ordering::Relaxed), 0, "one with a weak reference must not be freed by count");
    assert_eq!(values(&heap, std::slice::from_ref(&weak)), [Some(7)]);
    heap.collect();
    assert!(weak.is_cleared());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    drop(holders);
}

/// An object reachable only through weak references when concurrent marking starts: upgraded during marking and hung in another object, it must survive (read barrier);
/// the ones not upgraded are cleared as usual
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
        let x = m.upgrade(&weak[0]).expect("not collected yet");
        m.get(&holder).item.set(m, Some(x));
    });
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), 1, "only the one never taken out dies");
    assert_eq!(values(&heap, &weak), [Some(0), None]);
    assert_eq!(heap.mutate(|m| m.get(&holder).item.get().map(|x| x.value)), Some(0));
    heap.mutate(|m| m.get(&holder).item.set(m, None));
    heap.collect();
    assert!(weak[0].is_cleared());
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

/// Weak references stored in a heap object: usable while the holder lives; when the holder dies they are finalized with it (returning their slots) and the targets are collected as usual
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
        let sum: u64 = heap.mutate(|m| m.get(&holder).weak.iter().map(|w| m.upgrade(w).expect("attached").value).sum());
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

/// An object reachable only through another heap's Far: collecting its heap alone keeps the Far's entry as a root and the weak reference stays; it clears only in a group collection after the holder died too
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
    assert!(!weak.is_cleared(), "b has not been collected, the entry is still there");
    Heap::collect_group(&mut [&mut a, &mut b]);
    assert!(weak.is_cleared());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

/// Four threads keep making objects and weak references, taking objects out of weak references into their own slots and dropping old ones, while another thread marks concurrently; after each round:
/// everything attached upgrades to its value, everything that does not is finalized, and the finalized count is exactly made minus alive
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
                            // Every other slot gets an object taken from some earlier weak reference (possibly from an earlier round); if none upgrades, a new one
                            let pick = mine.len() / 2;
                            let got = m.upgrade(&mine[pick].1);
                            if let Some(y) = got {
                                assert_eq!(y.value, mine[pick].0, "wrong value taken out");
                            }
                            m.get(holder).item.set(m, Some(got.unwrap_or(x)));
                        });
                    }
                });
            }
        });
        made += THREADS_M * count(PER_ROUND);
        // Concurrent marking finished: objects made during marking are black and survive this round; every weak reference not cleared upgrades to its value
        let check = |heap: &Heap| -> usize {
            heap.mutate(|m| weak.iter().flatten().filter(|(value, w)| m.upgrade(w).inspect(|x| assert_eq!(x.value, *value)).is_some()).count())
        };
        heap.collect();
        check(&heap);
        // One more stop-the-world collection: only the attached ones remain
        heap.collect();
        let alive = check(&heap);
        assert_eq!(alive, THREADS_M, "each thread holds exactly one");
        assert_eq!(drops.load(Ordering::Relaxed), made - alive, "round {round}");
        for mine in &mut weak {
            mine.retain(|(_, w)| !w.is_cleared());
        }
    }
    drop(holders);
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), made);
}
