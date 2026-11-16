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
| paragc | commit `d063d29` |
| cppgc | V8 15.2.124.1 through rusty_v8 (`v8` crate 152.2.0) |
| Others | gc-arena 0.7.0, gc 0.5.1, dumpster 2.1.0 |
| Method | every configuration run 5 times in a fresh process, fastest kept; runs started only once the 1-minute load average was below 2.5 (2.2 at the start, 2.4 at the end) |
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
| **paragc** | `Heap::new(1)` | `Heap::new(n)`: a persistent pool of n − 1 workers | `collect_if_needed` after every tree |
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
| **paragc** | 20.7 | 7.8 (3.9) | **3.9** (13.5) |
| paragc, concurrent marking | 6.7 | 2.4 (2.7) | 1.5 (12.6) |
| paragc, concurrent sweeping | 13.9 | 5.3 (3.9) | 2.4 (12.3) |
| **paragc, concurrent marking + sweeping** | **0.03** | **0.03** | **0.06** |
| cppgc, attached to an isolate | 43.3 | 44.2 (1.0) | 44.4 (1.0) |
| cppgc, all parallel / concurrent flags | 44.6 | 44.4 (1.0) | 43.8 (1.0) |
| cppgc, detached heap | 43.2 | 42.7 (1.0) | 44.1 (1.0) |
| gc-arena | 16.2 | – | – |
| dumpster | 17.0 | – | – |
| gc (rust-gc) | 90.0 | – | – |

### GCBench: total time

| | 1 thread | 4 threads | 16 threads |
| --- | ---: | ---: | ---: |
| **paragc** | **70.1** | **61.1** (1.5) | 70.3 (3.1) |
| cppgc, attached to an isolate | 120.4 | 97.2 (1.3) | 102.0 (1.3) |
| cppgc, all parallel / concurrent flags | 117.5 | 91.4 (1.3) | 102.5 (1.4) |
| cppgc, detached heap | 173.4 | 168.7 (1.0) | 166.0 (1.0) |
| gc-arena | 116.9 | – | – |
| dumpster | 112.1 | – | – |
| gc (rust-gc) | 290.1 | – | – |

### Many mutators: total time

| | 1 thread | 4 threads | 16 threads |
| --- | ---: | ---: | ---: |
| **paragc** | **40.8** | **13.9** (3.7) | **10.1** (9.8) |
| dumpster (`sync`) | 118.9 | 217.3 (3.7) | 240.8 (7.8) |

cppgc, gc-arena and rust-gc allow one thread per heap, so they have no entry here.

## Reading the numbers

- **Serial pauses: gc-arena and dumpster are faster.** One collection of the 2M-node heap takes 16–17 ms with them and
  20.7 ms with paragc on one thread. paragc is ahead from 4 threads on (7.8 ms) and at 16 threads takes 3.9 ms.
- **Throughput: paragc is ahead even serially.** On one thread GCBench takes 70.1 ms against 112–120 ms for dumpster,
  gc-arena and cppgc; the many-mutator workload is 40.8 ms against dumpster's 118.9 ms, and dumpster gets slower as
  threads are added (reference counts shared across threads) while paragc gets faster.
- **GCBench does not scale past 4 threads.** Its collections are small; waking 16 workers costs more than it saves
  (16 threads: 3.1× the CPU for a slower result).
- **Concurrent marking and sweeping take the work out of the pause.** 20.7 ms becomes 0.03 ms on one thread: marking
  and sweeping still happen, on the pool and in mutators that sweep the blocks they take. In this benchmark no mutator
  runs during marking, so the grey queue is empty at the end and the remaining pause is bookkeeping proportional to the
  number of blocks; with mutators running, draining what the write barrier logged is added to it.
- **cppgc marks a full collection on one thread.** Attached or detached, with or without 16 worker threads and every
  parallel / concurrent marking flag, a forced full collection runs at CPU ÷ wall = 1.0 and takes about 44 ms. cppgc
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
