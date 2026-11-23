# Sweeping

Sweeping turns the marks into free space and runs destructors.

## The parallel sweep

Workers claim blocks in batches of 16. For each block, a worker:

1. walks the object-start bitmap;
2. for every object whose mark is not the current epoch, runs its destructor (unless it was moved; see
   [Evacuation](evacuation.md)) and clears its start bit;
3. classifies the block as **empty**, **recyclable** (some free lines) or **full**.

Empty blocks beyond what the last round used are returned to the system (`MADV_DONTNEED`); the rest stay as spares.

## Destructors and threads

By default, destructors run in parallel on every collector thread. `Heap::set_parallel_drop(false)` runs them only on
the thread that called the collection or `sweep_concurrently`. Finding the dead and recomputing lines stay parallel.
Turn parallel drop off when destructors free memory that the mutator thread allocated: frees from other threads make the
allocator contend.

## Lazy and concurrent sweeping

`finish_marking` only finishes marking. It hands every block and large object to the sweeper and returns. From then on:

- a mutator that needs a block first takes a swept one. Failing that, it claims an unswept block and sweeps it on the
  spot (lazy sweeping);
- `sweep_concurrently` sweeps everything left on the pool and waits for blocks others are still sweeping.

Claiming is an atomic cursor, so each block is swept exactly once, by whoever claims it, before it is handed out. New
objects carry the last finished epoch, so an allocation made during sweeping is never taken for dead.

`collect` is `finish_marking` followed by `sweep_concurrently`.

## Shrinking

`Heap::shrink`, called when the program is idle:

- runs a full collection;
- keeps no empty blocks;
- `madvise`s away the whole pages of free lines inside recyclable blocks.

Live objects are untouched, and the freed lines can be allocated into again afterwards.
