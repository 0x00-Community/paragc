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
| `Props.shape` | a `HandleSlot`: the shape's handle id, a strong edge | ✗ (handles belong to one private heap) |
| a prototype's `shape_root` | a `HandleSlot` to the root shape of objects with that prototype | ✗ |
| the `Shape` itself: property names, attributes, transitions, hash index | a GC object from `alloc_handle`; names are `SmallStr`; transitions are paragc `Weak`s | ✗ (it has a destructor) |
| dictionary-mode properties | GC objects holding `Vec` and `HashMap` | ✗ |
| strings | GC heap, text in the object's tail | ✓ |
| functions | bytecode, inline caches and literal shapes in a GC object per function (`Loaded`) | ✗ |
| host objects | `Box<dyn Any>` | ✗; the owner acts for others through the mailbox |

A shape is the object's layout: which property name sits in which slot, with which attributes. Objects with the same
properties added in the same order share one shape.

## How shapes live in the heap

1. **Identity is the handle id.** Inline caches compare shape identities, and the JIT bakes them into machine code
   (`CheckShape`). A shape is allocated with [`alloc_handle`](handles.md), and the object's shape word holds its handle
   id. When paragc moves the shape it rewrites only the handle entry, so the id, the caches and the machine code stay
   valid. The JIT never dereferences the word; it only compares it.
2. **Who keeps a shape alive.** Objects (through `Props.shape`), prototypes (`shape_root`), and the inline caches,
   literal caches and JIT code of a function, all held in that function's `Loaded` object. Transitions from a shape to
   its children are weak: a child nobody uses is collected, and adding the same property later makes a new one.
3. **Growth.** Transitions sit in a `Vec` that drops cleared entries whenever a child is added; the hash index is built
   lazily; property names are shared with the parent through an `Arc<Prefix>`. All of this is malloc'd memory that the
   shape's destructor frees when the shape dies.
4. **Compiled code is collected too.** Each function's `Loaded` is a handle object; closures, module records and
   paused frames hold it as a traced `Code`, and the functions of one module hang from one `Unit`. The executor's table
   from function ids to code holds only weak references.

## What that means for sharing

Shapes are GC objects now, but still private: handles exist only in private heaps, and a `Shape` has a destructor.
When an object is promoted into a cross-heap, its values and strings arrive intact while its shape word still names an
entry of the sending process's handle table. The next stage gives promoted objects a shape that lives in the
cross-heap, so that another process can tell which property each slot holds. See
[Limits and open problems](limits.md).
