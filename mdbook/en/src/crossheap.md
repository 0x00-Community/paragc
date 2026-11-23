# Cross-heap overview

A **cross-heap** (`CrossHeap`) is a heap that lives in a shared-memory file. Every process that can open the file maps
the heap and uses it: it can allocate, read, write and collect.

```toml
[dependencies]
paragc = { version = "0.6", features = ["crossheap"] }
```

The feature exists only on **Linux**. On macOS and Windows it compiles to nothing, and only the private heap is there.
`build.rs` emits `cfg(crossheap)` when the feature is on and the target is Linux, and every cross-heap item is gated on
that cfg.

## Two kinds of heap, two scopes of sharing

| | shared between threads | shared between processes |
| --- | --- | --- |
| private heap (`Heap`) | ✓ | ✗ |
| cross-heap (`CrossHeap`) | ✓ | ✓, for every process that can open its file |

| From → to | private heap | cross-heap |
| --- | --- | --- |
| private heap | ✓ direct pointer (`Far` between heaps) | ✓ `CrossHeapGlobal`, or an absolute address once promoted |
| cross-heap | ✗ (other processes cannot see private memory) | ✓ `CrossHeapSlot`, `CrossHeapRef`, `CrossHeapWeak` |

Because nothing in a cross-heap points into a private heap, private and cross-heaps never form cycles.

## Pieces

| Piece | Page |
| --- | --- |
| Where heaps are mapped; the file layout; how the file grows and shrinks | [Address space and file layout](crossheap-layout.md) |
| Objects without code: `#[derive(Shared)]`, type numbers, pointer maps, handles | [Objects, types and pointer maps](crossheap-objects.md) |
| Several processes in one heap; dying at any moment; heap numbers, owners, restarts | [Processes, crashes and lifetime](crossheap-processes.md) |
| References that never hold anything up; signatures another process can resolve; posting objects to the owner | [Weak references, signatures and the mailbox](crossheap-weak.md) |
| A cross-heap as the old generation of a private heap | [Promotion](promotion.md) |

## Trust

Every process that maps a heap can read all of it and corrupt it. Processes that share a heap trust each other; the
permission check is the file's permission. A signature from an untrusted party is only read, though, and a forged one
never crashes the reader.

## The goal behind it

j8 runtimes in different processes exchange JS values without copying. A process sends a 32-byte signature; the
receiver, or a broker such as dowin, maps the sender's heap, finds the object and reads it, or passes the signature on.
