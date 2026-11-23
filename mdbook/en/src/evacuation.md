# Evacuation

A stop-the-world collection can move live objects out of sparse blocks while it marks: Immix's opportunistic
defragmentation. Marking, copying and rewriting edges are one pass.

## Choosing blocks

- **Candidates**: blocks that have not been allocated into since they were swept and that hold less than half a block of
  live data (`EVACUATE_LIVE`). They are taken sparsest first, at least 4, with no upper cap.
- **Copy targets**: every other swept block.
- **Early full collections**: `collect_if_needed` runs a full collection early once blocks exceed "live data + one
  round of allowed allocation" by 1.5×.

## Copying

While marking, the first worker that reaches an object in a candidate block:

1. claims it by setting `BUSY` in its mark word;
2. copies it into its own copy buffer. The new copy is marked and gets its start bit and line marks;
3. writes the forwarding address into the old header (the `drop` word) and sets `FORWARDED`.

Other workers that reach the old copy either wait for `BUSY` to clear or follow the forwarding address.

## Rewriting edges

- Every edge is rewritten through `Trace::relocate`.
- Root slots are rewritten with a CAS, so a handle that another thread drops at the same time stays dropped.
- Weak references follow moved targets.
- The sweep clears the old copies' start bits without running their destructors.

## What is never moved

- Large objects.
- Anything during concurrent marking.
- Targets of `Far` references from other heaps: their blocks drop out of evacuation before marking starts. See
  [Far references](far.md).
- Objects that young collections in `SkipRemembered` mode promote in place. See [Young collections](young.md).

## Consequence for users

`Gc::as_ptr` may differ from one `mutate` to the next. Code must not keep raw addresses across a collection, except
through `Trace::relocate`.
