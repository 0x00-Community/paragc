# Far references and group collection

Several private heaps can live in one process: one per isolate, one per subsystem. `Far<'heap, 'scope, B>` lets an object in one
heap point straight at an object in another, with no copy and no indirection.

## Creating and reading

```rust
// inside both heaps' scope
let far = Far::new(&holder_m, &target_m, target_gc);
let holder = holder_m.alloc(Holder { far });
// later, reading needs the target heap's Scope
let t = holder.far.get(&target_m);
```

- A `Far` is branded with the **holder** heap, so it can only be stored in that heap's objects.
- Reading takes the **target** heap's `Scope`. Holding that mutator proves the target heap is open and that no
  collection of it is running.
- A `Far` never changes. To point elsewhere, put it in an object behind a `Slot` and replace that object.

## Entries: how the target heap sees it

Creating a `Far` claims a slot in the target heap's root table, which also records the holder heap. Finalizing the
object that holds the `Far` (by sweep or by reference count) returns the slot.

- **Collecting the target heap alone** treats these entries as roots. Young collections, reference counting and
  concurrent marking need nothing new.
- **Entries are pinned.** Before an evacuating collection marks anything, the blocks holding entry targets drop out of
  evacuation and become copy targets instead. A `Far` therefore never needs rewriting.

## Cycles across heaps: `collect_group`

A cycle A → B → A survives collecting A alone and B alone, because each side is an entry root in the other.
`Heap::collect_group(&mut [&mut Heap])` (and `CrossHeap::collect_group` for cross-heaps) collects several heaps together:

1. entries held from inside the group are not roots;
2. each heap marks from its own roots;
3. a traced `Far` hands its target to the target heap, seeded with that heap's epoch;
4. the heaps take turns until no heap has grey objects left;
5. entries held inside the group whose target stayed unmarked belong to dead holders and are cleared, so a later
   collection never seeds a freed object.

Group collections do not evacuate.

## Dropping order

`Far` holds an `Arc` of the target heap's root table, so dropping the target heap first and the holder later is safe.
Once the target heap is gone, its mutator cannot be obtained, so `get` can no longer be called.
