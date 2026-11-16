# paragc

A parallel garbage collector for Rust. [中文](README.zh-CN.md)

- **Mark-region heap (Immix-style):** 32 KiB blocks split into 256 B lines; small objects are bump-allocated into
  runs of free lines, objects over 8 KiB are allocated on their own. Objects never move.
- **Many mutators at once:** `Heap::mutate(&self)` can run on any number of threads; each one bump-allocates in
  its own buffer (`Tlab`) and claims whole blocks with one atomic operation. No locks anywhere.
- **Parallel collection:** marking runs on several threads with work stealing (one Chase–Lev deque per worker);
  sweeping, including running destructors, is split by block across the same threads.
- **Exclusive by borrowing:** `Heap::collect(&mut self)` cannot overlap a `mutate`, so there is no safepoint
  protocol; `Gc<'gc, T>` is branded so it cannot escape the `mutate` closure.
- `std` only.

## Usage

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

## Rules

- Heap types implement `Trace` (use `#[derive(Trace)]`) and must be `Send + Sync`: objects are read from any
  thread, and traced and dropped on collector threads. Mutable edges go in `Slot<'gc, T>` (an atomic pointer).
- A type with `Gc` fields must not implement `Drop`: destructors run in no particular order, so a `Drop` could see
  objects that were already dropped. The derive checks this; `#[trace(unsafe_drop)]` opts out.
- Only objects reachable from a `Root` survive a collection. `Root<B>` is a strong handle that can be stored
  anywhere and sent between threads.
- Object data must be aligned to at most 8 bytes (checked at compile time).

## Status

Version 0.1: stop-the-world, non-moving. Planned: concurrent marking (snapshot-at-the-beginning barrier on `Slot`),
opportunistic defragmentation of sparse blocks, and reference counting for young objects in the style of LXR.

## License

MIT
