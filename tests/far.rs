//! Direct references across heaps: [`Far`]. Run one at a time: `cargo test --test far -- --exact <name>`
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use paragc::{GroupMember, Far, ScopeLocal, Heap, Global, Slot, Trace, Tracer, YoungEvacuation, brand};

/// Collector threads per heap (fewer under Miri)
const THREADS: NonZeroUsize = NonZeroUsize::new(if cfg!(miri) { 2 } else { 4 }).unwrap();

/// Counts on drop (no ScopeLocal, so it may have Drop).
struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: no ScopeLocal
unsafe impl Trace for Counted {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut Tracer<'_>) {}
}

/// An object pointed to from another heap
#[derive(Trace)]
struct Item {
    value: u64,
    _counted: Counted,
}

brand!(ItemB = for<'scope> Item);

/// An object in the holder heap: points directly at an Item in another heap
#[derive(Trace)]
struct Holder<'scope> {
    value: u64,
    far: Far<'scope, ItemB>,
}

brand!(HolderB = for<'scope> Holder<'scope>);

/// One ring of the cycle: `out` points to this heap's Hop, and the Hop points through a Far to the next heap's Link (a Far is fixed once made, so make the Link first and then the Hop)
#[derive(Trace)]
struct Link<'scope> {
    heap: usize,
    _counted: Counted,
    out: Slot<'scope, Hop<'scope>>,
}

#[derive(Trace)]
struct Hop<'scope> {
    far: Far<'scope, LinkB>,
}

brand!(LinkB = for<'scope> Link<'scope>);

fn heaps(n: usize) -> Vec<Heap<'static>> {
    (0..n).map(|_| Heap::new_static(THREADS)).collect()
}

/// Make an Item and return its root.
fn item<'h>(heap: &Heap<'h>, value: u64, drops: &Arc<AtomicUsize>) -> Global<'h, ItemB> {
    heap.scope(|m| m.global(m.alloc(Item { value, _counted: Counted(drops.clone()) })))
}

/// Make a Holder in heap `holder`, pointing directly at the object of `root` in heap `target`.
fn hold<'h, 't>(holder: &Heap<'h>, target: &Heap<'t>, root: &Global<'t, ItemB>, value: u64) -> Global<'h, HolderB> {
    holder.scope(|ma| target.scope(|mb| ma.global(ma.alloc(Holder { value, far: Far::new(ma, mb, mb.get(root)) }))))
}

/// Read the target Item's value through the Holder's Far.
fn far_value<'h>(holder: &Heap<'h>, target: &Heap<'h>, root: &Global<'h, HolderB>) -> u64 {
    holder.scope(|ma| target.scope(|mb| ma.get(root).far.get(mb).value))
}

#[test]
fn far_targets_are_pinned_during_evacuation() {
    // The first quarter of the Items is pointed to directly from A (cannot move); the rest have only roots (can move). Keep one in KEEP and let the rest die ⇒ every block is sparse and an evacuation candidate
    const ITEMS: u64 = if cfg!(miri) { 5_600 } else { 40_000 };
    const KEEP: u64 = 40;
    let mut a = Heap::new_static(THREADS);
    let mut b = Heap::new_static(THREADS);
    let drops = Arc::new(AtomicUsize::new(0));
    let roots: Vec<(u64, Global<'_, ItemB>)> = b.scope(|m| {
        let mut kept = Vec::new();
        for value in 0..ITEMS {
            let gc = m.alloc(Item { value, _counted: Counted(drops.clone()) });
            if value % KEEP == 0 {
                kept.push((value, m.global(gc)));
            }
        }
        kept
    });
    let held: Vec<(u64, Global<'_, HolderB>)> = roots.iter().filter(|(v, _)| *v < ITEMS / 4).map(|(v, r)| (*v, hold(&a, &b, r, *v))).collect();
    // First pass: the sweep records each block's live bytes; second pass: evacuate the sparse blocks
    b.collect();
    b.collect();
    assert!(b.stats().evacuated_blocks > 0, "the blocks with only roots should be evacuated");
    a.collect();
    b.scope(|mb| {
        a.scope(|ma| {
            for (value, h) in &held {
                let via_far = ma.get(h).far.get(mb);
                assert_eq!(via_far.value, *value);
                let root = &roots.iter().find(|(v, _)| v == value).expect("same value").1;
                // Roots follow the objects that move; those pointed to by a Far must stay put, and both paths reach the same object
                assert!(ScopeLocal::ptr_eq(mb.get(root), via_far), "an object pointed to by a Far was moved");
            }
        });
    });
    for (value, r) in &roots {
        assert_eq!(b.scope(|m| m.get(r).value), *value);
    }
    let dead = usize::try_from(ITEMS - ITEMS.div_ceil(KEEP)).unwrap();
    assert_eq!(drops.load(Ordering::Relaxed), dead);
}

