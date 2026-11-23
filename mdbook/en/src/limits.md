# Limits and open problems

This page lists what paragc does not do yet and the places where its design is under strain. Read it before relying on
cross-heaps.

## The private heap

- **No stack scanning.** Every object that must survive a collection is reachable from a `Root`, `Far` or entry.
- **128 MiB per object.** The header stores the size in 24 bits of granules.
- **`Slot` is two words**: the value and the value last counted for reference counting.
- **Concurrent young collections do not exist.** A young collection is a pause, short as it is.
- **Address-indexed structures** must stay young while they point at young objects (see [Young collections](young.md)).
- **One flaky test**: `gc::background_release_while_mutating` in release builds.

## Cross-heaps

- **Metadata that never dies.** The type table and pointer maps only grow, against [core principle 1](principles.md).

- **Stop-the-world only.** A cross-heap has no concurrent marking, young collections, reference counting or evacuation.
  `collect` waits for every live process's mutators to leave, so a process that is alive but stuck inside `mutate`
  stalls collection. Only dead processes are skipped.
- **No compaction.** Cross-heap objects never move, so fragmentation is only reduced by reusing lines and punching empty
  blocks.
- **64 windows.** Heap numbers are window numbers in a 1 TiB region, so one directory holds at most 64 live cross-heaps,
  and every heap is at most 16 GiB.
- **`CrossHeapRef` registrations** are only returned by group collections. Code that rewrites them often should run
  group collections.
- **Trust.** Every process that maps a heap can read and corrupt all of it. There is no per-object protection and no
  locking. Concurrent writers from several processes get no consistency guarantees; this is a deliberate choice,
  with thread semantics expected to come from the language above.
- **Dead processes** can leave a few blocks or entries unusable, if they were killed between taking one and recording it.
- **Stamps** collide with probability about 2⁻²⁷, so a stale weak reference or signature can very rarely resolve to a
  new object of the same type at the same address.

## Promotion

- **The fixed region is per process.** `reserve_fixed` maps 1 TiB at 64 TiB with `MAP_FIXED_NOREPLACE`. If something
  else already sits there, it fails, and a process can hold it only once.
- **Concurrent marking is off** while a cross-heap is attached, and young evacuation is forced to `Full`. A
  runtime that promotes gives up paragc's shortest pauses.
- **Promotion panics on a non-shareable edge.** A shareable object that reaches a non-shareable one through a traced edge
  makes the collector panic. That is a programming error, but it surfaces in the collector rather than at the write.
- **The owner-only cross-heap grows until a full collection.** Young collections promote into it but never free in it.
  Only `Heap::collect` reclaims cross-heap garbage, so a runtime that mostly runs young collections must still schedule
  full ones.
- **Anonymous cross-heaps are private.** `create_anonymous` is visible only inside its process. A runtime that wants
  other processes to read its objects must create a file heap and pass its descriptor.
- **A short window of invisibility.** A private object written into a cross-heap slot stays private, and reads as empty
  elsewhere, until the next young collection.

## The object model above paragc (j8)

Sharing a JS value means the receiver must understand the object's layout, not just its bytes. In j8 today:

| Part of a JS object | Where it lives | Shareable |
| --- | --- | --- |
| strings | GC heap, bytes in the object's tail | ✓ (since j8 #49) |
| shapes (property names, attributes, transitions) | `Arc<Shape>` outside the heap; objects keep a raw `*const Shape` | ✗ |
| property values (inline slots, the store) | GC heap | only once their shape is shareable |
| dictionary-mode properties | GC objects holding `Vec` and `HashMap` | ✗ |
| functions | bytecode and `FunctionInfo` outside the heap | ✗ |
| host objects | `Box<dyn Any>` | ✗; the owner acts for others through the mailbox |

**Shapes are the hard part.** Shape identity is a memory address:

- the executor's inline caches compare shape addresses and keep `Arc<Shape>` to stay valid;
- the JIT bakes shape addresses into machine code (`CheckShape`).

A shape that is an ordinary moving GC object would break both. The candidate designs:

1. ~~**Immortal shapes in a fixed place.**~~ **Rejected**: it breaks [core principle 1](principles.md), nothing is immortal.
2. **Shapes as moving GC objects.** Memory is reclaimed, but inline caches must hold traced references and the JIT must
   either stop baking addresses or recompile when shapes move.
3. **A second, shareable description made at promotion.** The hot path is unchanged, but every object carries one more
   word, and later property additions must be kept in sync on both sides.

A fourth design follows from the principles: **shapes as GC objects in a space that does not move**. Their address stays fixed, so caches and the JIT keep using it as identity, and they are still collected when unreachable. With a cross-heap attached, that space is the cross-heap itself, which never moves and is mapped at the same address in every process. paragc needs pinned allocation for this. The decision is open; it blocks stage 3 of cross-process objects in j8.
