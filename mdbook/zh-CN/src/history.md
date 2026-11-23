# 最早的垃圾收集器

垃圾回收几乎和高级语言一样古老。今天用到的大部分想法，都是在 1960 到 1990 年间发明的。

## 1960：Lisp 与标记-清扫

John McCarthy 在《Recursive Functions of Symbolic Expressions and Their Computation by Machine》（1960）里描述的 Lisp，
所有数据都由 cons 单元构成，程序员没有办法释放它们。空闲链表用完时，系统：

1. 从程序的寄存器和变量出发；
2. 沿每个 `car`、`cdr` 指针走，给走到的每个单元置标记位；
3. 扫一遍全部内存，把没标记的单元放回空闲链表。

这就是**标记-清扫**，第一个垃圾收集器，实现在 IBM 704 上。整个回收期间程序是停着的。论文里把这个过程叫作「回收」
（reclamation），「垃圾回收」这个名字不久之后才出现。

## 1960：引用计数

同一年，George Collins 发表了《A Method for Overlapping and Erasure of Lists》。每个单元数着指向自己的指针，计数到 0 的单元
立即释放，连同只有它指着的东西一起。没有停顿，但每次改指针都有开销，环永远释放不掉。引用计数一直延续至今：Smalltalk-80、
Objective-C 和 Swift（ARC）、CPython、C++ 的 `shared_ptr`、Rust 的 `Rc`/`Arc`。

## 1963–1970：复制式收集器

- Marvin Minsky（1963）回收 Lisp 堆的办法，是把活单元复制到外存再复制回来。
- Robert Fenichel 和 Jerome Yochelson（1969）把内存分成两个**半区**。分配只填其中一个；回收时把可达的东西全部复制到另一个，
  然后两者交换角色。
- C. J. Cheney（1970）让复制变成**广度优先、非递归**：目标区本身就充当待扫描对象的队列，不需要栈。

复制式回收只碰活对象，留下的空闲内存是整块的，于是分配就是推一下指针。代价是一半内存闲着。

## 1970 年代：压缩

标记-压缩算法不用第二个区，就把活对象滑到堆的一端。LISP 2 算法第一遍算新地址，第二遍更新指针，第三遍搬对象。Jonkers（1979）
等人用指针穿线减少了遍数。压缩在不付半区内存代价的前提下保住了快速分配。

## 1975–1978：程序运行时回收

- Guy Steele（1975），以及 Edsger Dijkstra、Leslie Lamport 等人的《On-the-Fly Garbage Collection》（1978），描述了与程序
  **并发**运行的收集器。Dijkstra 的论文提出了**三色**抽象和插入式写屏障。
- Henry Baker（1978）让复制变成**增量**的、实时的：每次分配时收集器复制一点，再用**读屏障**保证程序只看到对象的新副本。
- 汤浅太一（Taiichi Yuasa，1990）发表了删除屏障，它是**开始时快照**标记的基础。

## 1983–1984：分代

Henry Lieberman 和 Carl Hewitt（1983）观察到大多数对象死得早，提出更频繁地回收新对象。David Ungar 在 Berkeley Smalltalk
里实现的 **Generation Scavenging**（1984）让它落地：小小的新生代经常复制，存活者晋升，记忆集记下老指向年轻的指针。此后几乎
每个生产用的收集器都是分代的。

## 1988：给 C 用的保守式回收

Hans Boehm、Alan Demers 和 Mark Weiser 为本来没打算用 GC 的 C 和 C++ 程序做了一个收集器。它**保守地**扫描栈、寄存器和数据段：
任何看起来像堆地址的字都让那个对象存活。它从不搬对象。Boehm GC 至今仍被 GNU Guile、Inkscape 以及许多语言运行时使用（Mono
早年也用过）。

## 1990–2000 年代：走向现代设计

- **并行收集器**在停顿里用多个线程，靠工作窃取平衡负载（Endo 等，1997；Flood 等，2001）。
- **大部分并发**的标记-清扫（Boehm 等，1991；Printezis 与 Detlefs，2000）成了 JVM 里 CMS 的基础。
- **区域式堆**让收集器可以单独回收或疏散堆的一部分，后来发展为 JVM 的 G1。
- **Immix**（Stephen Blackburn、Kathryn McKinley，2008）提出**标记-区域**堆：块切成行，在空闲行里递增分配，机会式地复制稀疏块。
  paragc 的堆就是 Immix 堆。
- **LXR**（赵文宇、Stephen Blackburn、Kathryn McKinley，2022）把 Immix 与老对象的引用计数、偶尔的并发追踪结合起来，表明低延迟
  不必牺牲吞吐量。paragc 照它的前三期实现。

## 教科书

Richard Jones、Antony Hosking、Eliot Moss 的《The Garbage Collection Handbook》（第 2 版，2023）深入覆盖了以上全部内容，是
标准参考书。
