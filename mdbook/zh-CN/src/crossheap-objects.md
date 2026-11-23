# 对象、类型与指针图

跨堆里的对象可能被一个从没链接过其类型代码的进程读取和回收，所以对象里不存函数指针。

## 可共享的类型：`#[derive(Shared)]`

```rust
use paragc::{CrossHeapSlot, Shared, cross_heap_brand};

#[repr(C)]
#[derive(Shared)]
struct Node<'p> {
    value: u64,
    next: CrossHeapSlot<'p, Node<'p>>,
}

cross_heap_brand!(NodeB = for<'p> Node<'p>);
```

- 必须 `#[repr(C)]`，这样各个进程、各个二进制看到的布局一致。
- 字段可以是普通数据、`CrossHeapSlot`、`CrossHeapRef`、`CrossHeapWeak`、嵌套的 `Shared` 类型和定长数组。晋升又加了存绝对
  地址的边：`ScopeLocal`、`Option<ScopeLocal>`、`Slot`、`WordSlot`。
- 不能有析构：`CrossHeapScope::alloc` 在编译期检查。
- 派生会生成**指针图**：每个指针字段的偏移，嵌套类型和数组都展开。

## 类型表

一个类型第一次分配时，在堆的类型表里登记：

- **类型键**是类型名、大小和指针图的哈希；
- 表项记着类型号、大小和指针图存在哪。

进程查Handle时会核对类型键，类型不对的Handle会被拒绝，而不是被读错。

## 对象头

跨堆对象的头是 16 字节的 `PHeader`：

| 字段 | 含义 |
| --- | --- |
| mark、info | 和私有对象头相同的两个字，所以块、行、清扫共用代码 |
| type | 类型号 |
| stamp | 出生戳（见[弱引用与签名](crossheap-weak.md)） |

回收按类型号的指针图走，从不调用代码。

## Handle

| Handle | 有效范围 | 存的是 |
| --- | --- | --- |
| `CrossHeapLocal<'p, T>` | 一次 `CrossHeap::scope` 里 | 本映射里的绝对地址 |
| `CrossHeapSlot<'p, T>` | 跨堆对象的字段 | 相对地址区起点的偏移，所有映射里都一样 |
| `CrossHeapGlobal<B>` | 任何地方，私有堆里也行 | 入口号；让对象存活 |
| `CrossHeapRef<'a, B>` | 堆 A 里对象的字段，指向堆 B | 目标偏移和 B 的化身号；在 B 里登记入口 |
| `CrossHeapWeak<B>` | 任何地方 | 一个字：偏移和出生戳；什么都不保 |

`CrossHeapSlot` 的 `get`、`set` 要传 `CrossHeapScope`，它知道本映射的基地址。

## 入口

入口表就是跨堆的根集。每个入口记着目标偏移和**持有方**：

- 一个挂接槽，表示某个进程；
- `HEAP_HOLDER | k`，表示另一个跨堆；
- 0，表示不归任何人：持久根。

`CrossHeapGlobal::index` 是入口号，在每个映射里都一样。`CrossHeapScope::find` 核对类型键后，把入口号变回 Handle；别的进程就是
这样拿到交给它的对象的。

## 大对象

超过 16 KiB 的对象占一串新块。它死后，这些块变回普通块。

## 回收

`CrossHeap::collect`：

- 从入口表出发并行标记，读每个对象的指针图，并行清扫；
- 从不搬对象。别的进程手里有偏移和弱引用字，它们必须一直有效；
- 空闲块和空闲入口放在文件里的无锁栈上，栈头带标签防 ABA。

跨堆没有并发标记、年轻回收、引用计数和疏散：`collect` 是全停的，要等每个活进程的 mutator 都离开。
