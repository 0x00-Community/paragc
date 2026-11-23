# Scopes and lifetimes

Every handle in paragc answers two questions: **which heap** it belongs to, and **how long** it may be used. paragc
answers both with Rust lifetimes, so the compiler, not the collector, rejects the mistakes. This page describes the model
introduced in 0.7.0.

## Three lifetimes

| Lifetime | Means | Carried by |
| --- | --- | --- |
| `'heap` | one private heap (a brand: every heap has its own) | `Heap<'heap>`, `Scope<'heap, _>`, every pointer type of that heap, `ScopeGlobal`, `Weak`, `Far` |
| `'xheap` | one cross-heap (shared memory) | `CrossHeap<'xheap>`, `CrossHeapScope<'xheap, _>`, `CrossHeapLocal`, `CrossHeapSlot`, `CrossHeapRef`, `CrossHeapGlobal` |
| `'scope` | a borrow of a scope: how long a direct pointer stays valid | `ScopeLocal<'heap, 'scope, T>`, and the slots inside objects (`Slot`, `WordSlot`, `HandleSlot`) |

A scope also has a lifetime of its own, `'life` in `Scope<'heap, 'life>`: how long the scope exists. Roots kept for
a scope (`ScopeGlobal<'heap, 'life, B>`) carry it.

`CrossHeapWeak` and `Signature` carry no lifetime: they are plain data meant to be sent to other processes and resolved
there.

## Scopes nest

The heap is the outermost scope. Every scope derives smaller ones; they are all the same struct, `Scope`, with a
shorter life.

<div class="dg">
<p class="dg-cap">A heap, a scope, and two scopes derived from it one after the other</p>
<div class="nest">
  <div class="label"><b>Heap&lt;'heap&gt;</b><small>the outermost scope; its roots are <code>ScopeGlobal&lt;'heap, 'heap, B&gt;</code></small></div>
  <div class="vals"><span class="val global">config: ScopeGlobal</span></div>
  <div class="nest">
    <div class="label"><b>heap.scope(|s| …)</b><small><code>Scope&lt;'heap, 's&gt;</code>, holds the heap exclusively</small></div>
    <div class="vals"><span class="val local">a: ScopeLocal</span><span class="val global">kept: ScopeGlobal&lt;'s&gt;</span></div>
    <div class="nest">
      <div class="label"><b>s.scope(|t| …)</b><small>first inner scope</small></div>
      <div class="vals"><span class="val local">b: ScopeLocal</span><span class="val local">c: ScopeLocal</span></div>
    </div>
    <div class="nest">
      <div class="label"><b>s.collect()</b><small>between the inner scopes</small></div>
    </div>
    <div class="nest">
      <div class="label"><b>s.scope(|u| …)</b><small>second inner scope</small></div>
      <div class="vals"><span class="val local">s.scope_in(&amp;kept)</span></div>
    </div>
  </div>
</div>
<div class="legend"><span class="local">direct pointer (dies at the next collection or nested scope)</span><span class="global">root (survives collections)</span></div>
</div>

## Two kinds of values

| | `ScopeLocal<'heap, 'scope, T>` | `ScopeGlobal<'heap, 'life, B>` |
| --- | --- | --- |
| What it is | the object's address | an entry of the heap's root table |
| Reading it | a plain load | one more load (the entry) |
| Keeps the object alive | while the scope it was borrowed from is not mutably borrowed | until dropped |
| Across a collection | **dies**: the borrow checker forbids using it afterwards | rewritten by the collector, still valid |
| Made by | `alloc`, reading a slot, `scope_in` | `scope_out` (to any enclosing scope), `global` (to the heap) |

`scope_out` carries an object out to an enclosing scope; `scope_in` brings it back into the current one as a direct
pointer. A `ScopeGlobal` is only a root, so it is safe to read from any scope inside the one it belongs to.

