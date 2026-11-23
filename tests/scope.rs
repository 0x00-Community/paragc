//! Nested scopes: collecting inside a scope, carrying objects out and in. Run one at a time: `cargo test --test scope -- --exact <name>`
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use paragc::{Heap, ScopeGlobal, Slot, Trace, Tracer, brand};

const THREADS: NonZeroUsize = NonZeroUsize::new(if cfg!(miri) { 2 } else { 4 }).unwrap();

struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: no pointers
unsafe impl Trace for Counted {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut Tracer<'_>) {}
}

#[derive(Trace)]
struct Node<'heap, 'scope> {
    value: u64,
    next: Slot<'heap, 'scope, Node<'heap, 'scope>>,
    tag: Counted,
}

brand!(NodeB = for<'heap, 'scope> Node<'heap, 'scope>);

fn sum<'heap, 'scope>(n: Option<paragc::ScopeLocal<'heap, 'scope, Node<'heap, 'scope>>>) -> u64 {
    let mut cur = n;
    let mut total = 0;
    while let Some(x) = cur {
        total += x.value;
        cur = x.next.get();
    }
    total
}

/// A list kept by a scope root survives collections made inside the same scope, garbage made around it is freed
#[test]
fn collect_inside_a_scope() {
    const LEN: u64 = if cfg!(miri) { 200 } else { 20_000 };
    let drops = Arc::new(AtomicUsize::new(0));
    Heap::new(THREADS, |mut heap| {
        heap.scope(|s| {
            let list = {
                let mut head = None;
                for value in 0..LEN {
                    let n = s.alloc(Node { value, next: head.into(), tag: Counted(drops.clone()) });
                    s.alloc(Node { value, next: None.into(), tag: Counted(drops.clone()) });
                    head = Some(n);
                }
                s.scope_out::<NodeB>(head.expect("LEN > 0"))
            };
            s.collect();
            s.collect();
            assert_eq!(drops.load(Ordering::Relaxed), usize::try_from(LEN).unwrap(), "the unrooted half is freed");
            assert_eq!(sum(Some(s.scope_in(&list))), LEN * (LEN - 1) / 2);
        });
    });
    assert_eq!(drops.load(Ordering::Relaxed), 2 * usize::try_from(LEN).unwrap());
}

/// Inner scopes carry objects out to the outer one; the outer scope collects between them and keeps only what was carried out
#[test]
fn carry_out_between_inner_scopes() {
    const ROUNDS: u64 = 8;
    const PER: u64 = if cfg!(miri) { 50 } else { 2_000 };
    let drops = Arc::new(AtomicUsize::new(0));
    Heap::new(THREADS, |mut heap| {
        heap.scope(|outer| {
            let mut kept: Vec<ScopeGlobal<'_, '_, NodeB>> = Vec::new();
            for round in 0..ROUNDS {
                let carried = outer.scope(|inner| {
                    let mut head = None;
                    for value in 0..PER {
                        head = Some(inner.alloc(Node { value: round * PER + value, next: head.into(), tag: Counted(drops.clone()) }));
                        inner.alloc(Node { value: 0, next: None.into(), tag: Counted(drops.clone()) });
                    }
                    inner.scope_out(head.expect("PER > 0"))
                });
                kept.push(carried);
                outer.collect_young();
            }
            outer.collect();
            assert_eq!(drops.load(Ordering::Relaxed), usize::try_from(ROUNDS * PER).unwrap(), "only the lists carried out survive");
            let total: u64 = kept.iter().map(|g| sum(Some(outer.scope_in(g)))).sum();
            let n = ROUNDS * PER;
            assert_eq!(total, n * (n - 1) / 2);
            kept.truncate(1);
            outer.collect();
            assert_eq!(drops.load(Ordering::Relaxed), usize::try_from(2 * ROUNDS * PER - PER).unwrap(), "dropping a ScopeGlobal releases its object");
        });
    });
}
