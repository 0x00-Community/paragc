# What paragc is

paragc is **a tracing garbage collector for Rust programs that share one heap across many threads**.

- Objects live in 32 KiB blocks cut into 256-byte lines, the layout of Immix (Blackburn & McKinley, 2008).
- Threads bump-allocate into free lines on their own.
- A collection marks and sweeps on every core.

## What sets it apart

| | |
| --- | --- |
| Many mutators | `Heap::scope(&self)` runs on any number of threads at once. Each thread allocates from its own buffer (`Tlab`) and claims whole blocks with one atomic operation. |
| Exclusive by borrowing | A collection takes `&mut` the heap or an exclusive scope, and every `ScopeLocal` is a borrow of its scope, so none survives it: the borrow checker forbids it. That borrow is the stop-the-world: there is no safepoint protocol and no lock. |
| Concurrent marking | `Heap::mark_concurrently(&self)` marks while mutators keep running. A snapshot-at-the-beginning barrier on `Slot` writes keeps it correct, and new objects are allocated black. |
| Concurrent sweeping | `Heap::finish_marking` only finishes marking. Mutators sweep a block themselves when they need one, and `sweep_concurrently` sweeps the rest on the pool. |
| Evacuation | A stop-the-world collection copies live objects out of sparse blocks while it marks (Immix defragmentation). |
| Young collections | `collect_young` traces only objects allocated since the last collection and compacts the survivors. |
| Reference counting | Old objects carry a small count, settled at young collections and released in the background (the LXR design). Full tracing only runs every few rounds, for cycles. |
| Weak references | `Weak<B>` does not keep its target alive; every kind of collection clears or moves it. |
| References between heaps | `Far` points straight from one private heap into another; `collect_group` reclaims cycles across them. |
| Cross-heap (Linux) | A `CrossHeap` lives in a shared-memory file. Several processes map it and use it at once, a killed process never blocks the others, and objects can be signed and found from another process. |

## Where it is used

paragc is the heap of the [j8](https://github.com/0x00-Community/j8) JavaScript engine. In j8:

- every JS object, string and closure is a paragc object;
- one isolate owns one `Heap`;
- with the `crossheap` feature, the isolate attaches a cross-heap as its old generation. JS values that survive a turn
  are promoted there, so another process can read them by signature.

## What it is not

- **Not a conservative collector.** paragc does not scan stacks. Every object that must survive a collection is
  reachable from a `ScopeGlobal`. A `ScopeLocal` is safe because the borrow checker ends it before any collection of its scope; `ScopeGlobal`s are rewritten instead.
- **Not `no_std`.** It needs threads, `mmap` and atomics from `std`.
- **Not a drop-in `Rc` replacement.** Objects are reached through branded handles. Types implement `Trace` (usually
  with `#[derive(Trace)]`), and a type with `ScopeLocal` fields must not implement `Drop`.
