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
| paragc | 0.2.1 (commit `bb6c81c`: concurrent recount, cheaper single-threaded recount); the other collectors were measured with `5c4c5c5` |
| cppgc | V8 15.2.124.1 through rusty_v8 (`v8` crate 152.2.0) |
| Others | gc-arena 0.7.0, gc 0.5.1, dumpster 2.1.0 |
| Method | every configuration run 5 times in a fresh process, fastest kept; runs started only once the 1-minute load average was below 2.5 (0.5 at the start, 0.8 at the end) |
| CPU column | CPU time of the whole process (all threads) over the timed region; CPU ÷ wall is how many threads were actually working |

## Workloads

| Name | What is timed |
| --- | --- |
| `pause` | A binary tree of 2^20 − 1 live nodes plus an equally large dead one, then **one full collection**. Threads = collector threads. |
| `pause-cm` / `pause-cs` / `pause-cms` | paragc only: the same heap, timing **only the time mutators are stopped**. `cm`: concurrent marking first (untimed), then the finishing collection. `cs`: marking in the pause, sweeping concurrent (untimed). `cms`: both concurrent. |
| `gcbench` | Boehm's GCBench: a stretch tree of depth 18, a long-lived tree of depth 16, and for depths 4, 6, …, 16 enough short-lived trees to allocate twice the stretch tree each. Each collector collects on its own schedule. Total time. |
| `alloc-mt` | 2048 short-lived trees of depth 10 in total, split over N allocating threads (16 rounds), next to a long-lived tree. Only for collectors that allow several mutator threads. Total time. |

Every node is the same shape everywhere: two child pointers and a `u64`. After every timed region the benchmark checks
that the live tree still has exactly 2^depth − 1 nodes.

## How each collector is driven

| | Serial | Parallel | Collecting in `gcbench` |
| --- | --- | --- | --- |
| **paragc** | `Heap::new(1)` | `Heap::new(n)`: a persistent pool of n − 1 workers | `collect_if_needed` after every tree: young collections with reference counting, a full collection every 16 |
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
| **paragc** | 22.4 | 9.1 (3.9) | **4.2** (14.2) |
| paragc, concurrent marking | 6.6 | 2.4 (3.0) | 1.5 (11.9) |
| paragc, concurrent sweeping | 15.9 | 6.5 (4.0) | 2.8 (12.7) |
| **paragc, concurrent marking + sweeping** | **0.03** | **0.03** | **0.05** |
| cppgc, attached to an isolate | 45.0 | 44.7 (1.0) | 43.4 (1.0) |
| cppgc, all parallel / concurrent flags | 43.5 | 43.7 (1.0) | 43.0 (1.0) |
| cppgc, detached heap | 42.5 | 42.7 (1.0) | 42.5 (1.0) |
| gc-arena | 15.8 | – | – |
| dumpster | 16.4 | – | – |
| gc (rust-gc) | 87.6 | – | – |

### GCBench: total time

| | 1 thread | 4 threads | 16 threads |
| --- | ---: | ---: | ---: |
| **paragc** | **53.7** | **59.1** (1.2) | **67.3** (2.1) |
| cppgc, attached to an isolate | 119.2 | 95.3 (1.3) | 100.4 (1.3) |
| cppgc, all parallel / concurrent flags | 117.4 | 94.9 (1.3) | 99.4 (1.3) |
| cppgc, detached heap | 173.2 | 171.7 (1.0) | 173.0 (1.0) |
| gc-arena | 115.3 | – | – |
| dumpster | 102.2 | – | – |
| gc (rust-gc) | 281.7 | – | – |

### Many mutators: total time

| | 1 thread | 4 threads | 16 threads |
| --- | ---: | ---: | ---: |
| **paragc** | **38.9** | **10.5** (3.6) | **9.8** (8.0) |
| dumpster (`sync`) | 116.4 | 296.2 (3.9) | 334.3 (13.2) |

cppgc, gc-arena and rust-gc allow one thread per heap, so they have no entry here.

## Reading the numbers

- **Throughput: young collections and reference counting.** GCBench on one thread takes 53.7 ms against 102–119 ms
  for dumpster, gc-arena and cppgc: `collect_if_needed` runs young collections, which trace only new objects, and old
  garbage is freed by reference counts in the background. The many-mutator workload is 38.9 ms against dumpster's
  116.4 ms, and dumpster gets slower as threads are added (reference counts shared across threads) while paragc gets
  faster.
- **Serial full pauses: gc-arena and dumpster are faster.** One full collection of the 2M-node heap takes about 16 ms
  with them and 22.4 ms with paragc on one thread. Part of that is the recount: a full collection rebuilds the
  reference counts, one increment per edge (20.7 ms before reference counting; 25.7 ms in 0.2.0, where the increment was
  a locked atomic even with a single marker). paragc is ahead from 4 threads on (9.1 ms) and takes 4.2 ms on 16.
- **GCBench does not get faster with threads.** Young collections are small; waking more workers costs more than it
  saves (16 threads: 2.1× the CPU for a slower result). The paragc GCBench and many-mutator numbers are 2–3 ms above the
  0.2.0 run although that code did not change; that run started at a load average of 0.5, this one at 2.5.
- **Concurrent marking and sweeping take the work out of the pause.** 22.4 ms becomes 0.03 ms on one thread: marking
  and sweeping still happen, on the pool and in mutators that sweep the blocks they take. In this benchmark no mutator
  runs during marking, so the grey queue is empty at the end and the remaining pause is bookkeeping proportional to the
  number of blocks; with mutators running, draining what the write barrier logged is added to it.
- **cppgc marks a full collection on one thread.** Attached or detached, with or without 16 worker threads and every
  parallel / concurrent marking flag, a forced full collection runs at CPU ÷ wall = 1.0 and takes about 43–45 ms. cppgc
  only uses other threads for background marking during the incremental collections V8 schedules itself (GCBench, about
  1.3). Measuring its concurrent cycle in `pause` would need driving V8's incremental GC, which this benchmark does not do.
- **cppgc pays for the Rust ⇄ C++ boundary.** Through rusty_v8 every allocation, every `Member` and every traced edge is
  an FFI call; attached to an isolate, V8's own (nearly empty) heap adds about 1.3–1.5 ms to each full collection. A C++
  embedder would see lower numbers.
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
