<a id="top"></a>
<div align="center">

# paragc

### Rust 的并行垃圾回收器。
**Immix 式标记-区域堆，多个 mutator 线程同时用，标记与清扫都并发 + 并行。全程无锁。**

[![Rust](https://img.shields.io/badge/Rust-100%25-orange?style=for-the-badge&logo=rust)](https://www.rust-lang.org)
[![Miri](https://img.shields.io/badge/Miri-clean-success?style=for-the-badge)](#-验证)
[![Lock-free](https://img.shields.io/badge/locks-none-blue?style=for-the-badge)](#-怎么做的)
[![License](https://img.shields.io/badge/License-MIT-brightgreen?style=for-the-badge)](#-许可)

[![Heap](https://img.shields.io/badge/heap-mark--region%20%C2%B7%20evacuating-8A2BE2?style=flat-square)](#-怎么做的)
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
| 🌊 **并发标记** | `Heap::mark_concurrently(&self)` 在 mutator 照常跑的同时标记（`Slot` 写入走初始快照写屏障、新对象黑分配）。mutator 只在开头短暂握手时等一下。 |
| 🧹 **并发清扫** | `Heap::finish_marking(&mut self)` 只做标记收尾、把块交给清扫器；mutator 要块时顺手扫（惰性清扫），`Heap::sweep_concurrently(&self)` 在线程池上扫剩下的。两者都用上，停顿只剩清空灰队列。 |
| 🏃 **并行回收** | 每个堆一个常驻线程池（两次回收之间线程挂起）。标记：每个工人一个 crossbeam 工作窃取双端队列、成批偷；清扫（含析构）：块按批分给同一批工人。 |
| 🏷️ **带品牌的句柄** | `Gc<'gc, T>` 逃不出 `mutate` 的闭包；`Root<B>` 不带生命周期，可以存在任何地方、跨线程传。 |
| 📦 **机会式疏散** | 暂停式回收在标记的同时把稀疏块里的活对象搬走（Immix defrag）：一遍里完成标记、复制、改写全部边。并发标记从不搬对象，所以 mutator 不需要读屏障。 |
| 🐣 **年轻回收** | `Heap::collect_young` 只追上次回收后分配的对象，从根与写屏障记下的槽出发，只扫上次回收后分配过的块；老对象一个不碰。`collect_if_needed` 平时做年轻回收、每几次穿插一次完整回收（LXR 式年轻对象回收的第一期）。 |
| 🔢 **引用计数** | 老对象带一个小计数，由 `Slot` 写屏障维护（每个槽每轮登记一次），在年轻回收时结算。停顿里只加增量、把减量放进队列；`sweep_concurrently` 在线程池上与 mutator 并发地连锁释放计数归零的对象，它们的行在下一次停顿还给分配器。追踪（完整回收）少跑，只为环与计数溢出的对象（LXR 第二、三期）。 |
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
| 并发标记 | 开头：先把上一轮扫完（它按「纪元 ≠ 那一轮」判死，必须在新一轮把活对象标成更新纪元之前结束）；握手字（最高位请求，低位是 mutate 里的线程数）挂上请求、等 mutate 里没人、把根标灰、撤请求——paragc 不扫栈，快照只能在没有 mutator 拿着未挂根的 `Gc` 时照；然后在线程池上标记。标记期间 `Slot` 写入把被覆盖的旧值标上并放进堆的灰队列（初始快照），新对象直接拿本轮纪元、连同行一起标上（黑分配）。空行 = 行标记早于上次完成的纪元，标记方刚写上新纪元的行不会被当成空行。`finish_marking` 并行清空灰队列 |
| 并发清扫 | `finish_marking` 把全部块与大对象交给清扫器就返回。领用是原子游标，每个块恰好被领到的那个线程扫一次、扫完才交出去：mutator 要块时先拿扫好的，没有就领一个没扫的当场扫；`sweep_concurrently` 在线程池上扫剩下的，并等别人手里领走的扫完。新对象带上次完成的纪元，清扫期间分配的不会被当成死的。`collect` = `finish_marking` + `sweep_concurrently` |
| 疏散 | 只在暂停式 `finish_marking`（没有并发标记在进行）时做。候选：清扫后没再分配过、存活不到块的 1/4 的块，至少 4 个、至多 1024 个；其余扫好的块当搬入的目标。标记时第一个追到候选块里对象的工人认领它（标记字：复制中），抄进自己的复制缓冲（新副本标成本轮，带起点位与行标记），旧头里留下新地址；别人等它或直接取新地址。每条边经 `Trace::relocate` 改写；根表槽用 CAS 改，同时在别的线程析构的句柄不会被改回来。清扫时旧副本只清起点位、不析构 |
| 年轻回收 | 粘性标记位：标着上次完成纪元的是老对象，新对象出生是 0（年轻）。年轻回收用同一个纪元标记，碰到老对象就停，追到的年轻对象晋升。老 → 年轻的边只能经 `Slot` 写入产生，所以不在并发标记时写屏障在写入年轻对象时记下这个槽；并发标记期间新对象都是黑的、收尾后都是老的，不用记。只扫上次清扫后分配过的块；任何回收都清空记忆集 |
| 引用计数 | RC(x) = 老对象指向 x 的边数；年轻对象的边在它晋升时才计入。计数放在头里和大小挨着（6 位，满了就粘住，外加一位奇偶：停顿式完整回收翻转它，标记的同时把每条边精确地重新计一遍）。`Slot` 记着上次计入的值；老对象的槽本轮第一次被写时登记，年轻回收时结算（新值 +1、上次计入的值 −1），当前的根各 +1，上一轮的根延迟到这时 −1。减量在全部增量之后处理；减到 0 的老对象把它的边逐个 −1、析构、清起点位，受影响的块重算行标记。并发标记同样重新计数（见下）。只有一个标记工人时没有别人改计数，重新计数用普通读写、不用带锁的原子读改写 |
| 后台释放 | `finish_young`（停顿）结算增量、把减量放进队列；`collect_young` = `finish_young` + `sweep_concurrently`。工人取减量，把计数减到 0 的那个线程析构对象、清起点位、把它的边的减量放进队列、记下脏块。计数为 0 的对象没有计入的引用，根与晋升的年轻对象都已计入，mutator 碰不到。脏块在下一次停顿开头按剩下的老对象重算行；上次清扫后被拿去分配过的块保留这个状态、交给年轻清扫（里面可能有没扫的死年轻对象）。任何回收开始前先做完未处理的减量（免得重新计数翻转奇偶后再减）。计数可信时 `collect_if_needed` 每 16 次年轻回收一次完整回收，否则每 4 次 |
| 并发重新计数 | 握手时翻转奇偶，丢掉握手前的登记（记的是旧计数下的值；活着的所在对象会被重新扫描）。标记工人扫描的每条边 +1。标记期间槽本轮第一次被写同样登记；工人重新计数一个槽后再读一次，期间 mutator 改写过就替它登记（年轻对象的槽还不计数，mutator 不会登记）。黑分配的对象记进队列，收尾时补算它们的边；然后结算登记的槽（新值 +1、上次计入的值 −1，减量只留给本轮标记为活的对象——死的马上被清扫）。收尾一返回计数就可信 |

工作窃取双端队列与退避来自 [crossbeam](https://github.com/crossbeam-rs/crossbeam)（`crossbeam-deque`、`crossbeam-utils`）；
只追加分段向量与块库存来自 [j8lockfree](https://crates.io/crates/j8lockfree)。

## 📏 规则

- 堆里的类型实现 `Trace`（用 `#[derive(Trace)]`），而且必须 `Send + Sync`：对象可能在任何线程上被读、在回收线程上被追踪和析构。
  要改的边放 `Slot<'gc, T>`（原子指针），写入要带 `Mutator`（`slot.set(m, v)`），写屏障才知道是否在并发标记。
- `Slot` 占两个字（值与上次计入的值）。单个对象的数据至多 128 MiB。
- 暂停式回收会搬对象。`Gc` 只在一次 `mutate` 里有效，所以察觉不到，但 `Gc::as_ptr` 两次 `mutate` 之间可能不同。
  手写有边的 `Trace` 还要实现 `relocate`（derive 会生成）。
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
| `concurrent_sweeping_while_relinking` | 同样的 mutator 贯穿并发标记、`finish_marking` 停顿与并发清扫（它们顺手扫块，另一个线程同时 `sweep_concurrently`）；从不析构还挂着的节点，计数恰好 |
| `evacuation_compacts_sparse_blocks` | 只留 1/16：下一次回收疏散稀疏块；块数下降、值不变、地址变了、析构数恰好 |
| `evacuation_with_shared_children` | 很多父节点共享少数子节点（一条 `Slot`、一条普通字段），多个工人争着认领同一个子节点；每个父节点的两条边都落到同一个新副本上，不会重复析构 |
| `young_collection_frees_only_young_garbage` | 挂在老对象槽上、老节点之后、另一个年轻节点底下的年轻对象都活着；年轻回收恰好析构扔掉的年轻对象（追踪）与扔掉的老对象（引用计数） |
| `young_collections_while_relinking` | mutator 在老、年轻对象之间挪节点，轮流年轻回收、并发标记后年轻回收收尾、完整回收；每轮计数恰好 |
| `background_release_while_mutating` | 整条老链一次扔掉：`finish_young` 返回时一个都还没析构；之后连锁释放在线程池上跑、mutator 同时照常挪，不析构还挂着的；下一次停顿空出的行被复用 |
| `concurrent_marking_recounts` | 一轮并发标记（mutator 挪动、黑对象把节点挂到自己底下）收尾后计数立刻可信：扔掉的老链在下一次年轻回收按计数释放，之后几轮不误收挂着的节点 |
| `reference_counting_leaves_cycles_to_tracing` | 整体扔掉的老对象链在年轻回收时按计数释放（500 层连锁）；两节点的环留到完整回收 |

全部测试也在 Miri 下跑（Stacked Borrows 和 Tree Borrows，`cfg(miri)` 下自动缩规模）。要加 `-Zmiri-ignore-leaks`：
crossbeam-epoch 的全局回收器退出时还拿着延迟释放的垃圾；每堆 4 个回收线程时 Stacked Borrows 还会在 crossbeam-epoch 内部报违例，
所以 Miri 下测试用 2 个；两条并发测试在 Stacked Borrows 下仍会碰到 crossbeam-epoch 内部那处违例，Tree Borrows 下干净：

```sh
cargo test --test gc -- --exact <名字>
MIRIFLAGS="-Zmiri-permissive-provenance -Zmiri-ignore-leaks" cargo +nightly miri test --test gc -- --exact <名字>
MIRIFLAGS="-Zmiri-permissive-provenance -Zmiri-ignore-leaks -Zmiri-tree-borrows" cargo +nightly miri test --test gc -- --exact <名字>
```

## 📊 性能对比

16 核机器上与 cppgc（V8 的 Oilpan）、gc-arena、gc（rust-gc）、dumpster 对比；墙钟 ms，5 次取最快。环境、工作负载、全部配置与怎么读：
**[benches/benchmark.zh-CN.md](benches/benchmark.zh-CN.md)**。

| | paragc | cppgc | gc-arena | dumpster | gc |
| --- | ---: | ---: | ---: | ---: | ---: |
| 停顿，100 万活 + 100 万死，1 线程 | 25.7 | 45.0 | **15.8** | 16.4 | 87.6 |
| 停顿，16 线程 | **4.3** | 43.4 | – | – | – |
| 停顿，并发标记 + 并发清扫，1 线程 | **0.03** | – | – | – | – |
| GCBench，1 线程 | **51.3** | 119.2 | 115.3 | 102.2 | 281.7 |
| GCBench，4 线程 | **55.4** | 95.3 | – | – | – |
| 多 mutator，16 线程 | **9.2** | – | – | 334.3 | – |

串行停顿 gc-arena、dumpster 更短（paragc 的完整回收还要重建引用计数）；多线程以及吞吐 paragc 领先。cppgc 的强制完整回收即使并行选项全开也只用一个线程标记，而且这里每次分配、每条边都要过一次 FFI。

## 🗺️ 路线

已完成：并行暂停式回收、常驻线程池、并发标记、并发 + 惰性清扫、机会式疏散、年轻回收、老对象引用计数（减量并发处理），
完整追踪每 16 次年轻回收才一次、并发标记顺带重新计数、单线程重新计数更便宜。可能的下一步：并发的年轻回收、更小的 `Slot`
（为了记住上次计入的值，现在占两个字）。

## 📜 许可

MIT © Jabberwocky238

<div align="right"><a href="#top">⬆ 回到顶部</a></div>
