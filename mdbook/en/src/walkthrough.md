# Sharing a JS object, step by step

This page follows one JS object from the process that creates it, through a broker, to another process, and shows which
parts of a j8 object can make the trip today. The example is the goal behind cross-heaps: an acdes process sends a JS
object to dowin, and dowin forwards it to another acdes without copying.

## The steps

1. **Allocate.** JS allocates the object in the isolate's private heap. If its type is shareable (`alloc_shared`), it is
   still an ordinary young object at a private address. Nothing decides at allocation time which heap it will live in,
   so allocation stays lazy.
2. **Mark it for sharing.** The host calls `scope.shares.share(mc, gc)`, which records a ticket and moves nothing.
3. **Promote at the end of the turn.** j8 never collects in the middle of running JS. When `Isolate::enter` returns,
   j8 runs one young collection if anything is pending. Shareable survivors, tails included, are copied into the
   attached cross-heap:
   - the cross-heap sits at 64 TiB + 16 GiB × heap number, the same address in every process;
   - the old place gets a forwarding address, and every private slot, root and weak reference is moved to the new one.
4. **Sign.** `sign_promoted` produces the 32-byte signature: offset and stamp, heap incarnation, type key.
5. **Send to the broker.** The signature and the heap file's descriptor go over a Unix socket. dowin opens the heap with
   `open_fd`, which maps it at the same window, and calls `resolve`. That checks the block, the start bit, the stamp and
   the type key, and hands back a pointer into shared memory. dowin reads the object in place.
6. **Forward.** dowin passes the same signature, and the descriptor if needed, to the other acdes, which repeats step 5.
   Nobody copied the object.

Still missing:

- the dowin side is not written yet;
- j8's test uses an anonymous cross-heap, which other processes cannot open. Real use needs a file heap and its
  descriptor.

## Where the parts of a j8 object live

| Part | Where | Shareable today |
| --- | --- | --- |
| `Object` shell (prototype, call target, class bits) | GC heap | depends on the rows below |
| `Props.inline` (first 4 property values) | inside the object, one NaN-boxed word each | ✓ |
| `Props.store` (properties 5 and up) | GC heap | ✓ |
| `Props.shape` | **a raw pointer to malloc'd memory**, invisible to the collector | ✗ |
| `Props.anchor`, a prototype's `shape_root` | GC objects holding an `Arc<Shape>` | ✗ (they have a destructor) |
| the `Shape` itself: property names, attributes, transitions, hash index | `Arc` outside the heap; names are `SmallStr` (short ones inline, long ones `Arc<str>`) | ✗ |
| dictionary-mode properties | GC objects holding `Vec` and `HashMap` | ✗ |
| strings | GC heap, text in the object's tail | ✓ |
| functions | bytecode and `FunctionInfo` outside the heap | ✗ |
| host objects | `Box<dyn Any>` | ✗; the owner acts for others through the mailbox |

A shape is the object's layout: which property name sits in which slot, with which attributes. Objects with the same
properties added in the same order share one shape.

## Why shapes are outside the heap

1. **Identity is the address, and paragc moves objects.** The executor's inline caches compare shape addresses, and
   the JIT bakes them into machine code (`CheckShape`). paragc's young collections and evacuation change object
   addresses. A malloc'd `Arc` never moves, so its address is a stable identity.
2. **A shape is a shared cache that grows.** Adding a property appends a child to the shape tree, lock-free, through
   `&self` (`Chain`, `Prefix`). A shape with many properties builds a hash index lazily (`OnceBox<HashMap>`). Both need
   growable malloc'd memory, at moments where no allocation context (`mc`) is at hand. As GC objects they would need
   `Box`es and destructors, and types with destructors cannot be shared.
3. **The GC still decides when they die.** A prototype's `shape_root`, and the `anchor` of an object whose shape is not
   in its prototype's tree, are GC objects that hold the `Arc`. When they die the count drops and the shapes go.
4. **History (inferred).** When j8 moved its heap to paragc (11-17, eb308d0), shapes kept their earlier `Arc`-based
   design. Sharing across processes was not a requirement then.

## What that means for sharing

When an object is promoted into a cross-heap, its values and strings arrive intact, but `Props.shape` still points into
the sending process's malloc heap. Another process sees the values in each slot without knowing which property name each
slot belongs to. The `Anchor` also carries a destructor, so it cannot be promoted at all. That is why stage 3 of the
cross-process work, shapes, has to come before ordinary objects can be shared. The candidate designs are listed in
[Limits and open problems](limits.md).
