//! Public heaps (heaps on shared memory). Run one at a time: `cargo test --features crossheap --test cross_heap -- --exact <name>`
#![cfg(all(target_os = "linux", feature = "crossheap"))]

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use paragc::{HeapDead, Heap, CrossHeapGlobal, CrossHeapLocal, CrossHeap, CrossHeapRef, CrossHeapSlot, CrossHeapSpace, CrossHeapWeak, ScopeGlobal, SIGNATURE_BYTES, Shared, Signature, Trace, brand, cross_heap_brand};

/// Collector threads per heap (fewer under Miri)
const THREADS: NonZeroUsize = NonZeroUsize::new(if cfg!(miri) { 2 } else { 4 }).unwrap();
/// Public heap object header
const HEADER: usize = 16;

#[repr(C)]
#[derive(Shared)]
struct Node<'heap, 'scope> {
    value: u64,
    next: CrossHeapSlot<'heap, 'scope, Node<'heap, 'scope>>,
}

cross_heap_brand!(NodeB = for<'heap, 'scope> Node<'heap, 'scope>);

/// Bytes taken by one Node (header + data, rounded to 8 bytes)
const NODE: usize = (HEADER + size_of::<Node<'static, 'static>>()).next_multiple_of(8);

/// Make a chain of `len` nodes (values 0..len) and return a handle to its head.
fn list<'x>(heap: &CrossHeap<'x>, len: u64) -> CrossHeapGlobal<'x, NodeB> {
    heap.scope(|m| {
        let mut top = None;
        for value in 0..len {
            top = Some(m.alloc(Node { value, next: top.into() }));
        }
        m.global(top.expect("len > 0"))
    })
}

fn sum<'x>(heap: &CrossHeap<'x>, root: &CrossHeapGlobal<'x, NodeB>) -> u64 {
    heap.scope(|m| {
        let (mut sum, mut cur) = (0, Some(root.get(m)));
        while let Some(n) = cur {
            sum += n.value;
            cur = n.next.get(m);
        }
        sum
    })
}

fn triangle(len: u64) -> u64 {
    len * (len - 1) / 2
}

/// Shared-memory file names for tests (pid + name, so they never clash).
fn shm_path(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/dev/shm/paragc-test-{}-{name}", std::process::id()))
}

#[test]
fn cross_heap_list_survives_and_garbage_is_reclaimed() {
    const LEN: u64 = if cfg!(miri) { 300 } else { 20_000 };
    let space = CrossHeapSpace::reserve().unwrap();
    let heap = unsafe { space.create_anonymous_unchecked(THREADS) }.unwrap();
    let kept = list(&heap, LEN);
    drop(list(&heap, LEN));
    heap.collect();
    assert_eq!(sum(&heap, &kept), triangle(LEN));
    assert_eq!(heap.stats().live, NODE * usize::try_from(LEN).unwrap(), "live bytes are exactly the chain kept");
    // Freed lines are reused: allocating as much again opens no new block
    let blocks = heap.stats().blocks;
    drop(list(&heap, LEN));
    assert_eq!(heap.stats().blocks, blocks);
    drop(kept);
    heap.collect();
    assert_eq!(heap.stats().live, 0);
}

/// Pointer slots in nested types and fixed-size arrays must all be in the pointer map
#[repr(C)]
#[derive(Shared)]
struct Inner<'heap, 'scope> {
    tag: u32,
    slot: CrossHeapSlot<'heap, 'scope, Node<'heap, 'scope>>,
}

#[repr(C)]
#[derive(Shared)]
struct Holder<'heap, 'scope> {
    many: [CrossHeapSlot<'heap, 'scope, Node<'heap, 'scope>>; 3],
    bytes: [u8; 5],
    inner: Inner<'heap, 'scope>,
}

cross_heap_brand!(HolderB = for<'heap, 'scope> Holder<'heap, 'scope>);

#[test]
fn pointer_map_covers_nested_fields_and_arrays() {
    let space = CrossHeapSpace::reserve().unwrap();
    let heap = unsafe { space.create_anonymous_unchecked(THREADS) }.unwrap();
    let holder: CrossHeapGlobal<'_, HolderB> = heap.scope(|m| {
        let leaf = |value| m.alloc(Node { value, next: CrossHeapSlot::default() });
        let h = m.alloc(Holder { many: [leaf(1).into(), leaf(2).into(), leaf(3).into()], bytes: [7; 5], inner: Inner { tag: 9, slot: leaf(4).into() } });
        // One not attached: it will be collected
        leaf(100);
        m.global(h)
    });
    heap.collect();
    let holder_bytes = (HEADER + size_of::<Holder<'static, 'static>>()).next_multiple_of(8);
    assert_eq!(heap.stats().live, holder_bytes + 4 * NODE, "the four attached leaves live, the unattached one died");
    heap.scope(|m| {
        let h = holder.get(m);
        let values: Vec<u64> = h.many.iter().chain([&h.inner.slot]).map(|s| s.get(m).expect("attached").value).collect();
        assert_eq!(values, [1, 2, 3, 4]);
        assert_eq!((h.bytes, h.inner.tag), ([7; 5], 9));
        // Cut two
        h.many[1].set(m, None);
        h.inner.slot.set(m, None);
    });
    heap.collect();
    assert_eq!(heap.stats().live, holder_bytes + 2 * NODE);
}

/// A large object (above half a block): takes a run of blocks of its own
#[repr(C)]
#[derive(Shared)]
struct Big<'heap, 'scope> {
    data: [u64; 2048],
    next: CrossHeapSlot<'heap, 'scope, Big<'heap, 'scope>>,
}

cross_heap_brand!(BigB = for<'heap, 'scope> Big<'heap, 'scope>);

#[test]
fn large_public_objects() {
    const COUNT: u64 = if cfg!(miri) { 3 } else { 40 };
    let space = CrossHeapSpace::reserve().unwrap();
    let heap = unsafe { space.create_anonymous_unchecked(THREADS) }.unwrap();
    let chain = |seed: u64| -> CrossHeapGlobal<'_, BigB> {
        heap.scope(|m| {
            let mut top = None;
            for i in 0..COUNT {
                top = Some(m.alloc(Big { data: core::array::from_fn(|k| seed + i + k as u64), next: top.into() }));
            }
            m.global(top.expect("COUNT > 0"))
        })
    };
    let kept = chain(1_000);
    drop(chain(5_000));
    heap.collect();
    let big = (HEADER + size_of::<Big<'static, 'static>>()).next_multiple_of(8);
    assert_eq!(heap.stats().live, big * usize::try_from(COUNT).unwrap());
    heap.scope(|m| {
        let (mut cur, mut i) = (Some(kept.get(m)), COUNT);
        while let Some(b) = cur {
            i -= 1;
            assert!(b.data.iter().enumerate().all(|(k, &v)| v == 1_000 + i + k as u64), "the large object's contents are intact");
            cur = b.next.get(m);
        }
        assert_eq!(i, 0);
    });
    // Blocks from dead large objects serve small objects: allocating small objects opens no new block
    let blocks = heap.stats().blocks;
    drop(list(&heap, if cfg!(miri) { 100 } else { 10_000 }));
    assert_eq!(heap.stats().blocks, blocks, "blocks of taken-apart large objects are reused by small objects");
}

