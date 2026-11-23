# 对象地址为什么会变

从外面用 paragc，第一个意外是：同一个对象在一次回收之后地址可能不同。本章从使用者的角度解释：你会看到什么、什么时候发生、paragc 为什么这样做、哪些东西能跨回收保存、哪些不能。

## 你会看到什么

```rust
let root = heap.scope(|m| m.global(m.alloc(Node { value: 1, next: None })));
let before = heap.scope(|m| m.scope_in(&root).as_ptr() as usize);
heap.collect_young();
let after = heap.scope(|m| m.scope_in(&root).as_ptr() as usize);
assert_ne!(before, after);            // 通常成立：存活者被复制走了
assert_eq!(heap.scope(|m| m.scope_in(&root).value), 1);   // 对象还是那个对象
```

`ScopeGlobal` 仍然找得到这个对象，值也没变，变的只是地址。

## 一个对象的一生

<div class="dg"><div class="dg-cap">一个对象，四个地址</div><div class="flow"><div class="step"><h5>分配</h5><span class="addr k5">0x7f3a_1c40_1040</span><p>年轻，在线程正在递增分配的块里；mark = 0</p></div><div class="arrow">→</div><div class="step"><h5>年轻回收</h5><span class="addr k2">0x7f3a_1c48_8a10</span><p>它活下来了，被复制进紧凑的块，变老</p></div><div class="arrow">→</div><div class="step"><h5>完整回收</h5><span class="addr k3">0x7f3a_1c5c_c230</span><p>它所在的块变稀疏了，疏散把它复制出去</p></div><div class="arrow">→</div><div class="step"><h5>晋升</h5><span class="addr k1">0x4008_0004_2a18</span><p>挂着跨堆、类型可共享：复制进共享内存，从此不再搬动</p></div></div></div>

## paragc 什么时候搬对象

| 事件 | 搬什么 | 设置 |
| --- | --- | --- |
| 年轻回收 | 年轻存活者 | `SkipRemembered`（缺省）：除了老对象或别的堆的 `Far` 直接指着的，都搬 · `Full`：全搬 · `Off`：不搬 |
| 全停的完整回收 | 稀疏块（活数据不到一半）里的活对象 | 总是开启 |
| 挂着跨堆时的年轻回收 | 可共享的存活者，搬进跨堆 | 只限 `alloc_shared` 的类型 |

从不搬：不回收的 scope 里的任何对象；并发标记、一起回收期间的任何对象；大对象（超过 16 KiB）；`Far` 的目标；已经在跨堆里的对象。

## 为什么要搬

搬动要付一次复制的代价，所以 paragc 只在它换来你能感觉到的好处时才搬：

- **分配一直是推一下指针。** 把年轻回收的存活者复制到一起，就空出整块整块的内存，后面的分配直接递增推进，不用搜索。
- **内存有上限。** 不搬的话，活对象散落各处的堆会留着每一个半空的块。在碎片化基准里，同样约 80 MB 的活数据，paragc 结束时 RSS 是 111 MB，不搬普通对象的 cppgc 是 445 MB。
- **年轻垃圾零成本。** 只碰存活者，死掉的对象根本不会被访问。
- **共享仍然是惰性的。** 晋升就是一次搬动：对象证明自己活得久之后，才被复制进跨堆、让别的进程看见，分配时什么都不用决定。

## 引用怎么跟着搬

回收器在旧副本里留下转发地址，并改写它知道的每一个引用：

<div class="dg"><div class="dg-cap">搬动之后的旧副本</div><div class="bf"><div class="k7" style="--w:4"><b>mark</b><small>置 FORWARDED</small><i>bit 31</i></div><div class="k2" style="--w:4"><b>info</b></div><div class="k3" style="--w:8"><b>visit</b></div><div class="k1" style="--w:8"><b>drop</b><small>→ 新地址</small></div><div class="dim" style="--w:20"><b>T</b><small>旧数据，不会再读</small></div></div></div>

- **`ScopeLocal`** 在搬动时不可能存在：它是对所属 scope 的借用，而会搬对象的回收要取 `&mut` 堆或这个 scope。
- **对象里的边**（`ScopeLocal`、`Slot`、`WordSlot` 字段）经 `Trace::relocate` 改写，`#[derive(Trace)]` 会生成它。
- **根、弱引用、`Far` 入口**放在回收器自己的表里，在表里改写。
- **老对象被记下的槽**在 `Full` 模式下改写；在 `SkipRemembered` 模式下，它们的目标干脆不搬。

## 哪些东西能跨回收保存

| 保存 | 为什么仍然有效 |
| --- | --- |
| `ScopeGlobal<'heap, 'heap, B>`、`Weak<B>` | 回收器改写它们的表槽 |
| 堆对象的字段 | 由 `Trace::relocate` 改写 |
| `WordSlot` 字的标签位 | 只有低 48 位会变 |
| `CrossHeapSlot`、`CrossHeapGlobal::index`、`CrossHeapWeak`、`Signature` | 它们指的是跨堆对象，而跨堆对象从不搬 |
| 从对象里复制出来的值 | 它们归你了 |

## 哪些会坏，该怎么做

| 写法 | 为什么会坏 | 改成 |
| --- | --- | --- |
| 把 `as_ptr()` 存进 Rust 结构，回收之后再用 | 对象可能搬走了，旧地址上可能已经是别的对象 | 存 `ScopeGlobal`（或 `Weak`），用 `s.scope_in` 取 |
| 按地址做键的哈希表 | 搬动后键就过期了 | 按内容或稳定的编号哈希；非用地址不可，就在 `relocate` 里重新哈希（见下） |
| 比较回收前后拿到的地址 | 同一个对象可能有两个地址 | 在两次回收之间比较，或比较稳定的编号（Handle） |
| 把地址交给 C、操作系统，或活得比 scope 久的线程 | 没有人会改写它 | 把字节复制出去；钉住空间在规划中（宗旨第 1 条：照样会被回收） |
| 把地址写死在生成的代码里 | 那段代码不是回收器认识的边 | 经 handle 引用目标、把 id 写进代码（j8 的 shape），或把代码做成被追踪的对象 |

## 在 j8 里

- **对象的 `===`** 比较的是 NaN-boxing 的字。两个操作数在同一轮、同一次 `scope` 里读出，结果一致。
- **`Map` 和 `Set`** 按地址索引对象（`MapKey::Ref`）。`Collection::relocate` 在搬动后重算每个对象键并重新插入，就是上面说的「在 relocate 里重新哈希」。
- **shape** 在内联缓存里被比较、被写死在 JIT 代码里。它们是经 [handle](handles.md) 引用的 GC 对象：shape 搬家只改写它的 handle 表项，缓存和机器码比较的 id 永远不变。
- **字符串**随便搬：堆外没有谁记着它们的地址，它们的字节在尾巴里跟着走。
