//! `WordSlot`: the slot holds a word encoded by the user (pointer words carry the user's own tag, other words are numbers); through every kind of collection tags and numbers stay as they were and the objects pointed to stay alive.
//! Run one at a time: `cargo test --test word -- --exact <name>`
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use paragc::{Gc, Heap, Global, Trace, Tracer, WORD_PAYLOAD, WordSlot, YoungEvacuation, brand, word_pointer};

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

/// A node of the chain: `next` is a pointer word (tag [`TAG`]) or a number word (the end of the chain); `n` is always a number word
#[derive(Trace)]
struct Node<'scope> {
    next: WordSlot<'scope>,
    n: WordSlot<'scope>,
    tag: Counted,
}

brand!(NodeB = for<'scope> Node<'scope>);

/// The user's own pointer tag (in the pointer range, not paragc's `RAW_TAG`)
const TAG: u64 = 0xFFF9 << 48;
/// The number at the end of the chain (bits of an f64: number words fall outside the pointer range)
const END: f64 = -1.5;

const THREADS: NonZeroUsize = NonZeroUsize::new(if cfg!(miri) { 2 } else { 4 }).unwrap();
const LEN: usize = if cfg!(miri) { 100 } else { 10_000 };

fn pointer(n: Gc<'_, Node<'_>>) -> u64 {
    TAG | n.as_ptr().expose_provenance() as u64
}

/// The node in a word (the tag must be unchanged)
fn node<'scope>(word: u64) -> Option<Gc<'scope, Node<'scope>>> {
    let p = word_pointer(word)?;
    assert_eq!(word & !WORD_PAYLOAD, TAG, "the tag changed");
    // SAFETY: the word was made by `pointer`, and the node is pointed to by a reachable node of the chain
    Some(unsafe { Gc::from_raw(p.cast::<Node<'scope>>()) })
}

fn number(n: f64) -> u64 {
    n.to_bits()
}

/// One root for the head node, then `len` nodes; node i has `n` = i
fn build<'h>(heap: &Heap<'h>, len: usize, drops: &Arc<AtomicUsize>) -> Global<'h, NodeB> {
    heap.scope(|m| {
        let mut next = number(END);
        for i in (0..len).rev() {
            #[expect(clippy::cast_precision_loss, reason = "below 2^53")]
            let n = m.alloc(Node { next: WordSlot::new(next), n: WordSlot::new(number(i as f64)), tag: Counted(Arc::clone(drops)) });
            next = pointer(n);
        }
        m.global(node(next).expect("len > 0"))
    })
}

/// Walk to the end of the chain: the node count; `n` increases in order and the end is END
fn walk<'h>(heap: &Heap<'h>, root: &Global<'h, NodeB>) -> usize {
    heap.scope(|m| {
        let mut cur = Some(m.get(root));
        let mut count = 0;
        let mut last = -1.0;
        while let Some(n) = cur {
            let v = f64::from_bits(n.n.get());
            assert!(v > last, "a number word changed: {v} is not above {last}");
            last = v;
            count += 1;
            let next = n.next.get();
            cur = node(next);
            if cur.is_none() {
                assert_eq!(next, number(END), "the number at the end changed");
            }
        }
        count
    })
}

/// After a full collection (with evacuation) the chain is intact and tags and numbers are unchanged; replacing a pointer word in the middle with a number word reclaims the whole second half
#[test]
fn survives_full_collections_and_cut_frees_the_rest() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new_static(THREADS);
    let root = build(&heap, LEN, &drops);
    for _ in 0..3 {
        heap.collect();
        assert_eq!(walk(&heap, &root), LEN);
    }
    assert_eq!(drops.load(Ordering::Relaxed), 0, "a live node was finalized");
    // Cut at node LEN / 2: its next becomes a number word
    heap.scope(|m| {
        let mut cur = m.get(&root);
        for _ in 1..LEN / 2 {
            cur = node(cur.next.get()).expect("the chain is long enough");
        }
        cur.next.set(m, number(END));
    });
    heap.collect();
    assert_eq!(walk(&heap, &root), LEN / 2);
    assert_eq!(drops.load(Ordering::Relaxed), LEN - LEN / 2, "the cut-off second half was not fully reclaimed");
    drop(root);
    drop(heap);
    assert_eq!(drops.load(Ordering::Relaxed), LEN);
}

/// Young nodes hung in old nodes' words (remembered set) survive young collections (all three evacuation modes); discarded young nodes are reclaimed
fn young(mode: YoungEvacuation) {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new_static(THREADS);
    heap.set_young_evacuation(mode);
    let root = build(&heap, LEN, &drops);
    heap.collect();
    // Insert a young node after every old node; make as many again and discard them
    heap.scope(|m| {
        let mut cur = Some(m.get(&root));
        while let Some(old) = cur {
            let next = old.next.get();
            let v = f64::from_bits(old.n.get());
            let y = m.alloc(Node { next: WordSlot::new(next), n: WordSlot::new(number(v + 0.5)), tag: Counted(Arc::clone(&drops)) });
            old.next.set(m, pointer(y));
            let _garbage = m.alloc(Node { next: WordSlot::new(pointer(y)), n: WordSlot::new(number(0.0)), tag: Counted(Arc::clone(&drops)) });
            cur = node(next);
        }
    });
    heap.collect_young();
    assert_eq!(walk(&heap, &root), LEN * 2, "nodes were lost from the chain after a young collection");
    assert_eq!(drops.load(Ordering::Relaxed), LEN, "the discarded young nodes were not reclaimed exactly");
    heap.collect();
    assert_eq!(walk(&heap, &root), LEN * 2);
    assert_eq!(drops.load(Ordering::Relaxed), LEN);
}

#[test]
fn young_collection_in_place() {
    young(YoungEvacuation::Off);
}

#[test]
fn young_collection_skipping_remembered() {
    young(YoungEvacuation::SkipRemembered);
}

#[test]
fn young_collection_moving_remembered() {
    young(YoungEvacuation::Full);
}

/// Rewriting words during concurrent marking (pointer ⇄ number): the chain is intact afterwards and counts are trustworthy (cutting again is reclaimed by counting)
#[test]
fn concurrent_marking_while_rewriting() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new_static(THREADS);
    let root = build(&heap, LEN, &drops);
    heap.collect();
    for round in 0..4 {
        assert!(heap.mark_concurrently());
        // Write each node's n as a pointer word (to the next node), then back to a number: the barrier runs in both directions
        heap.scope(|m| {
            let mut cur = Some(m.get(&root));
            while let Some(n) = cur {
                let v = n.n.get();
                let next = n.next.get();
                if node(next).is_some() {
                    n.n.set(m, next);
                }
                n.n.set(m, v);
                cur = node(next);
            }
        });
        if round % 2 == 0 { heap.collect_young() } else { heap.collect() }
        assert_eq!(walk(&heap, &root), LEN);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
    }
    heap.scope(|m| {
        m.get(&root).next.set(m, number(END));
    });
    heap.collect();
    heap.collect();
    assert_eq!(walk(&heap, &root), 1);
    assert_eq!(drops.load(Ordering::Relaxed), LEN - 1);
}
