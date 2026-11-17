# paragc benchmarks

**English** · [简体中文](benchmark.zh-CN.md)

paragc compared with [cppgc](https://chromium.googlesource.com/v8/v8/+/main/include/cppgc/) (V8's Oilpan, the C++ GC
behind Blink), [gc-arena](https://crates.io/crates/gc-arena), [gc](https://crates.io/crates/gc) (rust-gc) and
[dumpster](https://crates.io/crates/dumpster), serial and parallel.

## Setup

| | |
| --- | --- |
| Machine | Intel Core Ultra 7 356H, 16 cores (1 thread per core), Linux 7.0 |
| Rust | 1.98.1, release profile |
| paragc | 0.5.0 (commit `d44bad0`), private and public heaps; public-heap files in `/dev/shm`. Every collector was measured in the same run |
| cppgc | V8 15.2.124.1 through rusty_v8 (`v8` crate 152.2.0) |
| Others | gc-arena 0.7.0, gc 0.5.1, dumpster 2.1.0 |
| Method | every configuration run 5 times in a fresh process, fastest kept; runs started only once the 1-minute load average was below 2.5 (2.49 at the start, 2.61 at the end) |
| CPU column | CPU time of the whole process (all threads) over the timed region; CPU ÷ wall is how many threads were actually working |

## Workloads

| Name | What is timed |
| --- | --- |
| `pause` | A binary tree of 2^20 − 1 live nodes plus an equally large dead one, then **one full collection**. Threads = collector threads. |
| `pause-cm` / `pause-cs` / `pause-cms` | paragc only: the same heap, timing **only the time mutators are stopped**. `cm`: concurrent marking first (untimed), then the finishing collection. `cs`: marking in the pause, sweeping concurrent (untimed). `cms`: both concurrent. |
| `gcbench` | Boehm's GCBench: a stretch tree of depth 18, a long-lived tree of depth 16, and for depths 4, 6, …, 16 enough short-lived trees to allocate twice the stretch tree each. Each collector collects on its own schedule. Total time. |
| `alloc-mt` | 2048 short-lived trees of depth 10 in total, split over N allocating threads (16 rounds), next to a long-lived tree. Only for collectors that allow several mutator threads. Total time. |
| `frag` | Fragmentation: objects of four sizes (24 / 240 / 1600 / 12000 data bytes, 70 / 20 / 8 / 2 % of allocations), 400k per round, randomly overwriting a holder's slots with probability 1/7, 1/5, 1/3, 1/2 by size (overwritten objects become garbage, so lifetimes are random and survivors are scattered), 30 rounds, about 80 MB live. Total time, RSS after a final full collection, and peak RSS after each round (both minus the starting RSS). paragc and cppgc only. |

Every node is the same shape everywhere: two child pointers and a `u64`. After every timed region the benchmark checks
that the live tree still has exactly 2^depth − 1 nodes.

## How each collector is driven

| | Serial | Parallel | Collecting in `gcbench` |
| --- | --- | --- | --- |
| **paragc** | `Heap::new(1)` | `Heap::new(n)`: a persistent pool of n − 1 workers | `collect_if_needed` after every tree: young collections with reference counting, a full collection every 16 |
| **paragc, public heap** | `PublicHeap` on a `/dev/shm` file, 1 collector thread | n collector threads | stop-the-world `collect` when allocation since the last collection exceeds 2 × max(live, 4 MiB) (no young collections or reference counting yet) |
| **cppgc** | heap attached to an isolate (how Blink uses it), single-threaded platform plus `--single-threaded`; full collections through `request_garbage_collection_for_testing(Full)` | default platform with n worker threads | V8's own scheduling |
| **cppgc, all flags** | as above | as above, plus `--parallel-marking --concurrent-marking --cppheap-concurrent-marking --cppheap-incremental-marking --concurrent-marking-max-worker-num=n` | V8's own scheduling |
| **cppgc, detached** | heap not attached to an isolate, `collect_garbage_for_testing` | as above | paragc's trigger policy, by hand |
| **gc-arena** | `Arena`, `finish_cycle` | – | `collect_debt` after every tree |
| **gc** (rust-gc) | thread-local heap, `force_collect` | – | automatic |
| **dumpster** | `unsync`; `pause` times dropping the dead tree plus `collect` | `sync`, only in `alloc-mt` | automatic |

## Results

Wall ms (CPU ÷ wall); best of 5.

### Pause: one full collection, 1M live + 1M dead nodes

| | 1 thread | 4 threads | 16 threads |
| --- | ---: | ---: | ---: |
| **paragc** | 20.2 | 8.7 (3.8) | **4.2** (13.0) |
| paragc, concurrent marking | 4.6 | 2.4 (3.0) | 1.5 (15.6) |
| paragc, concurrent sweeping | 15.5 | 6.3 (4.0) | 2.8 (13.1) |
| **paragc, concurrent marking + sweeping** | **0.02** | **0.02** | **0.06** |
| paragc, public heap | 18.4 | 6.5 (3.9) | 3.3 (13.9) |
| cppgc, attached to an isolate | 44.2 | 44.3 (1.0) | 44.9 (1.0) |
| cppgc, all parallel / concurrent flags | 44.2 | 44.5 (1.0) | 44.0 (1.0) |
| cppgc, detached heap | 43.2 | 43.5 (1.0) | 43.3 (1.0) |
| gc-arena | 16.3 | – | – |
| dumpster | 16.3 | – | – |
| gc (rust-gc) | 90.2 | – | – |

### GCBench: total time

| | 1 thread | 4 threads | 16 threads |
| --- | ---: | ---: | ---: |
| **paragc** | **50.3** | **54.3** (1.3) | **64.3** (2.2) |
| paragc, public heap | 72.7 | 67.3 (1.3) | 83.2 (2.1) |
| cppgc, attached to an isolate | 121.0 | 94.2 (1.3) | 100.1 (1.3) |
| cppgc, all parallel / concurrent flags | 115.5 | 97.6 (1.3) | 98.4 (1.3) |
| cppgc, detached heap | 174.5 | 173.5 (1.0) | 175.5 (1.0) |
| gc-arena | 118.0 | – | – |
| dumpster | 97.8 | – | – |
| gc (rust-gc) | 294.6 | – | – |

### Many mutators: total time

| | 1 thread | 4 threads | 16 threads |
| --- | ---: | ---: | ---: |
| **paragc** | **35.8** | **9.4** (3.7) | **9.0** (8.5) |
| dumpster (`sync`) | 117.5 | 289.4 (3.9) | 343.5 (12.9) |

cppgc, gc-arena and rust-gc allow one thread per heap, so they have no entry here.

### Fragmentation: time and memory

| | threads | wall ms | RSS after MB | peak RSS MB |
| --- | ---: | ---: | ---: | ---: |
| **paragc** | 1 | **1578** | **110.5** | **415.6** |
| **paragc** | 16 | **1344** | **111.2** | **416.4** |
| cppgc, attached (V8 schedules) | 1 | 2010 | 444.7 | 1894.2 |
| cppgc, attached (V8 schedules) | 16 | 1824 | 477.0 | 1984.5 |
| cppgc, detached (paragc's trigger) | 1 | 4134 | 294.4 | 417.1 |
| cppgc, detached (paragc's trigger) | 16 | 4160 | 294.4 | 417.1 |

paragc 0.4.2 on the same workload ended at about 935 MB RSS with a peak of about 940 MB: blocks and large objects came from glibc,
which kept freed memory; young collections promoted in place, so scattered survivors each pinned a line; medium objects threw away the
rest of a hole; evacuation was capped at 1024 blocks per collection. What 0.5.0 does instead is under "Reading the numbers".

## Reading the numbers

- **Throughput: young collections and reference counting.** GCBench on one thread takes 50.3 ms against 98–121 ms
  for dumpster, gc-arena and cppgc: `collect_if_needed` runs young collections, which trace only new objects, and old
  garbage is freed by reference counts in the background. The many-mutator workload is 35.8 ms against dumpster's
  117.5 ms, and dumpster gets slower as threads are added (reference counts shared across threads) while paragc gets
  faster.
- **Serial full pauses: gc-arena and dumpster are faster.** One full collection of the 2M-node heap takes 16.3 ms
  with them and 20.2 ms with paragc on one thread. Part of that is the recount: a full collection rebuilds the
  reference counts, one increment per edge. paragc is ahead from 4 threads on (8.7 ms) and takes 4.2 ms on 16.
- **GCBench does not get faster with threads.** Young collections are small; waking more workers costs more than it
  saves (16 threads: 2.2× the CPU for a slower result).
- **Concurrent marking and sweeping take the work out of the pause.** 20.2 ms becomes 0.02 ms on one thread: marking
  and sweeping still happen, on the pool and in mutators that sweep the blocks they take. In this benchmark no mutator
  runs during marking, so the grey queue is empty at the end and the remaining pause is bookkeeping proportional to the
  number of blocks; with mutators running, draining what the write barrier logged is added to it.
- **Public heaps: shared memory costs little.** A full collection of the same tree in a public heap (a `/dev/shm` file,
  offset pointers, tracing by pointer map) takes 18.4 ms on one thread against 20.2 ms for the private heap: its header is
  16 bytes instead of 24 and it does not rebuild reference counts. GCBench is slower (72.7 ms against 50.3 ms) because
  every public-heap collection is a full one; young collections and reference counting are not there yet.
- **cppgc marks a full collection on one thread.** Attached or detached, with or without 16 worker threads and every
  parallel / concurrent marking flag, a forced full collection runs at CPU ÷ wall = 1.0 and takes about 43–45 ms. cppgc
  only uses other threads for background marking during the incremental collections V8 schedules itself (GCBench, about
  1.3). Measuring its concurrent cycle in `pause` would need driving V8's incremental GC, which this benchmark does not do.
- **cppgc pays for the Rust ⇄ C++ boundary.** Through rusty_v8 every allocation, every `Member` and every traced edge is
  an FFI call; attached to an isolate, V8's own (nearly empty) heap adds about 1.3–1.5 ms to each full collection. A C++
  embedder would see lower numbers.
- **Fragmentation: own pages + overflow allocation + evacuation.** Blocks are cut from segments paragc maps itself, and returned blocks are
  madvised away; objects larger than a line that do not fit the current hole go to an overflow cursor instead of discarding the hole; young
  collections move survivors that only young objects and roots point at into compact blocks; full collections evacuate every block under half
  live, sparsest first, with no block cap, and a full collection comes early once blocks exceed "live + one round of allowed allocation" by 1.5×.
  RSS after collection is 23–38 % of cppgc's and the peak matches cppgc on the same trigger (V8's own scheduling peaks at 1.9 GB). For a lower
  peak, call `Heap::shrink` when idle (it also returns whole pages of free lines, which then fault in again on reuse).
- **dumpster is reference counting.** These trees have no cycles, so dead nodes are freed the moment their last reference
  goes; its `pause` is really the cost of dropping the dead tree.

## Reproducing

```sh
cd benches
# cppgc needs V8's static library; RUSTY_V8_ARCHIVE points at a local copy, otherwise it is downloaded
RUSTY_V8_ARCHIVE=/path/to/librusty_v8.a ./run.sh                 # everything
./run.sh paragc cppgc                                           # a subset
REPS=10 THREADS=8 ./run.sh paragc                               # more repetitions, a different "all cores" tier
./target/release/bench-paragc pause-cms 16                      # one configuration
V8_FLAGS="--trace-gc" CPPGC_HEAP=detached ./target/release/bench-cppgc gcbench 4
```

`run.sh` prints the Markdown tables above. Run it on an idle machine: other processes compete with the collector threads
and the parallel numbers degrade first.
