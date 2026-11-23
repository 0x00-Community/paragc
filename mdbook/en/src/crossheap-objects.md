# Objects, types and pointer maps

Objects in a cross-heap can be read and collected by processes that never linked the code of their type, so they carry
no function pointers.

## Shareable types: `#[derive(Shared)]`

```rust
use paragc::{CrossHeapSlot, Shared, cross_heap_brand};

#[repr(C)]
#[derive(Shared)]
struct Node<'p> {
    value: u64,
    next: CrossHeapSlot<'p, Node<'p>>,
}

cross_heap_brand!(NodeB = for<'p> Node<'p>);
```

- `#[repr(C)]` is required, so the layout agrees across processes and binaries.
- Fields may be plain data, `CrossHeapSlot`, `CrossHeapRef`, `CrossHeapWeak`, nested `Shared` types and fixed-size
  arrays. Promotion adds absolute-address edges: `ScopeLocal`, `Option<ScopeLocal>`, `Slot`, `WordSlot`.
- No destructor: `CrossHeapScope::alloc` checks it at compile time.
- The derive generates a **pointer map**: the offset of every pointer field, through nested types and arrays.

## The type table

The first allocation of a type registers it in the heap's type table:

- the **type key** is a hash of the type name, its size and its pointer map;
- the entry records the type number, the size, and where the pointer map is stored.

A process that looks up a handle checks the type key, so a handle of the wrong type is refused rather than misread.

## The header

A cross-heap object has a 16-byte `PHeader`:

| Field | Meaning |
| --- | --- |
| mark, info | the same two words as a private header, so blocks, lines and sweeping are shared code |
| type | the type number |
| stamp | the birth stamp (see [Weak references and signatures](crossheap-weak.md)) |

Collection follows the pointer map of the type number; it never calls code.

## Handles

| Handle | Valid | Stores |
| --- | --- | --- |
| `CrossHeapLocal<'p, T>` | inside one `CrossHeap::scope` | an absolute address in this mapping |
| `CrossHeapSlot<'p, T>` | field of a cross-heap object | an offset from the region start, the same in every mapping |
| `CrossHeapGlobal<B>` | anywhere; in private heaps too | an entry number; holds the object alive |
| `CrossHeapRef<'a, B>` | field of an object in heap A, pointing into heap B | the target's offset and B's incarnation; registers an entry in B |
| `CrossHeapWeak<B>` | anywhere | one word: offset and stamp; holds nothing |

`get` and `set` on a `CrossHeapSlot` take the `CrossHeapScope`, which knows this mapping's base address.

## Entries

The entry table is the cross-heap's root set. Each entry records the target's offset and its **holder**:

- an attachment slot, meaning a process;
- `HEAP_HOLDER | k`, meaning another cross-heap;
- 0, meaning nobody: a persistent root.

`CrossHeapGlobal::index` is the entry number, valid in every mapping. `CrossHeapScope::find` turns an entry number back
into a handle after checking the type key; this is how another process picks up an object handed to it.

## Large objects

Objects above 16 KiB take a run of fresh blocks. When such an object dies, its blocks become ordinary blocks again.

## Collection

`CrossHeap::collect`:

- marks in parallel from the entry table, reading each object's pointer map, and sweeps in parallel;
- never moves objects. Other processes hold offsets and weak words, which must stay valid;
- keeps free blocks and free entries on lock-free stacks inside the file, with tagged heads against ABA.

Cross-heaps have no concurrent marking, young collections, reference counting or evacuation: `collect` is
stop-the-world and waits for every live process's mutators to leave.
