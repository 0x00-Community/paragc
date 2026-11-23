# Quick start

```toml
[dependencies]
paragc = "0.6"
```

A singly linked list, kept by a root and summed after a collection:

```rust
use paragc::{Gc, Heap, Root, Trace, brand};

#[derive(Trace)]
struct Node<'gc> {
    value: u64,
    next: Option<Gc<'gc, Node<'gc>>>,
}

// A root handle has no 'gc; the brand puts it back on when you read it
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

What happens here:

1. `heap.mutate(|m| ...)` enters the heap. `m` is a `Mutator<'gc>`, and every `Gc<'gc, T>` it hands out is tied to
   this one call by the `'gc` brand.
2. `m.alloc(value)` moves `value` into the heap and returns a `Gc`.
3. `m.root(gc)` turns a `Gc` into a `Root<NodeB>`. A root carries no lifetime, so it can leave the closure, be stored
   anywhere and be sent to other threads.
4. `heap.collect()` takes `&mut Heap`, so no `mutate` can be running. It marks from the roots and frees everything else.
5. `m.get(&root)` brings the object back as a `Gc` in a new `mutate`.

## Mutable edges

`Gc` fields are fixed once the object is built. A field that changes is a `Slot`:

```rust
use paragc::{Slot, Trace};

#[derive(Trace)]
struct Cell<'gc> {
    value: Slot<'gc, Node<'gc>>,
}

heap.mutate(|m| {
    let cell = m.alloc(Cell { value: None.into() });
    let node = m.alloc(Node { value: 1, next: None });
    cell.value.set(m, Some(node));        // the write barrier sees the Mutator
    assert_eq!(cell.value.get().map(|n| n.value), Some(1));
});
```

`Slot::set` takes the `Mutator` because the write barrier needs it. The barrier:

- logs old-to-young edges for young collections;
- keeps reference counts;
- marks the overwritten value during concurrent marking.

## Choosing a collection

| Call | Use it when |
| --- | --- |
| `collect_if_needed()` | Between units of work. It decides between nothing, a young collection and a full one. |
| `collect_young()` | You know most new objects are already garbage. |
| `collect()` | You want everything unreachable gone now. |
| `mark_concurrently()` then `finish_marking()` | Pauses must stay short while mutators keep running. |
| `shrink()` | The program is idle and memory should go back to the system. |

## Threads

`Heap` is `Sync`. Wrap it in an `Arc` and call `mutate` from as many threads as you like. A collection (`&mut self`)
waits until every one of them has left. Collector threads are a pool owned by the heap: they start at the first
collection and park between collections.
