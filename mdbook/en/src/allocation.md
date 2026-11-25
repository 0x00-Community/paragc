# Allocation

## The thread-local buffer

Each thread in a `scope` allocates through a `Tlab`: a cursor and a limit inside one block. An allocation that fits
bumps the cursor and writes the header; nothing else is shared.

When the current run of free lines is too small, the buffer tries, in order:

1. the next run of free lines in the same block (a line is free when its mark is older than the last finished epoch);
2. a recyclable block, one that the last sweep found with free lines;
3. an empty block;
4. a new block from the segment pool.

Taking a block is one atomic operation on a lock-free list.

## Overflow allocation

Objects larger than a line that do not fit the current run go to a second cursor, the **overflow cursor** (Immix
overflow allocation). It looks at up to `OVERFLOW_TRIES` (8) available blocks for a run long enough, otherwise it opens
a fresh block. The current run stays in place for small objects, so a medium object never throws away a good hole.

## Large objects

An object above `LARGE` (16 KiB) is allocated on its own:

- up to 64 KiB, from runs of blocks;
- from 64 KiB, with its own `mmap`.

Large objects are never moved. Sweeping frees them whole.

## Black allocation

During concurrent marking, a new object gets the current marking epoch, and its lines are marked as well. It is
allocated "black", so the marker never has to scan it. Its address goes to a list, so that its edges are counted at the
end of marking.

## Allocating with a tail

`Scope::alloc_tail(value, tail, fill)` allocates an object followed by `tail` bytes of plain data:

- the tail starts right after `T` (at `size_of::<T>()` from the data pointer), and `fill` writes all of it before the
  object can be reached;
- the collector never looks inside the tail, but copies it with the object when the object moves or is promoted,
  because the header size covers it;
- the type keeps the tail's length itself.

j8 stores the bytes of every JS string this way. `alloc_shared_tail` is the same for shareable objects (see
[Promotion](promotion.md)).

## Buffers across collections

A buffer remembers the heap and the collection count it belongs to. After a collection it starts over, because the
sweep may have reclassified its block.

## The allocation limit

`Heap::set_limit(Some(bytes))` arms a limit on the bytes the heap holds (`Heap::used`): its blocks (counted where a block
is taken from and given back to the heap's pages, so blocks the collector opens while evacuating count too) plus its large
objects. The count does not depend on which collection ran last, and it drops only when sweeping gives blocks back or
frees large objects. Taking a block for allocation (a swept one or a fresh one) or making a large object checks it; the
first one past the limit calls the hook set with `Heap::on_limit`, once, on the allocating thread, and the limit disarms
itself. Arming it again (`Scope::set_limit` works while mutators run) waits for the next block taken past it. Allocation
inside a block never looks at the limit, and with no limit armed the check is one atomic load per block.

The hook runs inside an allocation, so it must not touch the heap: it sets a flag that the runtime above acts on at a
safe point or at its next check. j8 uses it for a heap limit: the hook interrupts the running JS, which asks the
embedder's callback for a new limit and stops the JS when the callback does not raise it. Memory that objects own outside
the heap (a `Vec`, an `ArrayBuffer`'s bytes) is not counted, and neither is allocation in a cross-heap.
