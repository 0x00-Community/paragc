# Benchmarks

paragc against cppgc (V8's Oilpan), gc-arena, gc (rust-gc) and dumpster on a 16-core machine. The numbers are
wall-clock milliseconds, best of 5. Setup, workloads and every configuration are in
[`benches/benchmark.md`](https://github.com/0x00-Community/paragc/blob/main/benches/benchmark.md).

| | paragc | cppgc | gc-arena | dumpster | gc |
| --- | ---: | ---: | ---: | ---: | ---: |
| Pause, 1M live + 1M dead, 1 thread | 20.2 | 44.2 | **16.3** | **16.3** | 90.2 |
| Pause, 16 threads | **4.2** | 44.9 | – | – | – |
| Pause, concurrent marking + sweeping, 1 thread | **0.02** | – | – | – | – |
| GCBench, 1 thread | **50.3** | 121.0 | 118.0 | 97.8 | 294.6 |
| GCBench, 4 threads | **54.3** | 94.2 | – | – | – |
| Many mutators, 16 threads | **9.0** | – | – | 343.5 | – |
| Fragmentation (~80 MB live), RSS after / peak MB | **111 / 416** | 445 / 1894 | – | – | – |

How to read them:

- On a single thread, gc-arena and dumpster pause for less time. paragc's full collection also rebuilds reference counts.
- With threads, and in throughput, paragc leads.
- cppgc's forced full collection marks on one thread even with every parallel flag on, and pays an FFI call per
  allocation and per edge in this harness.

The comparison benchmarks depend on large libraries such as V8, so `benches/` is a workspace of its own.