#[test]
fn far_keeps_target_until_holder_dies() {
    let mut a = Heap::new_static(THREADS);
    let mut b = Heap::new_static(THREADS);
    let drops = Arc::new(AtomicUsize::new(0));
    let target = item(&b, 7, &drops);
    let holder = hold(&a, &b, &target, 1);
    drop(target);
    // The target's root is gone but the Far in A still points at it: collecting B alone (full or young) must not reclaim it
    b.collect();
    b.collect_young();
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    assert_eq!(far_value(&a, &b, &holder), 7);
    // The holder dies: A's collection finalizes the Holder and returns the entry; only then does B's collection reclaim the target
    drop(holder);
    b.collect();
    assert_eq!(drops.load(Ordering::Relaxed), 0, "A has not swept the Holder yet, the entry is still there");
    a.collect();
    b.collect();
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[test]
fn far_survives_holder_heap_drop_order() {
    // The target heap is dropped first: the Holder returns its slot when it is finalized later; the root table lives on through its Arc
    let a = Heap::new_static(THREADS);
    let b = Heap::new_static(THREADS);
    let drops = Arc::new(AtomicUsize::new(0));
    let target = item(&b, 3, &drops);
    let holder = hold(&a, &b, &target, 1);
    drop(target);
    drop(b);
    assert_eq!(drops.load(Ordering::Relaxed), 1, "dropping the target heap finalizes all its objects");
    drop(holder);
    drop(a);
}

/// Make a cycle: one Link per heap in `heaps`, the i-th pointing through a Hop directly at the (i+1)-th, the last back to the first. Returns the root of the first Link.
fn ring(heaps: &[Heap<'static>], drops: &Arc<AtomicUsize>) -> Global<'static, LinkB> {
    let links: Vec<Global<'_, LinkB>> = heaps.iter().enumerate().map(|(i, h)| h.scope(|m| m.global(m.alloc(Link { heap: i, _counted: Counted(drops.clone()), out: Slot::default() })))).collect();
    for i in 0..heaps.len() {
        let j = (i + 1) % heaps.len();
        heaps[i].scope(|ma| {
            heaps[j].scope(|mb| {
                let hop = ma.alloc(Hop { far: Far::new(ma, mb, mb.get(&links[j])) });
                ma.get(&links[i]).out.set(ma, Some(hop));
            });
        });
    }
    links.into_iter().next().expect("at least one heap")
}

/// Walk once around the cycle from the first Link and return the heap ids passed.
fn walk(heaps: &[Heap<'static>], first: &Global<'static, LinkB>) -> Vec<usize> {
    fn step<'a>(heaps: &[Heap<'static>], at: usize, link: ScopeLocal<'a, Link<'a>>, seen: &mut Vec<usize>) {
        seen.push(link.heap);
        let next = (at + 1) % heaps.len();
        if next == 0 {
            return;
        }
        let hop = link.out.get().expect("the cycle is linked");
        heaps[next].scope(|mb| step(heaps, next, hop.far.get(mb), seen));
    }
    let mut seen = Vec::new();
    heaps[0].scope(|m| step(heaps, 0, m.get(first), &mut seen));
    seen
}

#[test]
fn cross_heap_cycle_needs_group_collection() {
    let mut hs = heaps(2);
    let drops = Arc::new(AtomicUsize::new(0));
    let first = ring(&hs, &drops);
    assert_eq!(walk(&hs, &first), [0, 1]);
    drop(first);
    // Collected one by one: each other's entries are roots, nothing is reclaimed
    for _ in 0..2 {
        for h in &mut hs {
            h.collect();
        }
    }
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    let mut group: Vec<&mut dyn GroupMember> = hs.iter_mut().map(|h| h as &mut dyn GroupMember).collect();
    Heap::collect_group(&mut group);
    assert_eq!(drops.load(Ordering::Relaxed), 2);
    // Cleared entries and Fars finalized later never touch reclaimed objects
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
    let rings: Vec<Global<'_, LinkB>> = (0..RINGS).map(|_| ring(&hs, &drops)).collect();
    // Keep half
    let kept: Vec<Global<'_, LinkB>> = rings.into_iter().step_by(2).collect();
    let dead = (RINGS - kept.len()) * hs.len();
    // Put only the first 4 heaps in a group: the cycle spans 8 heaps and entries held from outside the group are roots ⇒ nothing is reclaimed
    {
        let mut group: Vec<&mut dyn GroupMember> = hs.iter_mut().take(4).map(|h| h as &mut dyn GroupMember).collect();
        Heap::collect_group(&mut group);
    }
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    let mut group: Vec<&mut dyn GroupMember> = hs.iter_mut().map(|h| h as &mut dyn GroupMember).collect();
    Heap::collect_group(&mut group);
    assert_eq!(drops.load(Ordering::Relaxed), dead);
    for first in &kept {
        assert_eq!(walk(&hs, first), (0..8).collect::<Vec<_>>());
    }
    // The kept cycles stay intact through single, young and further group collections
    for h in &mut hs {
        h.collect();
        h.collect_young();
    }
    let mut group: Vec<&mut dyn GroupMember> = hs.iter_mut().map(|h| h as &mut dyn GroupMember).collect();
    Heap::collect_group(&mut group);
    for first in &kept {
        assert_eq!(walk(&hs, first), (0..8).collect::<Vec<_>>());
    }
    drop(kept);
    let mut group: Vec<&mut dyn GroupMember> = hs.iter_mut().map(|h| h as &mut dyn GroupMember).collect();
    Heap::collect_group(&mut group);
    assert_eq!(drops.load(Ordering::Relaxed), RINGS * 8);
}

/// One board per heap: a row of slots, each holding a Holder (pointing directly at an Item in another heap)
#[derive(Trace)]
struct Board<'scope> {
    cells: Vec<Slot<'scope, Holder<'scope>>>,
}

brand!(BoardB = for<'scope> Board<'scope>);

/// A simple linear congruential generator (reproducible tests)
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
    let boards: Vec<Global<'_, BoardB>> = hs.iter().map(|h| h.scope(|m| m.global(m.alloc(Board { cells: (0..CELLS).map(|_| Slot::default()).collect() })))).collect();
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
                        // Holder value = Item value: reading it back proves the Far still points at the original object
                        let value = ((round * HEAPS + t) * STEPS + step) as u64;
                        hs[i].scope(|ma| {
                            hs[j].scope(|mb| {
                                let it = mb.alloc(Item { value, _counted: Counted(drops.clone()) });
                                made.fetch_add(1, Ordering::Relaxed);
                                let h = ma.alloc(Holder { value, far: Far::new(ma, mb, it) });
                                ma.get(&boards[i]).cells[cell].set(ma, Some(h));
                            });
                        });
                        // Read a cell somewhere else on the way
                        let (k, c) = (rng.below(HEAPS), rng.below(CELLS));
                        hs[k].scope(|mk| {
                            if let Some(h) = mk.get(&boards[k]).cells[c].get() {
                                let (_, target) = hs.iter().enumerate().find(|&(x, heap)| x != k && h.far.points_into(heap)).expect("the Far points at some other heap");
                                target.scope(|mt| assert_eq!(h.far.get(mt).value, h.value));
                            }
                        });
                    }
                });
            }
            // Concurrent marking and sweeping run alongside mutators (Fars made during marking must keep their targets too)
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
                let mut group: Vec<&mut dyn GroupMember> = hs.iter_mut().map(|h| h as &mut dyn GroupMember).collect();
                Heap::collect_group(&mut group);
            }
        }
    }
    drop(boards);
    // The boards are gone: the holders collect first (finalizing Holders and returning entries), then one group collection finishes up
    hs.iter_mut().for_each(Heap::collect);
    let mut group: Vec<&mut dyn GroupMember> = hs.iter_mut().map(|h| h as &mut dyn GroupMember).collect();
    Heap::collect_group(&mut group);
    assert_eq!(drops.load(Ordering::Relaxed), made.load(Ordering::Relaxed), "every Item should be reclaimed (no entry leaked)");
}

