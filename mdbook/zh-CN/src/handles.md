# 句柄与品牌

paragc 的几种句柄，区别在于**有效多久**，以及**是否让目标存活**。

| 句柄 | 有效期 | 是否保活 | 能放在哪 |
| --- | --- | --- | --- |
| `Gc<'gc, T>` | 一次 `mutate` | `mutate` 期间（这时不会开始回收） | 局部变量、堆对象的字段 |
| `Slot<'gc, T>` | 堆对象的字段 | 是，作为一条边 | 堆对象里 |
| `WordSlot<'gc>` | 堆对象的字段 | 字是指针时是 | 堆对象里 |
| `Root<B>` | 无 | 是 | 任何地方、任何线程 |
| `Weak<B>` | 无 | 否 | 任何地方、任何线程 |
| `Far<'a, B>` | 持有方堆的品牌 | 是，登记在目标堆里 | 持有方堆的对象里 |

## 为什么要品牌

`Gc` 就是一个裸指针（`NonNull<T>` 上的 `#[repr(transparent)]`）。只有在没有回收能搬走或释放目标时它才安全，而这正好是一次
`mutate` 的持续时间。传给 `mutate` 的闭包对 `'gc` 是泛型的（`for<'gc> FnOnce(&Mutator<'gc>) -> R`），所以：

- `Gc<'gc, T>` 带不出闭包，因为 `R` 里写不出 `'gc`；
- 一个堆的 `Gc` 放不进另一个堆的对象，因为两边的品牌不同。

`brand!` 声明一个类型，把生命周期映射回堆类型：

```rust
brand!(NodeB = for<'gc> Node<'gc>);
```

`Root<NodeB>` 不存生命周期；`m.get(&root)` 返回当前 `mutate` 里的 `Gc<'gc, Node<'gc>>`。

## 裸指针

`Gc::as_ptr` 与 `Gc::from_raw` 在同一次 `mutate` 里和裸数据指针互相转换，供 NaN-boxing 之类的技巧使用。存在对象里的裸地址
回收器看不见。存裸地址的类型必须：

- 在 `Trace::trace` 里通过 `Tracer::edge` 报告它们；
- 在 `Trace::relocate` 里把它们交给 `Tracer::relocate`，并写回新地址。

`WordSlot` 覆盖了最常见的 NaN-boxing 64 位字的情况（见[弱引用与字槽](weak.md)）。

## `Trace`

`#[derive(Trace)]` 生成：

- `trace`：报告每个可能存边的字段；
- `relocate`：疏散后改写这些字段；
- `NO_EDGES`：没有字段能存边时为真，这种类型的对象永远不会被压进标记栈。

派生拒绝实现了 `Drop` 的类型。析构没有固定顺序，`Drop` 可能看到已经析构过的对象。析构不碰任何 `Gc` 时，可以写
`#[trace(unsafe_drop)]` 跳过这个检查。

堆里的类型必须是 `Send + Sync`：对象会在回收线程上被追踪和析构。
