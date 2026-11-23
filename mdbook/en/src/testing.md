# Testing and Miri

## Test files

| File | Covers |
| --- | --- |
| `tests/gc.rs` | the private heap: survival, finalization counts, threads, large objects, deep structures, concurrent marking and sweeping, evacuation, young collections, reference counting, shrinking, serial drop |
| `tests/weak.rs` | weak references through every kind of collection, the read barrier during concurrent marking |
| `tests/word.rs` | `WordSlot` through full, young and concurrent collections |
| `tests/handle.rs` | handles: objects moving under them in every young mode, old holders without a barrier, entry reuse, concurrent marking, ids turned back into handles |
| `tests/scope.rs` | nested scopes: collecting inside a scope, carrying objects out between inner scopes |
| `tests/far.rs` | `Far`, pinning, group collections, cross-heap cycles of private heaps |
| `tests/cross_heap.rs` | cross-heaps in one process: pointer maps, large objects, two mappings, `CrossHeapRef`, group collections, weak references, signatures, mailbox, shrink, promotion, tails (needs `--features crossheap`) |
| `tests/processes.rs` | several processes on one heap: killed mutators, killed collectors, random kills, restarts, heap numbers, 8 processes with half killed (needs `--features crossheap`) |

The README's verification table lists every test and what it checks. Two `compile_fail` doc tests on `Scope` check
the borrow rules: a `ScopeLocal` used after a collection in its scope, and an object of one heap written into another's.

## Results (0.7.0)

| File | Tests | Result |
| --- | ---: | --- |
| `gc.rs` | 26 | pass |
| `weak.rs` | 8 | pass |
| `far.rs` | 7 | pass |
| `word.rs` | 5 | pass |
| `handle.rs` | 5 | pass |
| `scope.rs` | 2 | pass |
| `cross_heap.rs` | 18 | pass |
| `processes.rs` | 9 | pass |
| doc tests (README example, two `compile_fail`) | 3 | pass |

clippy reports nothing, with and without `crossheap`.

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
