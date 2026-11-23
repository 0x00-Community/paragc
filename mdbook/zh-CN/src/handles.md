# 句柄与品牌

paragc 的几种句柄，区别在于**有效多久**，以及**是否让目标存活**。

| 句柄 | 有效期 | 是否保活 | 能放在哪 |
| --- | --- | --- | --- |
| `Gc<'gc, T>` | 一次 `mutate` | `mutate` 期间（这时不会开始回收） | 局部变量、堆对象的字段 |
| `Slot<'gc, T>` | 堆对象的字段 | 是，作为一条边 | 堆对象里 |
| `WordSlot<'gc>` | 堆对象的字段 | 字是指针时是 | 堆对象里 |
| `Handle<'gc, T>` | 作为值是一次 `mutate`；也可以是堆对象的字段 | 是，作为一条边 | 局部变量、堆对象的字段 |
| `HandleSlot<'gc, T>` | 堆对象的字段 | 是，作为一条边 | 堆对象里 |
| `Root<B>` | 无 | 是 | 任何地方、任何线程 |
| `Weak<B>` | 无 | 否 | 任何地方、任何线程 |
| `Far<'a, B>` | 持有方堆的品牌 | 是，登记在目标堆里 | 持有方堆的对象里 |

## Handle：搬动后不变的身份

`Gc` 就是对象地址，回收一搬对象地址就变（见[对象地址为什么会变](moving.md)）。所有存了地址的地方都得改写：字段、记忆集、
按地址做键的哈希表、生成代码里的常量。`Handle` 免掉了这些。

```rust
#[derive(Trace)]
struct Shape { /* ... */ }

#[derive(Trace)]
struct Object<'gc> {
    shape: HandleSlot<'gc, Shape>,        // 可改写，不走屏障
}

heap.mutate(|m| {
    let shape = m.alloc_handle(Shape { /* ... */ });   // Handle<'gc, Shape>
    let obj = m.alloc(Object { shape: shape.into() });
    let s = obj.shape.get().unwrap().get();             // 解析一次：Gc<'gc, Shape>
    let id = shape.id();                                // 跨回收不变
});
```

- `Mutator::alloc_handle` 分配对象，同时在堆的 handle 表里给它一个**表项**。表项存对象当前的头地址；handle 就是表项的地址。
- 对象搬动时只改写表项。handle 本身、`==`、`Hash`、`Handle::id` 都不变，所以按 handle 做键的哈希表不用重新哈希，
  生成的代码也可以把 id 当常量来比较。
- 经 handle 读对象比 `Gc` 多一次依赖加载（先表项，再对象）。一次 `mutate` 里对象不搬，所以用 `Handle::get` 解析一次，
  之后一直用那个 `Gc`。
- 写 handle 不要 mutator、不走屏障：普通字段直接存，或者 `HandleSlot::set`。
- id 可以当普通字长存起来（NaN-box 的值、生成代码里的常量），再用 unsafe 的 `Handle::from_id` 变回 handle，前提是还有被追踪的
  地方持有这个 handle。

回收器怎样在没有屏障的情况下保证正确：

| 回收 | handle 表项 |
| --- | --- |
| 年轻回收 | 每个表项都是根：老对象可能拿走了年轻对象的 handle，却没有记忆集的槽记下来。疏散模式下对象照样搬，表项随之改写 |
| 并发标记 | 每个表项都是这一轮的根（handle 是按值拷来拷去的，快照看不到某个持有者丢掉了它） |
| 完整回收、一起回收 | 表项不是根：活对象持有的 handle 作为边来追踪；标记完以后，对象没被标到的表项清空并重用，搬走的改写 |

所以带 handle 的对象只由完整回收释放，年轻回收和引用计数都不会释放它（计数被粘住）。没有东西是永生的：一旦没有活对象持有这个
handle，下一次完整回收就释放对象和它的表项。死掉的表项，id 可能被新对象重用。

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
