# Scope 与生命周期

paragc 的每个 Handle 都要回答两个问题：**属于哪个堆**，以及**能用多久**。paragc 用 Rust 的生命周期回答这两个问题，所以出错时
是编译器拒绝，而不是回收器出事。本页讲 0.7.0 引入的这套模型。

## 三个生命周期

| 生命周期 | 含义 | 谁带着它 |
| --- | --- | --- |
| `'heap` | 一个私有堆（Brand：每个堆都有自己的） | `Heap<'heap>`、`Scope<'heap, _>`、这个堆的所有指针类型、`ScopeGlobal`、`Weak`、`Far` |
| `'xheap` | 一个跨堆（共享内存） | `CrossHeap<'xheap>`、`CrossHeapScope<'xheap, _>`、`CrossHeapLocal`、`CrossHeapSlot`、`CrossHeapRef`、`CrossHeapGlobal` |
| `'scope` | 对一个 scope 的借用：直接指针能用多久 | `ScopeLocal<'heap, 'scope, T>`，以及对象里的槽（`Slot`、`WordSlot`、`HandleSlot`） |

scope 自己还有一个生命周期，即 `Scope<'heap, 'life>` 里的 `'life`：这个 scope 存在多久。为某个 scope 保留的根
（`ScopeGlobal<'heap, 'life, B>`）带着它。

`CrossHeapWeak` 和 `Signature` 不带生命周期：它们是要发给别的进程、在那边重新解析的纯数据。

## scope 可以嵌套

堆是最外层的 scope。每个 scope 都能派生出更小的 scope；它们是同一个结构体 `Scope`，只是生命周期更短。

<div class="dg">
<p class="dg-cap">一个堆、一个 scope，以及先后从它派生出的两个小 scope</p>
<div class="nest">
  <div class="label"><b>Heap&lt;'heap&gt;</b><small>最外层的 scope；它的根是 <code>ScopeGlobal&lt;'heap, 'heap, B&gt;</code></small></div>
  <div class="vals"><span class="val global">config: ScopeGlobal</span></div>
  <div class="nest">
    <div class="label"><b>heap.scope(|s| …)</b><small><code>Scope&lt;'heap, 's&gt;</code>，独占堆</small></div>
    <div class="vals"><span class="val local">a: ScopeLocal</span><span class="val global">kept: ScopeGlobal&lt;'s&gt;</span></div>
    <div class="nest">
      <div class="label"><b>s.scope(|t| …)</b><small>第一个小 scope</small></div>
      <div class="vals"><span class="val local">b: ScopeLocal</span><span class="val local">c: ScopeLocal</span></div>
    </div>
    <div class="nest">
      <div class="label"><b>s.collect()</b><small>两个小 scope 之间回收</small></div>
    </div>
    <div class="nest">
      <div class="label"><b>s.scope(|u| …)</b><small>第二个小 scope</small></div>
      <div class="vals"><span class="val local">s.scope_in(&amp;kept)</span></div>
    </div>
  </div>
</div>
<div class="legend"><span class="local">直接指针（下一次回收或派生子 scope 时失效）</span><span class="global">根（跨回收有效）</span></div>
</div>

## 两种值

| | `ScopeLocal<'heap, 'scope, T>` | `ScopeGlobal<'heap, 'life, B>` |
| --- | --- | --- |
| 是什么 | 对象地址 | 堆的根表里的一项 |
| 读取 | 一次普通加载 | 多一次加载（先读表项） |
| 保活 | 借出它的 scope 没被可变借用之前 | drop 之前 |
| 经过一次回收 | **失效**：借用检查器不许之后再用 | 被回收器改写，继续有效 |
| 怎么得到 | `alloc`、读槽、`scope_in` | `scope_out`（带到任意一层外层 scope）、`global`（带到堆） |

`scope_out` 把对象带到外层 scope；`scope_in` 把它带回当前 scope，成为直接指针。`ScopeGlobal` 只是一个根，所以在它所属
scope 以内的任何 scope 里读它都是安全的。

