# 快速上手

```toml
[dependencies]
paragc = "0.6"
```

一个单链表，由根保住，回收之后再求和：

```rust
use paragc::{Gc, Heap, Root, Trace, brand};

#[derive(Trace)]
struct Node<'gc> {
    value: u64,
    next: Option<Gc<'gc, Node<'gc>>>,
}

// 根句柄不带 'gc；读的时候由品牌把生命周期装回去
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
let sum = heap.mutate(|m| {
    let mut sum = 0;
    let mut cur = Some(m.get(&list));
    while let Some(n) = cur {
        sum += n.value;
        cur = n.next;
    }
    sum
});
assert_eq!(sum, 999 * 1000 / 2);
```

这里发生了什么：

1. `heap.mutate(|m| ...)` 进入堆。`m` 是 `Mutator<'gc>`，它给出的每个 `Gc<'gc, T>` 都由品牌 `'gc` 绑在这一次调用上。
2. `m.alloc(value)` 把 `value` 搬进堆，返回一个 `Gc`。
3. `m.root(gc)` 把 `Gc` 变成 `Root<NodeB>`。根不带生命周期，可以带出闭包、存在任何地方、发给别的线程。
4. `heap.collect()` 要 `&mut Heap`，所以此时不可能有 `mutate` 在跑。它从根出发标记，其余的全部释放。
5. `m.get(&root)` 在新的 `mutate` 里把对象重新取成 `Gc`。

## 可变的边

`Gc` 字段在对象建好后就不变了。要变的字段用 `Slot`：

```rust
use paragc::{Slot, Trace};

#[derive(Trace)]
struct Cell<'gc> {
    value: Slot<'gc, Node<'gc>>,
}

heap.mutate(|m| {
    let cell = m.alloc(Cell { value: None.into() });
    let node = m.alloc(Node { value: 1, next: None });
    cell.value.set(m, Some(node));        // 写屏障要看到 Mutator
    assert_eq!(cell.value.get().map(|n| n.value), Some(1));
});
```

`Slot::set` 要传 `Mutator`，因为写屏障需要它。屏障负责：

- 为年轻回收记下老指向年轻的边；
- 维护引用计数；
- 并发标记期间把被覆盖的旧值标记上。

## 选哪种回收

| 调用 | 什么时候用 |
| --- | --- |
| `collect_if_needed()` | 两段工作之间。它自己决定什么都不做、做年轻回收还是完整回收。 |
| `collect_young()` | 你知道大部分新对象已经是垃圾。 |
| `collect()` | 想让所有不可达的对象马上消失。 |
| `mark_concurrently()` 再 `finish_marking()` | 停顿必须很短，mutator 要一直跑。 |
| `shrink()` | 程序空闲，内存该还给系统。 |

## 线程

`Heap` 是 `Sync` 的。用 `Arc` 包起来，就能在任意多个线程上调 `mutate`。回收（`&mut self`）会等所有线程都退出 `mutate`。
回收线程是堆自己的线程池：第一次回收时启动，两次回收之间挂起。
