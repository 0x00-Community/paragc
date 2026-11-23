# Weak references and word slots

## `Weak<B>`

A `Weak<B>` is like a `ScopeGlobal<'heap, 'heap, B>`: it has no lifetime and can be stored anywhere. Unlike a root, it does not keep its
target alive.

```rust
let weak = heap.scope(|m| m.weak::<NodeB>(m.alloc(Node { value: 1, next: None })));
heap.collect();
assert!(weak.is_cleared());
assert!(heap.scope(|m| m.upgrade(&weak).is_none()));
```

How it works:

- Each `Weak` owns a slot in the heap's weak table, which is not a root set.
- In every pause (full, young, the end of concurrent marking, group collections), after marking and evacuation and
  before sweeping, the table is walked:
  - a moved target gets its new address;
  - an unmarked target is cleared, by CAS, so that a handle dropped at the same moment is not revived.
- During `scope`, the table only points at live objects. `Scope::upgrade` gives an `Option<ScopeLocal>`, and
  `Weak::is_cleared` works outside `scope`.
- Live targets of weak references get a stuck reference count (see [Reference counting](refcount.md)). A young object
  with only weak references is cleared by the next young collection, an old one by the next full collection.

Weak references can also be stored inside heap objects; they go with their holder.

## `WordSlot`

A `WordSlot<'heap, 'scope>` holds a `u64` that the user encodes, such as a NaN-boxed JS value:

- words at or above `WORD_POINTER_MIN` (`0xFFF8 << 48`) are pointers. Their low 48 bits are an object's data address,
  and their top 16 bits are the user's tag, kept as is through evacuation;
- every other word is plain data.

`WordSlot` has the same barriers, counting and relocation as `Slot`. `word_pointer` decodes a word. A tagged word must
point at a live object of the same heap.
