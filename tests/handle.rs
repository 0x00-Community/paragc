//! Handles: references through the handle table. Run one at a time: `cargo test --test handle -- --exact <name>`
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use paragc::{Handle, HandleSlot, Heap, Root, Trace, Tracer, YoungEvacuation, brand};

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

#[derive(Trace)]
struct Keep<'gc> {
    items: Vec<Handle<'gc, Item>>,
}

brand!(KeepB = for<'gc> Keep<'gc>);

#[derive(Trace)]
struct Holder<'gc> {
    item: HandleSlot<'gc, Item>,
}

brand!(HolderB = for<'gc> Holder<'gc>);

fn count(n: u64) -> usize {
    usize::try_from(n).expect("small")
}

/// Ids and values of the kept handles, and the addresses of their objects
fn kept(heap: &Heap, keep: &Root<KeepB>) -> (Vec<usize>, Vec<u64>, Vec<usize>) {
    heap.mutate(|m| {
        let items = &m.get(keep).items;
        (items.iter().map(|h| h.id()).collect(), items.iter().map(|h| h.value).collect(), items.iter().map(|h| h.get().as_ptr().addr()).collect())
    })
}

/// Objects move under their handles (young evacuation or full-collection evacuation); the handles stay the same. Young collections keep every handled object, full collections free the ones no live object holds
#[test]
fn handles_stay_while_objects_move() {
    const N: u64 = if cfg!(miri) { 4_000 } else { 64_000 };
    const KEEP_EVERY: u64 = 16;
    for mode in [YoungEvacuation::Off, YoungEvacuation::SkipRemembered, YoungEvacuation::Full] {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut heap = Heap::new(THREADS);
        heap.set_young_evacuation(mode);
        let keep = heap.mutate(|m| {
            let items = (0..N).map(|value| m.alloc_handle(Item { value, tag: Counted(drops.clone()) })).filter(|h| h.value.is_multiple_of(KEEP_EVERY)).collect();
            m.root(m.alloc(Keep { items }))
        });
        let (ids, values, born) = kept(&heap, &keep);
        heap.collect_young();
        assert_eq!(drops.load(Ordering::Relaxed), 0, "{mode:?}: a young collection freed a handled object");
        heap.collect();
        let discarded = count(N - N / KEEP_EVERY);
        assert_eq!(drops.load(Ordering::Relaxed), discarded, "{mode:?}");
        heap.collect();
        let (ids_after, values_after, now) = kept(&heap, &keep);
        assert!(born.iter().zip(&now).any(|(b, a)| b != a), "{mode:?}: no object changed address: {:?}", heap.stats());
        assert_eq!(ids, ids_after, "{mode:?}");
        assert_eq!(values, values_after, "{mode:?}");
        assert_eq!(drops.load(Ordering::Relaxed), discarded, "{mode:?}: a moved old copy was finalized");
        drop(keep);
        heap.collect();
        assert_eq!(drops.load(Ordering::Relaxed), count(N), "{mode:?}");
    }
}

/// An old object takes a young object's handle through a slot with no barrier; young collections keep it
#[test]
fn old_objects_hold_young_handles_without_barrier() {
    for mode in [YoungEvacuation::Off, YoungEvacuation::SkipRemembered, YoungEvacuation::Full] {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut heap = Heap::new(THREADS);
        heap.set_young_evacuation(mode);
        let holder: Root<HolderB> = heap.mutate(|m| m.root(m.alloc(Holder { item: HandleSlot::default() })));
        heap.collect();
        heap.mutate(|m| m.get(&holder).item.set(Some(m.alloc_handle(Item { value: 7, tag: Counted(drops.clone()) }))));
        heap.collect_young();
        heap.collect_young();
        assert_eq!(heap.mutate(|m| m.get(&holder).item.get().map(|h| h.value)), Some(7), "{mode:?}");
        heap.mutate(|m| m.get(&holder).item.set(None));
        heap.collect();
        assert_eq!(drops.load(Ordering::Relaxed), 1, "{mode:?}");
    }
}

/// Entries of dead objects are reused by new handles
#[test]
fn dead_entries_are_reused() {
    const N: u64 = 1_000;
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new(THREADS);
    let ids = |heap: &Heap| -> (Root<KeepB>, HashSet<usize>) {
        heap.mutate(|m| {
            let items: Vec<_> = (0..N).map(|value| m.alloc_handle(Item { value, tag: Counted(drops.clone()) })).collect();
            let ids = items.iter().map(|h| h.id()).collect();
            (m.root(m.alloc(Keep { items })), ids)
        })
    };
    let (keep, first) = ids(&heap);
    drop(keep);
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), count(N));
    let (_keep, second) = ids(&heap);
    assert_eq!(first, second);
}

/// During concurrent marking a handle moves from one holder to another with no barrier: every entry is a root of the round, so its object survives
#[test]
fn concurrent_marking_keeps_moved_handles() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new(THREADS);
    let (a, b): (Root<HolderB>, Root<HolderB>) = heap.mutate(|m| {
        let item = m.alloc_handle(Item { value: 5, tag: Counted(drops.clone()) });
        (m.root(m.alloc(Holder { item: item.into() })), m.root(m.alloc(Holder { item: HandleSlot::default() })))
    });
    heap.collect();
    assert!(heap.mark_concurrently());
    heap.mutate(|m| {
        let (a, b) = (m.get(&a), m.get(&b));
        b.item.set(a.item.get());
        a.item.set(None);
    });
    heap.finish_marking();
    heap.sweep_concurrently();
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    assert_eq!(heap.mutate(|m| m.get(&b).item.get().map(|h| h.value)), Some(5));
    heap.mutate(|m| m.get(&b).item.set(None));
    heap.collect();
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}
