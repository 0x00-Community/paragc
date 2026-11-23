# Handles and brands

paragc's handle types differ in **which heap** they belong to, **how long they are valid** and **whether they keep their
target alive**. The lifetimes are explained in [Scopes and lifetimes](scopes.md).

| Handle | Valid | Keeps alive | Where it can live |
| --- | --- | --- | --- |
| `ScopeLocal<'heap, 'scope, T>` | while the scope it was borrowed from is not mutably borrowed | yes, until then | locals, fields of heap objects |
| `Slot<'heap, 'scope, T>` | field of a heap object | yes, as an edge | inside heap objects |
| `WordSlot<'heap, 'scope>` | field of a heap object | yes, when the word is a pointer | inside heap objects |
| `Handle<'heap, 'scope, T>` | as `ScopeLocal` (as a value); a field of a heap object | yes, as an edge | locals, fields of heap objects |
| `HandleSlot<'heap, 'scope, T>` | field of a heap object | yes, as an edge | inside heap objects |
| `ScopeGlobal<'heap, 'life, B>` | until dropped, within its scope; `'life = 'heap` for the heap's own | yes | anywhere, any thread |
| `Weak<'heap, B>` | until dropped | no | anywhere, any thread |
| `Far<'heap, 'scope, B>` | as `Slot`, of the holder heap | yes, registered in the target heap | inside objects of the holder heap |

## Handles: identity that survives moves

A `ScopeLocal` is the object's address, and the address changes whenever a collection moves the object (see
[why object addresses change](moving.md)). Everything that stores an address must then be rewritten: fields, the
remembered set, hash tables keyed by address, constants in generated code. A `Handle` avoids that.

```rust
#[derive(Trace)]
struct Shape { /* ... */ }

#[derive(Trace)]
struct Object<'heap, 'scope> {
    shape: HandleSlot<'heap, 'scope, Shape>,          // rewritable, no barrier
}

heap.scope(|s| {
    let shape = s.alloc_handle(Shape { /* ... */ });  // Handle<'heap, '_, Shape>
    let obj = s.alloc(Object { shape: shape.into() });
    let sh = obj.shape.get().unwrap().get();           // resolve once: ScopeLocal<'heap, '_, Shape>
    let id = shape.id();                               // stable across collections
});
```

- `Scope::alloc_handle` allocates the object and an **entry** for it in the heap's handle table. The entry holds the
  object's current header; the handle is the entry's address.
- When the object moves, only the entry is rewritten. The handle, `==`, `Hash` and `Handle::id` stay the same, so a
  hash table keyed by handles needs no rehashing and generated code may compare against the id as a constant.
- Reading through a handle costs one dependent load more than a `ScopeLocal` (entry, then object). Objects do not move
  while the scope is not collecting, so resolve once with `Handle::get` and keep the `ScopeLocal`.
- Writing a handle needs no scope and no barrier: plain fields and `HandleSlot::set`.
- An id can be stored as a plain word (a NaN-boxed value, a constant in generated code) and turned back with the
  unsafe `Handle::from_id`, as long as something traced still holds the handle.

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

A `ScopeLocal` is a bare pointer (`#[repr(transparent)]` over `NonNull<T>`). It is only safe while no collection can move
or free its target and only inside its own heap. Two lifetimes enforce both:

- `'scope` is a borrow of the scope the pointer came from. Collecting, or deriving a smaller scope, borrows the scope
  mutably, so the borrow checker ends every `ScopeLocal` first. Closures that open scopes are generic over the scope's
  life (`for<'life> FnOnce(&mut Scope<'heap, 'life>) -> R`), so nothing borrowed from a scope leaves it.
- `'heap` is the heap's brand: `Heap::new(threads, |heap| ..)` gives every heap its own, so a pointer of one heap cannot
  be stored in another heap's object.

A root keeps no `'scope`. `brand!` declares a type that maps the two lifetimes back onto a heap type:

```rust
brand!(NodeB = for<'heap, 'scope> Node<'heap, 'scope>);
```

`ScopeGlobal<'heap, 'heap, NodeB>` stores neither; `s.scope_in(&root)` returns `ScopeLocal<'heap, '_, Node<'heap, '_>>`
borrowed from the current scope.

## Raw pointers

`ScopeLocal::as_ptr` and `ScopeLocal::from_raw` convert to and from a raw data pointer taken in the same scope, with no
collection in between, for NaN-boxing and similar tricks. The collector cannot see a raw address stored in an object. A type that stores raw addresses must:

- report them in `Trace::trace` through `Tracer::edge`;
- pass them to `Tracer::relocate` in `Trace::relocate` and write the new address back.

`WordSlot` covers the common case of a NaN-boxed 64-bit word (see [Weak references and word slots](weak.md)).

## `Trace`

`#[derive(Trace)]` generates:

- `trace`, which reports every field that holds an edge;
- `relocate`, which rewrites those fields after evacuation;
- `NO_EDGES`, true when no field can hold an edge. Objects of such types are never pushed on the mark stack.

The derive refuses types that implement `Drop`. Destructors run in no particular order, so a `Drop` could see objects
that were already destroyed. `#[trace(unsafe_drop)]` opts out when the destructor touches no `ScopeLocal`.

Heap types must be `Send + Sync`: objects are traced and dropped on collector threads.
