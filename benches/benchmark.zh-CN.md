# paragc 性能对比

[English](benchmark.md) · **简体中文**

paragc 与 [cppgc](https://chromium.googlesource.com/v8/v8/+/main/include/cppgc/)（V8 的 Oilpan，Blink 用的 C++ 垃圾回收器）、
[gc-arena](https://crates.io/crates/gc-arena)、[gc](https://crates.io/crates/gc)（rust-gc）、[dumpster](https://crates.io/crates/dumpster)
的对比，串行与并行。

## 环境

| | |
| --- | --- |
| 机器 | Intel Core Ultra 7 356H，16 核（每核 1 线程），Linux 7.0 |
| Rust | 1.98.1，release |
| paragc | 0.5.0（提交 `d44bad0`），私有堆与公开堆；公开堆文件在 `/dev/shm`。全部回收器在同一轮里量 |
| cppgc | V8 15.2.124.1，经 rusty_v8（`v8` crate 152.2.0） |
| 其他 | gc-arena 0.7.0、gc 0.5.1、dumpster 2.1.0 |
| 方法 | 每个配置在新进程里跑 5 次取最快；1 分钟负载低于 2.5 才开始（开始 2.49、结束 2.61） |
| CPU 一栏 | 计时区间内整个进程（全部线程）的 CPU 时间；CPU ÷ 墙钟 就是实际有几个线程在干活 |

## 工作负载

| 名字 | 量什么 |
| --- | --- |
| `pause` | 2^20 − 1 个节点的活二叉树 + 同样大的一棵死树，然后**一次完整回收**。线程数 = 回收线程数 |
| `pause-cm` / `pause-cs` / `pause-cms` | 只有 paragc：同样的堆，**只量 mutator 被停住的时间**。`cm`：先并发标记（不计时），再量收尾回收；`cs`：标记在停顿里，清扫并发（不计时）；`cms`：两者都并发 |
| `gcbench` | Boehm 的 GCBench：先造一棵 18 层的拉伸树、一棵 16 层的长寿树，再对 4、6、…、16 层各造足够多的临时树（每档分配量是拉伸树的两倍）。各库按自己的节奏回收。总耗时 |
| `alloc-mt` | 总共 2048 棵 10 层的临时树，分给 N 个线程同时分配（分 16 轮），旁边一棵长寿树。只比允许多个 mutator 线程的库。总耗时 |
| `frag` | 碎片：四种大小（数据 24 / 240 / 1600 / 12000 字节，占分配次数 70 / 20 / 8 / 2 %）的对象，每轮 40 万个，按大小以 1/7、1/5、1/3、1/2 的比例随机覆盖持有者里的槽（被覆盖的成为垃圾 ⇒ 寿命随机、存活对象散在各处），30 轮，存活约 80 MB。量总耗时、最后一次完整回收后的 RSS 与每轮之后的峰值 RSS（都扣掉开始时的）。只有 paragc 与 cppgc |

各库的节点形状一样：两个孩子指针 + 一个 `u64`。每段计时之后都检查活树的节点数恰好是 2^层数 − 1。

## 各库怎么用

| | 串行 | 并行 | `gcbench` 里怎么回收 |
| --- | --- | --- | --- |
| **paragc** | `Heap::new(1)` | `Heap::new(n)`：n − 1 个常驻回收线程 | 每棵树后 `collect_if_needed`：年轻回收 + 引用计数，每 16 次一次完整回收 |
| **paragc，公开堆** | `/dev/shm` 文件上的 `PublicHeap`，1 个回收线程 | n 个回收线程 | 上次回收后新分配超过 2 × max(存活, 4 MiB) 时暂停式 `collect`（还没有年轻回收与引用计数） |
| **cppgc** | 挂在 isolate 上（Blink 的用法），单线程平台 + `--single-threaded`；完整回收用 `request_garbage_collection_for_testing(Full)` | 默认平台，n 个工作线程 | V8 自己调度 |
| **cppgc，选项全开** | 同上 | 同上，再加 `--parallel-marking --concurrent-marking --cppheap-concurrent-marking --cppheap-incremental-marking --concurrent-marking-max-worker-num=n` | V8 自己调度 |
| **cppgc，不挂 isolate** | 不挂 isolate，`collect_garbage_for_testing` | 同上 | 照 paragc 的触发策略手动回收 |
| **gc-arena** | `Arena`，`finish_cycle` | – | 每棵树后 `collect_debt` |
| **gc**（rust-gc） | 线程局部堆，`force_collect` | – | 自动 |
| **dumpster** | `unsync`；`pause` 量的是扔掉死树 + `collect` | `sync`，只在 `alloc-mt` | 自动 |

## 结果

墙钟 ms（CPU ÷ 墙钟）；5 次取最快。

### 停顿：一次完整回收，100 万活节点 + 100 万死节点

| | 1 线程 | 4 线程 | 16 线程 |
| --- | ---: | ---: | ---: |
| **paragc** | 20.2 | 8.7 (3.8) | **4.2** (13.0) |
| paragc，并发标记 | 4.6 | 2.4 (3.0) | 1.5 (15.6) |
| paragc，并发清扫 | 15.5 | 6.3 (4.0) | 2.8 (13.1) |
| **paragc，并发标记 + 并发清扫** | **0.02** | **0.02** | **0.06** |
| paragc，公开堆 | 18.4 | 6.5 (3.9) | 3.3 (13.9) |
| cppgc，挂 isolate | 44.2 | 44.3 (1.0) | 44.9 (1.0) |
| cppgc，并行 / 并发选项全开 | 44.2 | 44.5 (1.0) | 44.0 (1.0) |
| cppgc，不挂 isolate | 43.2 | 43.5 (1.0) | 43.3 (1.0) |
| gc-arena | 16.3 | – | – |
| dumpster | 16.3 | – | – |
| gc（rust-gc） | 90.2 | – | – |

### GCBench：总耗时

| | 1 线程 | 4 线程 | 16 线程 |
| --- | ---: | ---: | ---: |
| **paragc** | **50.3** | **54.3** (1.3) | **64.3** (2.2) |
| paragc，公开堆 | 72.7 | 67.3 (1.3) | 83.2 (2.1) |
| cppgc，挂 isolate | 121.0 | 94.2 (1.3) | 100.1 (1.3) |
| cppgc，并行 / 并发选项全开 | 115.5 | 97.6 (1.3) | 98.4 (1.3) |
| cppgc，不挂 isolate | 174.5 | 173.5 (1.0) | 175.5 (1.0) |
| gc-arena | 118.0 | – | – |
| dumpster | 97.8 | – | – |
| gc（rust-gc） | 294.6 | – | – |

### 多 mutator：总耗时

| | 1 线程 | 4 线程 | 16 线程 |
| --- | ---: | ---: | ---: |
| **paragc** | **35.8** | **9.4** (3.7) | **9.0** (8.5) |
| dumpster（`sync`） | 117.5 | 289.4 (3.9) | 343.5 (12.9) |

cppgc、gc-arena、rust-gc 一个堆只能一个线程用，这一项没有它们。

### 碎片：耗时与内存

| | 线程 | 墙钟 ms | 回收后 RSS MB | 峰值 RSS MB |
| --- | ---: | ---: | ---: | ---: |
| **paragc** | 1 | **1578** | **110.5** | **415.6** |
| **paragc** | 16 | **1344** | **111.2** | **416.4** |
| cppgc，挂 isolate（V8 自己调度） | 1 | 2010 | 444.7 | 1894.2 |
| cppgc，挂 isolate（V8 自己调度） | 16 | 1824 | 477.0 | 1984.5 |
| cppgc，不挂 isolate（照 paragc 的触发策略） | 1 | 4134 | 294.4 | 417.1 |
| cppgc，不挂 isolate（照 paragc 的触发策略） | 16 | 4160 | 294.4 | 417.1 |

paragc 0.4.2 在同一负载上回收后 RSS 约 935 MB、峰值约 940 MB：块与大对象找 glibc 要，释放了不还给系统；
年轻回收原地晋升，散落的存活对象各占一行；中对象丢掉空洞剩下的部分；疏散每次最多 1024 块。0.5.0 的做法见下面「怎么读」。

## 怎么读

- **吞吐：年轻回收 + 引用计数**：单线程 GCBench 50.3 ms，dumpster、gc-arena、cppgc 是 98–121 ms——`collect_if_needed` 平时做年轻回收，
  只追新对象，老垃圾由引用计数在后台释放。多 mutator 35.8 ms 对 dumpster 117.5 ms，而且 dumpster 线程越多越慢（引用计数跨线程共享），paragc 越来越快。
- **串行完整停顿 gc-arena、dumpster 更快**：这个 200 万节点的堆完整回收一次，它们 16.3 ms，paragc 单线程 20.2 ms。其中一部分是重新计数：
  完整回收每条边 +1 重建引用计数。paragc 从 4 线程起领先（8.7 ms），16 线程 4.2 ms。
- **GCBench 加线程不变快**：年轻回收每次都小，叫醒更多工人得不偿失（16 线程用了 2.2 倍的 CPU，结果反而慢）。
- **并发标记 + 并发清扫把活移出了停顿**：单线程 20.2 ms 变成 0.02 ms；标记和清扫照样要做，在线程池上、在顺手扫块的 mutator 里。
  这里标记期间没有 mutator 在跑，最后灰队列是空的，剩下的停顿是与块数成正比的簿记；有 mutator 在跑时还要加上清空写屏障攒下的灰对象。
- **公开堆：共享内存几乎不额外花钱**：同一棵树放在公开堆（`/dev/shm` 文件、偏移指针、按指针图追踪）里完整回收一次，单线程 18.4 ms，
  私有堆 20.2 ms——它的头 16 字节而不是 24，也不重建引用计数。GCBench 慢一些（72.7 对 50.3 ms），因为公开堆每次都是完整回收，年轻回收与引用计数还没做。
- **cppgc 的完整回收只用一个线程标记**：挂不挂 isolate、开不开 16 个工作线程和全部并行 / 并发标记选项，强制完整回收都是 CPU ÷ 墙钟 = 1.0、约 43–45 ms。
  只有 V8 自己调度的增量回收里，后台标记用上了别的线程（GCBench 约 1.3）。要在 `pause` 里量它的并发周期得去驱动 V8 的增量 GC，这里没做。
- **cppgc 要付 Rust ⇄ C++ 的边界开销**：经 rusty_v8 调用，每次分配、每个 `Member`、标记时每条边都是一次 FFI；挂 isolate 时 V8 自己那份
  （几乎空的）堆每次完整回收约多 1.3–1.5 ms。用 C++ 直接嵌入的数字会更低。
- **碎片：自管页 + 溢出分配 + 疏散**：块从自己 mmap 的段里切、还回来的块 madvise 掉物理内存；比一行大、当前空洞放不下的对象走溢出游标，不丢掉空洞；
  年轻回收把只被年轻对象和根指着的存活对象搬进紧凑的块；完整回收按存活升序疏散半空以下的块、不设块数上限，块占的比「存活 + 一轮允许新分配的」
  多出 1.5 倍就提前做完整回收。回收后 RSS 是 cppgc 的 23–38 %，峰值与照同样触发策略的 cppgc 持平（V8 自己调度时峰值到 1.9 GB）。要更低的峰值可以在空闲时调
  `Heap::shrink`（把凑满整页的空行也还给系统，之后再分配进去要重新缺页）
- **dumpster 是引用计数**：这些树没有环，死节点在最后一个引用消失时就释放了，它的 `pause` 其实是扔掉死树的耗时。

## 复现

```sh
cd benches
# cppgc 要 V8 的静态库：RUSTY_V8_ARCHIVE 指向本地的一份，不设就下载
RUSTY_V8_ARCHIVE=/path/to/librusty_v8.a ./run.sh                 # 全部
./run.sh paragc cppgc                                           # 只跑一部分
REPS=10 THREADS=8 ./run.sh paragc                               # 多跑几次、换「全部核」那一档
./target/release/bench-paragc pause-cms 16                      # 单个配置
V8_FLAGS="--trace-gc" CPPGC_HEAP=detached ./target/release/bench-cppgc gcbench 4
```

`run.sh` 打印的就是上面的 Markdown 表。请在空闲的机器上跑：别的进程会和回收线程抢 CPU，并行的数字最先变差。
