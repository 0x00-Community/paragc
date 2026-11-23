# Address space and file layout

## The address region and windows

`CrossHeapSpace::reserve` takes a range of **addresses**, not memory (`PROT_NONE` + `MAP_NORESERVE`):

- 64 windows × 16 GiB = 1 TiB, aligned to its own size;
- cross-heap number *k* is always mapped at window *k*;
- a process maps only the heaps it opened.

A pointer inside a cross-heap is an **offset from the start of the region**, so the same value names the same object in
every process, even across heaps. The base is found from any address in the region by masking the low bits.

### The fixed region

`CrossHeapSpace::reserve_fixed` reserves the region at `FIXED_BASE` = 64 TiB with `MAP_FIXED_NOREPLACE`. Every process
that does so maps heap *k* at the same absolute address, so plain absolute pointers (`ScopeLocal`, `Slot`) inside cross-heap
objects are valid everywhere. Promotion depends on this (see [Promotion](promotion.md)). A process can hold the fixed
region only once at a time.

## Inside a window

Every part sits at a fixed offset, so any process finds it without following a pointer:

```text
offset 0      superblock (one page): identity, owner, counters, stack heads, the attachment table at 1024
              type table (1024 entries: type key → type number, pointer map position)
              pointer maps (64 K offsets)
              entry table (one entry per 16 KiB of window)
              block table (state of every block)
              blocks (32 KiB each, handed out from a cursor)
```

The superblock holds a magic number (`paragcP5`), the owner, a random incarnation, the `persist` and `dead` flags, the
mailbox head, the mail count, the `owner_only` flag, and how far each part has been extended.

## Starting empty, growing on use

- A new heap's file is one page long: only the superblock.
- Every part is extended with `fallocate` when it is first used. `fallocate` only ever grows the file, so threads and
  processes can race on it safely.
- On tmpfs, `fallocate` takes memory right away, so memory is used up to the file length. The rest of the window costs
  nothing.
- A process entering `scope` compares its mapped length with the superblock and maps more when needed: one
  comparison, no system call in the common case.

## Giving memory back

Sweeping keeps empty blocks as spares, up to the amount allocated since the last collection. The rest are punched out:

- file heaps: `fallocate(PUNCH_HOLE | KEEP_SIZE)`;
- anonymous heaps: `madvise(MADV_REMOVE)`.

Runs of adjacent blocks are punched in one call. All-zero block metadata is a valid empty block, so punched blocks stay
on the free stack and are reused like any other. `CrossHeap::shrink` keeps no spares.

## Opening a heap

| Call | Use |
| --- | --- |
| `space.create(path, threads)` | a new heap in a file; its number is claimed in the file's directory |
| `space.create_anonymous(threads)` | a new heap with no file (anonymous shared memory), usable only inside this process; for tests, Miri and an isolate's own old generation |
| `space.open(path, threads)` | an existing heap by path |
| `space.open_fd(fd, threads)` | an existing heap from a file descriptor passed over a Unix socket (`SCM_RIGHTS`); no path needed |
| `heap.fd()` | the descriptor to pass (file heaps only) |
