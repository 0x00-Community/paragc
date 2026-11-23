# Handles and brands

paragc's handle types differ in **how long they are valid** and **whether they keep their target alive**.

| Handle | Lifetime | Keeps alive | Where it can live |
| --- | --- | --- | --- |
| `Gc<'gc, T>` | one `mutate` | while the `mutate` runs (no collection can start) | locals, fields of heap objects |
| `Slot<'gc, T>` | field of a heap object | yes, as an edge | inside heap objects |
| `WordSlot<'gc>` | field of a heap object | yes, when the word is a pointer | inside heap objects |
| `Handle<'gc, T>` | one `mutate` (as a value); a field of a heap object | yes, as an edge | locals, fields of heap objects |
| `HandleSlot<'gc, T>` | field of a heap object | yes, as an edge | inside heap objects |
| `Root<B>` | none | yes | anywhere, any thread |
| `Weak<B>` | none | no | anywhere, any thread |
| `Far<'a, B>` | brand of the holder heap | yes, registered in the target heap | inside objects of the holder heap |

## Handles: identity that survives moves

A `Gc` is the object's address, and the address changes whenever a collection moves the object (see
[why object addresses change](moving.md)). Everything that stores an address must then be rewritten: fields, the
remembered set, hash tables keyed by address, constants in generated code. A `Handle` avoids that.

```rust
#[derive(Trace)]
struct Shape { /* ... */ }

#[derive(Trace)]
struct Object<'gc> {
    shape: HandleSlot<'gc, Shape>,        // rewritable, no barrier
}

heap.mutate(|m| {
    let shape = m.alloc_handle(Shape { /* ... */ });   // Handle<'gc, Shape>
    let obj = m.alloc(Object { shape: shape.into() });
    let s = obj.shape.get().unwrap().get();             // resolve once: Gc<'gc, Shape>
    let id = shape.id();                                // stable across collections
});
```

- `Mutator::alloc_handle` allocates the object and an **entry** for it in the heap's handle table. The entry holds the
  object's current header; the handle is the entry's address.
- When the object moves, only the entry is rewritten. The handle, `==`, `Hash` and `Handle::id` stay the same, so a
  hash table keyed by handles needs no rehashing and generated code may compare against the id as a constant.
- Reading through a handle costs one dependent load more than a `Gc` (entry, then object). Objects do not move within
  a `mutate`, so resolve once with `Handle::get` and keep the `Gc` for the rest of it.
- Writing a handle needs no mutator and no barrier: plain fields and `HandleSlot::set`.

How the collector keeps this sound without a barrier:

| Collection | Handle entries |
| --- | --- |
| young | every entry is a root: an old object may have taken a young object's handle with no remembered slot. In evacuating modes the objects move and their entries are rewritten |
| concurrent marking | every entry is a root of the round (handles are copied as plain values, so the snapshot cannot see a holder losing them) |
| full, group | entries are not roots: handles held by live objects are traced as edges; after marking, entries whose object was not marked are cleared and reused, moved ones are rewritten |

Handled objects are therefore freed by full collections only, never by young collections or reference counting (their
counts are stuck). Nothing is immortal: once no live object holds the handle, the next full collection frees the object
and its entry. A dead entry's id can be reused by a new object.

## Why brands

A `Gc` is a bare pointer (`#[repr(transparent)]` over `NonNull<T>`). It is only safe while no collection can move or free
its target, which is exactly the duration of one `mutate`. The closure passed to `mutate` is generic over `'gc`
(`for<'gc> FnOnce(&Mutator<'gc>) -> R`), so:

- a `Gc<'gc, T>` cannot be returned out of the closure, because `R` cannot name `'gc`;
- a `Gc` from one heap cannot be stored in another heap's object, because the two brands differ.

`brand!` declares a type that maps a lifetime back onto a heap type:

```rust
brand!(NodeB = for<'gc> Node<'gc>);
```

`Root<NodeB>` stores no lifetime; `m.get(&root)` returns `Gc<'gc, Node<'gc>>` for the current `mutate`.

## Raw pointers

`Gc::as_ptr` and `Gc::from_raw` convert to and from a raw data pointer taken in the same `mutate`, for NaN-boxing and
similar tricks. The collector cannot see a raw address stored in an object. A type that stores raw addresses must:

- report them in `Trace::trace` through `Tracer::edge`;
- pass them to `Tracer::relocate` in `Trace::relocate` and write the new address back.

`WordSlot` covers the common case of a NaN-boxed 64-bit word (see [Weak references and word slots](weak.md)).

## `Trace`

`#[derive(Trace)]` generates:

- `trace`, which reports every field that holds an edge;
- `relocate`, which rewrites those fields after evacuation;
- `NO_EDGES`, true when no field can hold an edge. Objects of such types are never pushed on the mark stack.

The derive refuses types that implement `Drop`. Destructors run in no particular order, so a `Drop` could see objects
that were already destroyed. `#[trace(unsafe_drop)]` opts out when the destructor touches no `Gc`.

Heap types must be `Send + Sync`: objects are traced and dropped on collector threads.