#[test]
#[cfg_attr(miri, ignore = "Miri cannot map files")]
fn file_starts_empty_and_grows() {
    let path = shm_path("grow");
    let space = CrossHeapSpace::reserve().unwrap();
    let heap = unsafe { space.create_unchecked(&path, THREADS) }.unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    // Creating the heap wrote only the superblock page
    assert_eq!(meta.len(), 4096, "a new file holds only the superblock");
    let root = list(&heap, 50_000);
    let grown = std::fs::metadata(&path).unwrap();
    assert!(grown.len() > meta.len(), "the file grows as allocation needs");
    {
        use std::os::unix::fs::MetadataExt;
        // Real usage (blocks × 512) does not exceed the file length: unused parts in between take nothing
        assert!(grown.blocks() * 512 <= grown.len());
    }
    assert_eq!(sum(&heap, &root), triangle(50_000));
    drop(root);
    drop(heap);
    assert!(!path.exists(), "the file is deleted once every creating handle is dropped");
}

/// Each thread's own chain hangs here
#[repr(C)]
#[derive(Shared)]
struct Anchor<'heap, 'scope> {
    heads: [CrossHeapSlot<'heap, 'scope, Node<'heap, 'scope>>; 8],
}

cross_heap_brand!(AnchorB = for<'heap, 'scope> Anchor<'heap, 'scope>);

/// Chain `t`: push `push` nodes at the head (value = current length), then pop `pop` from the head; returns the new length.
fn churn<'x>(heap: &CrossHeap<'x>, anchor: &CrossHeapGlobal<'x, AnchorB>, t: usize, len: u64, push: u64, pop: u64) -> u64 {
    heap.scope(|m| {
        let top = &anchor.get(m).heads[t];
        for v in len..len + push {
            let n = m.alloc(Node { value: v, next: top.get(m).into() });
            top.set(m, Some(n));
        }
        for _ in 0..pop {
            let first = top.get(m).expect("the chain is not shorter than what is popped");
            top.set(m, first.next.get(m));
        }
        len + push - pop
    })
}

/// The values of chain `t` from head to tail must be len-1, len-2, …, 0.
fn check<'x>(heap: &CrossHeap<'x>, anchor: &CrossHeapGlobal<'x, AnchorB>, t: usize, len: u64) {
    heap.scope(|m| {
        let (mut cur, mut expect) = (anchor.get(m).heads[t].get(m), len);
        while let Some(n) = cur {
            expect -= 1;
            assert_eq!(n.value, expect, "values out of place on the chain: an object was collected early or a pointer is corrupted");
            cur = n.next.get(m);
        }
        assert_eq!(expect, 0, "the chain length does not match");
    });
}

/// Counts when a thread ends (including by panic), so the collecting thread stops: a panicking thread that did not count would leave it spinning
struct Finished<'a>(&'a AtomicUsize);

impl Drop for Finished<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

#[test]
#[cfg_attr(miri, ignore = "Miri cannot map files")]
fn two_mappings_of_one_heap() {
    const LEN: u64 = 10_000;
    const ROUNDS: u64 = 30;
    let path = shm_path("two");
    // Two regions in one process: the same heap mapped at two base addresses; pointers are stored as offsets, so both sides see the same object graph
    let (sa, sb) = (CrossHeapSpace::reserve().unwrap(), CrossHeapSpace::reserve().unwrap());
    let a = unsafe { sa.create_unchecked(&path, THREADS) }.unwrap();
    let b = unsafe { sb.open_unchecked(&path, THREADS) }.unwrap();
    assert_eq!(a.id(), b.id());
    let root = list(&a, LEN);
    let rb: CrossHeapGlobal<'_, NodeB> = b.scope(|m| m.find(root.index())).expect("found by entry number");
    assert!(b.scope(|m| m.find::<HolderB>(root.index())).is_none(), "not found when the type does not match");
    let pa = a.scope(|m| root.get(m).as_ptr().addr());
    let pb = b.scope(|m| rb.get(m).as_ptr().addr());
    assert_ne!(pa, pb, "the two mappings have different addresses");
    assert_eq!(sum(&b, &rb), triangle(LEN));
    // 4 threads on each side change their own chains while another thread collects from either side in turn: the handshake word is in the superblock, so they exclude each other across mappings
    let anchor = a.scope(|m| m.global::<AnchorB>(m.alloc(Anchor { heads: Default::default() })));
    let anchor_b: CrossHeapGlobal<'_, AnchorB> = b.scope(|m| m.find(anchor.index())).expect("found by entry number");
    let done = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for t in 0..8 {
            let (heap, anchor, done) = if t % 2 == 0 { (&a, &anchor, &done) } else { (&b, &anchor_b, &done) };
            s.spawn(move || {
                let _finished = Finished(done);
                let mut len = 0;
                for round in 0..ROUNDS {
                    len = churn(heap, anchor, t, len, 200, round % 50);
                    check(heap, anchor, t, len);
                }
            });
        }
        s.spawn(|| {
            let mut k = 0;
            while done.load(Ordering::Acquire) < 8 {
                if k % 2 == 0 { a.collect() } else { b.collect() }
                k += 1;
            }
        });
    });
    // The creating side lets go first: the file is deleted, but the other mapping still works
    drop((root, anchor));
    drop(a);
    assert!(!path.exists());
    b.collect();
    assert_eq!(sum(&b, &rb), triangle(LEN));
}

/// A private heap object holding a handle to a public heap
#[derive(Trace)]
struct Card {
    public: CrossHeapGlobal<'static, NodeB>,
    _drops: Counted,
}

brand!(CardB = for<'heap, 'scope> Card);

struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: no ScopeLocal
unsafe impl Trace for Counted {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut paragc::Tracer<'_>) {}
}

#[test]
fn private_heap_holds_public_handles() {
    const LEN: u64 = if cfg!(miri) { 50 } else { 5_000 };
    let space = CrossHeapSpace::reserve().unwrap();
    let public = unsafe { space.create_anonymous_unchecked(THREADS) }.unwrap();
    let mut private = unsafe { Heap::new_unchecked(THREADS) };
    let drops = Arc::new(AtomicUsize::new(0));
    let card: ScopeGlobal<'_, '_, CardB> = {
        let handle = list(&public, LEN);
        private.scope(|m| m.global(m.alloc(Card { public: handle, _drops: Counted(drops.clone()) })))
    };
    public.collect();
    assert_eq!(public.stats().live, NODE * usize::try_from(LEN).unwrap(), "handles in the private heap are roots of the public heap");
    let total = private.scope(|m| public.scope(|pm| {
        let (mut s, mut cur) = (0, Some(m.scope_in(&card).public.get(pm)));
        while let Some(n) = cur {
            s += n.value;
            cur = n.next.get(pm);
        }
        s
    }));
    assert_eq!(total, triangle(LEN));
    // The private heap's collection finalizes the Card ⇒ the handle returns its entry ⇒ the public heap's next collection reclaims the whole chain
    drop(card);
    private.collect();
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    public.collect();
    assert_eq!(public.stats().live, 0);
}

