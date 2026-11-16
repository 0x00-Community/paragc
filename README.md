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

Work-stealing deques and backoff come from [crossbeam](https://github.com/crossbeam-rs/crossbeam) (`crossbeam-deque`,
`crossbeam-utils`); the append-only segmented vector and the block stock come from
[j8lockfree](https://crates.io/crates/j8lockfree).

## 📏 Rules

- Heap types implement `Trace` (use `#[derive(Trace)]`) and must be `Send + Sync`: objects are read from any thread
  and traced and dropped on collector threads. Mutable edges go in `Slot<'gc, T>` (an atomic pointer); writing one
  takes the `Mutator` (`slot.set(m, v)`), so the barrier can see concurrent marking.
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

`benches/` holds one crate per collector (`benches/paragc`, `benches/cppgc`, `benches/gc-arena`, `benches/rust-gc`,
`benches/dumpster`) in a workspace of its own. `benches/run.sh` runs every configuration 5 times and keeps the fastest;
the CPU column is the whole process (all threads), so CPU ÷ wall is how many threads were actually busy.
Intel Core Ultra 7 356H (16 cores), Linux 7.0, Rust 1.98, load average below 3 unless noted.

**Pause:** one full collection with 2^20 − 1 live and 2^20 − 1 dead binary-tree nodes, wall ms (CPU ÷ wall)

| | 1 thread | 4 threads | 16 threads |
| --- | ---: | ---: | ---: |
| **paragc** | 22.3 | 7.6 (3.9) | **4.0** (13.5) |
| paragc, concurrent marking ¹ | 6.2 | 2.4 | 1.7 |
| paragc, concurrent sweeping ¹ | 14.6 | 5.3 | 2.5 |
| **paragc, concurrent marking + sweeping** ¹ | **0.03** | **0.04** | **0.05** |
| cppgc, attached to an isolate | 45.0 | 44.4 (1.0) | 44.6 (1.0) |
| cppgc, all parallel / concurrent marking flags | 44.7 | 43.3 (1.0) | 44.5 (1.0) |
| cppgc, detached heap | 42.3 | 43.4 (1.0) | 43.7 (1.0) |
| gc-arena | 16.6 | – | – |
| dumpster | 16.8 | – | – |
| gc (rust-gc) | 89.7 | – | – |

¹ Only the time mutators are stopped is measured: `mark_concurrently` runs first and is not timed; with concurrent
sweeping only `finish_marking` is timed and `sweep_concurrently` runs afterwards. No mutator ran during marking here, so
the grey queue was empty and the last pause is bookkeeping proportional to the number of blocks; with mutators running,
draining what the barrier logged is added to it.

**GCBench:** Boehm's benchmark (many short-lived trees plus one long-lived tree), each collector on its own schedule, total ms

| | 1 thread | 4 threads | 16 threads |
| --- | ---: | ---: | ---: |
| **paragc** | 114.3 | **58.6** (1.5) | 67.6 (2.7) |
| cppgc, attached to an isolate | 120.1 | 97.6 (1.3) | 102.3 (1.3) |
| cppgc, all parallel / concurrent marking flags | 114.5 | 97.3 (1.3) | 98.9 (1.3) |
| cppgc, detached heap | 174.5 | 174.3 (1.0) | 172.1 (1.0) |
| gc-arena | 117.7 | – | – |
| dumpster | 102.8 | – | – |
| gc (rust-gc) | 290.7 | – | – |

**Many mutators:** 2048 depth-10 trees in total, split over N allocating threads, total ms

| | 1 thread | 4 threads | 16 threads |
| --- | ---: | ---: | ---: |
| **paragc** | 70.4 | 14.5 (3.5) | **12.7** (8.2) |
| dumpster (`sync`) | 116.0 | 272.6 (3.9) | 340.8 (13.0) |

Reading these honestly:

- **Single-threaded, paragc is not the fastest.** gc-arena and dumpster pause in about 17 ms against paragc's 22.3 ms,
  and dumpster wins GCBench on one thread. paragc pulls ahead once it has threads.
- **Pauses scale to 16 threads; GCBench does not.** The persistent pool brought the 16-thread pause from 9.4 ms to
  4.0 ms, but GCBench is still fastest at 4 threads: its collections are small, and waking 16 workers does not pay off.
- **Concurrent marking and sweeping take both out of the pause.** The 20.9 ms single-thread pause becomes 0.03 ms;
  the work still happens, on the pool and in mutators that sweep the blocks they take.
- **cppgc marks the atomic pause on one thread.** With 16 worker threads and every parallel / concurrent marking flag
  on, a full collection still runs at CPU ÷ wall = 1.0; cppgc only uses other threads for background marking during
  incremental collections (GCBench, about 1.3). It is driven through rusty_v8, so every allocation, every `Member` and
  every traced edge crosses Rust ⇄ C++; attached to an isolate, V8's own (nearly empty) heap adds about 1.3–1.5 ms per
  full collection.
- **dumpster is reference counting:** these trees have no cycles, so dead nodes are freed the moment their last
  reference goes, and its "pause" is really the cost of dropping the tree.

## 🗺️ Roadmap

Done: parallel stop-the-world collection, a persistent pool, concurrent marking, concurrent and lazy sweeping,
opportunistic evacuation. Next:

1. Reference counting for young objects in the style of LXR: dead young objects free their lines at once, tracing
   handles cycles and old objects

## 📜 License

MIT © Jabberwocky238

<div align="right"><a href="#top">⬆ back to top</a></div>
