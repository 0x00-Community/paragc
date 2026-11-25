//! The collection threshold: the hook runs once when a collection becomes due, every collection arms it again, and a request calls it too.
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use paragc::{Heap, Scope, Trace, Tracer};

const THREADS: NonZeroUsize = NonZeroUsize::new(2).unwrap();
const MIB: usize = 1 << 20;
const SMALL: usize = 64;
const THRESHOLD: NonZeroUsize = NonZeroUsize::new(MIB).unwrap();

struct Words(#[expect(dead_code, reason = "the words only take room")] [u64; SMALL / 8]);

// SAFETY: no pointers
unsafe impl Trace for Words {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut Tracer<'_>) {}
}

fn garbage(s: &Scope<'_, '_>, bytes: usize) {
    for _ in 0..bytes / SMALL {
        let _ = s.alloc(Words([0; SMALL / 8]));
    }
}

fn counted(heap: &mut Heap<'_>) -> Arc<AtomicUsize> {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    heap.on_collect_needed(move || {
        seen.fetch_add(1, Ordering::Relaxed);
    });
    calls
}

#[test]
fn due_calls_the_hook_once() {
    Heap::new(THREADS, |mut heap| {
        let calls = counted(&mut heap);
        heap.set_collect_threshold(Some(THRESHOLD));
        heap.scope(|s| garbage(s, 4 * MIB));
        assert_eq!(calls.load(Ordering::Relaxed), 1, "not called again before a collection");
    });
}

#[test]
fn every_collection_arms_it_again() {
    Heap::new(THREADS, |mut heap| {
        let calls = counted(&mut heap);
        heap.set_collect_threshold(Some(THRESHOLD));
        heap.scope(|s| garbage(s, 4 * MIB));
        heap.collect();
        heap.scope(|s| garbage(s, 4 * MIB));
        heap.collect_young();
        heap.scope(|s| garbage(s, 4 * MIB));
        assert_eq!(calls.load(Ordering::Relaxed), 3);
    });
}

#[test]
fn collecting_inside_the_scope_arms_it_again() {
    Heap::new(THREADS, |mut heap| {
        let calls = counted(&mut heap);
        heap.set_collect_threshold(Some(THRESHOLD));
        heap.scope(|s| {
            while calls.load(Ordering::Relaxed) == 0 {
                let _ = s.alloc(Words([0; SMALL / 8]));
            }
            assert!(s.collect_if_needed(), "the hook said a collection is due");
            garbage(s, 4 * MIB);
        });
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    });
}

#[test]
fn collect_if_needed_follows_the_threshold() {
    Heap::new(THREADS, |mut heap| {
        heap.set_collect_threshold(Some(THRESHOLD));
        heap.scope(|s| garbage(s, 2 * MIB));
        assert!(heap.collect_if_needed());
        heap.set_collect_threshold(NonZeroUsize::new(64 * MIB));
        heap.scope(|s| garbage(s, 2 * MIB));
        assert!(!heap.collect_if_needed());
    });
}

#[test]
fn the_growth_policy_without_a_threshold() {
    Heap::new(THREADS, |mut heap| {
        let calls = counted(&mut heap);
        let due = heap.collect_threshold();
        heap.scope(|s| {
            while calls.load(Ordering::Relaxed) == 0 {
                let _ = s.alloc(Words([0; SMALL / 8]));
            }
        });
        assert!(heap.stats().allocated >= due, "called once {} bytes were taken", heap.stats().allocated);
        assert!(heap.collect_if_needed());
    });
}

#[test]
fn a_large_object_counts() {
    Heap::new(THREADS, |mut heap| {
        let calls = counted(&mut heap);
        heap.set_collect_threshold(Some(THRESHOLD));
        heap.scope(|s| {
            let _ = s.alloc_tail(Words([0; SMALL / 8]), 2 * MIB, |tail| tail.fill(std::mem::MaybeUninit::new(0)));
        });
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    });
}

#[test]
fn a_request_calls_the_hook_once_per_round() {
    Heap::new(THREADS, |mut heap| {
        let calls = counted(&mut heap);
        assert!(heap.request_collect());
        assert!(!heap.request_collect(), "already asked this round");
        heap.scope(|s| assert!(!s.request_collect()));
        heap.collect();
        heap.scope(|s| assert!(s.request_collect()));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    });
}

#[test]
fn no_hook_no_call() {
    Heap::new(THREADS, |mut heap| {
        heap.set_collect_threshold(Some(THRESHOLD));
        heap.scope(|s| garbage(s, 4 * MIB));
        assert!(!heap.request_collect());
    });
}
