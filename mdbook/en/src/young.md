# Young collections

Most objects die young. `Heap::collect_young` traces only the objects allocated since the last collection.

## Sticky mark bits

There is no separate nursery. An object marked with the last finished epoch is **old**; a new object starts with mark 0
and is **young**. A young collection marks with that same epoch:

- an old object it reaches already carries the epoch, so the trace stops there;
- a young object it reaches gets the epoch and becomes old (it is promoted).

## The remembered set

An edge from an old object to a young one can only come from a `Slot` write. Outside concurrent marking, the write
barrier logs the slot of an old object whenever the value written is young.

- Slots of young objects are not logged: a young collection reaches them through the object itself.
- During concurrent marking every new object is black and survives as old, so nothing is logged.
- Every collection clears the log.

A young collection marks from the roots and from the logged slots. It sweeps only the blocks allocated into since
their last sweep.

## Moving survivors

Surviving young objects move into compact blocks. `Heap::set_young_evacuation` chooses how:

| Mode | Behaviour |
| --- | --- |
| `SkipRemembered` (default) | Objects that an old object (a remembered slot) or another heap's `Far` points at directly are promoted in place. The rest move; only young objects and roots point at them, and those are all relocated. Old objects are never written. |
| `Full` | Remembered slots are rewritten too, so every young survivor can move. Use it only when old objects keep the address nowhere but in a `Slot`. |
| `Off` | Nothing moves; survivors are promoted in place. |

## A rule that follows from this

Young collections never scan old objects. A structure that indexes objects by address must itself be young while it
points at young objects: when it changes, move it into a fresh object.

## Scheduling

`collect_if_needed` runs young collections and a full collection every few rounds:

- every 16 young collections while reference counts are trusted (`YOUNG_PER_FULL_RC`);
- every 4 otherwise (`YOUNG_PER_FULL`).

With a cross-heap attached, young evacuation is forced to `Full`, because promotion moves every shareable survivor (see
[Promotion](promotion.md)).
