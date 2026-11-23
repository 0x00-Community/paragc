# paragc 是什么

paragc 是**给多个线程共用一个堆的 Rust 程序用的追踪式垃圾回收器**。

- 对象放在 32 KiB 的块里，块再切成 256 字节的行，这是 Immix 的布局（Blackburn & McKinley，2008）。
- 各线程在空闲行里各自按指针递增分配。
- 一次回收在所有核上标记和清扫。

## 和别的回收器不同在哪

| | |
| --- | --- |
| 多个 mutator | `Heap::scope_shared(&self)` 可以同时在任意多个线程上跑。每个线程从自己的缓冲（`Tlab`）分配，用一次原子操作领整块。 |
| 靠借用互斥 | 回收要 `&mut` 堆或某个独占 scope，而每个 `ScopeLocal` 都是对自己 scope 的借用，所以没有一个能活过回收：借用检查器不允许。这个借用就是全停：没有安全点协议，也没有锁。 |
| 并发标记 | `Heap::mark_concurrently(&self)` 在 mutator 继续跑的同时标记。`Slot` 写入上的「开始时快照」屏障保证正确，新对象直接分配成黑色。 |
| 并发清扫 | `Heap::finish_marking` 只把标记收尾。mutator 需要块时自己清扫一块，`sweep_concurrently` 在线程池上清扫剩下的。 |
| 疏散 | 全停回收在标记的同时把活对象从稀疏的块里搬出去（Immix 的碎片整理）。 |
| 年轻回收 | `collect_young` 只追踪上次回收以来新分配的对象，并把活下来的压紧。 |
| 引用计数 | 老对象带一个小计数，在年轻回收时结算、在后台释放（LXR 的做法）。完整追踪隔几轮才做一次，用来收环。 |
| 弱引用 | `Weak<B>` 不让目标存活；每种回收都会清空或移动它。 |
| 堆之间的引用 | `Far` 从一个私有堆直接指向另一个；`collect_group` 回收跨这些堆的环。 |
| 跨堆（Linux） | `CrossHeap` 放在共享内存文件里。多个进程同时映射使用，被杀的进程不会卡住别人，对象可以签名后让别的进程找到。 |

## 谁在用

paragc 是 [j8](https://github.com/0x00-Community/j8) JavaScript 引擎的堆。在 j8 里：

- 每个 JS 对象、字符串、闭包都是 paragc 对象；
- 一个 isolate 拥有一个 `Heap`；
- 开了 `crossheap` feature 时，isolate 把一个跨堆挂成自己的老年代。熬过一轮的 JS 值被晋升进去，别的进程就能凭签名读到。

## 它不是什么

- **不是保守式回收器。** paragc 不扫栈。回收后要活下来的对象，都必须能从某个 `ScopeGlobal` 走到。`ScopeLocal` 是
  安全的，因为借用检查器会在它所属 scope 的任何一次回收之前让它失效；`ScopeGlobal` 则由回收改写。
- **不是 `no_std`。** 它需要 `std` 的线程、`mmap` 和原子操作。
- **不是 `Rc` 的直接替代品。** 对象通过带Brand的Handle访问。类型要实现 `Trace`（通常用 `#[derive(Trace)]`），带 `ScopeLocal` 字段的
  类型不能实现 `Drop`。
