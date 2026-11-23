# Benchmarks

paragc against cppgc (V8's Oilpan), gc-arena, gc (rust-gc) and dumpster on a 16-core machine. The numbers are
wall-clock milliseconds, best of 5, measured with paragc 0.7.0 (the scope API). Setup, workloads and every configuration
are in [`benches/benchmark.md`](https://github.com/0x00-Community/paragc/blob/main/benches/benchmark.md); run them with
`benches/run.sh`.

| | paragc | cppgc | gc-arena | dumpster | gc |
| --- | ---: | ---: | ---: | ---: | ---: |
| Pause, 1M live + 1M dead, 1 thread | 21.5 | 43.9 | **16.1** | 16.3 | 88.0 |
| Pause, 16 threads | **4.2** | 44.8 | – | – | – |
| Pause, concurrent marking + sweeping, 1 thread | **0.02** | – | – | – | – |
| GCBench, 1 thread | **46.5** | 119.8 | 108.6 | 95.3 | 290.5 |
| GCBench, 4 threads | **54.3** | 90.6 | – | – | – |
| Many mutators, 16 threads | **8.9** | – | – | 321.8 | – |
| Fragmentation (~80 MB live), RSS after / peak MB | **111 / 416** | 444 / 1896 | – | – | – |
| Public heap: pause, 16 threads / GCBench, 1 thread | 3.3 / 72.5 | – | – | – | – |

How to read them:

- On a single thread, gc-arena and dumpster pause for less time. paragc's full collection also rebuilds reference counts.
- With threads, and in throughput, paragc leads.
- cppgc's forced full collection marks on one thread even with every parallel flag on, and pays an FFI call per
  allocation and per edge in this harness.

The run: Intel Core Ultra 7 356H, 16 cores, Linux 7.0, Rust 1.98.1; 1-minute load average 2.0 at the start and 1.6 at
the end, with a 6-core virtual machine running beside it, so single numbers move by a few percent between runs.

The comparison benchmarks depend on large libraries such as V8, so `benches/` is a workspace of its own.
