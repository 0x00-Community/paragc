<a id="top"></a>
<div align="center">

# paragc

### A parallel garbage collector for Rust.
**Immix-style mark-region heap, many mutator threads at once, parallel marking and sweeping. No locks.**

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
| 🏃 **Parallel collection** | Marking: one Chase–Lev deque per worker with work stealing. Sweeping, destructors included: blocks handed out in batches to the same workers. |
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
| Sweeping | Workers claim blocks in batches, walk the start bitmap, drop unmarked objects and classify each block as empty / recyclable / full. Spare empty blocks beyond a quarter of the heap go back to the system. |
| Roots | Each `Root<B>` owns a slot in an append-only table; freed slot numbers are folded into a read-only array at collection time and claimed with an atomic cursor during `mutate`. |
| Stale buffers | A buffer remembers (heap, collection count); after a collection it starts over, because its block was re-classified. |

The lock-free building blocks (Chase–Lev deque, append-only segmented vector, stock) come from
[j8lockfree](https://crates.io/crates/j8lockfree).

## 📏 Rules

- Heap types implement `Trace` (use `#[derive(Trace)]`) and must be `Send + Sync`: objects are read from any thread
  and traced and dropped on collector threads. Mutable edges go in `Slot<'gc, T>` (an atomic pointer).
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

All of them also run under Miri with both Stacked Borrows and Tree Borrows (scaled down under `cfg(miri)`):

```sh
cargo test --test gc -- --exact <name>
MIRIFLAGS="-Zmiri-permissive-provenance" cargo +nightly miri test --test gc -- --exact <name>
MIRIFLAGS="-Zmiri-permissive-provenance -Zmiri-tree-borrows" cargo +nightly miri test --test gc -- --exact <name>
```

## 🗺️ Roadmap

0.1 is stop-the-world and non-moving. Next:

1. Concurrent marking: a snapshot-at-the-beginning barrier on `Slot` writes, so mutators no longer wait for marking
2. Opportunistic evacuation of sparse blocks (Immix defragmentation)
3. Reference counting for young objects in the style of LXR: dead young objects free their lines at once, tracing
   handles cycles and old objects

## 📜 License

MIT © Jabberwocky238

<div align="right"><a href="#top">⬆ back to top</a></div>
