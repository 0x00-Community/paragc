# Handles and brands

paragc's handle types differ in **how long they are valid** and **whether they keep their target alive**.

| Handle | Lifetime | Keeps alive | Where it can live |
| --- | --- | --- | --- |
| `Gc<'gc, T>` | one `mutate` | while the `mutate` runs (no collection can start) | locals, fields of heap objects |
| `Slot<'gc, T>` | field of a heap object | yes, as an edge | inside heap objects |
| `WordSlot<'gc>` | field of a heap object | yes, when the word is a pointer | inside heap objects |
| `Root<B>` | none | yes | anywhere, any thread |
| `Weak<B>` | none | no | anywhere, any thread |
| `Far<'a, B>` | brand of the holder heap | yes, registered in the target heap | inside objects of the holder heap |

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