/// Young evacuation (`Full`) does not move young objects pointed to directly through a Far from another heap either; those with only roots move as usual.
#[test]
fn far_targets_are_pinned_during_young_evacuation() {
    const ITEMS: u64 = if cfg!(miri) { 400 } else { 4_000 };
    let a = Heap::new_static(THREADS);
    let mut b = Heap::new_static(THREADS);
    b.set_young_evacuation(YoungEvacuation::Full);
    let drops = Arc::new(AtomicUsize::new(0));
    let roots: Vec<(u64, Global<'_, ItemB>)> = (0..ITEMS).map(|v| (v, item(&b, v, &drops))).collect();
    let held: Vec<(u64, Global<'_, HolderB>)> = roots.iter().filter(|(v, _)| v % 2 == 0).map(|(v, r)| (*v, hold(&a, &b, r, *v))).collect();
    let before: Vec<usize> = b.scope(|m| roots.iter().map(|(_, r)| m.get(r).as_ptr().addr()).collect());
    b.collect_young();
    let after: Vec<usize> = b.scope(|m| roots.iter().map(|(_, r)| m.get(r).as_ptr().addr()).collect());
    for ((value, _), (x, y)) in roots.iter().zip(before.iter().zip(&after)) {
        if value % 2 == 0 {
            assert_eq!(x, y, "a young object pointed to by a Far was moved");
        }
    }
    assert!(roots.iter().zip(before.iter().zip(&after)).any(|((v, _), (x, y))| v % 2 == 1 && x != y), "young objects with only roots should move");
    for (value, h) in &held {
        assert_eq!(far_value(&a, &b, h), *value);
    }
    for (value, r) in &roots {
        assert_eq!(b.scope(|m| m.get(r).value), *value);
    }
    assert_eq!(drops.load(Ordering::Relaxed), 0);
}
