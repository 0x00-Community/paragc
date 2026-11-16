<a id="top"></a>
<div align="center">

# paragc

### Rust 的并行垃圾回收器。
**Immix 式标记-区域堆，多个 mutator 线程同时用，并发 + 并行标记，并行清扫。全程无锁。**

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
| 🌊 **并发标记** | `Heap::mark_concurrently(&self)` 在 mutator 照常跑的同时标记（`Slot` 写入走初始快照写屏障、新对象黑分配）；之后的 `collect` 只清空写屏障攒下的灰对象再清扫。mutator 只在开头短暂握手时等一下。 |
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
| 并发标记 | 开头：握手字（最高位请求，低位是 mutate 里的线程数）挂上请求、等 mutate 里没人、把根标灰、撤请求——paragc 不扫栈，快照只能在没有 mutator 拿着未挂根的 `Gc` 时照；然后在线程池上标记。标记期间 `Slot` 写入把被覆盖的旧值标上并放进堆的灰队列（初始快照），新对象直接拿本轮纪元、连同行一起标上（黑分配）。空行 = 行标记早于上次完成的纪元，标记方刚写上新纪元的行不会被当成空行。收尾的 `collect`：并行清空灰队列，再清扫 |

工作窃取双端队列与退避来自 [crossbeam](https://github.com/crossbeam-rs/crossbeam)（`crossbeam-deque`、`crossbeam-utils`）；
只追加分段向量与块库存来自 [j8lockfree](https://crates.io/crates/j8lockfree)。

## 📏 规则

- 堆里的类型实现 `Trace`（用 `#[derive(Trace)]`），而且必须 `Send + Sync`：对象可能在任何线程上被读、在回收线程上被追踪和析构。
  要改的边放 `Slot<'gc, T>`（原子指针），写入要带 `Mutator`（`slot.set(m, v)`），写屏障才知道是否在并发标记。
- 不要在同一个堆的 `mutate` 里调用 `mark_concurrently`：它会一直等自己走空。
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
| `concurrent_marking_while_relinking` | mutator 不停地把节点从一个槽摘下（只剩栈上引用）再挂到别处、新建、扔掉，另一个线程同时并发标记；每轮后链上节点数恰好等于理论值，析构数恰好等于扔掉的个数 |

全部测试也在 Miri 下跑（Stacked Borrows 和 Tree Borrows，`cfg(miri)` 下自动缩规模）。要加 `-Zmiri-ignore-leaks`：
crossbeam-epoch 的全局回收器退出时还拿着延迟释放的垃圾；每堆 4 个回收线程时 Stacked Borrows 还会在 crossbeam-epoch 内部报违例，
所以 Miri 下测试用 2 个；`concurrent_marking_while_relinking` 在 Stacked Borrows 下仍会碰到 crossbeam-epoch 内部那处违例，Tree Borrows 下干净：

```sh
cargo test --test gc -- --exact <名字>
MIRIFLAGS="-Zmiri-permissive-provenance -Zmiri-ignore-leaks" cargo +nightly miri test --test gc -- --exact <名字>
MIRIFLAGS="-Zmiri-permissive-provenance -Zmiri-ignore-leaks -Zmiri-tree-borrows" cargo +nightly miri test --test gc -- --exact <名字>
```

## 📊 性能对比

`benches/` 里每个回收器一个 crate（`benches/paragc`、`benches/cppgc`、`benches/gc-arena`、`benches/rust-gc`、
`benches/dumpster`），自成 workspace。`benches/run.sh` 每个配置跑 5 次取最快；CPU 一栏是整个进程（全部线程）的 CPU 时间，
CPU ÷ 墙钟 就是实际有几个线程在干活。Intel Core Ultra 7 356H（16 核）、Linux 7.0、Rust 1.98，除注明外负载低于 3 时测。

**停顿**：2^20 − 1 个活节点 + 2^20 − 1 个死节点（满二叉树），一次完整回收，墙钟 ms（CPU ÷ 墙钟）

| | 1 线程 | 4 线程 | 16 线程 |
| --- | ---: | ---: | ---: |
| **paragc** | 22.3 | 7.6 (3.9) | **4.0** (13.5) |
| **paragc，先并发标记** ¹ | 9.9 | 2.9 | **2.2** |
| cppgc，挂 isolate | 45.0 | 44.4 (1.0) | 44.6 (1.0) |
| cppgc，并行 / 并发标记选项全开 | 44.7 | 43.3 (1.0) | 44.5 (1.0) |
| cppgc，不挂 isolate | 42.3 | 43.4 (1.0) | 43.7 (1.0) |
| gc-arena | 16.6 | – | – |
| dumpster | 16.8 | – | – |
| gc（rust-gc） | 89.7 | – | – |

¹ 先 `mark_concurrently`（这段时间 mutator 照常跑），只量收尾的 `collect`：剩下的是清空写屏障攒下的灰对象 + 清扫。负载约 8 时测。

**GCBench**：Boehm 的经典基准（大量临时树 + 一棵长寿树），各库按自己的节奏回收，总耗时 ms

| | 1 线程 | 4 线程 | 16 线程 |
| --- | ---: | ---: | ---: |
| **paragc** | 114.3 | **58.6** (1.5) | 67.6 (2.7) |
| cppgc，挂 isolate | 120.1 | 97.6 (1.3) | 102.3 (1.3) |
| cppgc，并行 / 并发标记选项全开 | 114.5 | 97.3 (1.3) | 98.9 (1.3) |
| cppgc，不挂 isolate | 174.5 | 174.3 (1.0) | 172.1 (1.0) |
| gc-arena | 117.7 | – | – |
| dumpster | 102.8 | – | – |
| gc（rust-gc） | 290.7 | – | – |

**多 mutator**：总共 2048 棵 10 层的树，分给 N 个线程同时分配，总耗时 ms

| | 1 线程 | 4 线程 | 16 线程 |
| --- | ---: | ---: | ---: |
| **paragc** | 70.4 | 14.5 (3.5) | **12.7** (8.2) |
| dumpster（`sync`） | 116.0 | 272.6 (3.9) | 340.8 (13.0) |

怎么读：

- **单线程时 paragc 不是最快的**：停顿 gc-arena、dumpster 约 17 ms，paragc 22.3 ms；单线程 GCBench dumpster 最快。paragc 靠多线程领先。
- **停顿能扩展到 16 线程，GCBench 不能**：常驻线程池把 16 线程停顿从 9.4 ms 降到 4.0 ms；但 GCBench 仍是 4 线程最快——它每次回收都小，
  叫醒 16 个工人不划算。
- **并发标记把标记移出了停顿**：剩下的是清扫（析构一百万个死对象），并发清扫是路线的下一步。
- **cppgc 的原子停顿只用一个线程标记**：16 个工作线程、并行 / 并发标记选项全开，完整回收仍是 CPU ÷ 墙钟 = 1.0；只有增量回收里的
  后台标记用上别的线程（GCBench 约 1.3）。它经 rusty_v8 调用，每次分配、每个 `Member`、标记时每条边都过一次 Rust ⇄ C++；
  挂 isolate 时 V8 自己那份（几乎空的）堆每次完整回收约多 1.3–1.5 ms。
- **dumpster 是引用计数**：这些树没有环，死节点在最后一个引用消失时就释放了，它的「停顿」其实是扔掉整棵树的耗时。

## 🗺️ 路线

已完成：并行暂停式回收、常驻线程池、并发标记。之后：

1. 并发清扫：并发标记之后的停顿只剩清空灰队列
2. 稀疏块的机会式疏散（Immix defrag）
3. 年轻对象引用计数（LXR 式）：年轻对象死了立刻回收行，追踪只负责环与老对象

## 📜 许可

MIT © Jabberwocky238

<div align="right"><a href="#top">⬆ 回到顶部</a></div>
