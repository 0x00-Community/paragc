# Why object addresses change

From the outside, the first surprise with paragc is that the same object can have a different address in the next `mutate`. This chapter explains it from the user's side: what you see, when it happens, why paragc does it, and what you can and cannot keep across a collection.

## What you see

```rust
let root = heap.mutate(|m| m.root(m.alloc(Node { value: 1, next: None })));
let before = heap.mutate(|m| m.get(&root).as_ptr() as usize);
heap.collect_young();
let after = heap.mutate(|m| m.get(&root).as_ptr() as usize);
assert_ne!(before, after);            // usually true: the survivor was copied
assert_eq!(heap.mutate(|m| m.get(&root).value), 1);   // the object is the same
```

The `Root` still finds the object and the value is unchanged. Only the address moved.

## The life of one object

<div class="dg"><div class="dg-cap">One object, four addresses</div><div class="flow"><div class="step"><h5>Allocated</h5><span class="addr k5">0x7f3a_1c40_1040</span><p>young, in a block the thread bump-allocates into; mark = 0</p></div><div class="arrow">→</div><div class="step"><h5>Young collection</h5><span class="addr k2">0x7f3a_1c48_8a10</span><p>it survived, so it is copied into a compact block and becomes old</p></div><div class="arrow">→</div><div class="step"><h5>Full collection</h5><span class="addr k3">0x7f3a_1c5c_c230</span><p>its block became sparse, so evacuation copies it out</p></div><div class="arrow">→</div><div class="step"><h5>Promoted</h5><span class="addr k1">0x4008_0004_2a18</span><p>a cross-heap is attached and the type is shareable: copied into shared memory, where it never moves again</p></div></div></div>

## When paragc moves objects

| Event | What moves | Setting |
| --- | --- | --- |
| young collection | young survivors | `SkipRemembered` (default): all except those an old object or another heap's `Far` points at directly · `Full`: all · `Off`: none |
| stop-the-world full collection | live objects in sparse blocks (under half live) | always on |
| young collection with a cross-heap attached | shareable survivors, into the cross-heap | `alloc_shared` types only |

Never moved: anything during `mutate`; anything during concurrent marking or group collections; large objects (over 16 KiB); targets of `Far`; objects already in a cross-heap.

## Why move at all

Moving costs a copy, so paragc only does it when it buys something you notice:

- **Allocation stays a pointer bump.** Copying the survivors of a young collection together leaves whole blocks empty, and the next allocations bump through them without searching.
- **Memory stays bounded.** Without moving, a heap whose live objects are scattered keeps every half-empty block. In the fragmentation benchmark paragc ends at 111 MB RSS and cppgc, which does not move ordinary objects, at 445 MB for the same ~80 MB of live data.
- **Young garbage costs nothing.** Only survivors are touched; the dead are never visited.
- **Sharing stays lazy.** Promotion is a move: an object becomes visible to other processes by being copied into the cross-heap after it has proved it lives, so nothing has to decide at allocation time.

## How references follow the move

The collector leaves a forwarding address in the old copy and rewrites every reference it knows about:

<div class="dg"><div class="dg-cap">The old copy after a move</div><div class="bf"><div class="k7" style="--w:4"><b>mark</b><small>FORWARDED set</small><i>bit 31</i></div><div class="k2" style="--w:4"><b>info</b></div><div class="k3" style="--w:8"><b>visit</b></div><div class="k1" style="--w:8"><b>drop</b><small>→ new address</small></div><div class="dim" style="--w:20"><b>T</b><small>old data, never read again</small></div></div></div>

- **`Gc` handles** cannot exist during a move: they never leave `mutate`, and a moving collection takes `&mut Heap`.
- **Edges inside objects** (`Gc`, `Slot`, `WordSlot` fields) are rewritten through `Trace::relocate`, which `#[derive(Trace)]` generates.
- **Roots, weak references and `Far` entries** live in tables the collector owns, and are rewritten there.
- **Remembered slots** of old objects are rewritten in `Full` mode; in `SkipRemembered` mode their targets simply do not move.

## What you can keep across a collection

| Keep | Why it stays valid |
| --- | --- |
| `Root<B>`, `Weak<B>` | the collector rewrites their table slots |
| fields of heap objects | rewritten by `Trace::relocate` |
| the tag bits of a `WordSlot` word | only the low 48 bits change |
| `CrossHeapSlot`, `CrossHeapRoot::index`, `CrossHeapWeak`, `Signature` | they name cross-heap objects, which never move |
| values copied out of the object | they are yours |

## What breaks, and what to do instead

| Pattern | Why it breaks | Instead |
| --- | --- | --- |
| `as_ptr()` saved in a Rust struct, used in a later `mutate` | the object may have moved; the old address may now hold another object | keep a `Root` (or `Weak`) and call `m.get` |
| a hash map keyed by address | the key is stale after a move | hash the content or a stable id; if it must be the address, rehash in `relocate` (below) |
| comparing addresses from two different `mutate`s | the same object can have two addresses | compare inside one `mutate`, or compare a stable id |
| handing an address to C, the OS or another thread that outlives `mutate` | nobody rewrites it | copy the bytes out; a pinned space is planned (principle 1: it will still be collected) |
| addresses baked into generated code | the code is not an edge the collector knows | refer to the target through a handle and bake its id (j8's shapes), or make the code a traced object |

## In j8

- **`===` on objects** compares the NaN-boxed words. Both operands are read in the same turn, inside one `mutate`, so they agree.
- **`Map` and `Set`** index objects by address (`MapKey::Ref`). `Collection::relocate` recomputes the key of every object entry after a move and re-inserts it. That is the "rehash in relocate" pattern above.
- **Shapes** are compared in inline caches and baked into JIT code. They are GC objects referenced through [handles](handles.md): moving a shape rewrites only its handle entry, so the id that caches and machine code compare never changes.
- **Strings** move freely: nothing outside the heap keeps their address, and their bytes travel in the tail.