<div class="dg">
<p class="dg-cap">What a collection inside a scope does to each kind of value</p>
<div class="flow">
  <div class="step"><h5>allocate</h5><span class="addr k1">a = s.alloc(…)</span><p>a direct pointer, borrowed from <code>s</code></p></div>
  <div class="arrow">→</div>
  <div class="step"><h5>keep</h5><span class="addr k2">g = s.scope_out(a)</span><p>a root-table entry holds the address</p></div>
  <div class="arrow">→</div>
  <div class="step"><h5>collect</h5><span class="addr k4">s.collect()</span><p>needs <code>&amp;mut s</code>: <code>a</code> is dead; the object moves and the entry is rewritten</p></div>
  <div class="arrow">→</div>
  <div class="step"><h5>use again</h5><span class="addr k1">s.scope_in(&amp;g)</span><p>a new direct pointer at the new address</p></div>
</div>
</div>

## The rules, all checked by the compiler

1. **Collecting needs `&mut` the scope.** Every `ScopeLocal` borrowed from it must be dead by then. Using one after
   `collect` is a borrow error.
2. **Deriving a smaller scope needs `&mut` the scope too.** So the outer scope's direct pointers cannot be used inside
   the inner one; carry objects in with a `ScopeGlobal`.
3. **A scope that neither derives nor collects keeps its direct pointers valid throughout.** This is the common case:
   j8 runs a whole turn of JavaScript in one scope.
4. **Nothing of one heap can be used with another.** Every pointer and slot type carries `'heap`, and every heap made
   with `Heap::new(threads, |heap| ..)` has its own.

```rust,compile_fail
# use paragc::{Heap, Trace};
# #[derive(Trace)]
# struct X(u64);
Heap::new(std::num::NonZeroUsize::MIN, |mut heap| heap.scope(|s| {
    let x = s.alloc(X(1));
    s.collect();
    x.0 // error: `*s` is borrowed mutably by `collect` while `x` borrows it
}));
```

## Exclusive and shared scopes

| | `heap.scope(&mut self, ..)` | `heap.scope_shared(&self, ..)` / `scope_shared_in` |
| --- | --- | --- |
| Threads | one | any number, each with its own scope |
| Collect inside | yes (`collect`, `collect_young`, `collect_if_needed`, `shrink`) | no (panics) |
| Collections | happen inside, between derived scopes | wait until every shared scope has ended, as before |

Concurrency is outside the safe model: shared scopes keep the old rule (no collection while any of them is open), and
heaps that must be stored in structs or collections are made with `unsafe Heap::new_unchecked`, whose caller promises not
to mix heaps that share a brand.

## Why direct pointers, and not V8's handles

V8 gives C++ code a `Local<T>`: a pointer into a slot of the current `HandleScope`, which the collector rewrites. That
indirection is what lets V8 collect at any allocation. paragc keeps direct pointers and lets the borrow checker decide
where collection is allowed instead:

| | V8 | paragc |
| --- | --- | --- |
| Local value | `Local<T>`, a slot in the `HandleScope` | `ScopeLocal`, the address itself |
| Cost of reading | one extra load | none |
| Where collection may happen | at any allocation | where the scope is mutably borrowed |
| Carrying a value out | `EscapableHandleScope::Escape` copies into the outer scope | `scope_out` makes a root of the outer scope |
| Long-lived value | `Global<T>` | `ScopeGlobal<'heap, 'heap, B>` |

## Cross-heaps

Cross-heaps follow the same shape with `'xheap`: `space.create(path, threads, |cross| ..)` (or the unsafe
`*_unchecked` constructors), `cross.scope(|s| ..)` giving a `CrossHeapScope<'xheap, 'life>`, direct pointers
`CrossHeapLocal<'xheap, 'scope, T>`, and roots `CrossHeapGlobal<'xheap, B>`. A cross-heap is collected only outside its
scopes (other processes may be inside), so it has no in-scope collection.
