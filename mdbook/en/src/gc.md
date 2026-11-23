# What a garbage collector is

A program allocates memory for its data: a list node, a string, an object. At some point the data is no longer needed
and the memory should be reused. Someone has to decide **when**.

- In C, the programmer decides: `malloc` gives memory, `free` gives it back. Free too early and the program reads memory
  that now belongs to something else (use-after-free). Forget to free and memory leaks. Free twice and the allocator's
  own bookkeeping breaks.
- In Rust, the compiler decides for most data: every value has one owner, and the value is dropped when the owner goes
  out of scope. This works as long as the data forms a tree. Graphs with cycles, or data shared without a clear owner,
  need `Rc`/`Arc` (reference counting) or an arena, each with its own limits.
- A **garbage collector** (GC) decides at run time, by looking at the data itself. Memory that the program can no longer
  reach is **garbage**; the collector finds it and reuses it. The program only allocates and never frees.

The program is called the **mutator**: from the collector's point of view, all it does is change (mutate) the graph of
objects. The collector is the other party, which reclaims what the mutator can no longer use.

## Reachability

A collector cannot know which objects the program will use in the future; that is undecidable in general. It uses a safe
approximation instead: **an object is live if the program could still reach it**.

- **Roots** are the places the program reads directly: local variables on the stacks, registers, globals, and, in a
  library like paragc, handles the program explicitly registers (`Root`).
- An object is **reachable** if a chain of pointers leads to it from a root.
- Everything else is garbage. No chain of pointers leads to it, so the program cannot touch it again, and reusing its
  memory is safe.

Reachable objects may still never be used again. That memory is a leak a GC cannot fix, but it is never a crash.

## The two families: tracing and reference counting

| | Tracing | Reference counting |
| --- | --- | --- |
| Idea | from time to time, start at the roots, follow every pointer, and mark what you reach; the rest is garbage | every object counts the pointers to it; at zero it is garbage |
| Work is proportional to | live data (marking) and heap size (sweeping) | the number of pointer writes |
| Cycles | collected naturally | never reach zero: leak unless another mechanism handles them |
| When memory comes back | at the next collection | immediately when the count drops |
| Cost on the mutator | small (barriers only for some designs) | every pointer write updates two counts |

Real collectors mix the two. CPython counts references and traces for cycles. LXR, which paragc follows, counts
references for old objects and traces now and then.

## Tracing collectors: what happens to the garbage

After marking, a tracing collector has three choices.

| Algorithm | What it does | Pros | Cons |
| --- | --- | --- | --- |
| **Mark-sweep** | walk the heap and put every unmarked object on a free list | objects never move | fragmentation; allocation searches free lists |
| **Mark-compact** | slide live objects together at one end of the heap | no fragmentation; bump allocation | several passes over the heap; every pointer must be updated |
| **Copying** (semispace) | copy live objects into an empty space while tracing; the old space is wholly free | one pass, work proportional to live data only; bump allocation | half the memory sits empty; long-lived objects get copied again and again |
| **Mark-region** (Immix) | free memory in units of fixed-size lines inside blocks; copy only when a block is sparse | bump allocation into free lines; little copying | an object can leave a partly used line unusable |

## Allocation

- **Free-list allocation** searches lists of free chunks by size: flexible but slow, and it fragments.
- **Bump allocation** keeps a pointer and a limit and moves the pointer forward: a compare and an add. It needs
  contiguous free space, which is why compacting, copying and region collectors are fast to allocate from.
- **Thread-local allocation buffers (TLABs)**: each thread bumps inside its own chunk, so allocation needs no
  synchronization at all.

## When the collector runs

| Style | Meaning |
| --- | --- |
| **Stop-the-world** | all mutator threads stop while the collector works. Simplest; the stop is a **pause** |
| **Parallel** | the collector uses several threads during a pause. The pause is shorter; the mutators are still stopped |
| **Incremental** | the collector works in small slices interleaved with the mutator, one thread at a time |
| **Concurrent** | the collector runs on other threads while the mutators keep running |

Stopping the mutators safely needs **safepoints**: places in the program where every thread's roots are known. Runtimes
like the JVM poll at loop back-edges and calls. paragc avoids them: Rust's borrow checker proves that no mutator is
inside the heap when a stop-the-world collection runs.

## Tri-color marking and barriers

Dijkstra and his colleagues described tracing with three colors:

- **white**: not reached yet (garbage, if it stays white);
- **grey**: reached, but its pointers have not been scanned;
- **black**: reached and fully scanned.

Marking ends when nothing is grey. When the mutator runs **during** marking (incremental or concurrent), it can hide an
object: it stores a pointer to a white object into a black one and deletes the only other path. The collector would never
see it and would free a live object. **Barriers** prevent this. A barrier is a few instructions the compiler or library
runs on every pointer write (write barrier) or read (read barrier):

| Barrier | What it does |
| --- | --- |
| **Insertion** (Dijkstra) | when a pointer is stored, shade the new target grey |
| **Deletion / snapshot at the beginning** (Yuasa) | when a pointer is overwritten, shade the old target grey. Everything reachable when marking began survives |
| **Read barrier** (Baker, Brooks, load barriers) | when a pointer is loaded, make sure it points at the current copy of a moved object |

Objects allocated during marking are usually made black right away (**black allocation**).

## Generations

The **weak generational hypothesis** says that most objects die young. A **generational** collector:

- splits the heap into a young generation (the nursery) and one or more old generations;
- collects the young generation often and cheaply (a **minor** or **young** collection), and the whole heap rarely (a
  **major** or **full** collection);
- moves objects that survive a few young collections into the old generation (**promotion**).

A young collection does not trace the old generation, so it must know about pointers from old objects to young ones. The
write barrier records them in a **remembered set**, or marks the containing chunk of memory dirty in a **card table**.

## Precise or conservative

- A **precise** (exact) collector knows exactly which words are pointers: the compiler emits stack maps, or the library
  keeps handles. It can move objects, because it can update every pointer.
- A **conservative** collector treats any word that looks like a pointer into the heap as one, typically on the stack,
  where it lacks type information. It cannot move an object that such a word points at, since that word might really be
  an integer.

## Moving or not

Moving objects (copying, compacting, evacuating) removes fragmentation and allows bump allocation, but it changes
addresses. Everything that holds an address must be updated, and nobody outside the collector's knowledge may keep a raw
address across a collection. Non-moving collectors are simpler to embed, especially in C++, where raw pointers are
everywhere.

## Finalization and weak references

- A **finalizer** (destructor) runs when the collector frees an object: to close a file, release a lock, free native
  memory. The collector decides when, and in no particular order, so a finalizer must not depend on other collected
  objects.
- A **weak reference** points at an object without keeping it alive. After a collection it either still points at the
  object or is cleared. Caches and observer lists use them.

## What a collector is judged on

| Measure | Question |
| --- | --- |
| **Throughput** | what fraction of time goes to the program rather than the collector? |
| **Latency** | how long are the pauses, and how often? |
| **Footprint** | how much memory beyond the live data does it need? |
| **Mutator overhead** | what do barriers and allocation cost the program? |
| **Scalability** | does it use all the cores, for allocation and for collection? |

No design wins on all five. Every collector in the next chapters makes a different trade between them.
