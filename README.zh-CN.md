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

[🇺🇸 English](README.md) · **🇨🇳 简体中文** · [📖 paragc 之书](https://paragc.app238.com/zh-CN/)

</div>

---

## ⚡ paragc 是什么

paragc 是**给多个线程共用一个堆的 Rust 程序用的追踪式垃圾回收器**。对象放在 32 KiB 的块里，块切成 256 B 的行（Immix 布局）；
线程各自往空行里碰撞分配，回收时标记和清扫用上全部核。

## 🧭 核心宗旨

paragc 的每一部分都要守住下面几条。违反其中任何一条的设计，再方便也不采用。

1. **没有永生的东西。** 堆里的每个对象，一旦不可达就会被回收，运行时元数据也不例外：宿主的 shape、字节码、类型描述、缓存。没有永久区，没有「只分配不释放」的 arena，不存在设计上的泄漏。必须保持地址不变的东西，放进不搬家的空间，但照样回收。
2. **不用锁。** 分配、回收，以及线程之间、进程之间的协调，都靠原子操作、无锁结构和 futex 等待，任何地方都不拿互斥锁。
3. **靠借用互斥。** 全停回收取 `&mut Heap`，`ScopeLocal` 出不了自己的 `scope`。借用检查器证明此时没有 mutator 在里面，所以没有安全点、不扫栈、不做保守式钉住。
4. **进程不等死进程。** 在共享堆里，一个进程在任何一条指令上被杀，既不会卡住别人，也不会留下任何会被当作对象读取的东西。

代码里还没做到第 1 条的地方：

- 跨堆的**类型表和指针图**只增不减：一个类型的最后一个对象死后，它仍留在表里（容量 1024 个类型、64K 个指针图偏移）；
- 进程死在「领到块或入口」和「记下来」之间，那个块或入口就**永远用不上**了。

## 🔥 有什么不同

| | |
| --- | --- |
| 🧵 **多个 mutator** | `Heap::scope(&self)` 可以在任意多个线程上同时调用；各自在自己的分配缓冲（`Tlab`）里碰撞分配，整块领用只要一次原子操作。 |
| 🔒 **借用即独占** | `Heap::collect(&mut self)` 不可能和 `scope` 重叠：借用检查器就是「停住世界」，没有安全点协议，也没有锁。 |
| 🌊 **并发标记** | `Heap::mark_concurrently(&self)` 在 mutator 照常跑的同时标记（`Slot` 写入走初始快照写屏障、新对象黑分配）。mutator 只在开头短暂握手时等一下。 |
| 🧹 **并发清扫** | `Heap::finish_marking(&mut self)` 只做标记收尾、把块交给清扫器；mutator 要块时顺手扫（惰性清扫），`Heap::sweep_concurrently(&self)` 在线程池上扫剩下的。两者都用上，停顿只剩清空灰队列。析构缺省在全部回收线程上并行跑；`Heap::set_parallel_drop(false)` 改成只在调用回收 / `sweep_concurrently` 的线程上析构（判死、算行照旧并行），析构要释放 mutator 线程分配的内存时用它，免得跨线程释放让分配器争用。 |
| 🚧 **分配上限** | `Heap::set_limit(Some(bytes))` 给堆持有的字节（块加大对象，`Heap::used`）设上限；越过之后取的第一个块或大对象在分配线程上调一次 `Heap::on_limit` 的钩子，并解除上限。只有取块时才检查，块内分配没有多出开销。运行时用它中止长过堆上限的 JS；cross-heap 里的分配不计入。 |
| ⏰ **回收阈值** | `Heap::set_collect_threshold(Some(bytes))` 设定上次回收以来新占用多少字节就该回收（缺省用 `collect_if_needed` 的增长策略：存活字节 × `GROWTH`）。越过之后取的第一个块或大对象在分配线程上调一次 `Heap::on_collect_needed` 的钩子，`Heap::request_collect()` 可以从任何线程手动触发；每次回收后重新武装，`collect_if_needed` 用的是同一个阈值。paragc 自己没有安全点：钩子只通知上层运行时，由它在下一个安全点回收。 |
| 🏃 **并行回收** | 每个堆一个常驻线程池（两次回收之间线程挂起）。标记：每个工人一个 crossbeam 工作窃取双端队列、成批偷；清扫（含析构）：块按批分给同一批工人。 |
| 🏷️ **带Brand的 Handle** | `ScopeLocal<'heap, 'scope, T>` 逃不出 `scope` 的闭包；`ScopeGlobal<'heap, 'heap, B>` 不带生命周期，可以存在任何地方、跨线程传。 |
| 🪪 **Handle** | `Scope::alloc_handle` 返回 `Handle<'heap, 'scope, T>`，指向堆里 handle 表的表项而不是对象本身。对象搬动只改写表项，所以 handle、它的 `==` / `Hash` 和 `Handle::id` 永远不变（不用重新哈希，id 可以写进生成的代码）；读取多一次加载。handle 是强边，存的时候不走屏障（`HandleSlot`）；带 handle 的对象由完整回收释放。 |
| 🪶 **弱引用** | `Weak<B>` 像 `ScopeGlobal<'heap, 'heap, B>` 一样不带生命周期、放在哪都行，但不保活对象：每次回收（完整、年轻、并发标记收尾、一起回收）在标记之后、清扫之前清空目标没被标到的，搬走的跟着改地址；`Scope::upgrade` 取回 `Option<ScopeLocal>`，`Weak::is_cleared` 不进 scope 也能查。 |
| 🔣 **字槽** | `WordSlot<'heap, 'scope>` 放使用者自己编码的 `u64`（NaN 装箱之类）：不小于 `WORD_POINTER_MIN`（`0xFFF8 << 48`）的字是指针，低 48 位是对象数据地址、高 16 位是使用者自己的标签，疏散时原样保留；别的字是普通数据。屏障、计数、搬迁与 `Slot` 相同；`word_pointer` 解出字里的指针。 |
| 🔗 **堆之间直接引用** | `Far<'heap, 'scope, B>` 让一个堆的对象直接指着同一进程里另一个堆的对象：不复制、不经过间接表。每个堆仍然各自回收；`Heap::collect_group` 把几个堆一起追踪，收掉跨堆的环。 |
| 🗄️ **公开堆**（Linux） | `CrossHeap` 放在共享内存文件系统（`/dev/shm`、自己挂的 tmpfs / hugetlbfs）上的文件里：超级块、类型表、入口表、块状态表、块全在文件里、位置固定，指针存偏移，所以多个进程（有权限打开这个文件的）可以把同一个堆映射在各自不同的地址上、同时用。文件刚建时是空的，用到哪里长到哪里。对象头存类型号而不是代码，回收照类型的指针图追踪。进程在任何时刻被杀（scope 中、回收中）既不卡住别人，也不漏掉它持有的东西。公开堆之间可以直接互指（`CrossHeapRef`），`CrossHeap::collect_group` 收跨堆的环；同一目录里的堆号跨进程唯一，堆可以比建它的进程活得久。私有堆的对象里可以放指向它的 `CrossHeapGlobal<B>` Handle。 |
| 📦 **机会式疏散** | 暂停式回收在标记的同时把稀疏块里的活对象搬走（Immix defrag）：一遍里完成标记、复制、改写全部边。存活不到半块的都是候选，最稀疏的先搬、不设块数上限；块占的比「存活 + 一轮允许新分配的」多出 1.5 倍时 `collect_if_needed` 提前做完整回收。并发标记从不搬对象，所以 mutator 不需要读屏障。 |
| 🐣 **年轻回收** | `Heap::collect_young` 只追上次回收后分配的对象，从根与写屏障记下的槽出发，只扫上次回收后分配过的块；老对象一个不碰。存活的年轻对象搬进紧凑的块（`YoungEvacuation`，缺省 `SkipRemembered`：老对象直接指着的、别的堆经 `Far` 指着的原地晋升，其余的搬走，老对象一个字节都不改；`Full` 连记忆集里的槽也改写；`Off` 不搬）。`collect_if_needed` 平时做年轻回收、每几次穿插一次完整回收（LXR 式年轻对象回收的第一期）。 |
| 🔢 **引用计数** | 老对象带一个小计数，由 `Slot` 写屏障维护（每个槽每轮登记一次），在年轻回收时结算。停顿里只加增量、把减量放进队列；`sweep_concurrently` 在线程池上与 mutator 并发地连锁释放计数归零的对象，它们的行在下一次停顿还给分配器。追踪（完整回收）少跑，只为环与计数溢出的对象（LXR 第二、三期）。 |
| 🛡️ **编译期检查** | `#[derive(Trace)]` 拒绝被追踪的类型实现 `Drop`；对象数据对齐超过 8 字节直接编译报错。 |

## 🚀 用法

```rust
use paragc::{ScopeLocal, Heap, ScopeGlobal, Trace, brand};

#[derive(Trace)]
struct Node<'heap, 'scope> {
    value: u64,
    next: Option<ScopeLocal<'heap, 'scope, Node<'heap, 'scope>>>,
}

// 根Handle不带 'scope，读出来时由Brand套回去
brand!(NodeB = for<'heap, 'scope> Node<'heap, 'scope>);

let mut heap = Heap::default();
let list: ScopeGlobal<'heap, 'heap, NodeB> = heap.scope(|m| {
    let mut head = None;
    for value in 0..1000 {
        head = Some(m.alloc(Node { value, next: head }));
    }
    m.global(head.unwrap())
});
heap.collect();
```

## 🧠 怎么做的

| 部分 | 做法 |
| --- | --- |
| 块 | 按 32 KiB 对齐，对象地址抹低位就是块起点；块头是行标记（每行一个 `u32` 纪元）和对象起点位图（每 8 字节一位）。分配缓冲把落在同一个位图字里的对象起点位攒着，换字、领新块或 `scope` 返回时一次原子 OR 写进去（之前没人读：两次回收之间块只归这个缓冲，后台释放只清老对象的位） |
| 分配 | 分配缓冲拿着一个块，在「行标记 ≠ 当前纪元」的连续空行里碰撞；放不下找下一个空洞，块用完了依次领「有空行的块」→「全空的块」→ 新块。比一行大、当前空洞放不下的对象走溢出游标（Immix 的溢出分配）：它先在可用块里找有够大连续空行的（最多看 8 个），没有就开新块，当前空洞留给小对象。超过半块（16 KiB）的对象单独分配，64 KiB 起单独 mmap |
| 块的来源 | 每个堆自己 mmap 2 MiB 一段，切成按 32 KiB 对齐的块；还回来的块 `madvise(MADV_DONTNEED)` 后地址进无锁池复用，堆析构时解除映射（不经 glibc：对齐分配有额外开销，释放的内存它也不还给系统）。其他平台与 Miri 下走全局分配器 |
| 对象头 | 24 字节：追踪函数、析构函数、标记纪元（原子）、粒度数（最高位 = 大对象） |
| 标记 | 根分给各工人；`swap(纪元)` 抢到的线程负责这个对象：写它跨的行标记、压进自己的队列（没有边的类型不压） |
| 结束判定 | 自己的队列空、也偷不到 ⇒ 空闲计数 +1，边等边偷；全部工人空闲 ⇒ 结束 |
| 线程池 | 第一次回收时起、活到堆析构；分派 = 一个原子字（代号 + 参与人数）加一代、只叫醒参与的工人；空闲工人先退避再挂起；工人里的 panic 在回收线程上重新抛出 |
| 清扫 | 工人按批领块：按起点位图逐个看对象，没标上本轮纪元的析构、清位；块分成 全空 / 有空行 / 满，比上一轮领用的块数多出来的空块还给系统。`Heap::shrink`（空闲时调）做一次完整回收、空块一个不留，并把有空行的块里凑满整页的空行 madvise 掉 |
| 年轻疏散 | 上次回收后分配过的块都标成疏散；`SkipRemembered` 先把记忆集里的槽的目标与入口目标原地标上（本轮已标的不搬），其余追到的年轻对象复制进可用块与新块、留下转发地址，根表与被扫描的年轻对象的边改成新地址（`Trace::relocate`）；`Full` 由 0 号工人把记忆集里的槽也改成新地址。引用计数在标记之后结算（用搬后的地址）。年轻回收不扫描老对象：按地址建索引的结构只要指着年轻对象，它自己也得是年轻的（改动时挪进新分配的对象） |
| 根 | `ScopeGlobal<'heap, 'heap, B>` 占根表一个槽；根表只追加，空槽号在回收时整理成只读数组、scope 期间原子游标领取 |
| 弱引用 | `Weak<B>` 占弱表（同样的槽表，不当根）一个槽。每次停顿标记（含疏散）做完、清扫之前过一遍弱表：目标搬走了改成新地址，没标到本轮纪元的清空（Handle 同时析构时用 CAS，不改回去），所以 scope 里弱表只指着活对象。活着的目标引用计数粘住：按计数释放在后台、与 mutator 并发，弱引用来不及先清空，只能交给追踪；粘住在算出减量的同一次停顿里、早于后台处理，所以老对象只剩弱引用后要等下一次完整回收才清。并发标记期间 `upgrade` 走读屏障（和写屏障同一个 `remember`：标上、进灰队列），否则快照时只剩弱引用、标记中被取出挂到黑对象底下的对象会漏标 |
| 堆之间的引用 | 建 `Far`（同时在两个堆的 `scope` 里）在目标堆的根表领一个槽，槽里还记着持有方堆；持有 `Far` 的对象被析构（清扫或引用计数释放）时还槽。目标堆单独回收时这些入口就是根，年轻回收、引用计数、并发标记都不用改。入口对象不搬：疏散回收开始标记之前，入口目标所在的块撤掉疏散（改当搬入目标），年轻疏散则先把入口目标原地标上，所以 `Far` 永远不用改写。`collect_group` 不把组内持有的入口当根：各堆从自己的根出发标记，追踪到 `Far` 就把目标交给目标堆（按那个堆的纪元标灰），轮流标到没有哪个堆还有灰对象；组内持有、目标没被标到的入口属于已死的持有方，清空，免得之后的回收拿它去碰已释放的对象 |
| 公开堆布局 | `CrossHeapSpace::reserve` 保留一段地址（只占地址、不占内存），按自身大小对齐，切成 16 GiB 的窗口；公开堆 *k* 永远映射在第 *k* 个窗口。窗口里位置定死：超级块、类型表、指针图、入口表、块状态表、32 KiB 的块。文件建出来是空的（只写超级块那一页），每一段第一次用到时用 `fallocate` 延长——它只会把文件变长，多个线程同时调也安全。tmpfs 上 `fallocate` 当场占内存，所以文件长到哪里内存就占到哪里；文件长度以外的窗口不占 |
| 公开堆对象 | 16 字节头：和私有堆一样的标记字与信息字（所以块、行、清扫是同一份代码）加类型号。`#[derive(Shared)]` 要求 `#[repr(C)]`，生成指针图；一个类型第一次分配时把类型键（类型名、大小、指针图的哈希）与指针图登记进堆。`CrossHeapSlot` 存目标相对地址区起点的偏移，同一个值在每个映射里都对；`get` / `set` 带 `CrossHeapScope`（它知道本映射的起点）。`alloc` 在编译期拒绝有析构的类型。超过半块（16 KiB）的对象占一串新块，死了以后这些块变成普通块 |
| 公开堆回收 | `collect` 从入口表出发并行标记（读对象的指针图，从不调用它的代码），再并行清扫；不搬对象。空闲块、还回来的入口是文件里的无锁栈（栈头带改动次数防 ABA）。`CrossHeapGlobal::index` 是入口号，每个映射里都一样；`CrossHeapScope::find` 核对对象的类型键后凭它建一个新 Handle。清扫出来的空块按上次以来的分配量留一些备用，其余打洞还给系统（`fallocate` 打洞；匿名堆 `MADV_REMOVE`），连着的空块一次打；块元数据全零就是空块，所以它们照样留在可用栈里。`CrossHeap::shrink` 一个不留 |
| 跨堆弱引用与签名 | 对象头里有 32 位出生戳（堆身份与分配时的纪元打散）。`CrossHeapWeak` 是一个字：高位是对象偏移，低位是出生戳；不登记、不保活，谁回收都不看它，跨进程也不拖住任何一方。读时在目标堆的 scope 里核对：块开过、表项有内存、偏移处起点位已置（大对象则在首块开头）、戳对得上，否则是 `None`——位置被新对象占了、堆号换了主都认得出（撞戳概率约 2⁻²⁷）。`Signature`（32 字节：这个字 + 堆身份 + 类型键）发给别的进程，对方打开同一个堆（`CrossHeapSpace::open_fd` 凭交来的描述符，不需要路径）后 `resolve` 按类型取、`verify` 不按类型只核对；签名来自不可信的一方也只读、不崩 |
| 晋升进跨堆（老年代） | `Heap::attach_cross_heap` 把本进程建的跨堆挂到私有堆上当老年代；跨堆要建在固定地址的地址区上（`CrossHeapSpace::reserve_fixed`，每个进程都在 64 TiB 处），对象里存的绝对地址在每个进程里都对，所以晋升的对象原样沿用 `ScopeLocal` / `Slot` / `WordSlot` 字段（指针图里记成存绝对地址的边）。`Scope::alloc_shared`（类型同时是 `Trace` 与 `Shared`、没有析构）分配的对象活过一次年轻回收就被复制进跨堆：回收工人问对象的访问函数它在跨堆里的类型号，认领后复制、写跨堆的头，旧头转发到新位置；私有堆里指着它的槽、根、弱引用都改成新地址。私有堆的标记、计数、记忆集、写屏障碰到跨堆地址一律跳过。跨堆从此只由主人回收（别的进程、`collect_group` 都不碰它）：`Heap::collect` 先做一次年轻回收把可共享的晋升过去，再做私有完整标记，私有堆追踪到的跨堆边与根表里指着跨堆的当跨堆的根，一起回收；私有弱引用指着死掉的跨堆对象的清空。可共享的对象只能指着可共享的：晋升时碰到别的会 panic；私有对象写进跨堆对象的槽时先是私有地址（别的进程读到的是空），下一次年轻回收经记忆集把它晋升过去、改写那个槽（`Heap::promotion_pending` 告诉宿主一轮结束时该做一次年轻回收）。`CrossHeapScope::sign_promoted` 不要 Brand，直接凭私有 `ScopeLocal` 给晋升过去的对象签名。`Scope::alloc_tail` / `alloc_shared_tail` 在对象后面接一段纯字节（字符串、缓冲区），对象搬移或晋升时尾巴一起拷过去。挂着跨堆时不支持并发标记 |
| 信箱（主人代办） | `CrossHeapScope::post` 领一个不归任何进程的入口指着对象，压进超级块里的信箱栈，信件数（futex 字）加一并叫醒主人；`take_mail` 一次取走、先投的在前，`find` 取对象，处理完 `release` |
| 多进程 | 每个映射在超级块那一页里的挂接表占一个槽（32 个）：进程号 + 从 `/proc/<pid>/stat` 读的启动时间，进程号被复用也不会认错，僵尸算死。每个槽有自己的「scope 里线程数」；回收请求是一个字，里面是回收方的槽号。mutator 先加自己的计数再看请求，回收方先挂请求再看各槽计数（都是 SeqCst），两边至少有一方看到对方。等待用共享字上的 futex（不带进程私有标志），每 10 ms 醒来查对方进程还在不在。回收方开始下一次之前，先让上一次挡在门口的 mutator 进门。入口记着是哪个槽持有的 |
| 崩溃恢复 | 死在 scope 里的进程不等；下一个回收方把它的槽持有的入口全部还掉、空出槽（正挂着回收请求的那个槽除外，否则被新进程领走后看起来像回收方还活着）。回收到一半被杀的，由看到请求挂在死进程名下的人接手——门口的 mutator 或别的回收方——从头重做，纪元比死掉那次开始时用的更新（它标过的不能算这次标过）；清扫重做无害，因为它只在标记做完后才开始。进程死在任何一步，堆都读得通：对象写完才登记起点位（大对象写完才登记块种类）；大对象先拆尾块再拆头，拆到一半重做时跳过拆过的；回收方读块状态表、入口表之前先把它们延长到游标处，防进程死在「游标加了、文件还没延长」之间 |
| 公开堆之间的引用 | `CrossHeapRef<'a, B>` 放在公开堆 A（它的 Brand）的对象里，存目标的偏移。写入时在 B 登记一个入口、持有方记作「堆 A」，所以 B 单独回收时目标是根，回收 B 的进程不需要能打开 A；A 单独回收不跟进去。`CrossHeap::collect_group` 把本进程映射着的几个堆一起回收：组内堆持有的入口不当根，指向组里别的堆的边按那个堆的纪元标并报上来，标完后组内登记换成每对（来源堆, 目标）一项——先领新的再还旧的，回收方死在中间只会多不会少。改写 `CrossHeapRef` 留下的旧登记只在这里还。一起回收按堆号顺序拿每个堆的请求，全部走空才动手 |
| 等待不死锁 | 回收方与 mutator 像按顺序加锁那样拿堆：一起回收按堆号从小到大逐个挡住、逐个等走空，`CrossHeap::mutate_with` 进两个堆时先进堆号小的；按这个顺序嵌套的 mutator 不会和回收方互等。逆序嵌套照样能跑：回收方 50 ms 内等不到走空就放开手里的，随机退让一会儿（每次上限翻倍、封顶 200 ms）再来 |
| 堆的寿命、堆号与重启 | 公开堆归建它的进程：那个进程退出或被杀，堆就跟着死，除非调过 `CrossHeap::persist`。超级块记着主人（进程号与启动时间）、随机的堆身份、persist、dead；先发现的进程标死、删掉堆文件与标记，各映射放手后内存还回去。别的进程用 `is_alive` / `try_scope`（`HeapDead`）知道。`CrossHeapRef` 还存目标堆的身份，死堆的号被重用后读出来是 `None`、回收也不跟；入口记着持有方堆的身份，持有方是同目录里已死的堆时，它的登记在下一次回收还掉。堆号在目录里用 `.paragc-<k>` 标记领，所有判断都在标记的 `flock` 里做（锁到后再核对路径），建到一半的堆不会被当成过期的抢走。`CrossHeapGlobal::persist` 把根变成不归任何进程，凭入口号找回来，用 `CrossHeapScope::release` 还 |
| 缓冲过期 | 缓冲记着（堆, 回收次数），回收过就重来（它的块已被重新分类） |
| 并发标记 | 开头：先把上一轮扫完（它按「纪元 ≠ 那一轮」判死，必须在新一轮把活对象标成更新纪元之前结束）；握手字（最高位请求，低位是 scope 里的线程数）挂上请求、等 scope 里没人、把根标灰、撤请求——paragc 不扫栈，快照只能在没有 mutator 拿着未挂根的 `ScopeLocal` 时照；然后在线程池上标记。标记期间 `Slot` 写入把被覆盖的旧值标上并放进堆的灰队列（初始快照），新对象直接拿本轮纪元、连同行一起标上（黑分配）。空行 = 行标记早于上次完成的纪元，标记方刚写上新纪元的行不会被当成空行。`finish_marking` 并行清空灰队列 |
| 并发清扫 | `finish_marking` 把全部块与大对象交给清扫器就返回。领用是原子游标，每个块恰好被领到的那个线程扫一次、扫完才交出去：mutator 要块时先拿扫好的，没有就领一个没扫的当场扫；`sweep_concurrently` 在线程池上扫剩下的，并等别人手里领走的扫完。新对象带上次完成的纪元，清扫期间分配的不会被当成死的。`collect` = `finish_marking` + `sweep_concurrently` |
| 疏散 | 只在暂停式 `finish_marking`（没有并发标记在进行）时做。候选：清扫后没再分配过、存活不到块的 1/4 的块，至少 4 个、至多 1024 个；其余扫好的块当搬入的目标。标记时第一个追到候选块里对象的工人认领它（标记字：复制中），抄进自己的复制缓冲（新副本标成本轮，带起点位与行标记），旧头里留下新地址；别人等它或直接取新地址。每条边经 `Trace::relocate` 改写；根表槽用 CAS 改，同时在别的线程析构的Handle不会被改回来。清扫时旧副本只清起点位、不析构 |
| 年轻回收 | 粘性标记位：标着上次完成纪元的是老对象，新对象出生是 0（年轻）。年轻回收用同一个纪元标记，碰到老对象就停，追到的年轻对象晋升。老 → 年轻的边只能经 `Slot` 写入产生，所以不在并发标记时写屏障在老对象的槽写进年轻对象时记下这个槽（年轻对象自己的槽不记：年轻回收从对象本身追到；对象被回收追踪过一次，它的槽就标成老的）；并发标记期间新对象都是黑的、收尾后都是老的，不用记。只扫上次清扫后分配过的块；任何回收都清空记忆集 |
| 引用计数 | RC(x) = 老对象指向 x 的边数；年轻对象的边在它晋升时才计入。计数放在头里和大小挨着（6 位，满了就粘住，外加一位奇偶：停顿式完整回收翻转它，标记的同时把每条边精确地重新计一遍）。`Slot` 记着上次计入的值；老对象的槽本轮第一次被写时登记，年轻回收时结算（新值 +1、上次计入的值 −1），当前的根各 +1，上一轮的根延迟到这时 −1。减量在全部增量之后处理；减到 0 的老对象把它的边逐个 −1、析构、清起点位，受影响的块重算行标记。并发标记同样重新计数（见下）。只有一个标记工人时没有别人改计数，重新计数用普通读写、不用带锁的原子读改写 |
| 后台释放 | `finish_young`（停顿）结算增量、把减量放进队列；`collect_young` = `finish_young` + `sweep_concurrently`。工人取减量，把计数减到 0 的那个线程析构对象、清起点位、把它的边的减量放进队列、记下脏块。计数为 0 的对象没有计入的引用，根与晋升的年轻对象都已计入，mutator 碰不到。脏块在下一次停顿开头按剩下的老对象重算行；上次清扫后被拿去分配过的块保留这个状态、交给年轻清扫（里面可能有没扫的死年轻对象）。任何回收开始前先做完未处理的减量（免得重新计数翻转奇偶后再减）。计数可信时 `collect_if_needed` 每 16 次年轻回收一次完整回收，否则每 4 次 |
| 并发重新计数 | 握手时翻转奇偶，丢掉握手前的登记（记的是旧计数下的值；活着的所在对象会被重新扫描）。标记工人扫描的每条边 +1。标记期间槽本轮第一次被写同样登记；工人重新计数一个槽后再读一次，期间 mutator 改写过就替它登记（年轻对象的槽还不计数，mutator 不会登记）。黑分配的对象记进队列，收尾时补算它们的边；然后结算登记的槽（新值 +1、上次计入的值 −1，减量只留给本轮标记为活的对象——死的马上被清扫）。收尾一返回计数就可信 |

工作窃取双端队列与退避来自 [crossbeam](https://github.com/crossbeam-rs/crossbeam)（`crossbeam-deque`、`crossbeam-utils`）；
块表与根表用的只追加分段向量、只压入链是 paragc 自己的（`src/sync.rs`）。

## 📏 规则

- 堆里的类型实现 `Trace`（用 `#[derive(Trace)]`），而且必须 `Send + Sync`：对象可能在任何线程上被读、在回收线程上被追踪和析构。
  要改的边放 `Slot<'heap, 'scope, T>`（原子指针），写入要带 `Scope`（`slot.set(m, v)`），写屏障才知道是否在并发标记。
- `ScopeLocal<'heap, 'scope, T>` 是 `#[repr(transparent)]` 的 `NonNull<T>`。`ScopeLocal::from_raw` 把同一次 `scope` 里 `ScopeLocal::as_ptr` 得到的数据指针变回 `ScopeLocal`
  （比如从 NaN-boxing 的值里解出来）。存进堆对象里的裸地址回收器看不见：类型的 `Trace` 要用 `from_raw` 把它变回来报给
  `Tracer::edge`，并在 `relocate` 里交给 `Tracer::relocate`、把新地址写回去。
- `Slot` / `WordSlot` 占两个字（值与上次计入的值）。`WordSlot` 里带指针标签的字必须指着同一个堆里活着的对象。单个对象的数据至多 128 MiB。
- 暂停式回收会搬对象。`ScopeLocal` 只在一次 `scope` 里有效，所以察觉不到，但 `ScopeLocal::as_ptr` 两次 `scope` 之间可能不同。
  手写有边的 `Trace` 还要实现 `relocate`（derive 会生成）。
- 不要在同一个堆的 `scope` 里调用 `mark_concurrently`：它会一直等自己走空。
- 有 `ScopeLocal` 字段的类型不许实现 `Drop`：析构顺序不定，`Drop` 里可能碰到已经析构的对象。derive 会检查，`#[trace(unsafe_drop)]` 关掉检查。
- 只有从 `ScopeGlobal`（或别的堆里的 `Far`）可达的对象能活过回收。
- `Weak` 不保活：只有弱引用指着的年轻对象下一次年轻回收就清空，老对象下一次完整回收清空。读要带它所在堆的 `Scope`（`m.upgrade(&weak)`）。
- `Far` 用 `Far::new(holder, target, gc)` 建（同时在两个堆的 `scope` 里），只能放进持有方堆的对象里（Brand 限制），
  读要带目标堆的 `Scope`（`far.get(mb)`）。建好后不变；要改指向，把它放在 `Slot` 后面的对象里、换掉那个对象。同一个堆里的边用 `Slot`。
- 跨堆的环只有 `Heap::collect_group` 能收。
- 公开堆（跨堆、跨进程共享）在 `crossheap` feature 后面，只在 Linux 上有；别的平台打开这个 feature 也不编进来，只有私有堆。类型用 `#[derive(Shared)]`：`#[repr(C)]`，只有纯数据、`CrossHeapSlot`、嵌套的 `Shared` 类型与定长数组，没有析构。
  公开堆还没有并发标记、年轻回收、引用计数、疏散：`collect` 是暂停式的，要等每个活着的进程的 mutator 走空。
- 能写这个堆的进程都能把它写坏：共用一个堆的进程互相信任。死掉的进程可能留下少量用不了的块或入口（死在「领了、还没登记」之间），
  但它留下的任何东西都不会被当成对象去读。
- `CrossHeapRef` 只能放进它Brand对应的那个堆的对象里，读要带目标堆的 `CrossHeapScope`。每次写入都在目标堆登记一个入口，
  不再用的登记只有 `collect_group` 会还，所以频繁改写 `CrossHeapRef` 的程序要跑一起回收。
- 公开堆跟着建它的进程死（退出或被杀），除非调过 `persist`：别的进程看到 `is_alive() == false`、`try_scope` 返回 `HeapDead`；
  手里的Handle在放手之前照样能安全使用、析构。
- 同时进两个公开堆用 `CrossHeap::mutate_with`（或按堆号从小到大嵌套）；逆序嵌套也对，只是回收方要退让重来。
- 回收进行中释放的根，它指的对象可能要到下一次回收才死。
- 只支持 `std`。

## ✅ 验证

| 测试 | 覆盖什么 |
| --- | --- |
| `gc_from_raw_round_trip` | `ScopeLocal` 与 `Option<ScopeLocal>` 都是一个指针大小；`as_ptr` 拆开再 `from_raw` 取回的还是同一个对象 |
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
| `young_evacuation_moves_survivors` | 老持有者的槽与根各挂一条年轻链（中间夹垃圾）：`Full` 全搬、槽与根指到新地址；`SkipRemembered` 槽直接指着的头不动、其余搬；`Off` 不动；值都不变，之后引用计数照样收干净 |
| `medium_objects_overflow_without_wasting_holes` | 块里只剩单行空洞时交替分配小对象与中对象：小对象填进旧空洞，新开的块只够放中对象 |
| `shrink_keeps_objects_and_reuses_lines` | `shrink` 把空行还给系统后存活对象完好，接着往那些行里分配也对 |
| `crossing_calls_the_hook_once` · `arming_again_inside_a_scope` · `a_large_object_crosses_too` · `no_limit_no_hook` · `crossing_after_a_young_collection` · `crossing_while_sweeping_lazily` · `used_drops_when_blocks_go_back` | 分配上限：每次越过只调一次钩子，在 scope 里重新设上限，大对象也计入，没设上限时不调；年轻代回收之后、完整回收惰性清扫期间，老对象照样计入；块还回去以后计数下降 |
| `due_calls_the_hook_once` · `every_collection_arms_it_again` · `collecting_inside_the_scope_arms_it_again` · `collect_if_needed_follows_the_threshold` · `the_growth_policy_without_a_threshold` · `a_large_object_counts` · `a_request_calls_the_hook_once_per_round` · `no_hook_no_call` | 回收阈值：每轮只调一次钩子，完整回收、年轻代回收和 scope 里的回收都会重新武装；`collect_if_needed` 在同一个阈值回收，不到阈值不回收；没设阈值时按增长策略；大对象也计入；手动请求每轮只调一次钩子；没设钩子时不调 |
| `serial_drop_stays_on_the_caller` | 关掉并行析构：引用计数后台释放、年轻 / 完整清扫、堆析构的死对象都在调用方线程上析构，一个不少 |
| `promoted_slots_remember_young_values` | 第一次完整回收前被年轻回收晋升的持有者，之后写进去的年轻叶子在下一次年轻回收后还活着 |
| `young_collections_while_relinking` | mutator 在老、年轻对象之间挪节点，轮流年轻回收、并发标记后年轻回收收尾、完整回收；每轮计数恰好 |
| `background_release_while_mutating` | 整条老链一次扔掉：`finish_young` 返回时一个都还没析构；之后连锁释放在线程池上跑、mutator 同时照常挪，不析构还挂着的；下一次停顿空出的行被复用 |
| `concurrent_marking_recounts` | 一轮并发标记（mutator 挪动、黑对象把节点挂到自己底下）收尾后计数立刻可信：扔掉的老链在下一次年轻回收按计数释放，之后几轮不误收挂着的节点 |
| `cross_heap_list_survives_and_garbage_is_reclaimed` · `pointer_map_covers_nested_fields_and_arrays` · `large_public_objects` | 公开堆：回收后存活字节数精确、空出的行被复用、指针图覆盖嵌套字段与数组、超过 8 KiB 的对象内容完整且块被小对象复用 |
| `processes_share_one_heap` | 4 个子进程和父进程在同一个文件上各自压入弹出自己的链、回收；每条链的形状和长度与那个进程报的一致，存活字节数精确，退出后槽空出 |
| `killed_mutator_does_not_block_collection` | 子进程停在 scope 里、手里是一条 1 万节点链的唯一 Handle：回收在等它；`kill -9` 后回收做完、槽空出、那条链被回收 |
| `killed_collector_is_taken_over` · `random_kills_while_processes_churn` | 不停回收的子进程在随机时刻被杀（至少一次杀在回收中途、被接手），父进程一直核对一条 30 万节点的链；4 个改链的子进程被杀掉换新 30 次，父进程同时改链、回收；最后每条链形状都对、存活字节数精确 |
| `cross_heap_ref_keeps_target_until_group_collection` · `group_collection_returns_stale_registrations` | 只靠 `CrossHeapRef` 可达的目标挺过所在堆自己的回收（持有方死了也一样），直到一起回收；改写 2000 次留下 2000 项登记，一次一起回收全还掉、只留现在指着的 |
| `cycles_across_public_heaps_need_group_collection` | 跨 3 个堆的环挺过各堆单独回收、两个堆一起回收；三个一起恰好收掉没挂根的环；只在第一个堆挂根的环靠重建的登记挺过之后各堆的单独回收 |
| `nested_mutates_with_collectors_do_not_deadlock` | 一个线程先 A 后 B、另一个先 B 后 A 嵌套 scope，同时两个单独回收、一个一起回收在转 |
| `restart_after_the_creator_is_gone` · `heap_numbers_do_not_collide_across_processes` | 没有 `persist` 时堆跟着建堆进程死，正常退出或被杀都一样（打开失败、文件与标记删掉）；`persist` 之后被杀或退出，别的进程打开、凭入口号找到链、收拾建堆进程、还掉不归任何进程的根；不同进程在同一目录建的堆号不撞，过期的号收回重用 |
| `references_to_a_dead_heap_do_not_crash` | 主人放手堆 x：文件与标记删掉；另一个映射看到它死了、`try_scope` 拒绝，手里的Handle照样能用；x 的号被 z 重用，指向旧 x 的 `CrossHeapRef` 读出来是 `None`、一起回收不跟；x 在 y 里持有的登记在 y 下一次回收还掉 |
| `eight_processes_half_die` | 8 个进程各一个公开堆（50 万节点、指向其余每个堆的 `CrossHeapRef` 集线器）和一个私有堆（50 万节点、拿着其余每个公开堆的 Handle），后台一直在干活；杀掉 4 个；活着的每个都看到死堆已死、放掉 Handle、读得到每个活着的别人、自己的堆恰好剩「链 + 集线器 + 活着的人手里的礼物」、能和它们一起回收；死堆的文件与标记都没了。2 GB、不许交换的 cgroup 里：8 个都起来峰值 353 MB，杀掉后 182 MB |
| `simultaneous_creates_get_distinct_numbers` | 8 个进程同一时刻在一个目录里建堆，5 轮：堆号全不撞 |
| `collect_fresh_file_heap_without_roots` | 新的文件堆一个入口都没有就回收（没有可延长的） |
| `file_starts_empty_and_grows` · `two_mappings_of_one_heap` | 文件建好时只有一页、按用量变长；同一个堆映射在两个基址：凭入口号找到 Handle（类型不对时找不到），两边各 4 个线程改链、回收在两个映射间轮流，建堆的一方先放手 |
| `weak_reference_across_heaps_does_not_keep_target` | 另一个堆里的 `CrossHeapWeak` 不保活目标：目标没根后回收、存活 0，读出来 `None`，同一位置分配新对象后仍是 `None` |
| `signature_round_trip_and_forgeries` | 签名转字节再转回不变、按类型取得到、类型不对取不到；改戳、改堆身份、改类型键都不过；把偏移挪到前后 32 KiB 里每个粒度上，只有对象起点能过、不崩；对象死后失效 |
| `mailbox_delivers_in_order` | 投 3 封、回收后信还在，按投递顺序取出，还掉后堆里存活 0 |
| `promotion_into_attached_cross_heap` | 5000 个可共享节点一次年轻回收全部进跨堆、值不变；不可共享的对象留在私有堆、它的槽改成新地址；弱引用跟着搬；签名找得到晋升过去的对象；跨堆单独回收什么都不做；主人完整回收后存活字节数精确，链头死后私有弱引用清空、签名失效，最后全部回收；接着往跨堆对象的槽里写私有年轻对象：下一次年轻回收前它还是私有地址（别的进程看不到），回收后晋升过去、槽改成新地址。一个进程同一时刻只能保留一个固定地址区，所以两个场景放在一条测试里先后跑 |
| `shrink_returns_empty_blocks_and_open_by_fd` | 40 万个节点的垃圾回收后 `shrink`，堆文件实际占用降到四分之一以下；凭描述符打开的另一个映射认得签名；还掉的块再分配照样对 |
| `private_heap_holds_public_handles` · `eight_threads_churn_public_heap` | 私有堆对象里的 `CrossHeapGlobal` 保活目标，直到私有堆析构它；8 个线程压入弹出各自的链、另一个线程同时回收，每轮存活字节数精确 |
| `reference_counting_leaves_cycles_to_tracing` | 整体扔掉的老对象链在年轻回收时按计数释放（500 层连锁）；两节点的环留到完整回收 |
| `far_targets_are_pinned_during_young_evacuation` | 年轻疏散（`Full`）也不搬别的堆经 `Far` 直接指着的年轻对象，只有根的照常搬走 |
| `far_targets_are_pinned_during_evacuation` | 稀疏块，留下的对象一半还被别的堆直接指着：回收疏散了另一半，被指着的原地不动（根的路径与 `Far` 看到同一个对象），析构数精确 |
| `weak_does_not_keep_alive` · `weak_inside_heap_objects` | 只剩弱引用的对象被回收、弱引用清空，挂着的取得到原值；放在堆对象里的弱引用随持有者析构 |
| `weak_follows_evacuation` · `young_collection_clears_young_targets` | 疏散后弱引用与强引用看到同一个新副本、旧副本不析构；年轻回收三种搬法下没有强引用的年轻对象清空、活着的取得到原值 |
| `weak_targets_wait_for_tracing` | 老对象最后一个强引用没了：没有弱引用的按计数释放，有弱引用的留到完整回收 |
| `upgrade_during_concurrent_marking_keeps_target` · `weak_churn_while_marking_concurrently` | 快照时只剩弱引用、标记期间被取出挂进别的对象的对象活过收尾（读屏障）；4 个线程建弱引用、取出挂上，另一个线程并发标记，每轮析构数恰好 |
| `survives_full_collections_and_cut_frees_the_rest` · `young_collection_*` · `concurrent_marking_while_rewriting` | `WordSlot`：完整、年轻（三种搬法）、并发回收后标签与数原样，目标活着；指针字换成数后它指着的按计数、按追踪都能收掉 |
| `group_collection_clears_weak` | 只经别的堆的 `Far` 可达的对象：单独回收不清，持有者死后一起回收才清 |
| `far_keeps_target_until_holder_dies` · `far_survives_holder_heap_drop_order` | 自己没有根的目标在持有方活着时挺过所在堆的完整、年轻回收；持有方堆析构了持有方之后才收；目标堆先析构也安全 |
| `cross_heap_cycle_needs_group_collection` · `rings_over_eight_heaps` | 跨 2 个、8 个堆的环挺过各堆单独回收；缺几个堆的组收不掉（组外的持有方是根）；整组一起回收恰好收掉没挂根的环，留下的环走得通 |
| `eight_threads_relink_far_holders` | 8 线程 × 8 堆不停换掉指向别的堆的持有对象、读回来核对，另一个线程同时对每个堆并发标记；完整、年轻、一起回收轮流来；最后每个目标都被回收，没有提前的 |

全部测试也在 Miri 下跑（Stacked Borrows 和 Tree Borrows，`cfg(miri)` 下自动缩规模）。Miri 跑不了文件映射，所以 Miri 下公开地址区
是一块普通分配（4 个 16 MiB 的窗口）：不用文件的公开堆测试在它上面跑，两条用文件的跳过。要加 `-Zmiri-ignore-leaks`：
crossbeam-epoch 的全局回收器退出时还拿着延迟释放的垃圾；每堆 4 个回收线程时 Stacked Borrows 还会在 crossbeam-epoch 内部报违例，
所以 Miri 下测试用 2 个；`gc` 的两条并发测试和 `far_targets_are_pinned_during_evacuation`、`eight_threads_relink_far_holders`
在 Stacked Borrows 下仍会碰到 crossbeam-epoch 内部那处违例（crossbeam-deque 压栈时），Tree Borrows 下干净：

```sh
cargo test --test gc -- --exact <名字>          # 另有 --test far · --test weak · --test word；加 --features crossheap 另有 --test cross_heap · --test processes
MIRIFLAGS="-Zmiri-permissive-provenance -Zmiri-ignore-leaks" cargo +nightly miri test --test gc -- --exact <名字>
MIRIFLAGS="-Zmiri-permissive-provenance -Zmiri-ignore-leaks -Zmiri-tree-borrows" cargo +nightly miri test --test gc -- --exact <名字>
```

## 📊 性能对比

16 核机器上与 cppgc（V8 的 Oilpan）、gc-arena、gc（rust-gc）、dumpster 对比；墙钟 ms，5 次取最快。环境、工作负载、全部配置与怎么读：
**[benches/benchmark.zh-CN.md](benches/benchmark.zh-CN.md)**。

| | paragc | cppgc | gc-arena | dumpster | gc |
| --- | ---: | ---: | ---: | ---: | ---: |
| 停顿，100 万活 + 100 万死，1 线程 | 20.2 | 44.2 | **16.3** | **16.3** | 90.2 |
| 停顿，16 线程 | **4.2** | 44.9 | – | – | – |
| 停顿，并发标记 + 并发清扫，1 线程 | **0.02** | – | – | – | – |
| GCBench，1 线程 | **50.3** | 121.0 | 118.0 | 97.8 | 294.6 |
| GCBench，4 线程 | **54.3** | 94.2 | – | – | – |
| 多 mutator，16 线程 | **9.0** | – | – | 343.5 | – |
| 碎片（存活约 80 MB），回收后 / 峰值 RSS MB | **111 / 416** | 445 / 1894 | – | – | – |

串行停顿 gc-arena、dumpster 更短（paragc 的完整回收还要重建引用计数）；多线程以及吞吐 paragc 领先。cppgc 的强制完整回收即使并行选项全开也只用一个线程标记，而且这里每次分配、每条边都要过一次 FFI。

## 🗺️ 路线

已完成：并行暂停式回收、常驻线程池、并发标记、并发 + 惰性清扫、机会式疏散、年轻回收、老对象引用计数（减量并发处理），
完整追踪每 16 次年轻回收才一次、并发标记顺带重新计数、单线程重新计数更便宜、同一进程里堆之间的直接引用（一起回收收跨堆的环）。
放在共享内存上、多个进程同时用、任何一个被杀都不影响别人、彼此可以直接互指（一起回收收跨堆的环）的公开堆（[设计稿](docs/cross-heap.zh-CN.md)）。其他可能的：公开堆的并发标记、年轻回收、引用计数；并发的年轻回收、更小的 `Slot`
（为了记住上次计入的值，现在占两个字）。

## 📜 许可

MIT © Jabberwocky238

<div align="right"><a href="#top">⬆ 回到顶部</a></div>
