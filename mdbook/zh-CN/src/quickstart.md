# 快速上手

```toml
[dependencies]
paragc = "0.7"
```

一个单链表，由根保住，在同一个 scope 里回收之后再求和：

```rust
use paragc::{Heap, ScopeGlobal, Slot, Trace, brand};

#[derive(Trace)]
struct Node<'heap, 'scope> {
    value: u64,
    next: Slot<'heap, 'scope, Node<'heap, 'scope>>,
}

// 根不带 'scope；读的时候由 Brand 把读取方 scope 的借用装回去
brand!(NodeB = for<'heap, 'scope> Node<'heap, 'scope>);

let threads = std::thread::available_parallelism().unwrap();
let sum = Heap::new(threads, |mut heap| {
    heap.scope(|s| {
        let list: ScopeGlobal<'_, '_, NodeB> = {
            let mut head = None;
            for value in 0..1000 {
                head = Some(s.alloc(Node { value, next: head.into() }));
            }
            s.scope_out(head.unwrap())
        };
        s.collect();
        let mut sum = 0;
        let mut cur = Some(s.scope_in(&list));
        while let Some(n) = cur {
            sum += n.value;
            cur = n.next.get();
        }
        sum
    })
});
assert_eq!(sum, 999 * 1000 / 2);
```

这里发生了什么：

1. `Heap::new(threads, |heap| ..)` 造一个堆，它有自己独有的 Brand `'heap`；堆在闭包里活着。
2. `heap.scope(|s| ..)` 打开一个独占堆的 scope。`s.alloc(value)` 把 `value` 搬进堆，返回 `ScopeLocal`：从 `s` 借出的直接指针。
3. `s.scope_out(x)` 用一个根保住 `x`：`ScopeGlobal`，对象搬动时回收会改写它。
4. `s.collect()` 可变借用 `s`，所以从这里起所有 `ScopeLocal` 都失效了（编译器检查）。它从根出发标记，其余全部释放。
5. `s.scope_in(&root)` 把根重新变成直接指针。

嵌套 scope 和背后的规则见 [Scope 与生命周期](scopes.md)。

## 可变的边

`Slot` 字段经写屏障改写，写屏障要 scope：

```rust
use paragc::{Heap, Slot, Trace};

#[derive(Trace)]
struct Cell<'heap, 'scope> {
    value: Slot<'heap, 'scope, Cell<'heap, 'scope>>,
}

Heap::new(std::num::NonZeroUsize::MIN, |mut heap| heap.scope(|s| {
    let a = s.alloc(Cell { value: None.into() });
    let b = s.alloc(Cell { value: None.into() });
    a.value.set(s, Some(b));        // 写屏障要看到 scope
    assert!(a.value.get().is_some());
}));
```

写屏障负责：

- 为年轻回收记下老指向年轻的边；
- 维护引用计数；
- 并发标记期间把被覆盖的旧值标记上。

## 选哪种回收

在独占 scope 里（`s.collect…()`），或在 scope 之间对堆调用（`heap.collect…()`）：

| 调用 | 什么时候用 |
| --- | --- |
| `collect_if_needed()` | 两段工作之间。它自己决定什么都不做、做年轻回收还是完整回收。 |
| `collect_young()` | 你知道大部分新对象已经是垃圾。 |
| `collect()` | 想让所有不可达的对象马上消失。 |
| `heap.mark_concurrently()` 再 `heap.finish_marking()` | 停顿必须很短，共享 scope 要一直跑。 |
| `shrink()` | 程序空闲，内存该还给系统。 |

## 线程

`Heap` 是 `Sync` 的。任意多个线程都可以开 `heap.scope_shared(|s| ..)`（或带线程自己 `Tlab` 的 `scope_shared_in`）；共享
scope 里不能回收，回收（`&mut Heap`）会等所有共享 scope 都结束。回收线程是堆自己的线程池：第一次回收时启动，两次回收之间挂起。
