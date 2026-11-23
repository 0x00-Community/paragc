# 性能对比

paragc 与 cppgc（V8 的 Oilpan）、gc-arena、gc（rust-gc）、dumpster 在一台 16 核机器上的对比。数字是墙钟毫秒，5 次取最好，
用 paragc 0.7.0（scope API）测得。环境、负载和每种配置见
[`benches/benchmark.md`](https://github.com/0x00-Community/paragc/blob/main/benches/benchmark.zh-CN.md)；用 `benches/run.sh` 跑。

| | paragc | cppgc | gc-arena | dumpster | gc |
| --- | ---: | ---: | ---: | ---: | ---: |
| 停顿，100 万活 + 100 万死，1 线程 | 21.5 | 43.9 | **16.1** | 16.3 | 88.0 |
| 停顿，16 线程 | **4.2** | 44.8 | – | – | – |
| 停顿，并发标记 + 并发清扫，1 线程 | **0.02** | – | – | – | – |
| GCBench，1 线程 | **46.5** | 119.8 | 108.6 | 95.3 | 290.5 |
| GCBench，4 线程 | **54.3** | 90.6 | – | – | – |
| 多个 mutator，16 线程 | **8.9** | – | – | 321.8 | – |
| 碎片化（约 80 MB 活数据），回收后 / 峰值 RSS MB | **111 / 416** | 444 / 1896 | – | – | – |
| 跨堆：停顿 16 线程 / GCBench 1 线程 | 3.3 / 72.5 | – | – | – | – |

怎么读：

- 单线程时 gc-arena 和 dumpster 停顿更短。paragc 的完整回收还要重建引用计数。
- 多线程和吞吐量上 paragc 领先。
- cppgc 强制的完整回收即使开了所有并行开关也只在一个线程上标记，而且在这个测试里每次分配、每条边都要付一次 FFI 调用。

本轮环境：Intel Core Ultra 7 356H，16 核，Linux 7.0，Rust 1.98.1；开始时 1 分钟负载 2.0，结束时 1.6，旁边还跑着一台 6 核
虚拟机，所以单个数字在不同轮次之间会差几个百分点。

对比测试依赖 V8 这样的大库，所以 `benches/` 是一个独立的 workspace。
