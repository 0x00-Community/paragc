# From there to paragc

paragc is assembled from ideas in the previous chapters, plus one that only Rust allows. This page maps each part of
paragc to where it comes from; the rest of the book explains each part in detail.

## What paragc takes

| Idea | Origin | In paragc |
| --- | --- | --- |
| Tracing from roots | McCarthy, 1960 | every collection marks from `ScopeGlobal`s, entries and remembered slots |
| Tri-color marking, SATB barrier | Dijkstra et al. 1978; Yuasa 1990 | concurrent marking with a deletion barrier on `Slot` writes and black allocation ([Concurrent](concurrent.md)) |
| Bump allocation in thread-local buffers | copying collectors; JVM TLABs | `Tlab`, one per thread in `scope` ([Allocation](allocation.md)) |
| Mark-region heap: blocks and lines | Immix, 2008 | 32 KiB blocks, 256 B lines, overflow allocation ([Layout](layout.md)) |
| Opportunistic evacuation | Immix | sparse blocks copied while marking ([Evacuation](evacuation.md)) |
| Generations, remembered sets | Lieberman & Hewitt; Ungar | sticky-mark young collections, slot logging ([Young collections](young.md)) |
| Parallel marking with work stealing | Endo, Flood et al.; JVM Parallel and G1 | crossbeam deques per worker ([Marking](marking.md)) |
| Reference counting for old objects, background release | Collins 1960; LXR 2022 | 6-bit counts settled at young collections, decrements on the pool ([Reference counting](refcount.md)) |
| Weak references | Lisp weak pointers; Java `WeakReference` | `Weak`, cleared in every pause ([Weak references](weak.md)) |
| A collector as a library for a host language | Boehm GC, cppgc | a Rust crate; j8 builds its JS heap on it |

## What paragc adds

**The borrow checker as the stop-the-world.** JVMs need safepoints and stack maps. cppgc needs conservative stack
scanning. paragc needs neither:

- a `ScopeLocal<'heap, 'scope, T>` is a borrow of the scope it came from, so no unregistered pointer outlives it;
- a stop-the-world collection takes `&mut` the heap or an exclusive scope, so the compiler proves no direct pointer is alive across it.

That is why paragc can **move** objects (evacuation, young copying) without read barriers and without conservative
pinning, as a plain library.

**No locks anywhere.** Many mutators allocate at once from one heap. Blocks, roots, weak references and work queues are
lock-free structures, and the thread pool is woken with one atomic word.

**Heaps in shared memory, across processes.** No collector in the previous chapters lets several processes use one heap
at once. paragc's cross-heaps do:

- objects without code pointers, collected by pointer maps;
- offsets that mean the same thing in every process;
- crash recovery when a process dies mid-collection;
- weak references and signatures that never make one process wait for another;
- promotion of a private heap's survivors into a cross-heap.

The rest of the book starts from the user's side ([Quick start](quickstart.md)), then goes through the private heap,
references between heaps, and the cross-heap.
