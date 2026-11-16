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
| 🏃 **并行回收** | 标记：每个工人一个 Chase–Lev 双端队列、工作窃取；清扫（含析构）：块按批分给同一批工人。 |
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
| 清扫 | 工人按批领块：按起点位图逐个看对象，没标上本轮纪元的析构、清位；块分成 全空 / 有空行 / 满，多于总块数四分之一的空块还给系统 |
| 根 | `Root<B>` 占根表一个槽；根表只追加，空槽号在回收时整理成只读数组、mutate 期间原子游标领取 |
| 缓冲过期 | 缓冲记着（堆, 回收次数），回收过就重来（它的块已被重新分类） |

无锁基础件（Chase–Lev 双端队列、只追加分段向量、库存）来自 [j8lockfree](https://crates.io/crates/j8lockfree)。

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

全部测试也在 Miri 下跑（Stacked Borrows 和 Tree Borrows，`cfg(miri)` 下自动缩规模）：

```sh
cargo test --test gc -- --exact <名字>
MIRIFLAGS="-Zmiri-permissive-provenance" cargo +nightly miri test --test gc -- --exact <名字>
MIRIFLAGS="-Zmiri-permissive-provenance -Zmiri-tree-borrows" cargo +nightly miri test --test gc -- --exact <名字>
```

## 🗺️ 路线

0.1 是暂停式、不移动。之后：

1. 并发标记：`Slot` 写入加初始快照（SATB）屏障，mutator 不再等标记
2. 稀疏块的机会式疏散（Immix defrag）
3. 年轻对象引用计数（LXR 式）：年轻对象死了立刻回收行，追踪只负责环与老对象

## 📜 许可

MIT © Jabberwocky238

<div align="right"><a href="#top">⬆ 回到顶部</a></div>
