# Promotion: a cross-heap as the old generation

Allocating every shareable object directly in a cross-heap would mean deciding at allocation time which heap an object
goes to. Promotion avoids that. Objects are allocated in the private heap as usual, and those that survive a young
collection move into a cross-heap attached as the old generation. Allocation stays lazy, and short-lived objects never
touch shared memory.

## Setting it up

```rust
let space = CrossHeapSpace::reserve_fixed()?;          // the fixed region, at 64 TiB
let cross = space.create(path, threads)?;              // or create_anonymous for in-process use
let mut heap = Heap::new(threads);
heap.attach_cross_heap(&cross)?;
```

- The cross-heap must be in the **fixed region**, so absolute addresses stored in its objects are valid in every
  process that also reserved it.
- The cross-heap becomes **owner-only**: other processes and `collect_group` leave it alone. Only this private heap
  collects it.
- With a cross-heap attached, young evacuation is forced to `Full`, and concurrent marking is not supported.

## Shareable private objects

```rust
#[repr(C)]
#[derive(Trace, Shared)]
struct Node<'heap, 'scope> {
    value: u64,
    next: Slot<'heap, 'scope, Node<'heap, 'scope>>,
}

heap.scope(|m| m.alloc_shared(Node { value: 1, next: None.into() }));
```

`Scope::alloc_shared` (and `alloc_shared_tail`) takes a type that is both `Trace` and `Shared`, without `Drop`. Its
`ScopeLocal`, `Option<ScopeLocal>`, `Slot` and `WordSlot` fields are recorded in the pointer map as absolute-address edges (`WORD_EDGE`,
`PTR_EDGE`).

## What a young collection does

When a worker reaches a young object during a young collection:

1. it asks the object's `visit` function, in asking mode, whether the type is shareable and what its type number in the
   cross-heap is. The type is registered there on first use;
2. if the type is shareable, the worker claims the object, copies it with its own promotion buffer into the cross-heap,
   writes the `PHeader` (type number and birth stamp), and forwards the old header to the new place;
3. slots, roots and weak references in the private heap are relocated to the new address, as for any move.

A promoted object's edges are traced too. Its targets must be shareable; a non-shareable target reached from a promoted
object panics (`in_shared`).

The private heap skips cross-heap addresses everywhere: marking, reference counting, the remembered set, the write
barrier, root and weak tables.

## Writing a private object into a cross-heap object

A cross-heap object can be written after promotion, for example `promoted.next.set(m, Some(fresh))` where `fresh` is
still young. The barrier:

- records the slot in the remembered set;
- sets `promotion_pending`.

Until the next young collection, the slot holds a private address. Other processes treat an address outside the region as
empty. The next young collection promotes `fresh` and rewrites the slot through the remembered set. A host checks
`Heap::promotion_pending()` when a unit of work ends and runs a young collection if it is set.

## Collecting

`Heap::collect`:

1. runs a young collection, so every shareable survivor is promoted;
2. runs a full private mark;
3. collects the cross-heap with extra roots: the cross-heap edges the private mark found, and the root-table entries
   pointing into the cross-heap;
4. clears private weak references whose cross-heap target died.

`CrossHeap::collect` on an owner-only heap does nothing.

## Handing an object to another process

```rust
let sig = heap.scope(|m| cross.scope(|cm| cm.sign_promoted(m.scope_in(&root))));
```

`sign_promoted` returns `None` for an object that is still private. A signature keeps nothing alive. If the owner drops
the object and collects, the signature resolves to `None`.

## How j8 uses it

With j8's `crossheap` feature, an `Isolate` takes a cross-heap (`attach_cross_heap`):

- `scope.shares.share(mc, gc)` records an object to share and returns a ticket;
- j8 never collects in the middle of running JS. At the end of `Isolate::enter`, if anything is pending, it runs one
  young collection and then signs the recorded objects;
- `take_signatures` hands out (ticket, signature) pairs.

JS strings are shareable: their bytes, and the UTF-16 units of non-ASCII text, live in the object's tail.
