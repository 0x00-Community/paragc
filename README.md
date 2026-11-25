<a id="top"></a>
<div align="center">

# paragc

### A parallel garbage collector for Rust.
**Immix-style mark-region heap, many mutator threads at once, concurrent and parallel marking and sweeping. No locks.**

[![Rust](https://img.shields.io/badge/Rust-100%25-orange?style=for-the-badge&logo=rust)](https://www.rust-lang.org)
[![Miri](https://img.shields.io/badge/Miri-clean-success?style=for-the-badge)](#-verification)
[![Lock-free](https://img.shields.io/badge/locks-none-blue?style=for-the-badge)](#-how-it-works)
[![License](https://img.shields.io/badge/License-MIT-brightgreen?style=for-the-badge)](#-license)

[![Heap](https://img.shields.io/badge/heap-mark--region%20%C2%B7%20evacuating-8A2BE2?style=flat-square)](#-how-it-works)
[![Mark](https://img.shields.io/badge/mark-parallel%20%C2%B7%20work%20stealing-262D3A?style=flat-square)](#-how-it-works)
[![std](https://img.shields.io/badge/std-required-283272?style=flat-square)](#-rules)

**🇺🇸 English** · [🇨🇳 简体中文](README.zh-CN.md) · [📖 The paragc book](https://paragc.app238.com)

</div>

---

## ⚡ What is paragc

paragc is **a tracing garbage collector for Rust programs that share one heap across many threads**. Objects live in
32 KiB blocks cut into 256 B lines (the Immix layout); threads bump-allocate into free lines on their own, and a
collection marks and sweeps on every core.

## 🧭 Core principles

These hold for every part of paragc. A design that breaks one of them is rejected, however convenient it is.

1. **Nothing is immortal.** Every object the heap holds is reclaimed once it is unreachable. That includes runtime metadata: a host's shapes, bytecode, type descriptions and caches. There is no permanent space, no "allocate and never free" arena, and no leak by design. If something must keep its address, it gets a space that does not move, and it is still collected.
2. **No locks.** Allocation, collection and coordination between threads and processes use atomics, lock-free structures and futex waits. Nothing takes a mutex.
3. **Exclusion by borrowing.** A stop-the-world collection takes `&mut Heap`, and a `ScopeLocal` cannot leave its `scope`. The borrow checker proves no mutator is inside, so there are no safepoints, no stack scanning and no conservative pinning.
4. **A process never waits for a dead one.** In shared heaps, a process killed at any instruction neither blocks the others nor leaves anything that is read as an object.

Where the code does not meet principle 1 yet:

- a cross-heap's **type table and pointer maps** only grow: a type stays registered after its last object dies (capacity 1024 types, 64 K pointer-map offsets);
- a process killed between taking a block or an entry and recording it leaves that block or entry **unusable for good**.

## 🔥 Why it's different

| | |
| --- | --- |
| 🧵 **Many mutators** | `Heap::scope(&self)` runs on any number of threads at once. Each bump-allocates in its own buffer (`Tlab`) and claims whole blocks with one atomic operation. |
| 🔒 **Exclusive by borrowing** | `Heap::collect(&mut self)` cannot overlap a `scope`: the borrow checker is the stop-the-world, so there is no safepoint protocol and no lock. |
| 🌊 **Concurrent marking** | `Heap::mark_concurrently(&self)` marks while mutators keep running (a snapshot-at-the-beginning barrier on `Slot` writes, new objects allocated black). Mutators wait only for a brief handshake at the start. |
| 🧹 **Concurrent sweeping** | `Heap::finish_marking(&mut self)` only finishes marking and hands the blocks to the sweeper; mutators sweep a block themselves when they need one (lazy sweeping) while `Heap::sweep_concurrently(&self)` sweeps the rest on the pool. With both, the pause is just draining the grey queue. Destructors run in parallel on all collector threads by default; `Heap::set_parallel_drop(false)` runs them only on the thread that called the collection / `sweep_concurrently` (finding the dead and recomputing lines stay parallel), for destructors that free memory the mutator thread allocated, where cross-thread frees make the allocator contend. |
| 🚧 **Allocation limit** | `Heap::set_limit(Some(bytes))` arms a limit on the bytes the heap holds (its blocks plus its large objects, `Heap::used`); the first block taken or large object made past it calls the `Heap::on_limit` hook once, on the allocating thread, and disarms the limit. Only taking blocks checks it, so allocation inside a block costs nothing more. Runtimes use it to stop JS that grows past a heap limit; cross-heap allocation is not counted. |
| 🏃 **Parallel collection** | A persistent pool per heap (threads park between collections). Marking: one crossbeam work-stealing deque per worker, stealing in batches. Sweeping, destructors included: blocks handed out in batches to the same workers. |
| 🏷️ **Branded handles** | `ScopeLocal<'heap, 'scope, T>` cannot escape the `scope` closure. `ScopeGlobal<'heap, 'heap, B>` handles carry no lifetime and can be stored anywhere and sent across threads. |
| 🪪 **Handles** | `Scope::alloc_handle` gives a `Handle<'heap, 'scope, T>` that points at an entry of the heap's handle table instead of at the object. Moving the object rewrites only the entry, so the handle, its `==` / `Hash` and `Handle::id` never change (no rehashing, ids can go into generated code); reading costs one extra load. Handles are strong edges stored with no barrier (`HandleSlot`); handled objects are freed by full collections. |
| 🪶 **Weak references** | `Weak<B>` is like `ScopeGlobal<'heap, 'heap, B>` (no lifetime, store it anywhere) but does not keep its object alive: every collection (full, young, concurrent-marking finish, group) clears the ones whose target was not marked, after marking and before sweeping, and moves the rest along with evacuated objects. `Scope::upgrade` gives an `Option<ScopeLocal>`; `Weak::is_cleared` works outside `scope`. |
| 🔣 **Word slots** | `WordSlot<'heap, 'scope>` holds a `u64` the user encodes (NaN-boxed values and the like): words at or above `WORD_POINTER_MIN` (`0xFFF8 << 48`) are pointers whose low 48 bits are an object's data address, and the top 16 bits are the user's own tag, kept as is through evacuation; every other word is plain data. Same barriers, counting and relocation as `Slot`; `word_pointer` decodes a word. |
| 🔗 **References between heaps** | `Far<'heap, 'scope, B>` lets an object in one heap point straight at an object in another heap of the same process: no copy, no indirection. Each heap still collects on its own; `Heap::collect_group` traces several heaps together to reclaim cycles that span them. |
| 🗄️ **Public heaps** (Linux) | A `CrossHeap` lives in a file on a shared-memory filesystem (`/dev/shm`, a tmpfs or hugetlbfs mount): everything — header, type table, entries, block table, blocks — is inside the file, at fixed offsets, and pointers are offsets, so several processes (anyone allowed to open the file) map the same heap at different addresses and use it at once. The file starts empty and grows as it is used. Objects carry a type number instead of code, and collection follows the type's pointer map. A process killed at any point — mutating, collecting — neither blocks the others nor leaks what it held. Public heaps point straight at each other (`CrossHeapRef`), and `CrossHeap::collect_group` reclaims cycles across them; heap numbers are unique per directory across processes, and a heap can outlive the process that created it. Private heaps can hold `CrossHeapGlobal<B>` handles into it. |
| 📦 **Opportunistic evacuation** | A stop-the-world collection moves the live objects out of sparse blocks while it marks (Immix defragmentation): one pass marks, copies and rewrites every edge. Every block under half live is a candidate, sparsest first, with no block cap; `collect_if_needed` runs a full collection early once blocks exceed "live + one round of allowed allocation" by 1.5×. Concurrent marking never moves anything, so mutators need no read barrier. |
| 🐣 **Young collections** | `Heap::collect_young` traces only objects allocated since the last collection, starting from the roots and the slots the write barrier logged, and sweeps only the blocks allocated into since; old objects are not touched. Surviving young objects move into compact blocks (`YoungEvacuation`, default `SkipRemembered`: objects an old object or another heap's `Far` points at directly are promoted in place, the rest move, and old objects are never written; `Full` also rewrites the remembered slots; `Off` moves nothing). `collect_if_needed` runs young collections and a full one every few rounds (the first stage of LXR-style young-object reclamation). |
| 🔢 **Reference counting** | Old objects carry a small reference count, maintained by the `Slot` write barrier (each slot logged once per round) and settled at young collections. The pause only applies increments and queues the decrements; `sweep_concurrently` frees objects whose count drops to zero on the pool, cascading, while mutators run, and their lines return to the allocator at the next pause. Tracing (a full collection) runs rarely, for cycles and objects whose count overflowed (LXR stages two and three). |
| 🛡️ **Checked at compile time** | `#[derive(Trace)]` rejects `Drop` on traced types, and object data aligned above 8 bytes is a compile error. |

## 🚀 Usage

```rust
use paragc::{Heap, ScopeGlobal, Slot, Trace, brand};

#[derive(Trace)]
struct Node<'heap, 'scope> {
    value: u64,
    next: Slot<'heap, 'scope, Node<'heap, 'scope>>,
}

// A ScopeGlobal has no `'scope`; the brand puts the reading scope's borrow back on
brand!(NodeB = for<'heap, 'scope> Node<'heap, 'scope>);

let threads = std::thread::available_parallelism().unwrap();
let sum = Heap::new(threads, |mut heap| {
    heap.scope(|s| {
        let list: ScopeGlobal<'_, '_, NodeB> = {
            let mut head = None;
            for value in 0..1000 {
                head = Some(s.alloc(Node { value, next: head.into() }));
            }
            s.scope_out(head.unwrap())
        };
        // Collecting borrows the scope mutably: every ScopeLocal is dead now, only roots survive
        s.collect();
        let mut sum = 0;
        let mut cur = Some(s.scope_in(&list));
        while let Some(n) = cur {
            sum += n.value;
            cur = n.next.get();
        }
        sum
    })
});
assert_eq!(sum, 999 * 1000 / 2);
```

## 🧠 How it works

| Part | Design |
| --- | --- |
| Blocks | Aligned to 32 KiB, so masking an object address finds its block. The block header holds line marks (one `u32` epoch per line) and an object-start bitmap (one bit per 8 bytes). An allocation buffer collects the start bits of the objects it allocates in the same bitmap word and writes them with one atomic OR when it moves to another word, adopts a new block, or a `scope` returns (nobody else reads them before: the buffer owns its blocks between collections, and background release only clears bits of old objects). |
| Allocation | A buffer owns one block and bumps through runs of lines whose mark ≠ the current epoch; when a run is too small it moves to the next one, then takes a block with free lines, then an empty block, then a new one. Objects larger than a line that do not fit the current run go to an overflow cursor (Immix overflow allocation): it looks at up to 8 available blocks for a long enough run, else opens a new block, and the current run stays for small objects. Objects over half a block (16 KiB) are allocated on their own, from 64 KiB with their own mmap. |
| Where blocks come from | Each heap maps 2 MiB segments itself and cuts them into 32 KiB-aligned blocks; returned blocks are `madvise(MADV_DONTNEED)`ed and their addresses reused from a lock-free pool, and the segments are unmapped when the heap drops (not glibc: aligned allocation costs extra, and freed memory is not given back). Other platforms and Miri use the global allocator. |
| Object header | 24 bytes: trace function, drop function, mark epoch (atomic), size in granules (top bit = large). |
| Marking | Roots are spread over the workers. Whoever swaps in the epoch first owns the object: it marks the lines it spans and pushes it to its own deque (types without edges are not pushed). |
| Termination | A worker whose deque is empty and that cannot steal counts itself idle and keeps stealing; when every worker is idle, marking is done. |
| Thread pool | Started at the first collection, lives as long as the heap. A collection is handed out by bumping one atomic word (generation + number of participants) and unparking only those workers; idle workers back off, then park. A panic on a worker is re-raised on the collecting thread. |
| Sweeping | Workers claim blocks in batches, walk the start bitmap, drop unmarked objects and classify each block as empty / recyclable / full. Empty blocks beyond what the last round used go back to the system. `Heap::shrink` (call it when idle) runs a full collection, keeps no empty blocks, and madvises away whole pages of free lines in recyclable blocks. |
| Young evacuation | Every block allocated into since the last collection is marked for evacuation. `SkipRemembered` first marks the targets of remembered slots and of entries in place (objects already marked this round never move); every other young object reached is copied into available or new blocks, leaving a forwarding address, and the root table and the edges of scanned young objects are rewritten (`Trace::relocate`). `Full` has worker 0 rewrite the remembered slots too. Reference counts are settled after marking, with the moved addresses. Young collections never scan old objects, so a structure that indexes objects by address must itself be young while it points at young objects (move it into a fresh object when it changes). |
| Roots | Each `ScopeGlobal<'heap, 'heap, B>` owns a slot in an append-only table; freed slot numbers are folded into a read-only array at collection time and claimed with an atomic cursor during `scope`. |
| Weak references | Each `Weak<B>` owns a slot in the weak table (the same slot table, not a root set). In every pause, once marking (and evacuation) is done and before sweeping, the table is walked: a moved target gets its new address, an unmarked one is cleared (by CAS, so a handle dropped at the same time is not revived); during `scope` the table only points at live objects. Live targets get a stuck reference count: release by count runs in the background, concurrently with mutators, too late to clear weak references first, so only tracing may free them. Sticking happens in the same pause that computes the decrements, before the background processes them; an old object left with only weak references is cleared by the next full collection. During concurrent marking `upgrade` is a read barrier (the write barrier's `remember`: mark it, push it grey); otherwise an object only weakly reachable at the snapshot, taken out and stored under a black object during marking, would be missed. |
| References between heaps | Creating a `Far` (inside both heaps' `scope`) claims a slot in the target heap's root table that also records the holder heap; finalizing the object that holds the `Far` (sweeping or a reference-count release) returns the slot. Collecting the target heap alone treats these entries as roots, so young collections, reference counting and concurrent marking need nothing new. Entries are pinned: before an evacuating collection marks anything, the blocks holding entry targets drop out of evacuation (and become copy targets), so a `Far` never needs rewriting. `collect_group` does not take entries held inside the group as roots: each heap marks from its own roots, a traced `Far` hands its target to the target heap (seeded with that heap's epoch), and the heaps take turns until no heap has grey objects left; entries held inside the group whose target stayed unmarked belong to dead holders and are cleared, so a later collection never seeds a freed object. |
| Public heap layout | `CrossHeapSpace::reserve` takes a range of addresses (address space only, no memory), aligned to its own size and cut into 16 GiB windows; public heap *k* is always mapped at window *k*. A window is laid out at fixed offsets: superblock, type table, pointer maps, the entry table, the block table, then 32 KiB blocks. The file is created empty (only the superblock page is written) and every part is extended with `fallocate` when first used — it only ever grows the file, so threads can race on it safely, On tmpfs `fallocate` takes memory right away, so memory is used up to the file length; the rest of the window takes none. |
| Public heap objects | A 16-byte header: the same mark and info words as a private object (so blocks, lines and sweeping are shared code) plus a type number. `#[derive(Shared)]` requires `#[repr(C)]` and generates a pointer map; the first allocation of a type registers its key (type name, size and map, hashed) and map in the heap. A `CrossHeapSlot` stores the target's offset from the start of the address range, so the same value is correct in every mapping; `get` and `set` take the `CrossHeapScope`, which knows this mapping's base. `alloc` rejects types with destructors at compile time. Objects over half a block (16 KiB) take a run of fresh blocks; when they die, the blocks become ordinary blocks. |
| Public heap collection | `collect` marks in parallel from the entry table (reading each object's pointer map, never calling its code) and sweeps in parallel; it does not move objects. Free blocks and free entries are lock-free stacks inside the file (tagged heads against ABA). `CrossHeapGlobal::index` is the entry number, valid in every mapping; `CrossHeapScope::find` turns it into a new handle after checking that the object's type key matches. Empty blocks found by the sweep are kept as spares up to the amount allocated since the last collection; the rest are given back to the system (a `fallocate` hole punch; `MADV_REMOVE` for anonymous heaps), one call per run of adjacent blocks. All-zero block metadata is a valid empty block, so they stay on the free stack. `CrossHeap::shrink` keeps none. |
| Weak references and signatures across heaps | Every object header has a 32-bit birth stamp (the heap identity and the allocation epoch, mixed). `CrossHeapWeak` is one word: the object's offset in the high bits, its stamp in the low bits. It registers nothing and keeps nothing alive; no collection looks at it, so even across processes it holds up no one. A read inside the target heap's scope checks that the block was opened, its table entry has memory, the start bit is set at that offset (or it is the start of a large object's first block) and the stamp matches; otherwise it is `None`, also when the slot holds a new object or the heap number has a new owner (stamps collide with probability about 2⁻²⁷). A `Signature` (32 bytes: that word, the heap identity and the type key) goes to another process, which opens the same heap (`CrossHeapSpace::open_fd` from a passed descriptor, no path needed) and calls `resolve` to get it by type or `verify` to check it without the type; a signature from an untrusted party is only read and never crashes the reader. |
| Promotion into a cross-heap (old generation) | `Heap::attach_cross_heap` attaches a cross-heap created by this process to a private heap as its old generation. The cross-heap must live in the fixed-address region (`CrossHeapSpace::reserve_fixed`, at 64 TiB in every process), so absolute addresses stored in objects are valid in every process and promoted objects keep their `ScopeLocal` / `Slot` / `WordSlot` fields (recorded in the pointer map as absolute-address edges). Objects from `Scope::alloc_shared` (types that are both `Trace` and `Shared`, without `Drop`) are copied into the cross-heap when they survive a young collection: the worker asks the object's visit function for its type number in the cross-heap, claims and copies it, writes the cross-heap header and forwards the old header; slots, roots and weak references in the private heap are moved to the new address. Marking, counting, the remembered set and the write barrier of the private heap skip cross-heap addresses. From then on only the owner collects the cross-heap (other processes and `collect_group` leave it alone): `Heap::collect` first runs a young collection to promote shareable objects, then a full private mark; cross-heap edges found by it and roots pointing into the cross-heap are the cross-heap's extra roots, and both are collected together; private weak references to dead cross-heap objects are cleared. Shareable objects may only point to shareable ones (promotion panics otherwise), and a private object written into a cross-heap object stays a private address (other processes read it as empty) until the next young collection promotes it and rewrites the slot through the remembered set (`Heap::promotion_pending` tells a host to run one when a turn ends). `CrossHeapScope::sign_promoted` signs a promoted object straight from a private `ScopeLocal`, without a brand. `Scope::alloc_tail` / `alloc_shared_tail` put a run of plain bytes right after the object (strings, buffers); the tail is copied with the object when it moves or is promoted. Concurrent marking is not supported while a cross-heap is attached. |
| Mailbox (the owner acts for others) | `CrossHeapScope::post` takes an entry owned by no process for the object, pushes it on the mailbox stack in the superblock, bumps the mail count (a futex word) and wakes the owner; `take_mail` takes everything, oldest first; `find` gets the object and `release` returns the entry when done. |
| Processes | Every mapping takes a slot in an attachment table inside the superblock page (32 slots): process id plus start time from `/proc/<pid>/stat`, so a reused pid is never mistaken for the old process, and zombies count as dead. Each slot has its own count of threads inside `scope`; the collection request is one word holding the collector's slot. A mutator increments its count and then checks the request, the collector raises the request and then checks the counts (all sequentially consistent), so at least one side sees the other. Waiting is a futex on the shared word (not process-private), waking every 10 ms to check whether the other process is still alive. A collector that was waited on by stalled mutators lets them in before taking the next collection. Entries record which slot holds them. |
| Crash recovery | A dead process inside `scope` is not waited for; the next collector releases every entry its slot held and frees the slot (never the slot that still owns the request, or a newcomer reusing it would look like a live collector). A collector killed half way is taken over by whoever sees the request owned by a dead process — a mutator at the door or another collector — and the collection is redone from scratch with a newer epoch than the one the dead collector started (its marks must not count as this round's); sweeping again is harmless, because it only ran after marking had finished. Each step a process can die in leaves the heap readable: an object is written before its start bit (or, for a large object, its block kinds) is published; a large object's tail blocks are freed before its head, so an interrupted free is redone and skips what was done; the collector extends the block and entry tables up to their counters before reading them, in case a process died between bumping a counter and growing the file. |
| References between public heaps | A `CrossHeapRef<'a, B>` lives in an object of public heap A (its brand) and stores the target's offset. Writing one registers an entry in B whose holder is "heap A", so collecting B alone keeps the target, and the process collecting B needs no access to A; collecting A alone does not follow it. `CrossHeap::collect_group` collects heaps mapped in one process together: entries held by heaps of the group are not roots, an edge into another heap of the group is marked with that heap's epoch and reported, and afterwards the group's registrations are replaced by one per reported (heap, target) pair — new ones claimed before old ones are released, so a collector killed in between leaves a superset. Stale registrations (from overwritten `CrossHeapRef`s) are only returned there. A group takes every heap's request in heap order and starts only when all have drained. |
| Waiting without deadlock | Collectors and mutators acquire heaps like ordered locks. A group collection takes each heap in heap-number order and drains it before taking the next, and `CrossHeap::mutate_with` enters two heaps lowest number first; a mutator nesting in that order never waits on a heap a collector already holds while the collector waits on it. Nesting in the other order still works: a collector that cannot drain within 50 ms releases what it holds, pauses for a random, doubling interval (capped at 200 ms) and retries. |
| Heap lifetime, numbers and restarts | A public heap belongs to the process that created it: when that process exits or is killed, the heap dies with it unless `CrossHeap::persist` was called. The superblock records the owner (pid and start time), a random incarnation, `persist` and `dead`; whoever notices first marks it dead and deletes the file and its marker, and memory goes back once every mapping is dropped. `is_alive` and `try_scope` (`HeapDead`) tell the other processes. A `CrossHeapRef` also stores its target's incarnation, so once a dead heap's number is reused it reads as `None` and collections do not follow it; an entry records its holder heap's incarnation, so a heap's registrations held by a dead heap in the same directory are returned at its next collection. Heap numbers are claimed per directory through `.paragc-<k>` markers, every decision taken under the marker's `flock` (re-checked against the path after locking), so a half-created heap is never mistaken for a stale one. `CrossHeapGlobal::persist` makes a root that belongs to no process, found again by entry number and returned with `CrossHeapScope::release`. |
| Stale buffers | A buffer remembers (heap, collection count); after a collection it starts over, because its block was re-classified. |
| Concurrent marking | Start: finish the previous sweep (it judges by "mark ≠ that epoch", so it must end before the next marking stamps live objects with a newer one), raise a request bit in the gate word (top bit; low bits count threads inside `scope`), wait until no mutator is inside, mark the roots grey, drop the request. paragc does not scan stacks, so the snapshot has to be taken when no mutator holds an unrooted `ScopeLocal`. Then mark on the pool. During marking, a `Slot` write marks the value it overwrites and puts it in the heap's grey queue (snapshot at the beginning), new objects get the marking epoch and their lines marked (black). A line is free when its mark is older than the last finished epoch, so lines the marker has just stamped are never handed out. `finish_marking` drains the grey queue in parallel. |
| Concurrent sweeping | `finish_marking` hands every block and large object to the sweeper and returns. Claiming is an atomic cursor, so each block is swept exactly once by whoever claims it, before it is handed out: a mutator that needs a block first takes a swept one, otherwise claims an unswept block and sweeps it on the spot; `sweep_concurrently` sweeps the rest on the pool and waits for blocks others are still sweeping. New objects carry the last finished epoch, so allocations during sweeping are never taken for dead. `collect` = `finish_marking` + `sweep_concurrently`. |
| Evacuation | Only in a stop-the-world `finish_marking` (no concurrent marking in progress). Candidates: blocks that have not been allocated into since they were swept and hold less than a quarter of a block of live data; at least 4, at most 1024. The other swept blocks become copy targets. While marking, the first worker to reach an object in a candidate block claims it (mark word: copying), copies it into its own copy buffer (the new copy is marked, with start bit and line marks) and leaves a forwarding address in the old header; others wait or take the new address. Every edge is rewritten through `Trace::relocate`; root slots with a CAS, so a handle dropped on another thread meanwhile stays dropped. Sweeping clears the old copies' start bits without running their destructors. |
| Young collections | Sticky mark bits: objects marked with the last finished epoch are old, new objects start at 0 and are young. A young collection marks with that same epoch, so an old object stops the trace and a reached young object is promoted. Old → young edges can only come from `Slot` writes, so outside concurrent marking the barrier logs a slot of an old object whenever the value written is young (slots of young objects are not logged, since a young collection reaches them through the object itself; once a collection has traced an object, its slots are marked old); during concurrent marking every new object is black and survives as old, so nothing is logged. Only blocks allocated into since their last sweep are swept; the log is cleared by every collection. |
| Reference counting | RC(x) = edges to x from old objects; edges from a young object are counted when it is promoted. The count lives in the header next to the size (6 bits, sticky when full, plus a parity bit: a stop-the-world full collection flips it and recounts every edge exactly while it marks). A `Slot` remembers the value it last counted; an old object's slot is logged on its first write in a round, and a young collection settles it (new value +1, last counted −1), adds 1 for each current root and takes 1 off the previous round's roots (deferred root counting). Decrements run after all increments; an old object reaching zero has its edges decremented, is finalized and its start bit cleared, and the lines of affected blocks are recomputed. Concurrent marking recounts too (see below). With a single marking worker nobody else touches the counts, so the recount uses plain loads and stores instead of locked atomic read-modify-writes. |
| Background release | `finish_young` (the pause) settles increments and pushes decrements onto a queue; `collect_young` = `finish_young` + `sweep_concurrently`. Workers pop a decrement; whoever brings a count to zero finalizes the object, clears its start bit, queues decrements for its edges and records the block as dirty. Objects at zero have no counted references, and every root and promoted young object was counted, so mutators cannot reach them. Dirty blocks get their lines recomputed from the old objects left at the start of the next pause; a block allocated into since its last sweep keeps that status and is left to the young sweep (it may hold unswept dead young objects). Every collection first finishes pending decrements, before a recount could flip the parity under them. `collect_if_needed` runs a full collection every 16 young ones while counts are trusted, every 4 otherwise. |
| Concurrent recount | The handshake flips the parity and drops the slots logged before it (their old values belong to the previous counts; live containers get rescanned). Markers count every edge they scan. A slot is logged on its first write in the round during marking too; a marker that recounts a slot rereads it afterwards and logs it itself if a mutator wrote it in between (a young object's slot is not counted yet, so the mutator would not have logged it). Objects allocated black are queued and their edges counted at the finish; logged slots are then settled (new value +1, value last counted −1, decrements only for objects marked this round, since the dead ones are about to be swept). Counts are trusted again as soon as the finish returns. |

Work-stealing deques and backoff come from [crossbeam](https://github.com/crossbeam-rs/crossbeam) (`crossbeam-deque`,
`crossbeam-utils`); the append-only segmented vector and the push-only list used for blocks and roots are paragc's own
(`src/sync.rs`).

## 📏 Rules

- Heap types implement `Trace` (use `#[derive(Trace)]`) and must be `Send + Sync`: objects are read from any thread
  and traced and dropped on collector threads. Mutable edges go in `Slot<'heap, 'scope, T>` (an atomic pointer); writing one
  takes the `Scope` (`slot.set(m, v)`), so the barrier can see concurrent marking.
- `ScopeLocal<'heap, 'scope, T>` is `#[repr(transparent)]` over `NonNull<T>`. `ScopeLocal::from_raw` turns a data pointer from `ScopeLocal::as_ptr`,
  taken in the same `scope`, back into a `ScopeLocal` (for example out of a NaN-boxed value). Raw addresses stored inside heap
  objects are invisible to the collector: the type's `Trace` must rebuild them with `from_raw`, report them through
  `Tracer::edge`, and in `relocate` pass them to `Tracer::relocate` and write the new address back.
- A `Slot` / `WordSlot` is two words (the value and the value last counted). A `WordSlot` word with a pointer tag must point at a live object of the same heap. A single object's data is at most 128 MiB.
- Objects can move during a stop-the-world collection. A `ScopeLocal` is only valid inside one `scope`, so this is invisible,
  but `ScopeLocal::as_ptr` may differ from one `scope` to the next. Hand-written `Trace` impls with edges must also implement
  `relocate` (the derive does it).
- Never call `mark_concurrently` from inside a `scope` on the same heap: it would wait for itself to leave.
- A type with `ScopeLocal` fields must not implement `Drop`: destructors run in no particular order, so a `Drop` could see
  objects that were already dropped. The derive checks this; `#[trace(unsafe_drop)]` opts out.
- Only objects reachable from a `ScopeGlobal` (or from a `Far` in another heap) survive a collection.
- A `Weak` does not keep its object alive: a young object with only weak references is cleared by the next young collection, an old one by the next full collection. Reading takes a `Scope` of its heap (`m.upgrade(&weak)`).
- A `Far` is created with `Far::new(holder, target, gc)` while inside both heaps' `scope`, can only be stored in the
  holder heap's objects (its brand), and is read with the target heap's `Scope` (`far.get(mb)`). It never changes;
  to point elsewhere, keep it in an object behind a `Slot` and replace that object. Edges within one heap use `Slot`.
- Cycles that span heaps are reclaimed only by `Heap::collect_group`.
- Public heaps (cross-heap and cross-process sharing) sit behind the `crossheap` feature and exist only on Linux; on other platforms the feature compiles to nothing and only the private heap is there. Their types use `#[derive(Shared)]`: `#[repr(C)]`, only plain data, `CrossHeapSlot`,
  nested `Shared` types and fixed-size arrays, no destructor. Public heaps do not yet have concurrent marking, young
  collections, reference counting or evacuation: `collect` is stop-the-world, and waits for every live process's
  mutators to leave.
- Every process that writes a heap can corrupt it: processes sharing a heap trust each other. A process that dies may
  leave a few blocks or entries unusable (killed between taking one and recording it); nothing it left is ever read as
  an object.
- A `CrossHeapRef` can only be stored in objects of the heap it is branded with and is read with the target heap's
  `CrossHeapScope`. Every write registers an entry in the target heap; only `collect_group` returns registrations that
  are no longer used, so code that rewrites `CrossHeapRef`s often should run group collections.
- A public heap dies with the process that created it (exit or kill) unless `persist` was called: other processes see
  `is_alive() == false` and `try_scope` returns `HeapDead`; their handles stay safe to use and drop until they let go.
- To work in two public heaps at once use `CrossHeap::mutate_with` (or nest in ascending heap number); nesting in the
  other order is correct but makes collectors back off and retry.
- A root released while a collection is running may keep its object alive until the next one.
- `std` only.

## ✅ Verification

| Test | What it covers |
| --- | --- |
| `gc_from_raw_round_trip` | `ScopeLocal` and `Option<ScopeLocal>` are one pointer wide; a `ScopeLocal` taken apart with `as_ptr` and rebuilt with `from_raw` is the same object |
| `rooted_list_survives` · `garbage_is_finalized` · `heap_drop_finalizes_everything` | Survival, exact finalization counts, dropping the heap drops everything |
| `threads_mutate_at_once` · `blocks_are_reused` · `large_objects` | Concurrent mutators, line reuse keeps the block count flat, large objects |
| `deep_binary_tree` | A 20-level complete binary tree (≈1M nodes): cut half, then all, exact counts |
| `deep_chain` | A 1M-node chain cut in the middle; depth never touches the call stack |
| `eight_threads_eight_heaps_crossed` | 8 threads × 8 heaps: roots held across heaps, checked and released on other threads, released while all heaps collect in parallel, cycles through large objects |
| `crossed_deep_trees` | 8 heaps, each with a deep chain and a 15-level tree whose leaves anchor into the next heap's chain; every cut matches the expected finalization count |
| `concurrent_marking_while_relinking` | Mutators keep unlinking nodes (leaving them only on their stacks) and relinking them elsewhere, allocating and discarding, while another thread marks concurrently; after every cycle the reachable count is exact, and finalization equals the discards |
| `concurrent_sweeping_while_relinking` | The same mutators across concurrent marking, the `finish_marking` pause and concurrent sweeping (they sweep blocks lazily while another thread runs `sweep_concurrently`); nothing still linked is ever finalized, counts stay exact |
| `evacuation_compacts_sparse_blocks` | Keep 1 object in 16: the next collection evacuates the sparse blocks; fewer blocks, values intact, addresses changed, finalization exact |
| `evacuation_with_shared_children` | Many parents share a few children (one `Slot` and one plain field each), so several workers race to claim the same child; both edges of every parent end up at the same new copy, nothing is finalized twice |
| `young_collection_frees_only_young_garbage` | Young objects hung off an old object's slot, off an old node and under another young node survive; a young collection finalizes exactly the discarded young objects (by tracing) and the discarded old ones (by reference count) |
| `young_evacuation_moves_survivors` | A young chain on an old holder's slot and one on a root (garbage in between): `Full` moves all of it and slot and root follow; `SkipRemembered` keeps the slot's direct target in place and moves the rest; `Off` moves nothing; values unchanged, and reference counting still frees everything afterwards |
| `medium_objects_overflow_without_wasting_holes` | Small and medium objects in turn while blocks only have one-line holes: the small ones fill the old holes and the new blocks are just enough for the medium ones |
| `shrink_keeps_objects_and_reuses_lines` | After `shrink` returns free lines to the system, live objects are intact and allocating into those lines again works |
| `crossing_calls_the_hook_once` · `arming_again_inside_a_scope` · `a_large_object_crosses_too` · `no_limit_no_hook` · `crossing_after_a_young_collection` · `crossing_while_sweeping_lazily` · `used_drops_when_blocks_go_back` | The allocation limit: one hook call per crossing, armed again from inside a scope, large objects count, nothing without a limit; old objects still count after a young collection and while a full one sweeps lazily; the count drops once blocks go back |
| `serial_drop_stays_on_the_caller` | With parallel drop off, objects freed by reference counting, young / full sweeps and heap drop are all destroyed on the caller thread, none missing |
| `promoted_slots_remember_young_values` | A holder promoted by a young collection before the first full one: a young leaf written into it afterwards survives the next young collection |
| `young_collections_while_relinking` | Mutators moving nodes between old and young objects, with young collections, concurrent marking finished by a young collection, and full collections in turn; counts exact every round |
| `background_release_while_mutating` | An old chain dropped at once: `finish_young` returns with nothing freed yet; the cascade then runs on the pool while mutators keep relinking, freeing nothing still linked; at the next pause the freed lines are reused |
| `concurrent_marking_recounts` | After one concurrent mark (mutators relinking, black objects hanging nodes under themselves) the counts are trusted at once: a dropped old chain is freed by count at the next young collection, and later rounds never free a linked node |
| `reference_counting_leaves_cycles_to_tracing` | Chains of old objects dropped at once are freed by their counts at a young collection (a 500-deep cascade); two-node rings stay until a full collection |
| `far_targets_are_pinned_during_young_evacuation` | Young evacuation (`Full`) does not move young objects another heap points at through `Far`; those held only by roots move |
| `far_targets_are_pinned_during_evacuation` | Sparse blocks, half of the survivors also held from another heap: the collection evacuates the other half, the held objects stay where they are (the root path and the `Far` see the same object), finalization exact |
| `weak_does_not_keep_alive` · `weak_inside_heap_objects` | Objects left with only weak references are collected and their weak references cleared, live ones read back their values; weak references stored in heap objects go with their holder |
| `weak_follows_evacuation` · `young_collection_clears_young_targets` | After evacuation weak and strong references see the same new copy and the old copy is not finalized; under all three young evacuation modes young objects with no strong reference are cleared and survivors read back their values |
| `weak_targets_wait_for_tracing` | An old object loses its last strong reference: without a weak reference it is released by count, with one it waits for a full collection |
| `upgrade_during_concurrent_marking_keeps_target` · `weak_churn_while_marking_concurrently` | An object only weakly reachable at the snapshot, taken out during marking and stored in another object, survives the finish (read barrier); 4 threads make weak references, take objects out and store them while another thread marks concurrently, with exact finalization counts every round |
| `survives_full_collections_and_cut_frees_the_rest` · `young_collection_*` · `concurrent_marking_while_rewriting` | `WordSlot`: tags and numbers unchanged through full, young (all three modes) and concurrent collections, the targets stay alive; replacing a pointer word with a number frees what it held, by count and by tracing |
| `group_collection_clears_weak` | An object reachable only through another heap's `Far`: not cleared by its own heap's collection, cleared by a group collection after the holder dies |
| `far_keeps_target_until_holder_dies` · `far_survives_holder_heap_drop_order` | A target with no root of its own survives full and young collections of its heap while the holder lives, and goes once the holder heap has finalized the holder; dropping the target heap first is safe |
| `cross_heap_cycle_needs_group_collection` · `rings_over_eight_heaps` | Rings across 2 and 8 heaps survive collecting each heap alone; a group missing some of the heaps keeps them (outside holders are roots); the full group frees exactly the unrooted rings, the kept rings stay walkable |
| `cross_heap_list_survives_and_garbage_is_reclaimed` · `pointer_map_covers_nested_fields_and_arrays` · `large_public_objects` | Public heaps: exact live bytes after collection, freed lines reused, pointer maps through nested fields and arrays, objects over 16 KiB intact and their blocks reused for small objects |
| `processes_share_one_heap` | 4 child processes and the parent push and pop their own chains and collect, all on one file; every chain has the shape and length its process reported, live bytes exact, the slots freed on exit |
| `killed_mutator_does_not_block_collection` | A child stops inside `scope` holding the only handle to a 10 000-node chain: a collection waits for it; `kill -9`, and the collection finishes, the slot is freed and the chain reclaimed |
| `killed_collector_is_taken_over` · `random_kills_while_processes_churn` | A child collecting in a loop is killed at random times (at least one kill lands mid-collection and is taken over) while the parent keeps a 300 000-node chain checked; 4 children churning are killed and replaced 30 times while the parent churns and collects; afterwards every chain is well formed and live bytes match exactly |
| `cross_heap_ref_keeps_target_until_group_collection` · `group_collection_returns_stale_registrations` | A target only reachable through a `CrossHeapRef` survives its heap's own collections, also after the holder died, until a group collection; 2 000 rewrites leave 2 000 registrations that one group collection returns, keeping exactly the current target |
| `cycles_across_public_heaps_need_group_collection` | Rings across 3 heaps survive collecting each heap and a group of 2; the group of 3 frees exactly the unrooted ring; the ring rooted only in the first heap survives later collections of each heap through the rebuilt registrations |
| `nested_mutates_with_collectors_do_not_deadlock` | One thread nests A then B, another B then A, while two single collectors and a group collector loop |
| `restart_after_the_creator_is_gone` · `heap_numbers_do_not_collide_across_processes` | Without `persist` the heap dies with its creator, whether it exits or is killed (opening it fails, file and marker gone); after `persist`, killed or exited, another process opens it, finds the chain by entry number, reaps the creator and releases the persistent root; heap numbers from different processes in one directory never collide and a stale number is reused |
| `references_to_a_dead_heap_do_not_crash` | The owner drops heap x: its file and marker go; another mapping sees it dead and `try_scope` refuses while its handles still work; x's number is reused by z, and a `CrossHeapRef` into old x reads `None` and is not followed by a group collection; registrations x held in y are returned at y's next collection |
| `eight_processes_half_die` | 8 processes, each with a public heap (500 000 nodes, a hub of `CrossHeapRef`s into every other heap) and a private heap (500 000 nodes, a handle into every other public heap), working in the background; 4 are killed; each survivor finds the dead heaps dead, drops its handles, reads every live peer, has exactly its own chain + hub + the gifts held by live peers left, and group-collects with them; the dead heaps' files and markers are gone. Under a 2 GB cgroup without swap: peak 353 MB with all 8 up, 182 MB after the kills |
| `simultaneous_creates_get_distinct_numbers` | 8 processes create heaps in one directory at the same moment, 5 times: all numbers distinct |
| `collect_fresh_file_heap_without_roots` | Collecting a new file heap that has no entries yet (nothing to extend) |
| `file_starts_empty_and_grows` · `two_mappings_of_one_heap` | The file is one page after creation and grows only as used; one heap mapped at two base addresses: handles found by entry number (and refused on a type mismatch), 4 threads per mapping relinking while collections alternate between the mappings, the creator dropped first |
| `weak_reference_across_heaps_does_not_keep_target` | A `CrossHeapWeak` in another heap does not keep its target: once rootless the target is collected (live 0) and reads `None`, still `None` after a new object takes the slot |
| `signature_round_trip_and_forgeries` | A signature survives the byte round trip, resolves by type and not by the wrong type; a changed stamp, heap identity or type key fails; moving the offset to every granule within 32 KiB passes only at object starts and never crashes; it fails after the object dies |
| `mailbox_delivers_in_order` | Three posts survive a collection, come out in posting order, and the heap is empty once they are released |
| `promotion_into_attached_cross_heap` | 5000 shareable nodes all move into the cross-heap in one young collection with values intact; a non-shareable object stays private with its slot moved to the new address; weak references follow; a signature finds the promoted object; collecting the cross-heap on its own does nothing; after the owner's full collection live bytes are exact, and once the head dies its private weak reference clears and the signature fails; finally everything is reclaimed; then a private young object written into a cross-heap object's slot stays private (invisible to other processes) until the next young collection promotes it and rewrites the slot. The two scenarios run in one test because a process can hold the fixed-address region only once at a time |
| `shrink_returns_empty_blocks_and_open_by_fd` | After 400 000 nodes of garbage, `shrink` brings the file's real usage under a quarter; a mapping opened from the descriptor resolves a signature; the returned blocks are reused correctly |
| `private_heap_holds_public_handles` · `eight_threads_churn_public_heap` | A `CrossHeapGlobal` inside a private-heap object keeps its target alive until the private heap finalizes it; 8 threads pushing and popping chains while another collects, live bytes exact every round |
| `eight_threads_relink_far_holders` | 8 threads × 8 heaps keep replacing holders that point into other heaps and reading them back, while another thread marks every heap concurrently; full, young and group collections in turn; every target finalized at the end, none early |

All of them also run under Miri with both Stacked Borrows and Tree Borrows (scaled down under `cfg(miri)`). Miri cannot
map files, so under Miri the public address range is an ordinary allocation (4 windows of 16 MiB): the public-heap tests
without a file run on it, and the two file tests are skipped.
`-Zmiri-ignore-leaks` is needed because crossbeam-epoch's global collector still holds deferred garbage at exit; at
4 collector threads per heap Stacked Borrows also reports a violation inside crossbeam-epoch itself, so the tests use 2
under Miri; the two concurrent `gc` tests and `far_targets_are_pinned_during_evacuation` /
`eight_threads_relink_far_holders` still hit that crossbeam-epoch violation under Stacked Borrows (inside a
crossbeam-deque push), and are clean under Tree Borrows:

```sh
cargo test --test gc -- --exact <name>          # also --test far · --test weak · --test word; with --features crossheap also --test cross_heap · --test processes
MIRIFLAGS="-Zmiri-permissive-provenance -Zmiri-ignore-leaks" cargo +nightly miri test --test gc -- --exact <name>
MIRIFLAGS="-Zmiri-permissive-provenance -Zmiri-ignore-leaks -Zmiri-tree-borrows" cargo +nightly miri test --test gc -- --exact <name>
```

## 📊 Benchmarks

Against cppgc (V8's Oilpan), gc-arena, gc (rust-gc) and dumpster on a 16-core machine; wall ms, best of 5. Setup,
workloads, every configuration and how to read them: **[benches/benchmark.md](benches/benchmark.md)**.

| | paragc | cppgc | gc-arena | dumpster | gc |
| --- | ---: | ---: | ---: | ---: | ---: |
| Pause, 1M live + 1M dead, 1 thread | 20.2 | 44.2 | **16.3** | **16.3** | 90.2 |
| Pause, 16 threads | **4.2** | 44.9 | – | – | – |
| Pause, concurrent marking + sweeping, 1 thread | **0.02** | – | – | – | – |
| GCBench, 1 thread | **50.3** | 121.0 | 118.0 | 97.8 | 294.6 |
| GCBench, 4 threads | **54.3** | 94.2 | – | – | – |
| Many mutators, 16 threads | **9.0** | – | – | 343.5 | – |
| Fragmentation (~80 MB live), RSS after / peak MB | **111 / 416** | 445 / 1894 | – | – | – |

Serially gc-arena and dumpster pause shorter (paragc's full collection also rebuilds reference counts); with threads,
and in throughput, paragc leads. cppgc's forced full
collection marks on one thread even with every parallel flag on, and pays an FFI call per allocation and edge here.

## 🗺️ Roadmap

Done: parallel stop-the-world collection, a persistent pool, concurrent marking, concurrent and lazy sweeping,
opportunistic evacuation, young collections, reference counting for old objects with decrements processed concurrently,
full tracing run only every 16 young collections, concurrent marking that recounts, a cheaper single-threaded
recount, and direct references between heaps of one process with group collection for cycles across them. Next: public
heaps in shared memory, used by several processes at once and surviving any of them being killed, pointing at each
other with group collection for cycles across them ([design](docs/cross-heap.zh-CN.md)). Other possible steps: public
heaps with concurrent marking, young collections and reference counting; a concurrent young collection, and a smaller `Slot` (it is two words to remember the
value last counted).

## 📜 License

MIT © Jabberwocky238

<div align="right"><a href="#top">⬆ back to top</a></div>
