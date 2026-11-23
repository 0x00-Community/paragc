# Blocks, lines and headers

## Sizes

| Constant | Value | Meaning |
| --- | --- | --- |
| `BLOCK` | 32 KiB | unit the heap hands out; aligned to its size |
| `LINE` | 256 B | unit of reuse inside a block (128 lines per block) |
| `GRANULE` | 8 B | allocation alignment; object sizes are whole granules |
| `LARGE` | 16 KiB | objects above half a block are allocated on their own |
| `LARGE_MMAP` | 64 KiB | large objects from here up get their own `mmap` |
| `HEADER` | 24 B | private object header |

Because blocks are aligned to 32 KiB, masking the low 15 bits of any object address gives its block.

## The block header

Each block keeps, outside its data area:

- **line marks**: one `u32` epoch per line. A line is free when its mark is older than the last finished epoch, so
  clearing marks is never needed; a new collection just uses a newer epoch;
- **the object-start bitmap**: one bit per granule, set at the first granule of every object. Sweeping and
  cross-heap lookups walk it to find objects;
- a state (free, being allocated into, recyclable, full, swept) and counters used by evacuation.

An allocation buffer collects the start bits of the objects it allocates in one bitmap word and publishes them with a
single atomic OR. It does that when it moves to another word, adopts another block, or when the `mutate` returns.
Nobody reads those bits earlier: the buffer owns its blocks until the next collection.

## Where blocks come from

On Linux:

- each heap maps 2 MiB chunks itself and cuts them into aligned blocks;
- a returned block first goes to a **warm pool** and keeps its memory, because the next round probably needs as many
  blocks;
- only when the warm pool is full is the block `madvise(MADV_DONTNEED)`ed, giving its memory back; its address then
  goes to a cold pool for reuse. Warm blocks are handed out first;
- the chunks are unmapped when the heap is dropped.

paragc bypasses glibc here: aligned allocations cost extra, and memory freed to glibc is not returned to the system.

On macOS, Windows and under Miri, blocks come from the global allocator.

## The object header

A private object starts with a 24-byte `Header`:

| Field | Bits | Meaning |
| --- | --- | --- |
| mark | 32 | epoch of the last collection that marked it; `FORWARDED` / `BUSY` bits during evacuation |
| info | 32 | size in granules (24 bits), reference count (6 bits), parity bit, large bit |
| visit | 64 | `unsafe fn(*mut u8, &mut Tracer)` for the type: traces or relocates its edges |
| drop | 64 | `Option<unsafe fn(*mut u8)>`, the destructor; after a move it holds the forwarding address |

The first two words (`Core`) are shared with cross-heap objects, whose 16-byte `PHeader` has a type number and a birth
stamp instead of function pointers. Blocks, lines, sweeping and marking therefore use the same code for both heaps.

A single object's data can be at most 128 MiB (24 bits of granules).
