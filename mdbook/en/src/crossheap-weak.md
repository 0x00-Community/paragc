# Weak references, signatures and the mailbox

These three mechanisms carry a reference across processes without making any process wait for another.

## Birth stamps

Every cross-heap object header has a 32-bit **birth stamp**: a SplitMix64 mix of the heap's incarnation and the
allocation epoch. Two objects that occupy the same address at different times almost certainly have different stamps.

## `CrossHeapWeak`

A `CrossHeapWeak<B>` is one word: the object's offset (in 8-byte units) in the high bits and its stamp in the low 27 bits.

- It registers nothing in the target heap and keeps nothing alive.
- No collection looks at it, so across processes it holds up no one.

Reading it inside the target heap's `mutate` checks, in order:

1. the block was opened, and its table entry has memory behind it;
2. the start bit is set at that offset, or the offset is the start of a large object's first block;
3. the stamp matches.

If any check fails the result is `None`. This covers a dead object, a new object in the same place, and a heap number
reused by a new heap. Stamps collide with a probability of about 2⁻²⁷.

## Signatures

A `Signature` is 32 bytes: the weak word, the heap's incarnation and the type key. It is the thing one process sends to
another.

```rust
// sender
let sig: Signature = cross.mutate(|cm| cm.sign(obj));
let bytes: [u8; SIGNATURE_BYTES] = sig.into();
send(&bytes, cross.fd());                  // e.g. over a Unix socket with SCM_RIGHTS

// receiver
let sig = Signature::from(&bytes);
let heap = space.open_fd(fd, threads)?;
let value = heap.mutate(|cm| cm.resolve::<NodeB>(&sig).map(|n| n.value));
```

| Call | Use |
| --- | --- |
| `sign(gc)` | sign an object in a cross-heap |
| `sign_promoted(gc)` | sign a promoted object straight from a private `Gc`, with no brand |
| `resolve::<B>(&sig)` | the object, if it is alive and of type `B` |
| `verify(&sig)` | check a signature without knowing the type |

A signature from an untrusted party is only read. A forged or stale one gives `None` and never crashes the reader.

## The mailbox: the owner acts for others

Some resources cannot be shared: file descriptors, `Box`es, anything private to the owner's process. Another process
asks the owner to act instead:

1. `CrossHeapMutator::post(obj)` takes an entry owned by no process, so the request survives the poster's death;
2. it pushes the entry on the mailbox stack in the superblock;
3. it bumps the mail count, a futex word, and wakes the owner.

The owner polls `CrossHeap::mail()`, the count received so far, or sleeps in `wait_mail(seen, timeout)` until it changes. It must not wait inside the heap's own `mutate`, or collections would wait for it forever. `take_mail` takes everything, oldest
first; `find` gets each object, and `release` returns its entry when done.

## Shrinking and descriptors

`CrossHeap::shrink` runs a collection that keeps no spare blocks, which brings the file's real usage down after a burst
of garbage. `CrossHeap::fd` gives the file descriptor to pass to another process. The receiver needs no path, and the
heap is mapped at the same window number in every process.
