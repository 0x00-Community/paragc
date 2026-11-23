# Code conventions

## No locks

paragc takes no locks, not even inside collections. Shared state changes through atomics, lock-free stacks with tagged
heads, append-only segmented vectors and futex waits. The work-stealing deques and backoff come from crossbeam; the
append-only vector and the push-only list are paragc's own (`src/sync.rs`).

## Exclusion by borrowing

Operations that need the world stopped take `&mut Heap`. The borrow checker is the safepoint protocol. New code should
keep it that way rather than add runtime coordination for private heaps.

## Source layout

The source is flat; similar things live together:

| File | Contents |
| --- | --- |
| `layout.rs` | blocks, lines, headers, the cross-heap window layout and superblock |
| `heap.rs` | `Heap`, `Scope`, `Tlab`, collections, `CrossHeap`, `CrossHeapScope`, promotion |
| `mark.rs` | `Tracer`, the work-stealing crew, the copier, the cross-heap marker |
| `sweep.rs` | sweeping and large objects |
| `handle.rs` | `ScopeLocal`, `Slot`, `WordSlot`, `ScopeGlobal`, `Weak`, `Far`, cross-heap handles, `Signature`, `Shared` |
| `shm.rs` | the cross-heap address region, windows, heap numbers, process liveness (Linux) |
| `sync.rs` | lock-free building blocks and futex waits |

Cross-heap items are gated on `cfg(crossheap)`, set by `build.rs` only on Linux with the `crossheap` feature.

## Style

- Code, comments, messages and docs in English; READMEs in English (default) and Chinese.
- `cargo clippy --all-targets` is clean with `pedantic`, with and without `--features crossheap`.
- Named constants, never bare numbers, for sizes, thresholds and bit fields.
- Every `unsafe` block states its safety argument in a `// SAFETY:` comment.

## Releases

The repository's `main` is protected: every change goes through a pull request on GitHub (0x00-Community/paragc).
Versions are published to crates.io (`paragc` and `paragc-macros`, same version) and tagged `vX.Y.Z`.
