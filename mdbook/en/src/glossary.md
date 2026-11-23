# Glossary

| Term | Meaning |
| --- | --- |
| block | 32 KiB aligned unit of heap memory |
| line | 256 B unit of reuse inside a block |
| granule | 8 B, the allocation alignment |
| epoch | the number a collection writes into mark words and line marks; never cleared |
| mutate | `Heap::mutate`, a call during which the thread may allocate and use `Gc` handles |
| brand | the `'gc` lifetime tying a `Gc` to one `mutate`; also the `brand!` type mapping it back |
| root | `Root<B>`, a handle with no lifetime that keeps its object alive |
| entry | a root-table slot claimed for a reference from outside the heap (`Far`, cross-heap entries) |
| handle | `Handle<'gc, T>`, a reference to an entry of the handle table; moving the object rewrites the entry, never the handle |
| tlab | thread-local allocation buffer |
| overflow cursor | second allocation cursor for medium objects that do not fit the current run |
| black allocation | giving new objects the marking epoch during concurrent marking |
| SATB | snapshot at the beginning: the write barrier marks overwritten values during concurrent marking |
| evacuation | copying live objects out of sparse blocks while marking |
| forwarding address | the new address left in an old header after a move |
| young / old | not yet / already marked with the last finished epoch (sticky mark bits) |
| remembered set | slots of old objects that were written with young values |
| promotion | a young object becoming old: in place, by move, or into an attached cross-heap |
| RC | reference count of edges from old objects |
| parity | the bit that tells current counts from the previous generation's |
| `Far` | a direct reference from one private heap into another |
| group collection | collecting several heaps together to reclaim cycles across them |
| cross-heap | a heap in a shared-memory file, usable by several processes |
| region / window | the reserved address range for cross-heaps / one heap's 16 GiB slice of it |
| fixed region | the region at 64 TiB, the same address in every process |
| superblock | the first page of a cross-heap |
| attachment | a process's slot in a cross-heap's superblock |
| incarnation | a random identity a cross-heap gets at creation |
| birth stamp | 32-bit tag in a cross-heap header from the incarnation and the allocation epoch |
| type key | hash of a shareable type's name, size and pointer map |
| pointer map | offsets of the pointer fields of a shareable type |
| signature | 32 bytes (weak word, incarnation, type key) that identify a cross-heap object to another process |
| owner-only | a cross-heap only its owner may collect (an attached old generation) |
| mailbox | a stack in the superblock where other processes post objects for the owner to act on |
