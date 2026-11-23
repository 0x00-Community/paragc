# Roadmap

## Done

| Version | What |
| --- | --- |
| up to 0.4 | parallel stop-the-world collection and a persistent pool; concurrent marking; concurrent and lazy sweeping; opportunistic evacuation; young collections; reference counting with background release; tracing every 16 young collections; concurrent recount; `Far` and group collections |
| 0.5 | cross-heaps on shared memory with several processes and crash recovery; `CrossHeapRef` and group collections across cross-heaps; heap numbers and restarts; heap lifetime tied to the owner; `WordSlot` and `Weak` |
| 0.6.0 | the `crossheap` feature (Linux only); the `Public*` API renamed to `CrossHeap*`; `CrossHeapWeak`, birth stamps and signatures; `open_fd`; the mailbox; shrinking cross-heaps; promotion into an attached cross-heap with owner-only collection; deferred promotion of private objects written into cross-heap slots; `sign_promoted` |
| 0.6.1 | `alloc_tail` / `alloc_shared_tail` |
| 0.6.2 | `Handle` / `HandleSlot` / `alloc_handle`: references through a handle table |
| 0.6.3 | `Handle::from_id` |
| 0.7.0 | scopes: `Heap<'heap>` / `Scope<'heap, 'life>` nesting and collecting in place, `ScopeLocal` (was `Gc`), `ScopeGlobal` (was `Root`), `Slot` (was `GcSlot`), `'heap` / `'xheap` brands |

## Next

- **Cross-process JS objects in j8**, stage by stage:
  1. an isolate takes a cross-heap and signs shared objects when a turn ends (done);
  2. strings in the GC heap (done);
  3. shapes and compiled functions in the GC heap, referenced through handles;
  4. ordinary objects and arrays;
  5. functions, with bytecode in the cross-heap;
  6. host objects, through the owner and the mailbox.
- **Cross-heaps with concurrent marking, young collections and reference counting**, so a large shared old generation
  does not mean long pauses.
- **Concurrent marking with a cross-heap attached.**
- **A concurrent young collection**, and a one-word `Slot`.
