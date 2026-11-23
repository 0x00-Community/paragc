# Processes, crashes and lifetime

A process may be killed at any instruction, including in the middle of a collection. The rules on this page keep the
heap readable and keep the other processes running.

## The attachment table

Every mapping takes one of 32 slots in the superblock page (at offset 1024). A slot holds:

- the process id;
- the process start time, read from `/proc/<pid>/stat`. A reused pid is never mistaken for the old process, and
  zombies count as dead;
- the number of its threads inside `mutate`;
- the number of its threads waiting at the door.

## The handshake

A collection request is one word holding the collector's slot (0 means none).

- A mutator increments its count and then checks the request.
- A collector raises the request and then checks every count.
- All of these are sequentially consistent, so at least one side sees the other.

Waiting is a futex on the shared word (not process-private). Waiters wake every 10 ms to check whether the other process
is still alive. A collector that was waited on by stalled mutators lets them in before taking the next collection, so
back-to-back collections cannot starve mutators.

## Crash recovery

- **A dead mutator** is not waited for. The next collector releases every entry its slot held and frees the slot, but
  never the slot that still owns the request: a newcomer reusing it would look like a live collector.
- **A dead collector** is taken over by whoever sees the request owned by a dead process, a mutator at the door or
  another collector. The collection is redone from scratch with an epoch newer than the one the dead collector started,
  because its marks must not count for this round. Sweeping again is harmless, since it only ran after marking had
  finished.
- **Every step leaves the heap readable**:
  - an object is written before its start bit (or a large object's block kinds) is published;
  - a large object's tail blocks are freed before its head, so an interrupted free is redone and skips the work already
    done;
  - the collector extends the block and entry tables up to their counters before reading them, in case a process died
    between bumping a counter and growing the file.

A process that dies may leave a few blocks or entries unusable (killed between taking one and recording it). Nothing it
left is ever read as an object.

## Owners and lifetime

A cross-heap belongs to the process that created it. The superblock records the owner (pid and start time) and a random
**incarnation**.

- When the owner exits or is killed, the heap dies with it unless `CrossHeap::persist` was called. Whoever notices first
  marks it dead and deletes the file and its marker. Memory goes back once every mapping is dropped.
- Other processes see `is_alive() == false`, and `try_mutate` returns `HeapDead`. Their handles stay safe to use and to
  drop.
- A `CrossHeapRef` stores its target heap's incarnation. Once a dead heap's number is reused, the reference reads `None`
  and collections do not follow it.
- `CrossHeapRoot::persist` makes a root that belongs to no process. It is found again by entry number and returned with
  `CrossHeapMutator::release`.

## Heap numbers

Numbers are claimed per directory through `.paragc-<k>` marker files. Every decision is taken under the marker's
`flock`, re-checked against the path after locking, so a half-created heap is never mistaken for a stale one and
simultaneous creators get distinct numbers.

## Several heaps at once: avoiding deadlock

Collectors and mutators take heaps like ordered locks:

- a group collection takes each heap in heap-number order and drains it before taking the next;
- `CrossHeap::mutate_with` enters two heaps lowest number first.

A mutator nesting in that order never waits on a heap that a collector holds while the collector waits on it. Nesting in
the other order still works: a collector that cannot drain within 50 ms releases what it holds, pauses for a random,
doubling interval (capped at 200 ms) and retries.

## Who collects

Any attached process can call `collect` or `collect_group`, except on a heap marked **owner-only**. A cross-heap attached
to a private heap as its old generation is owner-only (see [Promotion](promotion.md)).
