<a id="top"></a>
<div align="center">

# paragc

### A parallel garbage collector for Rust.
**Immix-style mark-region heap, many mutator threads at once, concurrent and parallel marking and sweeping. No locks.**

[![Rust](https://img.shields.io/badge/Rust-100%25-orange?style=for-the-badge&logo=rust)](https://www.rust-lang.org)
[![Miri](https://img.shields.io/badge/Miri-clean-success?style=for-the-badge)](#-verification)
[![Lock-free](https://img.shields.io/badge/locks-none-blue?style=for-the-badge)](#-how-it-works)
[![License](https://img.shields.io/badge/License-MIT-brightgreen?style=for-the-badge)](#-license)

[![Heap](https://img.shields.io/badge/heap-mark--region%20%C2%B7%20evacuating-8A2BE2?style=flat-square)](#-how-it-works)
[![Mark](https://img.shields.io/badge/mark-parallel%20%C2%B7%20work%20stealing-262D3A?style=flat-square)](#-how-it-works)
[![std](https://img.shields.io/badge/std-required-283272?style=flat-square)](#-rules)

**🇺🇸 English** · [🇨🇳 简体中文](README.zh-CN.md)

</div>

---

## ⚡ What is paragc

paragc is **a tracing garbage collector for Rust programs that share one heap across many threads**. Objects live in
32 KiB blocks cut into 256 B lines (the Immix layout); threads bump-allocate into free lines on their own, and a
collection marks and sweeps on every core.

## 🔥 Why it's different

| | |
| --- | --- |
| 🧵 **Many mutators** | `Heap::mutate(&self)` runs on any number of threads at once. Each bump-allocates in its own buffer (`Tlab`) and claims whole blocks with one atomic operation. |
| 🔒 **Exclusive by borrowing** | `Heap::collect(&mut self)` cannot overlap a `mutate`: the borrow checker is the stop-the-world, so there is no safepoint protocol and no lock. |
| 🌊 **Concurrent marking** | `Heap::mark_concurrently(&self)` marks while mutators keep running (a snapshot-at-the-beginning barrier on `Slot` writes, new objects allocated black). Mutators wait only for a brief handshake at the start. |
| 🧹 **Concurrent sweeping** | `Heap::finish_marking(&mut self)` only finishes marking and hands the blocks to the sweeper; mutators sweep a block themselves when they need one (lazy sweeping) while `Heap::sweep_concurrently(&self)` sweeps the rest on the pool. With both, the pause is just draining the grey queue. |
| 🏃 **Parallel collection** | A persistent pool per heap (threads park between collections). Marking: one crossbeam work-stealing deque per worker, stealing in batches. Sweeping, destructors included: blocks handed out in batches to the same workers. |
| 🏷️ **Branded handles** | `Gc<'gc, T>` cannot escape the `mutate` closure. `Root<B>` handles carry no lifetime and can be stored anywhere and sent across threads. |
| 📦 **Opportunistic evacuation** | A stop-the-world collection moves the live objects out of sparse blocks while it marks (Immix defragmentation): one pass marks, copies and rewrites every edge. Concurrent marking never moves anything, so mutators need no read barrier. |
| 🐣 **Young collections** | `Heap::collect_young` traces only objects allocated since the last collection, starting from the roots and the slots the write barrier logged, and sweeps only the blocks allocated into since; old objects are not touched. `collect_if_needed` runs young collections and a full one every few rounds (the first stage of LXR-style young-object reclamation). |
| 🔢 **Reference counting** | Old objects carry a small reference count, maintained by the `Slot` write barrier (each slot logged once per round) and settled at young collections. The pause only applies increments and queues the decrements; `sweep_concurrently` frees objects whose count drops to zero on the pool, cascading, while mutators run, and their lines return to the allocator at the next pause. Tracing (a full collection) runs rarely, for cycles and objects whose count overflowed (LXR stages two and three). |
| 🛡️ **Checked at compile time** | `#[derive(Trace)]` rejects `Drop` on traced types, and object data aligned above 8 bytes is a compile error. |

## 🚀 Usage

```rust
use paragc::{Gc, Heap, Root, Trace, brand};

#[derive(Trace)]
struct Node<'gc> {
    value: u64,
    next: Option<Gc<'gc, Node<'gc>>>,
}

// A root handle has no `'gc`; the brand puts it back on when you read it
brand!(NodeB = for<'gc> Node<'gc>);

let mut heap = Heap::default();
let list: Root<NodeB> = heap.mutate(|m| {
    let mut head = None;
    for value in 0..1000 {
        head = Some(m.alloc(Node { value, next: head }));
    }
    m.root(head.unwrap())
});
heap.collect();
let sum = heap.mutate(|m| {
    let mut sum = 0;
    let mut cur = Some(m.get(&list));
    while let Some(n) = cur {
        sum += n.value;
        cur = n.next;
    }
    sum
});
assert_eq!(sum, 999 * 1000 / 2);
```

## 🧠 How it works

| Part | Design |
| --- | --- |
| Blocks | Aligned to 32 KiB, so masking an object address finds its block. The block header holds line marks (one `u32` epoch per line) and an object-start bitmap (one bit per 8 bytes). |
| Allocation | A buffer owns one block and bumps through runs of lines whose mark ≠ the current epoch; when a run is too small it moves to the next one, then takes a block with free lines, then an empty block, then a new one. Objects over 8 KiB are allocated on their own. |
| Object header | 24 bytes: trace function, drop function, mark epoch (atomic), size in granules (top bit = large). |
| Marking | Roots are spread over the workers. Whoever swaps in the epoch first owns the object: it marks the lines it spans and pushes it to its own deque (types without edges are not pushed). |
| Termination | A worker whose deque is empty and that cannot steal counts itself idle and keeps stealing; when every worker is idle, marking is done. |
| Thread pool | Started at the first collection, lives as long as the heap. A collection is handed out by bumping one atomic word (generation + number of participants) and unparking only those workers; idle workers back off, then park. A panic on a worker is re-raised on the collecting thread. |
| Sweeping | Workers claim blocks in batches, walk the start bitmap, drop unmarked objects and classify each block as empty / recyclable / full. Spare empty blocks beyond a quarter of the heap go back to the system. |
| Roots | Each `Root<B>` owns a slot in an append-only table; freed slot numbers are folded into a read-only array at collection time and claimed with an atomic cursor during `mutate`. |
| Stale buffers | A buffer remembers (heap, collection count); after a collection it starts over, because its block was re-classified. |
| Concurrent marking | Start: finish the previous sweep (it judges by "mark ≠ that epoch", so it must end before the next marking stamps live objects with a newer one), raise a request bit in the gate word (top bit; low bits count threads inside `mutate`), wait until no mutator is inside, mark the roots grey, drop the request. paragc does not scan stacks, so the snapshot has to be taken when no mutator holds an unrooted `Gc`. Then mark on the pool. During marking, a `Slot` write marks the value it overwrites and puts it in the heap's grey queue (snapshot at the beginning), new objects get the marking epoch and their lines marked (black). A line is free when its mark is older than the last finished epoch, so lines the marker has just stamped are never handed out. `finish_marking` drains the grey queue in parallel. |
| Concurrent sweeping | `finish_marking` hands every block and large object to the sweeper and returns. Claiming is an atomic cursor, so each block is swept exactly once by whoever claims it, before it is handed out: a mutator that needs a block first takes a swept one, otherwise claims an unswept block and sweeps it on the spot; `sweep_concurrently` sweeps the rest on the pool and waits for blocks others are still sweeping. New objects carry the last finished epoch, so allocations during sweeping are never taken for dead. `collect` = `finish_marking` + `sweep_concurrently`. |
| Evacuation | Only in a stop-the-world `finish_marking` (no concurrent marking in progress). Candidates: blocks that have not been allocated into since they were swept and hold less than a quarter of a block of live data; at least 4, at most 1024. The other swept blocks become copy targets. While marking, the first worker to reach an object in a candidate block claims it (mark word: copying), copies it into its own copy buffer (the new copy is marked, with start bit and line marks) and leaves a forwarding address in the old header; others wait or take the new address. Every edge is rewritten through `Trace::relocate`; root slots with a CAS, so a handle dropped on another thread meanwhile stays dropped. Sweeping clears the old copies' start bits without running their destructors. |
| Young collections | Sticky mark bits: objects marked with the last finished epoch are old, new objects start at 0 and are young. A young collection marks with that same epoch, so an old object stops the trace and a reached young object is promoted. Old → young edges can only come from `Slot` writes, so outside concurrent marking the barrier logs the slot whenever the value written is young; during concurrent marking every new object is black and survives as old, so nothing is logged. Only blocks allocated into since their last sweep are swept; the log is cleared by every collection. |
| Reference counting | RC(x) = edges to x from old objects; edges from a young object are counted when it is promoted. The count lives in the header next to the size (6 bits, sticky when full, plus a parity bit: a stop-the-world full collection flips it and recounts every edge exactly while it marks). A `Slot` remembers the value it last counted; an old object's slot is logged on its first write in a round, and a young collection settles it (new value +1, last counted −1), adds 1 for each current root and takes 1 off the previous round's roots (deferred root counting). Decrements run after all increments; an old object reaching zero has its edges decremented, is finalized and its start bit cleared, and the lines of affected blocks are recomputed. Concurrent marking recounts too (see below). With a single marking worker nobody else touches the counts, so the recount uses plain loads and stores instead of locked atomic read-modify-writes. |
| Background release | `finish_young` (the pause) settles increments and pushes decrements onto a queue; `collect_young` = `finish_young` + `sweep_concurrently`. Workers pop a decrement; whoever brings a count to zero finalizes the object, clears its start bit, queues decrements for its edges and records the block as dirty. Objects at zero have no counted references, and every root and promoted young object was counted, so mutators cannot reach them. Dirty blocks get their lines recomputed from the old objects left at the start of the next pause; a block allocated into since its last sweep keeps that status and is left to the young sweep (it may hold unswept dead young objects). Every collection first finishes pending decrements, before a recount could flip the parity under them. `collect_if_needed` runs a full collection every 16 young ones while counts are trusted, every 4 otherwise. |
| Concurrent recount | The handshake flips the parity and drops the slots logged before it (their old values belong to the previous counts; live containers get rescanned). Markers count every edge they scan. A slot is logged on its first write in the round during marking too; a marker that recounts a slot rereads it afterwards and logs it itself if a mutator wrote it in between (a young object's slot is not counted yet, so the mutator would not have logged it). Objects allocated black are queued and their edges counted at the finish; logged slots are then settled (new value +1, value last counted −1, decrements only for objects marked this round, since the dead ones are about to be swept). Counts are trusted again as soon as the finish returns. |

Work-stealing deques and backoff come from [crossbeam](https://github.com/crossbeam-rs/crossbeam) (`crossbeam-deque`,
`crossbeam-utils`); the append-only segmented vector and the block stock come from
[j8lockfree](https://crates.io/crates/j8lockfree).

## 📏 Rules

- Heap types implement `Trace` (use `#[derive(Trace)]`) and must be `Send + Sync`: objects are read from any thread
  and traced and dropped on collector threads. Mutable edges go in `Slot<'gc, T>` (an atomic pointer); writing one
  takes the `Mutator` (`slot.set(m, v)`), so the barrier can see concurrent marking.
- A `Slot` is two words (the value and the value last counted). A single object's data is at most 128 MiB.
- Objects can move during a stop-the-world collection. A `Gc` is only valid inside one `mutate`, so this is invisible,
  but `Gc::as_ptr` may differ from one `mutate` to the next. Hand-written `Trace` impls with edges must also implement
  `relocate` (the derive does it).
- Never call `mark_concurrently` from inside a `mutate` on the same heap: it would wait for itself to leave.
- A type with `Gc` fields must not implement `Drop`: destructors run in no particular order, so a `Drop` could see
  objects that were already dropped. The derive checks this; `#[trace(unsafe_drop)]` opts out.
- Only objects reachable from a `Root` survive a collection.
- A root released while a collection is running may keep its object alive until the next one.
- `std` only.

## ✅ Verification

| Test | What it covers |
| --- | --- |
| `rooted_list_survives` · `garbage_is_finalized` · `heap_drop_finalizes_everything` | Survival, exact finalization counts, dropping the heap drops everything |
| `threads_mutate_at_once` · `blocks_are_reused` · `large_objects` | Concurrent mutators, line reuse keeps the block count flat, large objects |
| `deep_binary_tree` | A 20-level complete binary tree (≈1M nodes): cut half, then all, exact counts |
| `deep_chain` | A 1M-node chain cut in the middle; depth never touches the call stack |
| `eight_threads_eight_heaps_crossed` | 8 threads × 8 heaps: roots held across heaps, checked and released on other threads, released while all heaps collect in parallel, cycles through large objects |
| `crossed_deep_trees` | 8 heaps, each with a deep chain and a 15-level tree whose leaves anchor into the next heap's chain; every cut matches the expected finalization count |
| `concurrent_marking_while_relinking` | Mutators keep unlinking nodes (leaving them only on their stacks) and relinking them elsewhere, allocating and discarding, while another thread marks concurrently; after every cycle the reachable count is exact, and finalization equals the discards |
| `concurrent_sweeping_while_relinking` | The same mutators across concurrent marking, the `finish_marking` pause and concurrent sweeping (they sweep blocks lazily while another thread runs `sweep_concurrently`); nothing still linked is ever finalized, counts stay exact |
| `evacuation_compacts_sparse_blocks` | Keep 1 object in 16: the next collection evacuates the sparse blocks; fewer blocks, values intact, addresses changed, finalization exact |
| `evacuation_with_shared_children` | Many parents share a few children (one `Slot` and one plain field each), so several workers race to claim the same child; both edges of every parent end up at the same new copy, nothing is finalized twice |
| `young_collection_frees_only_young_garbage` | Young objects hung off an old object's slot, off an old node and under another young node survive; a young collection finalizes exactly the discarded young objects (by tracing) and the discarded old ones (by reference count) |
| `young_collections_while_relinking` | Mutators moving nodes between old and young objects, with young collections, concurrent marking finished by a young collection, and full collections in turn; counts exact every round |
| `background_release_while_mutating` | An old chain dropped at once: `finish_young` returns with nothing freed yet; the cascade then runs on the pool while mutators keep relinking, freeing nothing still linked; at the next pause the freed lines are reused |
| `concurrent_marking_recounts` | After one concurrent mark (mutators relinking, black objects hanging nodes under themselves) the counts are trusted at once: a dropped old chain is freed by count at the next young collection, and later rounds never free a linked node |
| `reference_counting_leaves_cycles_to_tracing` | Chains of old objects dropped at once are freed by their counts at a young collection (a 500-deep cascade); two-node rings stay until a full collection |

All of them also run under Miri with both Stacked Borrows and Tree Borrows (scaled down under `cfg(miri)`).
`-Zmiri-ignore-leaks` is needed because crossbeam-epoch's global collector still holds deferred garbage at exit; at
4 collector threads per heap Stacked Borrows also reports a violation inside crossbeam-epoch itself, so the tests use 2
under Miri; the two concurrent tests still hit that crossbeam-epoch violation under Stacked Borrows, and are clean under
Tree Borrows:

```sh
cargo test --test gc -- --exact <name>
MIRIFLAGS="-Zmiri-permissive-provenance -Zmiri-ignore-leaks" cargo +nightly miri test --test gc -- --exact <name>
MIRIFLAGS="-Zmiri-permissive-provenance -Zmiri-ignore-leaks -Zmiri-tree-borrows" cargo +nightly miri test --test gc -- --exact <name>
```

## 📊 Benchmarks

Against cppgc (V8's Oilpan), gc-arena, gc (rust-gc) and dumpster on a 16-core machine; wall ms, best of 5. Setup,
workloads, every configuration and how to read them: **[benches/benchmark.md](benches/benchmark.md)**.

| | paragc | cppgc | gc-arena | dumpster | gc |
| --- | ---: | ---: | ---: | ---: | ---: |
| Pause, 1M live + 1M dead, 1 thread | 25.7 | 45.0 | **15.8** | 16.4 | 87.6 |
| Pause, 16 threads | **4.3** | 43.4 | – | – | – |
| Pause, concurrent marking + sweeping, 1 thread | **0.03** | – | – | – | – |
| GCBench, 1 thread | **51.3** | 119.2 | 115.3 | 102.2 | 281.7 |
| GCBench, 4 threads | **55.4** | 95.3 | – | – | – |
| Many mutators, 16 threads | **9.2** | – | – | 334.3 | – |

Serially gc-arena and dumpster pause shorter (paragc's full collection also rebuilds reference counts); with threads,
and in throughput, paragc leads. cppgc's forced full
collection marks on one thread even with every parallel flag on, and pays an FFI call per allocation and edge here.

## 🗺️ Roadmap

Done: parallel stop-the-world collection, a persistent pool, concurrent marking, concurrent and lazy sweeping,
opportunistic evacuation, young collections, reference counting for old objects with decrements processed concurrently,
full tracing run only every 16 young collections, concurrent marking that recounts, and a cheaper single-threaded
recount. Possible next steps: a concurrent young collection, and a smaller `Slot` (it is two words to remember the
value last counted).

## 📜 License

MIT © Jabberwocky238

<div align="right"><a href="#top">⬆ back to top</a></div>