<div class="dg">
<p class="dg-cap">scope 里的一次回收对两种值分别做了什么</p>
<div class="flow">
  <div class="step"><h5>分配</h5><span class="addr k1">a = s.alloc(…)</span><p>直接指针，从 <code>s</code> 借出</p></div>
  <div class="arrow">→</div>
  <div class="step"><h5>保留</h5><span class="addr k2">g = s.scope_out(a)</span><p>根表的一项存着地址</p></div>
  <div class="arrow">→</div>
  <div class="step"><h5>回收</h5><span class="addr k4">s.collect()</span><p>要 <code>&amp;mut s</code>：<code>a</code> 失效；对象被搬走，表项被改写</p></div>
  <div class="arrow">→</div>
  <div class="step"><h5>再用</h5><span class="addr k1">s.scope_in(&amp;g)</span><p>指向新地址的新直接指针</p></div>
</div>
</div>

## 规则，全部由编译器检查

1. **回收要 `&mut` 当前 scope。** 从它借出的 `ScopeLocal` 到那时必须都已不再使用；回收之后再用就是借用错误。
2. **派生小 scope 也要 `&mut` 当前 scope。** 所以外层的直接指针不能在内层里用；要带进去，用 `ScopeGlobal`。
3. **一个不派生、不回收的 scope，直接指针全程有效。** 这是最常见的情况：j8 一轮 JavaScript 就在一个 scope 里跑完。
4. **一个堆的东西不能拿到另一个堆用。** 每个指针类型、槽类型都带 `'heap`，而 `Heap::new(threads, |heap| ..)` 造出的每个堆
   都有自己的 `'heap`。

```rust,compile_fail
# use paragc::{Heap, Trace};
# #[derive(Trace)]
# struct X(u64);
Heap::new(std::num::NonZeroUsize::MIN, |mut heap| heap.scope(|s| {
    let x = s.alloc(X(1));
    s.collect();
    x.0 // 错误：`x` 还借着 `*s`，`collect` 却要可变借用它
}));
```

## 独占 scope 与共享 scope

| | `heap.scope(&mut self, ..)` | `heap.scope_shared(&self, ..)` / `scope_shared_in` |
| --- | --- | --- |
| 线程 | 一个 | 任意多个，每个线程一个 scope |
| 在 scope 里回收 | 可以（`collect`、`collect_young`、`collect_if_needed`、`shrink`） | 不行（panic） |
| 回收在哪发生 | scope 里，两个派生 scope 之间 | 等所有共享 scope 都结束，和以前一样 |

多线程不在安全模型之内：共享 scope 沿用旧规矩（只要还有一个开着就不回收）；必须存进结构体或集合的堆，用
`unsafe Heap::new_unchecked` 创建，由调用方保证共用一个 Brand 的不同堆之间不混用对象。

## 为什么用直接指针，而不是 V8 的 Handle

V8 给 C++ 代码的是 `Local<T>`：指向当前 `HandleScope` 里一个槽的指针，由回收器改写这个槽。正是这一层间接，让 V8 能在任何
一次分配时回收。paragc 保留直接指针，改由借用检查器决定哪里允许回收：

| | V8 | paragc |
| --- | --- | --- |
| 局部值 | `Local<T>`，`HandleScope` 里的一个槽 | `ScopeLocal`，地址本身 |
| 读取代价 | 多一次加载 | 没有 |
| 哪里能回收 | 任何一次分配 | scope 被可变借用的地方 |
| 把值带出去 | `EscapableHandleScope::Escape` 复制到外层 | `scope_out` 在外层 scope 建一个根 |
| 长期持有 | `Global<T>` | `ScopeGlobal<'heap, 'heap, B>` |

## 跨堆

跨堆照同样的形状，用 `'xheap`：`space.create(path, threads, |cross| ..)`（或 unsafe 的 `*_unchecked` 构造函数），
`cross.scope(|s| ..)` 得到 `CrossHeapScope<'xheap, 'life>`，直接指针 `CrossHeapLocal<'xheap, 'scope, T>`，根
`CrossHeapGlobal<'xheap, B>`。跨堆只在它的 scope 之外回收（别的进程可能正在里面），所以没有 scope 内回收。
