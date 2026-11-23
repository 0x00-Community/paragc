# 弱引用与字槽

## `Weak<B>`

`Weak<B>` 和 `Root<B>` 一样不带生命周期，可以存在任何地方。不同的是它不让目标存活。

```rust
let weak = heap.mutate(|m| m.weak::<NodeB>(m.alloc(Node { value: 1, next: None })));
heap.collect();
assert!(weak.is_cleared());
assert!(heap.mutate(|m| m.upgrade(&weak).is_none()));
```

工作方式：

- 每个 `Weak` 在堆的弱引用表里占一个槽，这张表不是根集。
- 每次停顿（完整回收、年轻回收、并发标记收尾、一起回收），在标记和疏散之后、清扫之前，走一遍这张表：
  - 搬走了的目标，换成新地址；
  - 没标记到的目标，用 CAS 清空，免得同一刻被丢掉的句柄复活。
- `mutate` 期间，表里只指着活对象。`Mutator::upgrade` 返回 `Option<Gc>`，`Weak::is_cleared` 在 `mutate` 外也能用。
- 弱引用的活目标计数被粘住（见[引用计数](refcount.md)）。只有弱引用的年轻对象在下一次年轻回收时清掉，老对象在下一次
  完整回收时清掉。

弱引用也可以存在堆对象里，跟着持有它的对象走。

## `WordSlot`

`WordSlot<'gc>` 存一个由使用者自己编码的 `u64`，例如 NaN-boxing 的 JS 值：

- 大于等于 `WORD_POINTER_MIN`（`0xFFF8 << 48`）的字是指针：低 48 位是对象数据地址，高 16 位是使用者的标签，疏散时原样保留；
- 其余的字都是普通数据。

`WordSlot` 的屏障、计数和改写都和 `Slot` 一样。`word_pointer` 解码一个字。带指针标签的字必须指着同一个堆里的活对象。
