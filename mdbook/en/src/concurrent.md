# Concurrent marking and sweeping

With concurrent marking, the only pauses are a short handshake at the start and draining the grey queue at the end.

## Start: the handshake

`Heap::mark_concurrently(&self)`:

1. finishes the previous sweep. The sweep judges objects by "mark ≠ that epoch", so it must end before marking stamps
   live objects with a newer epoch;
2. raises the request bit in the gate word. The top bit is the request; the low bits count threads inside `scope`;
3. waits until no mutator is inside. paragc does not scan stacks, so the snapshot must be taken while no mutator holds
   an unrooted `ScopeLocal`;
4. marks the roots grey and drops the request.

Marking then runs on the pool while mutators continue.

## During marking

- **Snapshot at the beginning.** Writing a `Slot` marks the value it overwrites and pushes it to the heap's grey
  queue. Every object reachable at the handshake is therefore found, even if a mutator unlinks it.
- **Black allocation.** New objects get the marking epoch and their lines are marked.
- **Read barrier for weak references.** `upgrade` marks and greys its target. Otherwise an object that was only weakly
  reachable at the snapshot could be taken out and stored under a black object, and be missed.
- **Line reuse.** A line is free only when its mark is older than the last finished epoch, so lines that the marker
  has just stamped are never handed out.

## End: the pause

`finish_marking(&mut self)` drains the grey queue in parallel, finishes reference counting (see below), clears dead weak
references and hands the blocks to the sweeper. `sweep_concurrently` then sweeps while mutators run.

## Recounting

Concurrent marking also rebuilds reference counts:

- the handshake flips the count parity and drops the slots logged before it;
- markers count every edge they scan;
- a marker that recounts a slot rereads it afterwards, and logs it itself if a mutator wrote it in between;
- objects allocated black are queued and their edges counted at the finish;
- logged slots are then settled.

Counts are trusted again as soon as the finish returns.

## Limits

- Concurrent marking never moves objects, so mutators need no read barrier for moved objects.
- Never call `mark_concurrently` from inside a `scope` on the same heap: it would wait for itself to leave.
- While a cross-heap is attached, concurrent marking is not supported.
