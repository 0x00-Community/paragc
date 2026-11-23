# Handle 与 Brand

paragc 的几种 Handle，区别在于**属于哪个堆**、**有效多久**，以及**是否让目标存活**。生命周期的含义见 [Scope 与生命周期](scopes.md)。

| Handle | 有效期 | 是否保活 | 能放在哪 |
| --- | --- | --- | --- |
| `ScopeLocal<'heap, 'scope, T>` | 借出它的 scope 没被可变借用之前 | 是，在那之前 | 局部变量、堆对象的字段 |
| `Slot<'heap, 'scope, T>` | 堆对象的字段 | 是，作为一条边 | 堆对象里 |
| `WordSlot<'heap, 'scope>` | 堆对象的字段 | 字是指针时是 | 堆对象里 |
| `Handle<'heap, 'scope, T>` | 作为值同 `ScopeLocal`；也可以是堆对象的字段 | 是，作为一条边 | 局部变量、堆对象的字段 |
| `HandleSlot<'heap, 'scope, T>` | 堆对象的字段 | 是，作为一条边 | 堆对象里 |
| `ScopeGlobal<'heap, 'life, B>` | drop 之前，在它所属的 scope 以内；堆自己的 `'life = 'heap` | 是 | 任何地方、任何线程 |
| `Weak<'heap, B>` | drop 之前 | 否 | 任何地方、任何线程 |
| `Far<'heap, 'scope, B>` | 同持有方堆的 `Slot` | 是，登记在目标堆里 | 持有方堆的对象里 |

## Handle：搬动后不变的身份

`ScopeLocal` 就是对象地址，回收一搬对象地址就变（见[对象地址为什么会变](moving.md)）。所有存了地址的地方都得改写：字段、记忆集、
按地址做键的哈希表、生成代码里的常量。`Handle` 免掉了这些。

```rust
#[derive(Trace)]
struct Shape { /* ... */ }

#[derive(Trace)]
struct Object<'heap, 'scope> {
    shape: HandleSlot<'heap, 'scope, Shape>,          // 可改写，不走屏障
}

heap.scope(|s| {
    let shape = s.alloc_handle(Shape { /* ... */ });  // Handle<'heap, '_, Shape>
    let obj = s.alloc(Object { shape: shape.into() });
    let sh = obj.shape.get().unwrap().get();           // 解析一次：ScopeLocal<'heap, '_, Shape>
    let id = shape.id();                                // 跨回收不变
});
```

- `Scope::alloc_handle` 分配对象，同时在堆的 Handle 表里给它一个**表项**。表项存对象当前的头地址；Handle 就是表项的地址。
- 对象搬动时只改写表项。Handle 本身、`==`、`Hash`、`Handle::id` 都不变，所以按 Handle 做键的哈希表不用重新哈希，
  生成的代码也可以把 id 当常量来比较。
- 经 Handle 读对象比 `ScopeLocal` 多一次依赖加载（先表项，再对象）。scope 不回收时对象不搬，所以用 `Handle::get` 解析一次，
  之后一直用那个 `ScopeLocal`。
- 写 Handle 不要 mutator、不走屏障：普通字段直接存，或者 `HandleSlot::set`。
- id 可以当普通字长存起来（NaN-box 的值、生成代码里的常量），再用 unsafe 的 `Handle::from_id` 变回 Handle，前提是还有被追踪的
  地方持有这个 Handle。

回收器怎样在没有屏障的情况下保证正确：

| 回收 | Handle 表项 |
| --- | --- |
| 年轻回收 | 每个表项都是根：老对象可能拿走了年轻对象的 Handle，却没有记忆集的槽记下来。疏散模式下对象照样搬，表项随之改写 |
| 并发标记 | 每个表项都是这一轮的根（Handle 是按值拷来拷去的，快照看不到某个持有者丢掉了它） |
| 完整回收、一起回收 | 表项不是根：活对象持有的 Handle 作为边来追踪；标记完以后，对象没被标到的表项清空并重用，搬走的改写 |

所以带 Handle 的对象只由完整回收释放，年轻回收和引用计数都不会释放它（计数被粘住）。没有东西是永生的：一旦没有活对象持有这个
Handle，下一次完整回收就释放对象和它的表项。死掉的表项，id 可能被新对象重用。

## 为什么要 Brand

`ScopeLocal` 就是一个裸指针（`NonNull<T>` 上的 `#[repr(transparent)]`）。它只在没有回收能搬走或释放目标、并且只在自己那个堆里
才安全。两个生命周期分别保证这两点：

- `'scope` 是对借出它的 scope 的借用。回收、派生子 scope 都要可变借用这个 scope，所以借用检查器会先让所有 `ScopeLocal` 失效。
  打开 scope 的闭包对 scope 的生命周期是泛型的（`for<'life> FnOnce(&mut Scope<'heap, 'life>) -> R`），从 scope 借出的东西
  带不出去。
- `'heap` 是堆的 Brand：`Heap::new(threads, |heap| ..)` 给每个堆一个独有的 Brand，所以一个堆的指针放不进另一个堆的对象。

根不带 `'scope`。`brand!` 声明一个类型，把两个生命周期映射回堆类型：

```rust
brand!(NodeB = for<'heap, 'scope> Node<'heap, 'scope>);
```

`ScopeGlobal<'heap, 'heap, NodeB>` 两个都不存；`s.scope_in(&root)` 返回从当前 scope 借出的
`ScopeLocal<'heap, '_, Node<'heap, '_>>`。

## 裸指针

`ScopeLocal::as_ptr` 与 `ScopeLocal::from_raw` 在同一个 scope 里、中间不回收时，和裸数据指针互相转换，供 NaN-boxing 之类的技巧使用。存在对象里的裸地址
回收器看不见。存裸地址的类型必须：

- 在 `Trace::trace` 里通过 `Tracer::edge` 报告它们；
- 在 `Trace::relocate` 里把它们交给 `Tracer::relocate`，并写回新地址。

`WordSlot` 覆盖了最常见的 NaN-boxing 64 位字的情况（见[弱引用与字槽](weak.md)）。

## `Trace`

`#[derive(Trace)]` 生成：

- `trace`：报告每个可能存边的字段；
- `relocate`：疏散后改写这些字段；
- `NO_EDGES`：没有字段能存边时为真，这种类型的对象永远不会被压进标记栈。

派生拒绝实现了 `Drop` 的类型。析构没有固定顺序，`Drop` 可能看到已经析构过的对象。析构不碰任何 `ScopeLocal` 时，可以写
`#[trace(unsafe_drop)]` 跳过这个检查。

堆里的类型必须是 `Send + Sync`：对象会在回收线程上被追踪和析构。
