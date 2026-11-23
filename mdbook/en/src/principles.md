# Core principles

These hold for every part of paragc. A design that breaks one of them is rejected, however convenient it is.

1. **Nothing is immortal.** Every object the heap holds is reclaimed once it is unreachable. That includes runtime metadata: a host's shapes, bytecode, type descriptions and caches. There is no permanent space, no "allocate and never free" arena, and no leak by design. If something must keep its address, it gets a space that does not move, and it is still collected.
2. **No locks.** Allocation, collection and coordination between threads and processes use atomics, lock-free structures and futex waits. Nothing takes a mutex.
3. **Exclusion by borrowing.** A stop-the-world collection takes `&mut Heap` (or `&mut` an exclusive scope), and a `ScopeLocal` is a borrow of its scope. The borrow checker proves no direct pointer is alive across it, so there are no safepoints, no stack scanning and no conservative pinning.
4. **A process never waits for a dead one.** In shared heaps, a process killed at any instruction neither blocks the others nor leaves anything that is read as an object.

Where the code does not meet principle 1 yet:

- a cross-heap's **type table and pointer maps** only grow: a type stays registered after its last object dies (capacity 1024 types, 64 K pointer-map offsets);
- a process killed between taking a block or an entry and recording it leaves that block or entry **unusable for good**.
