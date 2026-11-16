<a id="top"></a>
<div align="center">

# paragc

### Rust 的并行垃圾回收器。
**Immix 式标记-区域堆，多个 mutator 线程同时用，并行标记与清扫。全程无锁。**

[![Rust](https://img.shields.io/badge/Rust-100%25-orange?style=for-the-badge&logo=rust)](https://www.rust-lang.org)
[![Miri](https://img.shields.io/badge/Miri-clean-success?style=for-the-badge)](#-验证)
[![Lock-free](https://img.shields.io/badge/locks-none-blue?style=for-the-badge)](#-怎么做的)
[![License](https://img.shields.io/badge/License-MIT-brightgreen?style=for-the-badge)](#-许可)

[![Heap](https://img.shields.io/badge/heap-mark--region%20%C2%B7%20non--moving-8A2BE2?style=flat-square)](#-怎么做的)
[![Mark](https://img.shields.io/badge/mark-parallel%20%C2%B7%20work%20stealing-262D3A?style=flat-square)](#-怎么做的)
[![std](https://img.shields.io/badge/std-required-283272?style=flat-square)](#-规则)

[🇺🇸 English](README.md) · **🇨🇳 简体中文**

</div>

---

## ⚡ paragc 是什么

paragc 是**给多个线程共用一个堆的 Rust 程序用的追踪式垃圾回收器**。对象放在 32 KiB 的块里，块切成 256 B 的行（Immix 布局）；
线程各自往空行里碰撞分配，回收时标记和清扫用上全部核。

## 🔥 有什么不同

| | |
| --- | --- |
| 🧵 **多个 mutator** | `Heap::mutate(&self)` 可以在任意多个线程上同时调用；各自在自己的分配缓冲（`Tlab`）里碰撞分配，整块领用只要一次原子操作。 |
| 🔒 **借用即独占** | `Heap::collect(&mut self)` 不可能和 `mutate` 重叠：借用检查器就是「停住世界」，没有安全点协议，也没有锁。 |
| 🏃 **并行回收** | 每个堆一个常驻线程池（两次回收之间线程挂起）。标记：每个工人一个 crossbeam 工作窃取双端队列、成批偷；清扫（含析构）：块按批分给同一批工人。 |
| 🏷️ **带品牌的句柄** | `Gc<'gc, T>` 逃不出 `mutate` 的闭包；`Root<B>` 不带生命周期，可以存在任何地方、跨线程传。 |
| 📌 **不移动** | 对象从不移动，指针就是普通指针，不需要转发。 |
| 🛡️ **编译期检查** | `#[derive(Trace)]` 拒绝被追踪的类型实现 `Drop`；对象数据对齐超过 8 字节直接编译报错。 |

## 🚀 用法

```rust
use paragc::{Gc, Heap, Root, Trace, brand};

#[derive(Trace)]
struct Node<'gc> {
    value: u64,
    next: Option<Gc<'gc, Node<'gc>>>,
}

// 根句柄不带 'gc，读出来时由品牌套回去
brand!(NodeB = for<'gc> Node<'gc>);

let mut heap = Heap::default();
let list: Root<NodeB> = heap.mutate(|m| {
    let mut head = None;
    for value in 0..1000 {
        head = Some(m.alloc(Node { value, next: head }));
    }
    m.root(head.unwrap())
});
heap.collect();
```

## 🧠 怎么做的

| 部分 | 做法 |
| --- | --- |
| 块 | 按 32 KiB 对齐，对象地址抹低位就是块起点；块头是行标记（每行一个 `u32` 纪元）和对象起点位图（每 8 字节一位） |
| 分配 | 分配缓冲拿着一个块，在「行标记 ≠ 当前纪元」的连续空行里碰撞；放不下找下一个空洞，块用完了依次领「有空行的块」→「全空的块」→ 新块。超过 8 KiB 的对象单独分配 |
| 对象头 | 24 字节：追踪函数、析构函数、标记纪元（原子）、粒度数（最高位 = 大对象） |
| 标记 | 根分给各工人；`swap(纪元)` 抢到的线程负责这个对象：写它跨的行标记、压进自己的队列（没有边的类型不压） |
| 结束判定 | 自己的队列空、也偷不到 ⇒ 空闲计数 +1，边等边偷；全部工人空闲 ⇒ 结束 |
| 线程池 | 第一次回收时起、活到堆析构；分派 = 一个原子字（代号 + 参与人数）加一代、只叫醒参与的工人；空闲工人先退避再挂起；工人里的 panic 在回收线程上重新抛出 |
| 清扫 | 工人按批领块：按起点位图逐个看对象，没标上本轮纪元的析构、清位；块分成 全空 / 有空行 / 满，多于总块数四分之一的空块还给系统 |
| 根 | `Root<B>` 占根表一个槽；根表只追加，空槽号在回收时整理成只读数组、mutate 期间原子游标领取 |
| 缓冲过期 | 缓冲记着（堆, 回收次数），回收过就重来（它的块已被重新分类） |

工作窃取双端队列与退避来自 [crossbeam](https://github.com/crossbeam-rs/crossbeam)（`crossbeam-deque`、`crossbeam-utils`）；
只追加分段向量与块库存来自 [j8lockfree](https://crates.io/crates/j8lockfree)。

## 📏 规则

- 堆里的类型实现 `Trace`（用 `#[derive(Trace)]`），而且必须 `Send + Sync`：对象可能在任何线程上被读、在回收线程上被追踪和析构。
  要改的边放 `Slot<'gc, T>`（原子指针）。
- 有 `Gc` 字段的类型不许实现 `Drop`：析构顺序不定，`Drop` 里可能碰到已经析构的对象。derive 会检查，`#[trace(unsafe_drop)]` 关掉检查。
- 只有从 `Root` 可达的对象能活过回收。
- 回收进行中释放的根，它指的对象可能要到下一次回收才死。
- 只支持 `std`。

## ✅ 验证

| 测试 | 覆盖什么 |
| --- | --- |
| `rooted_list_survives` · `garbage_is_finalized` · `heap_drop_finalizes_everything` | 存活、析构数恰好、丢掉堆就析构全部 |
| `threads_mutate_at_once` · `blocks_are_reused` · `large_objects` | 多 mutator 并发、行复用使块数不涨、大对象 |
| `deep_binary_tree` | 20 层满二叉树（约 100 万节点）：剪半棵、全释放，析构数恰好 |
| `deep_chain` | 100 万节点单链从中间剪断；深度不吃调用栈 |
| `eight_threads_eight_heaps_crossed` | 8 线程 × 8 堆：跨堆持有根、别的线程检查与释放、全部堆并行回收的同时释放根、经大对象成环 |
| `crossed_deep_trees` | 8 堆各一条深链 + 一棵 15 层满二叉树，树叶锚在下一个堆的深链上；每次剪断后析构数都恰好等于理论值 |

全部测试也在 Miri 下跑（Stacked Borrows 和 Tree Borrows，`cfg(miri)` 下自动缩规模）。要加 `-Zmiri-ignore-leaks`：
crossbeam-epoch 的全局回收器退出时还拿着延迟释放的垃圾；每堆 4 个回收线程时 Stacked Borrows 还会在 crossbeam-epoch 内部报违例，
所以 Miri 下测试用 2 个：

```sh
cargo test --test gc -- --exact <名字>
MIRIFLAGS="-Zmiri-permissive-provenance -Zmiri-ignore-leaks" cargo +nightly miri test --test gc -- --exact <名字>
MIRIFLAGS="-Zmiri-permissive-provenance -Zmiri-ignore-leaks -Zmiri-tree-borrows" cargo +nightly miri test --test gc -- --exact <名字>
```

## 📊 性能对比

`benches/` 里每个回收器一个 crate（`benches/paragc`、`benches/cppgc`、`benches/gc-arena`、`benches/rust-gc`、
`benches/dumpster`），自成 workspace。`benches/run.sh` 每个配置跑 5 次取最快；CPU 一栏是整个进程（全部线程）的 CPU 时间，
CPU ÷ 墙钟 就是实际有几个线程在干活。Intel Core Ultra 7 356H（16 核）、Linux 7.0、Rust 1.98，负载低于 3 时测。

**停顿**：2^20 − 1 个活节点 + 2^20 − 1 个死节点（满二叉树），一次完整回收，墙钟 ms（CPU ÷ 墙钟）

| | 1 线程 | 4 线程 | 16 线程 |
| --- | ---: | ---: | ---: |
| **paragc** | 21.9 | 13.0 (3.9) | **9.4** (13.7) |
| cppgc，挂 isolate | 43.6 | 44.2 (1.0) | 44.9 (1.0) |
| cppgc，并行 / 并发标记选项全开 | 44.1 | 43.3 (1.0) | 44.2 (1.0) |
| cppgc，不挂 isolate | 43.5 | 43.7 (1.0) | 43.5 (1.0) |
| gc-arena | 16.4 | – | – |
| dumpster | 17.0 | – | – |
| gc（rust-gc） | 90.2 | – | – |

**GCBench**：Boehm 的经典基准（大量临时树 + 一棵长寿树），各库按自己的节奏回收，总耗时 ms

| | 1 线程 | 4 线程 | 16 线程 |
| --- | ---: | ---: | ---: |
| **paragc** | 116.2 | **67.8** (1.8) | 82.5 (4.0) |
| cppgc，挂 isolate | 114.9 | 92.2 (1.3) | 102.4 (1.3) |
| cppgc，并行 / 并发标记选项全开 | 114.4 | 97.3 (1.3) | 99.0 (1.4) |
| cppgc，不挂 isolate | 166.6 | 172.6 (1.0) | 174.6 (1.0) |
| gc-arena | 117.6 | – | – |
| dumpster | 103.2 | – | – |
| gc（rust-gc） | 291.1 | – | – |

**多 mutator**：总共 2048 棵 10 层的树，分给 N 个线程同时分配，总耗时 ms

| | 1 线程 | 4 线程 | 16 线程 |
| --- | ---: | ---: | ---: |
| **paragc** | 70.9 | **16.3** (3.6) | 19.5 (8.4) |
| dumpster（`sync`） | 116.0 | 264.7 (3.9) | 341.1 (12.9) |

怎么读：

- **单线程时 paragc 不是最快的**：停顿 gc-arena、dumpster 是 16–17 ms，paragc 21.9 ms；单线程 GCBench dumpster 最快。paragc 靠多线程领先。
- **paragc 过了 4 线程扩展性差**：16 线程停顿 9.4 ms 却烧掉 129 ms CPU，GCBench 16 线程反而比 4 线程慢——每次回收都现开线程、
  空闲的工人在自旋。修它排在路线第一位。
- **cppgc 的原子停顿只用一个线程标记**：16 个工作线程、并行 / 并发标记选项全开，完整回收仍是 CPU ÷ 墙钟 = 1.0；只有增量回收里的
  后台标记用上别的线程（GCBench 约 1.3）。它经 rusty_v8 调用，每次分配、每个 `Member`、标记时每条边都过一次 Rust ⇄ C++；
  挂 isolate 时 V8 自己那份（几乎空的）堆每次完整回收约多 1.3–1.5 ms。
- **dumpster 是引用计数**：这些树没有环，死节点在最后一个引用消失时就释放了，它的「停顿」其实是扔掉整棵树的耗时。

## 🗺️ 路线

0.1 是暂停式、不移动。之后：

1. 并发标记：`Slot` 写入加初始快照（SATB）屏障，mutator 不再等标记
2. 稀疏块的机会式疏散（Immix defrag）
3. 年轻对象引用计数（LXR 式）：年轻对象死了立刻回收行，追踪只负责环与老对象

## 📜 许可

MIT © Jabberwocky238

<div align="right"><a href="#top">⬆ 回到顶部</a></div>
