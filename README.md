<a id="top"></a>
<div align="center">

# paragc

### A parallel garbage collector for Rust.
**Immix-style mark-region heap, many mutator threads at once, concurrent and parallel marking, parallel sweeping. No locks.**

[![Rust](https://img.shields.io/badge/Rust-100%25-orange?style=for-the-badge&logo=rust)](https://www.rust-lang.org)
[![Miri](https://img.shields.io/badge/Miri-clean-success?style=for-the-badge)](#-verification)
[![Lock-free](https://img.shields.io/badge/locks-none-blue?style=for-the-badge)](#-how-it-works)
[![License](https://img.shields.io/badge/License-MIT-brightgreen?style=for-the-badge)](#-license)

[![Heap](https://img.shields.io/badge/heap-mark--region%20%C2%B7%20non--moving-8A2BE2?style=flat-square)](#-how-it-works)
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
| 🌊 **Concurrent marking** | `Heap::mark_concurrently(&self)` marks while mutators keep running (a snapshot-at-the-beginning barrier on `Slot` writes, new objects allocated black); the following `collect` only drains what the barrier logged and sweeps. Mutators wait only for a brief handshake at the start. |
| 🏃 **Parallel collection** | A persistent pool per heap (threads park between collections). Marking: one crossbeam work-stealing deque per worker, stealing in batches. Sweeping, destructors included: blocks handed out in batches to the same workers. |
| 🏷️ **Branded handles** | `Gc<'gc, T>` cannot escape the `mutate` closure. `Root<B>` handles carry no lifetime and can be stored anywhere and sent across threads. |
| 📌 **Non-moving** | Objects never move, so pointers stay plain pointers and nothing needs forwarding. |
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
| Concurrent marking | Start: raise a request bit in the gate word (top bit; low bits count threads inside `mutate`), wait until no mutator is inside, mark the roots grey, drop the request. paragc does not scan stacks, so the snapshot has to be taken when no mutator holds an unrooted `Gc`. Then mark on the pool. During marking, a `Slot` write marks the value it overwrites and puts it in the heap's grey queue (snapshot at the beginning), new objects get the marking epoch and their lines marked (black). A line is free when its mark is older than the last finished epoch, so lines the marker has just stamped are never handed out. `collect` finishes: drain the grey queue in parallel, then sweep. |

Work-stealing deques and backoff come from [crossbeam](https://github.com/crossbeam-rs/crossbeam) (`crossbeam-deque`,
`crossbeam-utils`); the append-only segmented vector and the block stock come from
[j8lockfree](https://crates.io/crates/j8lockfree).

## 📏 Rules

- Heap types implement `Trace` (use `#[derive(Trace)]`) and must be `Send + Sync`: objects are read from any thread
  and traced and dropped on collector threads. Mutable edges go in `Slot<'gc, T>` (an atomic pointer); writing one
  takes the `Mutator` (`slot.set(m, v)`), so the barrier can see concurrent marking.
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

All of them also run under Miri with both Stacked Borrows and Tree Borrows (scaled down under `cfg(miri)`).
`-Zmiri-ignore-leaks` is needed because crossbeam-epoch's global collector still holds deferred garbage at exit; at
4 collector threads per heap Stacked Borrows also reports a violation inside crossbeam-epoch itself, so the tests use 2
under Miri; `concurrent_marking_while_relinking` still hits that crossbeam-epoch violation under Stacked Borrows, and is
clean under Tree Borrows:

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
| **paragc, after concurrent marking** ¹ | 9.9 | 2.9 | **2.2** |
| cppgc, attached to an isolate | 45.0 | 44.4 (1.0) | 44.6 (1.0) |
| cppgc, all parallel / concurrent marking flags | 44.7 | 43.3 (1.0) | 44.5 (1.0) |
| cppgc, detached heap | 42.3 | 43.4 (1.0) | 43.7 (1.0) |
| gc-arena | 16.6 | – | – |
| dumpster | 16.8 | – | – |
| gc (rust-gc) | 89.7 | – | – |

¹ `mark_concurrently` first (mutators would keep running meanwhile), then only the finishing `collect` is timed: what
is left is draining the barrier's grey objects and sweeping. Measured at load average about 8.

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
- **Concurrent marking takes marking out of the pause.** What remains is sweeping, which runs the destructors of a
  million dead objects; concurrent sweeping is next on the roadmap.
- **cppgc marks the atomic pause on one thread.** With 16 worker threads and every parallel / concurrent marking flag
  on, a full collection still runs at CPU ÷ wall = 1.0; cppgc only uses other threads for background marking during
  incremental collections (GCBench, about 1.3). It is driven through rusty_v8, so every allocation, every `Member` and
  every traced edge crosses Rust ⇄ C++; attached to an isolate, V8's own (nearly empty) heap adds about 1.3–1.5 ms per
  full collection.
- **dumpster is reference counting:** these trees have no cycles, so dead nodes are freed the moment their last
  reference goes, and its "pause" is really the cost of dropping the tree.

## 🗺️ Roadmap

Done: parallel stop-the-world collection, a persistent pool, concurrent marking. Next:

1. Concurrent sweeping, so the pause after concurrent marking is only the grey-queue drain
2. Opportunistic evacuation of sparse blocks (Immix defragmentation)
3. Reference counting for young objects in the style of LXR: dead young objects free their lines at once, tracing
   handles cycles and old objects

## 📜 License

MIT © Jabberwocky238

<div align="right"><a href="#top">⬆ back to top</a></div>
