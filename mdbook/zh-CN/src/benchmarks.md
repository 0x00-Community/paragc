# 性能对比

在 16 核机器上和 cppgc（V8 的 Oilpan）、gc-arena、gc（rust-gc）、dumpster 对比。数字是墙钟毫秒，取 5 次最好。环境、负载和
全部配置见 [`benches/benchmark.md`](https://github.com/0x00-Community/paragc/blob/main/benches/benchmark.md)。

| | paragc | cppgc | gc-arena | dumpster | gc |
| --- | ---: | ---: | ---: | ---: | ---: |
| 停顿，1M 活 + 1M 死，1 线程 | 20.2 | 44.2 | **16.3** | **16.3** | 90.2 |
| 停顿，16 线程 | **4.2** | 44.9 | – | – | – |
| 停顿，并发标记 + 并发清扫，1 线程 | **0.02** | – | – | – | – |
| GCBench，1 线程 | **50.3** | 121.0 | 118.0 | 97.8 | 294.6 |
| GCBench，4 线程 | **54.3** | 94.2 | – | – | – |
| 多 mutator，16 线程 | **9.0** | – | – | 343.5 | – |
| 碎片化（约 80 MB 活），回收后 / 峰值 RSS MB | **111 / 416** | 445 / 1894 | – | – | – |

怎么看：

- 单线程时 gc-arena 和 dumpster 停顿更短。paragc 的完整回收还要重建引用计数。
- 多线程和吞吐量上 paragc 领先。
- cppgc 的强制完整回收即使打开所有并行开关也只在一个线程上标记，而且在这个测试框架里每次分配、每条边都要付一次 FFI 调用。

对比用的基准依赖 V8 这样的大库，所以 `benches/` 是一个单独的 workspace。
