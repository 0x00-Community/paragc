# 晋升：把跨堆当老年代

如果每个可共享的对象都直接分配在跨堆里，就得在分配时决定对象去哪个堆。晋升避开了这一点：对象照常分配在私有堆里，熬过一次
年轻回收的，才搬进挂成老年代的跨堆。分配仍然是惰性的，短命对象根本不碰共享内存。

## 怎么挂

```rust
let space = CrossHeapSpace::reserve_fixed()?;          // 固定地址区，在 64 TiB
let cross = space.create(path, threads)?;              // 只在本进程用的话也可以 create_anonymous
let mut heap = Heap::new(threads);
heap.attach_cross_heap(&cross)?;
```

- 跨堆必须在**固定地址区**里，这样它的对象里存的绝对地址，在同样占了固定地址区的每个进程里都对。
- 跨堆变成**只归主人回收**：别的进程和 `collect_group` 都不碰它，只有这个私有堆回收它。
- 挂着跨堆时，年轻疏散强制为 `Full`，不支持并发标记。

## 可共享的私有对象

```rust
#[repr(C)]
#[derive(Trace, Shared)]
struct Node<'gc> {
    value: u64,
    next: Slot<'gc, Node<'gc>>,
}

heap.mutate(|m| m.alloc_shared(Node { value: 1, next: None.into() }));
```

`Mutator::alloc_shared`（以及 `alloc_shared_tail`）要求类型同时是 `Trace` 和 `Shared`，且没有 `Drop`。它的 `Gc`、
`Option<Gc>`、`Slot`、`WordSlot` 字段在指针图里记成存绝对地址的边（`WORD_EDGE`、`PTR_EDGE`）。

## 年轻回收做了什么

年轻回收中，工人碰到一个年轻对象时：

1. 以询问模式调用对象的 `visit` 函数，问类型是否可共享、在跨堆里的类型号是多少。类型第一次用到时在跨堆里登记；
2. 可共享的话，工人认领对象，用自己的晋升缓冲把它复制进跨堆，写 `PHeader`（类型号和出生戳），再把旧头转发到新位置；
3. 私有堆里的槽、根、弱引用改到新地址，和任何一次搬动一样。

晋升过去的对象的边也会被追踪。它的目标必须可共享；从晋升对象走到不可共享的目标会 panic（`in_shared`）。

私有堆在所有地方都跳过跨堆地址：标记、引用计数、记忆集、写屏障、根表、弱引用表。

## 把私有对象写进跨堆对象

晋升之后，跨堆对象还可以被写，例如 `promoted.next.set(m, Some(fresh))`，而 `fresh` 还是年轻的。这时写屏障：

- 把这个槽记进记忆集；
- 置上 `promotion_pending`。

在下一次年轻回收之前，这个槽里是私有地址。别的进程把地址区外的地址当作空。下一次年轻回收把 `fresh` 晋升过去，并经记忆集
改写这个槽。宿主在一段工作结束时检查 `Heap::promotion_pending()`，为真就做一次年轻回收。

## 回收

`Heap::collect`：

1. 先做一次年轻回收，把可共享的存活者都晋升过去；
2. 做一次私有堆的完整标记；
3. 带着额外的根回收跨堆：私有标记发现的跨堆边，以及根表里指向跨堆的项；
4. 清掉跨堆目标已死的私有弱引用。

对只归主人回收的堆调 `CrossHeap::collect` 什么都不做。

## 把对象交给别的进程

```rust
let sig = heap.mutate(|m| cross.mutate(|cm| cm.sign_promoted(m.get(&root))));
```

对还在私有堆里的对象，`sign_promoted` 返回 `None`。签名不保活：主人丢掉对象并回收后，签名解析出来是 `None`。

## j8 怎么用

开了 j8 的 `crossheap` feature 时，`Isolate` 接收一个跨堆（`attach_cross_heap`）：

- `scope.shares.share(mc, gc)` 记下一个要共享的对象，返回一张票；
- j8 从不在 JS 运行中途回收。在 `Isolate::enter` 结束时，如果有待办，就做一次年轻回收，然后给记下的对象签名；
- `take_signatures` 交出（票, 签名）对。

JS 字符串是可共享的：它的字节，以及非 ASCII 文本的 UTF-16 码元，都放在对象的尾巴里。
