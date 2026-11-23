# Testing and Miri

## Test files

| File | Covers |
| --- | --- |
| `tests/gc.rs` | the private heap: survival, finalization counts, threads, large objects, deep structures, concurrent marking and sweeping, evacuation, young collections, reference counting, shrinking, serial drop |
| `tests/weak.rs` | weak references through every kind of collection, the read barrier during concurrent marking |
| `tests/word.rs` | `WordSlot` through full, young and concurrent collections |
| `tests/far.rs` | `Far`, pinning, group collections, cross-heap cycles of private heaps |
| `tests/cross_heap.rs` | cross-heaps in one process: pointer maps, large objects, two mappings, `CrossHeapRef`, group collections, weak references, signatures, mailbox, shrink, promotion, tails (needs `--features crossheap`) |
| `tests/processes.rs` | several processes on one heap: killed mutators, killed collectors, random kills, restarts, heap numbers, 8 processes with half killed (needs `--features crossheap`) |

The README's verification table lists every test and what it checks.

## Running them

```sh
cargo test --test gc -- --exact <name>
cargo test --features crossheap --test cross_heap
cargo test --features crossheap --test processes
```

`promotion_into_attached_cross_heap` runs several scenarios inside one test, because a process can hold the fixed region
only once at a time and tests in one file run on parallel threads.

## Miri

Every test also runs under Miri, with both Stacked Borrows and Tree Borrows (scaled down under `cfg(miri)`):

```sh
MIRIFLAGS="-Zmiri-permissive-provenance -Zmiri-ignore-leaks" cargo +nightly miri test --test gc -- --exact <name>
MIRIFLAGS="-Zmiri-permissive-provenance -Zmiri-ignore-leaks -Zmiri-tree-borrows" cargo +nightly miri test --test gc -- --exact <name>
```

- Miri cannot map files. Under Miri the cross-heap region is an ordinary allocation (4 windows of 16 MiB); the tests
  without a file run on it, and the file tests are skipped.
- `-Zmiri-ignore-leaks` is needed because crossbeam-epoch's global collector still holds deferred garbage at exit.
- At 4 collector threads Stacked Borrows reports a violation inside crossbeam-epoch itself, so tests use 2 under Miri.
  A few concurrent tests still hit it under Stacked Borrows and are clean under Tree Borrows.

## Known flaky test

`gc::background_release_while_mutating` can fail in release builds; it passes in debug builds. The failure predates
0.6.0.
