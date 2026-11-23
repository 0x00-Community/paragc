# Bit layouts

Every word paragc and j8 put in memory, bit by bit.

## A private object

An object is a 24-byte header, the data, and an optional tail. The total is rounded up to 8 bytes:
`(HEADER + size_of::<T>() + tail).next_multiple_of(8)`.

```text
offset 0   mark   u32   bits 0–29  epoch of the last mark (0 = young; ceiling MAX_EPOCH = 2^30 − 1)
                        bit 30     BUSY: a collector thread claimed it and is copying it
                        bit 31     FORWARDED: moved; the new address is in the drop word
offset 4   info   u32   bits 0–23  size in granules, header included (× 8 bytes; at most 128 MiB)
                        bits 24–29 reference count (sticks at 63)
                        bit 30     count parity (which generation of counts this one belongs to)
                        bit 31     large object
offset 8   visit  fn    traces, relocates or answers the type question (see Marking)
offset 16  drop   fn?   destructor, or none; after a move, the forwarding address
offset 24  data T, then the tail
```

`mark` and `info` together are `Core`, the part blocks, lines and sweeping look at.

## A cross-heap object

The header is 16 bytes and has no code pointers:

```text
offset 0   mark   u32   as above
offset 4   info   u32   as above (so the block code is shared)
offset 8   ty     u32   index in the heap's type table; collection follows that type's pointer map
offset 12  stamp  u32   birth stamp: SplitMix64 of the heap's incarnation and the allocation epoch
offset 16  data
```

When a private object is promoted, its 24-byte header is replaced by a 16-byte one, and the data and tail are copied
unchanged.

## Block metadata

For every 32 KiB block:

| Table | Size | Meaning |
| --- | --- | --- |
| line marks | 128 × `u32` = 512 B | epoch per 256-byte line; a line is free when its mark is older than the last finished epoch |
| start bitmap | 4096 bits = 512 B | one bit per 8-byte granule, set at the first granule of each object |

## Edges and words

| Thing | Layout |
| --- | --- |
| `Gc<'gc, T>` | one pointer to the data (after the header) |
| `Slot<'gc, T>` | two words: the current value, and the value last counted for reference counting |
| `WordSlot<'gc>` | two words. A value word ≥ `WORD_POINTER_MIN` (`0xFFF8 << 48`) is a pointer: top 16 bits are the user's tag (kept through moves), low 48 bits the data address. Any other word is plain data |
| `CrossHeapSlot` | one `u64`: offset from the start of the cross-heap region, the same in every process |
| `CrossHeapWeak` | one `u64`: high 37 bits are the offset ÷ 8 (a 1 TiB region needs 40 bits, minus 3 for alignment), low 27 bits the stamp |
| `Signature` | 32 bytes: the weak word (8), the heap incarnation (8), the type key (16) |

## j8's JS values

A JS `Value` is one 64-bit word, NaN-boxed:

- a double is stored as is (NaNs are canonicalised to `0x7FF8_0000_0000_0000`);
- every other value is `0xFFF0 << 48 | (tag + 1) << 48 | payload`.

| Top 16 bits | Kind | Payload |
| --- | --- | --- |
| `0xFFF1` | undefined | – |
| `0xFFF2` | null | – |
| `0xFFF3` | boolean | 0 or 1 |
| `0xFFF5` | uninitialised binding (TDZ) | – |
| `0xFFF6` | lazy value | index |
| `0xFFF8` | string | address of a `JsStr` |
| `0xFFF9` | symbol | address |
| `0xFFFA` | bigint | address |
| `0xFFFB` | object | address of an `Object` |
| `0xFFFC` | accessor pair | address |
| `0xFFFD` | context (closure environment) | address |

Every kind that points into the heap has a top half ≥ `0xFFF8`, which is exactly the range `WordSlot` treats as a
pointer. A JS value stored in a `WordSlot` (j8's `ValueCell`) is therefore an edge the collector traces, counts and
relocates, and the tag survives the move.

## Example: a JS string

`JsStr` is 32 bytes of fields followed by its text in the tail:

```text
private header 24 B
JsStr          32 B  buf    8  none: the text is in my tail; some: a view into another string's buffer
                     len    4  UTF-8 bytes
                     units  4  UTF-16 code units (equal to len exactly when the text is ASCII)
                     start  4  start in the buffer (views)
                     cap    4  tail capacity for UTF-8 (spare room for appending)
                     used   4  bytes of the buffer claimed so far (atomic; appending claims with a CAS)
                     pad    4
tail           cap B UTF-8, then (non-ASCII only) padding to 2 and units × u16 of UTF-16
```

A 40-byte ASCII string is 24 + 32 + 40 = 96 bytes = 12 granules, and the low 24 bits of `info` hold 12. Promoted into a
cross-heap, the same string is 16 + 32 + 40 = 88 bytes.
