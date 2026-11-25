//! The allocation limit: the hook runs once per crossing, only when a block or a large object is taken.
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use paragc::{Heap, Scope, ScopeGlobal, Trace, Tracer, brand};

const THREADS: NonZeroUsize = NonZeroUsize::new(2).unwrap();
const MIB: usize = 1 << 20;
const SMALL: usize = 64;

struct Words(#[expect(dead_code, reason = "the words only take room")] [u64; SMALL / 8]);

// SAFETY: no pointers
unsafe impl Trace for Words {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut Tracer<'_>) {}
}

brand!(WordsB = for<'heap, 'scope> Words);

fn keep<'heap>(s: &Scope<'heap, '_>, kept: &mut Vec<ScopeGlobal<'heap, 'heap, WordsB>>, bytes: usize) {
    for _ in 0..bytes / SMALL {
        kept.push(s.global(s.alloc(Words([0; SMALL / 8]))));
    }
}

fn crossing_after(young: bool) {
    Heap::new(THREADS, |mut heap| {
        let calls = counted(&mut heap);
        let mut kept = Vec::new();
        heap.scope(|s| keep(s, &mut kept, 4 * MIB));
        heap.collect();
        if young {
            heap.collect_young();
        } else {
            heap.finish_marking();
        }
        heap.set_limit(Some(6 * MIB));
        heap.scope(|s| keep(s, &mut kept, 4 * MIB));
        assert_eq!(calls.load(Ordering::Relaxed), 1, "the bytes kept from before the collection still count");
        drop(kept);
    });
}

#[test]
fn crossing_after_a_young_collection() {
    crossing_after(true);
}

#[test]
fn crossing_while_sweeping_lazily() {
    crossing_after(false);
}

fn counted(heap: &mut Heap<'_>) -> Arc<AtomicUsize> {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    heap.on_limit(move || {
        seen.fetch_add(1, Ordering::Relaxed);
    });
    calls
}

#[test]
fn crossing_calls_the_hook_once() {
    Heap::new(THREADS, |mut heap| {
        let calls = counted(&mut heap);
        heap.set_limit(Some(MIB));
        heap.scope(|s| {
            while s.used() < 4 * MIB {
                let _ = s.alloc(Words([0; SMALL / 8]));
            }
        });
        assert_eq!(calls.load(Ordering::Relaxed), 1, "disarmed after the first crossing");
    });
}

#[test]
fn arming_again_inside_a_scope() {
    Heap::new(THREADS, |mut heap| {
        let calls = counted(&mut heap);
        heap.set_limit(Some(MIB));
        heap.scope(|s| {
            while calls.load(Ordering::Relaxed) == 0 {
                let _ = s.alloc(Words([0; SMALL / 8]));
            }
            s.set_limit(Some(s.used() + MIB));
            while s.used() < 4 * MIB {
                let _ = s.alloc(Words([0; SMALL / 8]));
            }
        });
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    });
}

#[test]
fn a_large_object_crosses_too() {
    Heap::new(THREADS, |mut heap| {
        let calls = counted(&mut heap);
        heap.scope(|s| {
            s.set_limit(Some(s.used() + 1));
            let _ = s.alloc_tail(Words([0; SMALL / 8]), 2 * MIB, |tail| tail.fill(std::mem::MaybeUninit::new(0)));
        });
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    });
}

#[test]
fn no_limit_no_hook() {
    Heap::new(THREADS, |mut heap| {
        let calls = counted(&mut heap);
        heap.scope(|s| {
            while s.used() < 4 * MIB {
                let _ = s.alloc(Words([0; SMALL / 8]));
            }
        });
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    });
}

#[test]
fn used_drops_when_blocks_go_back() {
    Heap::new(THREADS, |mut heap| {
        heap.scope(|s| {
            while s.used() < 8 * MIB {
                let _ = s.alloc(Words([0; SMALL / 8]));
            }
        });
        let before = heap.used();
        heap.shrink();
        assert!(heap.used() < before / 4, "the garbage's blocks went back: {} of {before}", heap.used());
    });
}
