# Marking and the thread pool

## Epochs

Marks are epochs, not bits. Each collection takes a new epoch number. An object is marked in this round when its mark
word equals the current epoch, and a line is live when its mark is the last finished epoch. Nothing ever has to be
cleared between collections.

## Parallel marking

1. The roots are spread over the workers.
2. A worker reaching an object swaps the epoch into its mark word. Whoever swaps first owns the object. It marks the
   lines the object spans and pushes the object on its own deque. Types without edges (`NO_EDGES`) are never pushed.
3. A worker pops an object and calls its `visit` function in tracing mode; each edge goes back to step 2.

Each worker has a crossbeam work-stealing deque. A worker that runs dry steals in batches from the others.

**Termination.** A worker whose deque is empty and that cannot steal counts itself idle and keeps trying. When every
worker is idle at once, marking is complete.

Deep structures never touch the call stack: a million-node chain is a million deque pushes, not a million frames.

## The pool

The pool starts at the first collection and lives as long as the heap.

- A collection is handed out by bumping one atomic word that holds a generation number and the number of participants.
  Only that many workers are unparked.
- Idle workers back off, then park.
- A panic on a worker is caught and raised again on the thread that called the collection.

## The tracer modes

The same `visit` function serves several purposes, selected by the `Tracer`:

| Mode | What `visit` does |
| --- | --- |
| tracing | calls `Trace::trace`; each edge is marked and pushed |
| relocating | calls `Trace::relocate`; each edge is rewritten to its forwarding address |
| asking (cross-heap) | answers whether the type is shareable and, if so, its type number in the cross-heap |

The asking mode lets the collector learn about a type from the object alone, without a type registry in the private
heap.
