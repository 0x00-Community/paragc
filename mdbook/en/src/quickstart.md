# Quick start

```toml
[dependencies]
paragc = "0.7"
```

A linked list, kept by a root, summed after a collection in the same scope:

```rust
use paragc::{Heap, ScopeGlobal, Slot, Trace, brand};

#[derive(Trace)]
struct Node<'heap, 'scope> {
    value: u64,
    next: Slot<'heap, 'scope, Node<'heap, 'scope>>,
}

// A root has no 'scope; the brand puts the reading scope's borrow back on
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

What happens:

1. `Heap::new(threads, |heap| ..)` makes a heap with a brand `'heap` of its own; the heap lives for the closure.
2. `heap.scope(|s| ..)` opens a scope holding the heap exclusively. `s.alloc(value)` moves `value` into the heap and
   returns a `ScopeLocal`, a direct pointer borrowed from `s`.
3. `s.scope_out(x)` keeps `x` in a root: a `ScopeGlobal`, which collections rewrite when the object moves.
4. `s.collect()` borrows `s` mutably, so every `ScopeLocal` is dead from here on (the compiler checks it). It marks from
   the roots and frees the rest.
5. `s.scope_in(&root)` turns the root back into a direct pointer.

See [Scopes and lifetimes](scopes.md) for nested scopes and the rules behind them.

## Mutable edges

`Slot` fields are rewritten through the write barrier, which needs the scope:

```rust
use paragc::{Heap, Slot, Trace};

#[derive(Trace)]
struct Cell<'heap, 'scope> {
    value: Slot<'heap, 'scope, Cell<'heap, 'scope>>,
}

Heap::new(std::num::NonZeroUsize::MIN, |mut heap| heap.scope(|s| {
    let a = s.alloc(Cell { value: None.into() });
    let b = s.alloc(Cell { value: None.into() });
    a.value.set(s, Some(b));        // the write barrier needs the scope
    assert!(a.value.get().is_some());
}));
```

The barrier:

- records old-to-young edges for young collections;
- keeps reference counts;
- during concurrent marking, marks the overwritten value.

## Choosing a collection

Inside an exclusive scope (`s.collect…()`) or on the heap between scopes (`heap.collect…()`):

| Call | Use it when |
| --- | --- |
| `collect_if_needed()` | Between units of work. It decides between nothing, a young collection and a full one. |
| `collect_young()` | You know most new objects are already garbage. |
| `collect()` | You want everything unreachable gone now. |
| `heap.mark_concurrently()` then `heap.finish_marking()` | Pauses must stay short while shared scopes keep running. |
| `shrink()` | The program is idle and memory should go back to the system. |

## Threads

`Heap` is `Sync`. Open `heap.scope_shared(|s| ..)` (or `scope_shared_in` with a thread's own `Tlab`) from as many threads
as you like; a shared scope cannot collect, and a collection (`&mut Heap`) waits until every shared scope has ended.
Collector threads are a pool owned by the heap: they start at the first collection and park between collections.