#[test]
fn eight_threads_churn_public_heap() {
    const ROUNDS: u64 = if cfg!(miri) { 2 } else { 40 };
    const PUSH: u64 = if cfg!(miri) { 20 } else { 500 };
    let space = CrossHeapSpace::reserve().unwrap();
    let heap = unsafe { space.create_anonymous_unchecked(THREADS) }.unwrap();
    let anchor = heap.scope(|m| m.global::<AnchorB>(m.alloc(Anchor { heads: Default::default() })));
    let lens: Vec<AtomicU64> = (0..8).map(|_| AtomicU64::new(0)).collect();
    for round in 0..ROUNDS {
        std::thread::scope(|s| {
            for (t, len) in lens.iter().enumerate() {
                let (heap, anchor) = (&heap, &anchor);
                s.spawn(move || {
                    let now = churn(heap, anchor, t, len.load(Ordering::Relaxed), PUSH, (round * 7 + t as u64) % PUSH);
                    len.store(now, Ordering::Relaxed);
                    check(heap, anchor, t, now);
                });
            }
            // Collecting while threads scope: wait for them to leave, collect, then let them in again
            s.spawn(|| heap.collect());
        });
        heap.collect();
        let live: u64 = lens.iter().map(|l| l.load(Ordering::Relaxed)).sum();
        let anchor_bytes = (HEADER + size_of::<Anchor<'static, 'static>>()).next_multiple_of(8);
        assert_eq!(heap.stats().live, anchor_bytes + NODE * usize::try_from(live).unwrap(), "live bytes are exactly the chains");
    }
}

#[test]
#[cfg_attr(miri, ignore = "Miri cannot map files")]
fn collect_fresh_file_heap_without_roots() {
    // Collect a new file with no entries at all (no part has a single item yet, so nothing must be extended by length 0), then collect after allocating without rooting
    let path = shm_path("fresh");
    let space = CrossHeapSpace::reserve().unwrap();
    let heap = unsafe { space.create_unchecked(&path, THREADS) }.unwrap();
    heap.collect();
    drop(list(&heap, 1_000));
    heap.collect();
    assert_eq!(heap.stats().live, 0);
    assert_eq!(heap.stats().collections, 2);
}

/// An object pointing into another public heap
#[repr(C)]
#[derive(Shared)]
struct Link<'heap, 'scope> {
    heap: u64,
    next: CrossHeapRef<'heap, 'scope, LinkB>,
}

cross_heap_brand!(LinkB = for<'heap, 'scope> Link<'heap, 'scope>);

const LINK: usize = (HEADER + size_of::<Link<'static, 'static>>()).next_multiple_of(8);

/// Make a Link in each heap of `heaps`, the i-th pointing at the (i+1)-th (with `ring`, the last back to the first). Returns each heap's Link handle.
fn links<'x>(heaps: &[CrossHeap<'x>], ring: bool) -> Vec<CrossHeapGlobal<'x, LinkB>> {
    let handles: Vec<CrossHeapGlobal<'_, LinkB>> = heaps.iter().enumerate().map(|(i, h)| h.scope(|m| m.global(m.alloc(Link { heap: i as u64, next: CrossHeapRef::default() })))).collect();
    let n = if ring { heaps.len() } else { heaps.len() - 1 };
    for i in 0..n {
        let j = (i + 1) % heaps.len();
        heaps[i].scope(|ma| heaps[j].scope(|mb| handles[i].get(ma).next.set(ma, mb, Some(handles[j].get(mb)))));
    }
    handles
}

/// The next of Link i is Link i+1 (read inside the scopes of both heaps at once).
fn linked<'x>(heaps: &[CrossHeap<'x>], handles: &[CrossHeapGlobal<'x, LinkB>], i: usize) -> bool {
    let j = (i + 1) % heaps.len();
    heaps[i].scope(|mi| heaps[j].scope(|mj| handles[i].get(mi).next.get(mj).is_some_and(|n| CrossHeapLocal::ptr_eq(n, handles[j].get(mj)) && n.heap == j as u64)))
}

impl Link<'_, '_> {
    /// A Link pointing nowhere yet.
    fn alone(heap: u64) -> Self {
        Self { heap, next: CrossHeapRef::default() }
    }
}

fn anonymous_heaps(space: &CrossHeapSpace, n: usize) -> Vec<CrossHeap<'_>> {
    (0..n).map(|_| unsafe { space.create_anonymous_unchecked(THREADS) }.unwrap()).collect()
}

