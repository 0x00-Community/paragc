# Reference counting

Old objects carry a small reference count. Most old garbage is freed by its count, in the background, and full tracing
only runs every few rounds for cycles and overflowed counts. This follows LXR (Zhao, Blackburn & McKinley, 2022),
stages one to three.

## What is counted

RC(x) is the number of edges to x from **old** objects. Edges from a young object are counted when that object is
promoted.

The count lives in the header's info word:

- 6 bits, which stick when they reach the maximum;
- a parity bit. A stop-the-world full collection flips it and recounts every edge exactly while it marks; a count with
  the wrong parity is from the previous generation and starts again from 1.

## Maintaining counts

A `Slot` is two words: the value, and the value it last counted.

1. An old object's slot is logged on its first write in a round.
2. A young collection settles it: +1 for the new value, −1 for the value last counted.
3. Roots are counted lazily (deferred root counting): +1 for each current root, −1 for the previous round's roots.
4. Decrements run after all increments.

An old object reaching zero:

- has its edges decremented;
- is finalized;
- has its start bit cleared.

The lines of the affected blocks are then recomputed.

## Background release

The pause (`finish_young`) only settles increments and pushes the decrements onto a queue. `collect_young` is
`finish_young` + `sweep_concurrently`. On the pool:

- workers pop decrements;
- whoever brings a count to zero finalizes the object, clears its start bit, queues decrements for its edges and records
  the block as dirty;
- objects at zero have no counted references, and every root and promoted young object was counted, so mutators cannot
  reach them.

Dirty blocks get their lines recomputed from the old objects left at the start of the next pause. Every collection
first finishes pending decrements, so a recount never flips the parity under them.

## Cycles and weak targets

- Cycles never reach zero; the periodic full collection reclaims them.
- An object with a weak reference gets a stuck count. Release runs in the background, too late to clear weak references
  first, so only tracing may free such an object.

## Single worker

With one marking worker nobody else touches the counts, so the recount uses plain loads and stores instead of locked
atomic read-modify-writes.