fn live(heaps: &[CrossHeap<'_>]) -> Vec<usize> {
    heaps.iter().map(|h| h.stats().live).collect()
}

fn collect_each(heaps: &[CrossHeap<'_>]) {
    for h in heaps {
        h.collect();
    }
}

fn collect_all(heaps: &[CrossHeap<'_>]) {
    let group: Vec<&CrossHeap<'_>> = heaps.iter().collect();
    CrossHeap::collect_group(&group);
}

#[test]
fn cross_heap_ref_keeps_target_until_group_collection() {
    let space = CrossHeapSpace::reserve().unwrap();
    let heaps = anonymous_heaps(&space, 2);
    let mut handles = links(&heaps, false);
    // Drop the handle to the Link in B: only A's object still points at it through a CrossHeapRef
    drop(handles.pop());
    collect_each(&heaps);
    assert_eq!(live(&heaps), [LINK, LINK], "when B collects on its own, the registration held by A is a root");
    let target = heaps[0].scope(|ma| heaps[1].scope(|mb| handles[0].get(ma).next.get(mb).map(|n| n.heap)));
    assert_eq!(target, Some(1));
    // A's object is no longer wanted either: A's own collection reclaims it, but A does not touch B's registration, so B's own collection still treats it as a root
    drop(handles);
    collect_each(&heaps);
    assert_eq!(live(&heaps), [0, LINK]);
    // Group collection: tracing decides registrations held inside the group, and A no longer has an object pointing at it
    collect_all(&heaps);
    assert_eq!(live(&heaps), [0, 0]);
}

/// From the Link in the first heap, go once around the three heaps: 0 → 1 → 2 → back to the same object in 0 (reading inside three nested scopes).
fn ring_intact<'x>(heaps: &[CrossHeap<'x>], first: &CrossHeapGlobal<'x, LinkB>) -> bool {
    heaps[0].scope(|m0| {
        heaps[1].scope(|m1| {
            heaps[2].scope(|m2| {
                let a = first.get(m0);
                let Some(b) = a.next.get(m1) else { return false };
                let Some(c) = b.next.get(m2) else { return false };
                let Some(back) = c.next.get(m0) else { return false };
                (a.heap, b.heap, c.heap) == (0, 1, 2) && CrossHeapLocal::ptr_eq(back, a)
            })
        })
    })
}

#[test]
fn cycles_across_public_heaps_need_group_collection() {
    let space = CrossHeapSpace::reserve().unwrap();
    let heaps = anonymous_heaps(&space, 3);
    // The kept cycle has a handle only in the first heap: the objects in the other two live only through cross-heap references (registrations)
    let first = links(&heaps, true).swap_remove(0);
    let dropped = links(&heaps, true);
    drop(dropped);
    collect_each(&heaps);
    assert_eq!(live(&heaps), [2 * LINK; 3], "a cycle across three heaps cannot be reclaimed by single collections");
    // Collect only two heaps together: the cycle passes through the third heap outside the group, whose registration is a root
    {
        let two: Vec<&CrossHeap<'_>> = heaps.iter().take(2).collect();
        CrossHeap::collect_group(&two);
    }
    assert_eq!(live(&heaps), [2 * LINK; 3]);
    collect_all(&heaps);
    assert_eq!(live(&heaps), [LINK; 3], "collecting all three together reclaims the cycle without a root");
    assert!(ring_intact(&heaps, &first), "the rooted cycle is still linked");
    // Single collections after the group collection: the other two heaps keep their objects only through the rebuilt registrations
    collect_each(&heaps);
    assert_eq!(live(&heaps), [LINK; 3], "the group collection rebuilt the cross-heap registrations inside the group");
    collect_all(&heaps);
    collect_each(&heaps);
    assert!(ring_intact(&heaps, &first));
    assert_eq!(live(&heaps), [LINK; 3]);
}

#[test]
fn group_collection_returns_stale_registrations() {
    const SETS: u64 = if cfg!(miri) { 20 } else { 2_000 };
    let space = CrossHeapSpace::reserve().unwrap();
    let heaps = anonymous_heaps(&space, 2);
    let holder = heaps[0].scope(|m| m.global::<LinkB>(m.alloc(Link { heap: 0, next: CrossHeapRef::default() })));
    // Repoint it at new objects in B over and over: every write registers an entry in B, and old registrations are returned only by a group collection
    for i in 0..SETS {
        heaps[0].scope(|ma| heaps[1].scope(|mb| holder.get(ma).next.set(ma, mb, Some(mb.alloc(Link { heap: i, next: CrossHeapRef::default() })))));
    }
    collect_each(&heaps);
    assert_eq!(live(&heaps), [LINK, LINK * usize::try_from(SETS).unwrap()], "old registrations are still roots in single collections");
    collect_all(&heaps);
    assert_eq!(live(&heaps), [LINK, LINK], "after the group collection only the current target remains");
    let last = heaps[0].scope(|ma| heaps[1].scope(|mb| holder.get(ma).next.get(mb).expect("points somewhere").heap));
    assert_eq!(last, SETS - 1);
    // Rebuilt registrations still keep things alive: collect B on its own again
    heaps[1].collect();
    assert_eq!(heaps[1].stats().live, LINK);
}

#[test]
fn nested_mutates_with_collectors_do_not_deadlock() {
    const ROUNDS: usize = if cfg!(miri) { 3 } else { 300 };
    let space = CrossHeapSpace::reserve().unwrap();
    let heaps = anonymous_heaps(&space, 2);
    let handles = links(&heaps, true);
    std::thread::scope(|s| {
        // One thread enters A then B, another B then A; meanwhile two collect separately and one collects the group
        for (outer, inner) in [(0, 1), (1, 0)] {
            let (heaps, handles) = (&heaps, &handles);
            s.spawn(move || {
                for _ in 0..ROUNDS {
                    heaps[outer].scope(|mo| {
                        heaps[inner].scope(|mi| {
                            let fresh = mi.alloc(Link { heap: inner as u64, next: CrossHeapRef::default() });
                            handles[outer].get(mo).next.set(mo, mi, Some(fresh));
                            handles[outer].get(mo).next.set(mo, mi, Some(handles[inner].get(mi)));
                        });
                    });
                }
            });
        }
        for h in &heaps {
            s.spawn(move || {
                for _ in 0..ROUNDS / 3 {
                    h.collect();
                }
            });
        }
        s.spawn(|| {
            for _ in 0..ROUNDS / 3 {
                collect_all(&heaps);
            }
        });
    });
    collect_all(&heaps);
    assert!(linked(&heaps, &handles, 0) && linked(&heaps, &handles, 1));
    assert_eq!(live(&heaps), [LINK, LINK]);
}

/// A directory of its own (the heap-number markers in it belong to one test only).
struct Dir(std::path::PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let dir = std::path::PathBuf::from(format!("/dev/shm/paragc-pdir-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }

    fn file(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
#[cfg_attr(miri, ignore = "Miri cannot map files")]
fn references_to_a_dead_heap_do_not_crash() {
    let dir = Dir::new("dead");
    let space = CrossHeapSpace::reserve().unwrap();
    let x = unsafe { space.create_unchecked(dir.file("x"), THREADS) }.unwrap();
    let y = unsafe { space.create_unchecked(dir.file("y"), THREADS) }.unwrap();
    // y_link (in y, with a handle) → x_link (in x) → y_only (in y, pointed to only by x_link: the registration's holder is x)
    let x_link = x.scope(|m| m.global::<LinkB>(m.alloc(Link::alone(0))));
    let y_link = y.scope(|m| m.global::<LinkB>(m.alloc(Link::alone(1))));
    x.scope(|mx| y.scope(|my| {
        y_link.get(my).next.set(my, mx, Some(x_link.get(mx)));
        x_link.get(mx).next.set(mx, my, Some(my.alloc(Link::alone(2))));
    }));
    let x_number = x.id();
    // Another region in the same process also maps x
    let other = CrossHeapSpace::reserve().unwrap();
    let x2 = unsafe { other.open_unchecked(dir.file("x"), THREADS) }.unwrap();
    let held: CrossHeapGlobal<'_, LinkB> = x2.scope(|m| m.find(x_link.index())).expect("found by entry number");
    // x's owner (the creating handle and the handles it made) lets go: x dies with it and the file is deleted
    drop(x_link);
    drop(x);
    assert!(!dir.file("x").exists() && !dir.file(&format!(".paragc-{x_number}")).exists(), "the dead heap's file and marker were deleted");
    assert!(!x2.is_alive());
    assert_eq!(x2.try_scope(|_| ()), Err(HeapDead));
    // The side still mapping it can still use and drop its handles (memory returns only when the last mapping lets go)
    assert_eq!(x2.scope(|m| held.get(m).heap), 0);
    drop(held);
    drop(x2);
    // x's number is taken by the new heap z: the identity of y_link's reference into x does not match, so it reads None
    let z = unsafe { space.create_unchecked(dir.file("z"), THREADS) }.unwrap();
    assert_eq!(z.id(), x_number, "the dead heap's number is reclaimed and reused");
    let z_obj: CrossHeapGlobal<'_, LinkB> = z.scope(|m| m.global(m.alloc(Link::alone(7))));
    let (points, read) = y.scope(|my| z.scope(|mz| (y_link.get(my).next.points_into(&z), y_link.get(my).next.get(mz).is_some())));
    assert!(!points && !read, "a reference into a dead heap must not read as an object of the new heap");
    // The group collection does not follow this old reference (otherwise it would mark whatever sits at the same offset in z as an object)
    CrossHeap::collect_group(&[&y, &z]);
    assert_eq!(z.stats().live, LINK, "z holds only its own rooted object");
    // y collects: the references x registered in y while alive (holder x is dead) are returned, and y_only, pointed to only by x, is reclaimed
    y.collect();
    assert_eq!(y.stats().live, LINK, "registrations held by the dead heap are returned, leaving only the rooted one in y");
    drop((y_link, z_obj));
}

/// Lives in another heap and points weakly at a Node
#[repr(C)]
#[derive(Shared)]
struct Watcher {
    target: CrossHeapWeak<NodeB>,
}

cross_heap_brand!(WatcherB = for<'heap, 'scope> Watcher);

#[test]
fn weak_reference_across_heaps_does_not_keep_target() {
    let space = CrossHeapSpace::reserve().unwrap();
    let (a, b) = (unsafe { space.create_anonymous_unchecked(THREADS) }.unwrap(), unsafe { space.create_anonymous_unchecked(THREADS) }.unwrap());
    let target = b.scope(|m| m.global::<NodeB>(m.alloc(Node { value: 7, next: None.into() })));
    let watcher = a.scope_with(&b, |ma, mb| ma.global::<WatcherB>(ma.alloc(Watcher { target: target.get(mb).into() })));
    let read = || a.scope_with(&b, |ma, mb| watcher.get(ma).target.get(mb).map(|n| n.value));
    assert_eq!(read(), Some(7));
    b.collect();
    assert_eq!(read(), Some(7), "the target still has a root");
    drop(target);
    b.collect();
    assert_eq!(b.stats().live, 0, "a weak reference keeps nothing alive");
    assert_eq!(read(), None);
    // Even with a new object in its place it is recognized as not the original
    let fresh = list(&b, 1000);
    assert_eq!(read(), None);
    assert_eq!(a.scope(|m| watcher.get(m).target.heap()), Some(b.id()));
    drop(fresh);
}

/// The bit where the offset starts in a weak reference word (with a 1 TiB 64-bit region and 8-byte granules the birth stamp takes the low 27 bits)
const OFFSET_SHIFT: u32 = 27;
/// Bytes covered by the forgery scan before and after the original object
const NEARBY: usize = 4096 * 8;

#[test]
#[cfg_attr(miri, ignore = "offset bits are written for the 64-bit region")]
fn signature_round_trip_and_forgeries() {
    const LEN: u64 = 2000;
    let space = CrossHeapSpace::reserve().unwrap();
    let heap = unsafe { space.create_anonymous_unchecked(THREADS) }.unwrap();
    let root = list(&heap, LEN);
    let sig = heap.scope(|m| m.sign(root.get(m)));
    let bytes: [u8; SIGNATURE_BYTES] = sig.into();
    let back = Signature::from(&bytes);
    assert_eq!(back, sig);
    assert_eq!(back.heap(), Some(heap.id()));
    heap.scope(|m| {
        assert!(m.verify(&back));
        assert_eq!(m.resolve::<NodeB>(&back).map(|n| n.value), Some(LEN - 1));
        assert!(m.resolve::<HolderB>(&back).is_none(), "wrong type");
    });
    let forged = |f: &dyn Fn(&mut [u8; SIGNATURE_BYTES])| {
        let mut b = bytes;
        f(&mut b);
        Signature::from(&b)
    };
    heap.scope(|m| {
        assert!(!m.verify(&forged(&|b| b[0] ^= 1)), "wrong birth stamp");
        assert!(!m.verify(&forged(&|b| b[8] ^= 1)), "wrong heap identity");
        assert!(!m.verify(&forged(&|b| b[16] ^= 1)), "wrong type key");
    });
    // Move the offset in the word to every granule nearby: only data starts of live objects pass (objects allocated in the same scope share the stamp); the rest neither crash nor pass
    let word = u64::from_le_bytes(bytes[..8].try_into().unwrap());
    let hits = heap.scope(|m| {
        let mut hits = 0;
        for delta in -4096_i64..4096 {
            let w = word.wrapping_add_signed(delta << OFFSET_SHIFT);
            let s = forged(&|b| b[..8].copy_from_slice(&w.to_le_bytes()));
            hits += usize::from(m.verify(&s));
        }
        hits
    });
    assert!((1..=NEARBY / NODE + 1).contains(&hits), "the original signature passes and only object starts pass: {hits}");
    drop(root);
    heap.collect();
    heap.scope(|m| assert!(!m.verify(&sig), "the signature fails once the object is dead"));
}

#[test]
fn mailbox_delivers_in_order() {
    let space = CrossHeapSpace::reserve().unwrap();
    let heap = unsafe { space.create_anonymous_unchecked(THREADS) }.unwrap();
    let seen = heap.mail();
    heap.scope(|m| {
        for value in 0..3 {
            m.post(m.alloc(Node { value, next: None.into() }));
        }
    });
    assert_eq!(heap.mail(), seen + 3);
    heap.wait_mail(seen, std::time::Duration::ZERO);
    heap.collect();
    let got = heap.scope(|m| {
        let mail = m.take_mail();
        let values: Vec<u64> = mail.iter().map(|&i| m.find::<NodeB>(i).expect("the letter is there").get(m).value).collect();
        for i in mail {
            assert!(m.release(i));
        }
        assert!(m.take_mail().is_empty());
        values
    });
    assert_eq!(got, [0, 1, 2], "letters survive a collection and come out earliest first");
    heap.collect();
    assert_eq!(heap.stats().live, 0);
}

#[test]
#[cfg_attr(miri, ignore = "Miri cannot map files")]
fn shrink_returns_empty_blocks_and_open_by_fd() {
    use std::os::unix::fs::MetadataExt;
    let path = shm_path("shrink");
    let (sa, sb) = (CrossHeapSpace::reserve().unwrap(), CrossHeapSpace::reserve().unwrap());
    let a = unsafe { sa.create_unchecked(&path, THREADS) }.unwrap();
    let b = unsafe { sb.open_fd_unchecked(a.fd().unwrap().try_clone_to_owned().unwrap(), THREADS) }.unwrap();
    let kept = list(&a, 100);
    drop(list(&a, 400_000));
    let full = std::fs::metadata(&path).unwrap().blocks();
    a.collect();
    let collected = std::fs::metadata(&path).unwrap().blocks();
    b.shrink();
    let shrunk = std::fs::metadata(&path).unwrap().blocks();
    assert!(collected <= full);
    assert!(shrunk * 4 < full, "shrinking returned the empty blocks ({full} ⇒ {collected} ⇒ {shrunk} units of 512 bytes)");
    let sig = a.scope(|m| m.sign(kept.get(m)));
    assert_eq!(b.scope(|m| m.resolve::<NodeB>(&sig).map(|n| n.value)), Some(99), "a mapping opened from the fd recognizes the signature");
    assert_eq!(sum(&a, &kept), triangle(100));
    let again = list(&a, 400_000);
    assert_eq!(sum(&a, &again), triangle(400_000), "returned blocks are used again");
    drop((kept, again, b, a));
    assert!(!path.exists());
}

/// A node allocated in a private heap that can be promoted into the cross-heap
#[repr(C)]
#[derive(Trace, Shared)]
struct SNode<'heap, 'scope> {
    value: u64,
    next: paragc::Slot<'heap, 'scope, SNode<'heap, 'scope>>,
}

brand!(SNodeB = for<'heap, 'scope> SNode<'heap, 'scope>);
cross_heap_brand!(SNodeX = for<'heap, 'scope> SNode<'heap, 'scope>);

/// A plain object in a private heap, pointing at a shareable node
#[derive(Trace)]
struct Keeper<'heap, 'scope> {
    node: paragc::Slot<'heap, 'scope, SNode<'heap, 'scope>>,
}

brand!(KeeperB = for<'heap, 'scope> Keeper<'heap, 'scope>);

fn snode_sum<'h>(heap: &Heap<'h>, root: &ScopeGlobal<'h, 'h, SNodeB>) -> (u64, bool) {
    heap.scope_shared(|m| {
        let (mut sum, mut all_crossed, mut cur) = (0, true, Some(m.scope_in(root)));
        while let Some(n) = cur {
            sum += n.value;
            all_crossed &= in_fixed(n.as_ptr().addr());
            cur = n.next.get();
        }
        (sum, all_crossed)
    })
}

/// The fixed-address region: 1 TiB from 64 TiB
const FIXED_BASE: usize = 64 << 40;
const FIXED_REGION: usize = 1 << 40;

fn in_fixed(addr: usize) -> bool {
    (FIXED_BASE..FIXED_BASE + FIXED_REGION).contains(&addr)
}

/// A process can reserve the fixed-address region only once at a time, so the promotion scenarios run one after another in a single test.
#[test]
#[cfg_attr(miri, ignore = "Miri has no fixed-address region")]
fn promotion_into_attached_cross_heap() {
    young_survivors_promote_into_attached_cross_heap();
    private_writes_into_cross_heap_objects_wait_for_the_next_young_collection();
    tails_travel_with_their_objects();
    cross_heap_objects_keep_private_edges();
    handles_live_in_the_cross_heap();
    small_tails_of_handles_survive_promotion();
}

fn young_survivors_promote_into_attached_cross_heap() {
    const LEN: u64 = 5000;
    let space = CrossHeapSpace::reserve_fixed().unwrap();
    let cross = unsafe { space.create_anonymous_unchecked(THREADS) }.unwrap();
    let mut heap = unsafe { Heap::new_unchecked(THREADS) };
    heap.attach_cross_heap(&cross).unwrap();
    let (root, keeper, weak) = heap.scope(|m| {
        let mut top: Option<paragc::ScopeLocal<'_, '_, SNode<'_, '_>>> = None;
        for value in 0..LEN {
            top = Some(m.alloc_shared(SNode { value, next: top.into() }));
        }
        let top = top.unwrap();
        let keeper = m.alloc(Keeper { node: top.next.get().into() });
        (m.global::<SNodeB>(top), m.global::<KeeperB>(keeper), m.weak::<SNodeB>(top))
    });
    assert_eq!(snode_sum(&heap, &root), (triangle(LEN), false));
    heap.collect_young();
    assert_eq!(snode_sum(&heap, &root), (triangle(LEN), true), "the whole chain was promoted into the cross-heap with values intact");
    let node_bytes = (HEADER + size_of::<SNode<'static, 'static>>()).next_multiple_of(8);
    let sig = heap.scope(|m| {
        let keeper = m.scope_in(&keeper);
        assert!(!in_fixed(keeper.as_ptr().addr()), "a non-shareable object stays in the private heap");
        assert_eq!(keeper.node.get().map(|n| n.value), Some(LEN - 2), "the slot in the private object was rewritten to the new address");
        assert!(m.upgrade(&weak).is_some_and(|n| in_fixed(n.as_ptr().addr())), "the weak reference followed the move");
        let sig = cross.scope(|cm| cm.sign(cm.adopt::<SNodeX, _>(m.scope_in(&root)).expect("in the cross-heap")));
        assert_eq!(cross.scope(|cm| cm.sign_promoted(m.scope_in(&root))), Some(sig), "signing without a brand gives the same signature");
        assert_eq!(cross.scope(|cm| cm.sign_promoted(keeper)), None, "a private object has no signature");
        sig
    });
    assert_eq!(cross.scope(|cm| cm.resolve::<SNodeX>(&sig).map(|n| n.value)), Some(LEN - 1), "the signature finds the promoted object");
    // Only the owner collects the cross-heap: collecting it on its own does nothing
    cross.collect();
    assert_eq!(cross.stats().collections, 0);
    // The owner's full collection: everything the private heap points at stays alive
    heap.collect();
    assert_eq!(cross.stats().live, node_bytes * usize::try_from(LEN).unwrap());
    assert_eq!(snode_sum(&heap, &root), (triangle(LEN), true));
    // Only the private keeper still points at the second node: the head dies and the rest live through the keeper
    drop(root);
    heap.collect();
    assert_eq!(cross.stats().live, node_bytes * usize::try_from(LEN - 1).unwrap());
    assert!(heap.scope(|m| m.upgrade(&weak).is_none()), "the cross-heap object died, so the private weak reference is cleared");
    assert!(cross.scope(|cm| cm.resolve::<SNodeX>(&sig).is_none()));
    drop(keeper);
    heap.collect();
    assert_eq!(cross.stats().live, 0);
}

fn private_writes_into_cross_heap_objects_wait_for_the_next_young_collection() {
    let space = CrossHeapSpace::reserve_fixed().unwrap();
    let cross = unsafe { space.create_anonymous_unchecked(THREADS) }.unwrap();
    let mut heap = unsafe { Heap::new_unchecked(THREADS) };
    heap.attach_cross_heap(&cross).unwrap();
    let root = heap.scope(|m| m.global::<SNodeB>(m.alloc_shared(SNode { value: 1, next: None.into() })));
    heap.collect_young();
    assert!(!heap.promotion_pending());
    heap.scope(|m| {
        let fresh = m.alloc_shared(SNode { value: 2, next: None.into() });
        m.scope_in(&root).next.set(m, Some(fresh));
    });
    assert!(heap.promotion_pending(), "a private object was written into a cross-heap object");
    heap.scope(|m| {
        let next = m.scope_in(&root).next.get().expect("just written");
        assert_eq!(next.value, 2, "the owner reads the private object as usual");
        assert!(!in_fixed(next.as_ptr().addr()), "still private until the next young collection");
        assert!(cross.scope(|cm| cm.adopt::<SNodeX, _>(next).is_none()), "other processes cannot see it yet");
    });
    heap.collect_young();
    assert!(!heap.promotion_pending());
    heap.scope(|m| {
        let next = m.scope_in(&root).next.get().expect("still there");
        assert_eq!(next.value, 2);
        assert!(in_fixed(next.as_ptr().addr()), "the young collection promoted it and rewrote the cross-heap slot");
        assert!(cross.scope(|cm| cm.adopt::<SNodeX, _>(next).is_some()));
    });
    let node_bytes = (HEADER + size_of::<SNode<'static, 'static>>()).next_multiple_of(8);
    heap.collect();
    assert_eq!(cross.stats().live, 2 * node_bytes);
    drop(root);
    heap.collect();
    assert_eq!(cross.stats().live, 0);
}

/// A byte string kept in the tail after its length
#[repr(C)]
#[derive(Trace, Shared)]
struct Bytes {
    len: u64,
}

brand!(BytesB = for<'heap, 'scope> Bytes);

impl Bytes {
    fn tail(&self) -> &[u8] {
        // SAFETY: allocated with alloc_tail / alloc_shared_tail and len bytes of tail
        unsafe { std::slice::from_raw_parts((&raw const *self).add(1).cast::<u8>(), usize::try_from(self.len).unwrap()) }
    }

    fn fill(len: usize) -> impl FnOnce(&mut [std::mem::MaybeUninit<u8>]) {
        move |tail| {
            assert_eq!(tail.len(), len);
            for (i, b) in tail.iter_mut().enumerate() {
                b.write(u8::try_from(i % 251).unwrap());
            }
        }
    }

    fn intact(&self) -> bool {
        self.tail().iter().enumerate().all(|(i, &b)| usize::from(b) == i % 251)
    }
}

fn tails_travel_with_their_objects() {
    const SMALL: usize = 1000;
    const BIG: usize = 300_000;
    let space = CrossHeapSpace::reserve_fixed().unwrap();
    let cross = unsafe { space.create_anonymous_unchecked(THREADS) }.unwrap();
    let mut heap = unsafe { Heap::new_unchecked(THREADS) };
    heap.attach_cross_heap(&cross).unwrap();
    let roots = heap.scope(|m| {
        let len = |n: usize| u64::try_from(n).unwrap();
        [
            m.global::<BytesB>(m.alloc_shared_tail(Bytes { len: len(SMALL) }, SMALL, Bytes::fill(SMALL))),
            m.global::<BytesB>(m.alloc_shared_tail(Bytes { len: len(BIG) }, BIG, Bytes::fill(BIG))),
            m.global::<BytesB>(m.alloc_tail(Bytes { len: len(SMALL) }, SMALL, Bytes::fill(SMALL))),
        ]
    });
    heap.collect_young();
    heap.collect();
    heap.scope(|m| {
        let [small, big, private] = roots.each_ref().map(|r| m.scope_in(r));
        assert!(in_fixed(small.as_ptr().addr()) && in_fixed(big.as_ptr().addr()), "shareable tails were promoted");
        assert!(!in_fixed(private.as_ptr().addr()));
        assert!(small.intact() && big.intact() && private.intact(), "the tails came along unchanged");
    });
}

/// A private object that can never be promoted (not shareable)
#[derive(Trace)]
struct Secret<'heap, 'scope> {
    value: u64,
    next: paragc::Slot<'heap, 'scope, Secret<'heap, 'scope>>,
}

brand!(SecretB = for<'heap, 'scope> Secret<'heap, 'scope>);

/// A shareable object pointing at a private one
#[repr(C)]
#[derive(Trace, Shared)]
struct Keeps<'heap, 'scope> {
    value: u64,
    secret: paragc::Slot<'heap, 'scope, Secret<'heap, 'scope>>,
    next: paragc::Slot<'heap, 'scope, Keeps<'heap, 'scope>>,
}

brand!(KeepsB = for<'heap, 'scope> Keeps<'heap, 'scope>);

/// A shareable type whose objects decide at each young collection whether they may be promoted
#[repr(C)]
#[derive(Trace)]
struct Gated {
    share: u64,
}

// SAFETY: no pointer slots
unsafe impl Shared for Gated {
    const NO_EDGES: bool = true;

    fn edges(_: &mut paragc::EdgeMap, _: u32) {}

    fn shareable(&self) -> bool {
        self.share != 0
    }
}

brand!(GatedB = for<'heap, 'scope> Gated);

fn secret_values<'h>(heap: &Heap<'h>, root: &ScopeGlobal<'h, 'h, KeepsB>) -> Vec<Option<u64>> {
    heap.scope_shared(|m| {
        let (mut out, mut cur) = (Vec::new(), Some(m.scope_in(root)));
        while let Some(h) = cur {
            assert!(in_fixed(h.as_ptr().addr()), "every holder was promoted");
            out.push(h.secret.get().map(|s| {
                assert!(!in_fixed(s.as_ptr().addr()), "secrets stay private");
                s.value + s.next.get().map_or(0, |n| n.value)
            }));
            cur = h.next.get();
        }
        out
    })
}

fn cross_heap_objects_keep_private_edges() {
    const HOLDERS: u64 = 300;
    let space = CrossHeapSpace::reserve_fixed().unwrap();
    let cross = unsafe { space.create_anonymous_unchecked(THREADS) }.unwrap();
    let mut heap = unsafe { Heap::new_unchecked(THREADS) };
    heap.attach_cross_heap(&cross).unwrap();
    let old = heap.scope(|m| m.global::<SecretB>(m.alloc(Secret { value: 1000, next: None.into() })));
    heap.collect_young();
    let (root, weaks) = heap.scope(|m| {
        let mut top = None;
        let mut weaks = Vec::new();
        for value in 0..HOLDERS {
            let tail = m.alloc(Secret { value: 1, next: None.into() });
            let secret = if value % 3 == 0 { m.scope_in(&old) } else { m.alloc(Secret { value, next: Some(tail).into() }) };
            weaks.push(m.weak::<SecretB>(secret));
            top = Some(m.alloc_shared(Keeps { value, secret: Some(secret).into(), next: top.into() }));
        }
        (m.global::<KeepsB>(top.unwrap()), weaks)
    });
    let expected: Vec<Option<u64>> = (0..HOLDERS).rev().map(|v| Some(if v % 3 == 0 { 1000 } else { v + 1 })).collect();
    heap.collect_young();
    assert_eq!(secret_values(&heap, &root), expected, "holders were promoted, their secrets stayed private and readable");
    for round in 0..4 {
        heap.scope(|m| {
            for value in 0..20_000 {
                m.alloc(Secret { value, next: None.into() });
            }
        });
        heap.collect();
        assert_eq!(secret_values(&heap, &root), expected, "pinned targets survive full collection {round}");
    }
    let fresh = heap.scope(|m| {
        let h = m.scope_in(&root);
        let fresh = m.alloc(Secret { value: 7, next: Some(m.alloc(Secret { value: 3, next: None.into() })).into() });
        h.secret.set(m, Some(fresh));
        m.weak::<SecretB>(fresh)
    });
    heap.collect_young();
    heap.collect();
    let mut changed = expected.clone();
    changed[0] = Some(10);
    assert_eq!(secret_values(&heap, &root), changed, "a private object written into a cross-heap slot is pinned");
    heap.scope(|m| m.scope_in(&root).secret.set(m, None));
    heap.collect();
    changed[0] = None;
    assert_eq!(secret_values(&heap, &root), changed);
    assert!(heap.scope(|m| m.upgrade(&fresh).is_none()), "no longer pinned, so collected");
    let gated = heap.scope(|m| [0, 1].map(|share| m.global::<GatedB>(m.alloc_shared(Gated { share }))));
    heap.collect_young();
    heap.scope(|m| {
        assert!(!in_fixed(m.scope_in(&gated[0]).as_ptr().addr()), "an object that declines stays private");
        assert!(in_fixed(m.scope_in(&gated[1]).as_ptr().addr()));
    });
    drop((root, gated, old));
    heap.collect();
    assert_eq!(cross.stats().live, 0);
    heap.collect();
    assert!(heap.scope(|m| weaks.iter().all(|w| m.upgrade(w).is_none())), "secrets held only by dead cross-heap objects go one full collection later");
}

/// A shareable object reached through a handle
#[repr(C)]
#[derive(Trace, Shared)]
struct Key {
    value: u64,
}

/// A shareable object holding handles
#[repr(C)]
#[derive(Trace, Shared)]
struct Keyed<'heap, 'scope> {
    key: paragc::HandleSlot<'heap, 'scope, Key>,
    next: paragc::Slot<'heap, 'scope, Keyed<'heap, 'scope>>,
}

brand!(KeyedB = for<'heap, 'scope> Keyed<'heap, 'scope>);
cross_heap_brand!(KeyedX = for<'heap, 'scope> Keyed<'heap, 'scope>);

/// A private object holding a handle
#[derive(Trace)]
struct KeyKeeper<'heap, 'scope> {
    key: paragc::HandleSlot<'heap, 'scope, Key>,
}

brand!(KeyKeeperB = for<'heap, 'scope> KeyKeeper<'heap, 'scope>);

fn keyed<'h>(heap: &Heap<'h>, root: &ScopeGlobal<'h, 'h, KeyedB>) -> Vec<(usize, u64)> {
    heap.scope_shared(|m| {
        let (mut out, mut cur) = (Vec::new(), Some(m.scope_in(root)));
        while let Some(k) = cur {
            let h = k.key.get().expect("every holder has a key");
            out.push((h.id(), h.value));
            cur = k.next.get();
        }
        out
    })
}

fn handles_live_in_the_cross_heap() {
    const KEYS: u64 = 500;
    let space = CrossHeapSpace::reserve_fixed().unwrap();
    let cross = unsafe { space.create_anonymous_unchecked(THREADS) }.unwrap();
    let mut heap = unsafe { Heap::new_unchecked(THREADS) };
    heap.attach_cross_heap(&cross).unwrap();
    let (root, keeper) = heap.scope(|m| {
        let mut top = None;
        let mut first = None;
        for value in 0..KEYS {
            let key = m.alloc_shared_handle(Key { value });
            assert!(in_fixed(key.id()), "the handle entry lives in the cross-heap");
            first.get_or_insert(key);
            top = Some(m.alloc_shared(Keyed { key: key.into(), next: top.into() }));
        }
        let keeper = m.alloc(KeyKeeper { key: first.into() });
        (m.global::<KeyedB>(top.unwrap()), m.global::<KeyKeeperB>(keeper))
    });
    let before = keyed(&heap, &root);
    assert_eq!(before.iter().map(|&(_, v)| v).collect::<Vec<_>>(), (0..KEYS).rev().collect::<Vec<_>>());
    heap.collect_young();
    assert_eq!(keyed(&heap, &root), before, "promotion keeps every handle's identity and value");
    heap.scope(|m| {
        let top = m.scope_in(&root);
        assert!(in_fixed(top.as_ptr().addr()) && in_fixed(top.key.get().unwrap().get().as_ptr().addr()), "holders and keys moved over");
        let sum: u64 = cross.scope(|cm| {
            let (mut sum, mut cur) = (0, cm.adopt::<KeyedX, _>(top));
            while let Some(k) = cur {
                sum += k.key.get().unwrap().value;
                cur = k.next.get().and_then(|n| cm.adopt::<KeyedX, _>(n));
            }
            sum
        });
        assert_eq!(sum, triangle(KEYS - 1) + KEYS - 1, "the cross-heap's own view follows the handles");
    });
    for _ in 0..3 {
        heap.collect();
        assert_eq!(keyed(&heap, &root), before, "handles survive full collections");
    }
    let key_bytes = (HEADER + size_of::<Key>()).next_multiple_of(8);
    let keyed_bytes = (HEADER + size_of::<Keyed<'static, 'static>>()).next_multiple_of(8);
    assert_eq!(cross.stats().live, usize::try_from(KEYS).unwrap() * (key_bytes + keyed_bytes));
    drop(root);
    heap.collect();
    assert_eq!(cross.stats().live, key_bytes, "only the key held by the private keeper is left");
    assert_eq!(heap.scope(|m| m.scope_in(&keeper).key.get().map(|k| k.value)), Some(0));
    drop(keeper);
    heap.collect();
    assert_eq!(cross.stats().live, 0);
    let ids: Vec<usize> = before.iter().map(|&(id, _)| id).collect();
    let reused = heap.scope(|m| m.alloc_shared_handle(Key { value: 9 }).id());
    assert!(ids.contains(&reused), "dead handles returned their entries");
    let (bytes, id) = heap.scope(|m| {
        let b = m.alloc_shared_handle_tail(Bytes { len: 5000 }, 5000, Bytes::fill(5000));
        (m.global::<BytesB>(b.get()), b.id())
    });
    heap.collect_young();
    heap.scope(|m| {
        let b = m.scope_in(&bytes);
        assert!(in_fixed(b.as_ptr().addr()) && b.intact(), "a handle object's tail moves over with it");
        assert_eq!(unsafe { paragc::Handle::<Bytes>::from_id(id) }.get().as_ptr(), b.as_ptr(), "the handle follows it");
    });
}

fn small_tails_of_handles_survive_promotion() {
    let space = CrossHeapSpace::reserve_fixed().unwrap();
    let cross = unsafe { space.create_anonymous_unchecked(THREADS) }.unwrap();
    let mut heap = unsafe { Heap::new_unchecked(THREADS) };
    // Handles taken before the cross-heap is attached live in the private handle table; their objects are promoted all the same
    let early = heap.scope(|m| (0..50usize).map(|i| m.global::<BytesB>(m.alloc_shared_handle_tail(Bytes { len: 24 + i as u64 }, 24 + i, Bytes::fill(24 + i)).get())).collect::<Vec<_>>());
    heap.attach_cross_heap(&cross).unwrap();
    let all = heap.scope(|m| {
        (0..2000usize)
            .map(|i| {
                let len = 8 + i % 40;
                let g = if i % 2 == 0 { m.alloc_shared_handle_tail(Bytes { len: len as u64 }, len, Bytes::fill(len)).get() } else { m.alloc_shared_tail(Bytes { len: len as u64 }, len, Bytes::fill(len)) };
                m.alloc(Secret { value: i as u64, next: None.into() });
                std::mem::forget(m.weak::<BytesB>(g));
                m.global::<BytesB>(g)
            })
            .collect::<Vec<_>>()
    });
    heap.collect();
    heap.collect();
    heap.scope(|m| {
        for (i, g) in all.iter().enumerate() {
            let b = m.scope_in(g);
            assert!(in_fixed(b.as_ptr().addr()), "{i} promoted");
            assert!(b.intact(), "{i}: tail of {} bytes intact", b.len);
        }
        assert!(early.iter().all(|g| m.scope_in(g).intact()));
    });
    drop((all, early));
    heap.collect();
    assert_eq!(cross.stats().live, 0);
}
