# The first garbage collectors

Garbage collection is almost as old as high-level programming. Most of the ideas used today were invented between 1960
and 1990.

## 1960: Lisp and mark-sweep

John McCarthy's Lisp, described in *Recursive Functions of Symbolic Expressions and Their Computation by Machine* (1960),
built all data from cons cells and gave the programmer no way to free them. When the free list ran out, the system:

1. started from the program's registers and variables;
2. followed every `car` and `cdr` pointer, setting a mark bit in each cell it reached;
3. swept through all of memory, putting every unmarked cell back on the free list.

That is **mark-sweep**, the first garbage collector, implemented on the IBM 704. It stopped the program for the whole
collection. The paper called the process "reclamation"; the name "garbage collection" came soon after.

## 1960: reference counting

In the same year George Collins published *A Method for Overlapping and Erasure of Lists*. Each cell counts the pointers
to it, and a cell whose count reaches zero is freed at once, together with whatever only it pointed to. No pause, but
every pointer change costs work, and cycles are never freed. Reference counting has lived on ever since: in Smalltalk-80,
Objective-C and Swift (ARC), CPython, C++ `shared_ptr` and Rust's `Rc`/`Arc`.

## 1963–1970: copying collectors

- Marvin Minsky (1963) collected a Lisp heap by copying the live cells out to secondary storage and back.
- Robert Fenichel and Jerome Yochelson (1969) split memory into two **semispaces**. Allocation fills one; a collection
  copies everything reachable into the other, and the roles swap.
- C. J. Cheney (1970) made the copy **breadth-first and non-recursive**: the to-space itself serves as the queue of
  objects still to scan, so no stack is needed.

Copying collection touches only live objects and leaves free memory in one piece, so allocation becomes a pointer bump.
The price is half the memory standing idle.

## 1970s: compaction

Mark-compact algorithms slide live objects to one end of the heap without a second space. The LISP 2 algorithm computes
new addresses in one pass, updates pointers in a second and moves objects in a third. Jonkers (1979) and others reduced
the passes with pointer threading. Compaction keeps allocation fast without the semispace's memory cost.

## 1975–1978: collecting while the program runs

- Guy Steele (1975) and Edsger Dijkstra, Leslie Lamport and colleagues (*On-the-Fly Garbage Collection*, 1978)
  described collectors that run **concurrently** with the program. Dijkstra's paper introduced the **tri-color**
  abstraction and the insertion write barrier.
- Henry Baker (1978) made copying **incremental** and real-time: the collector copies a little on each allocation, and a
  **read barrier** makes sure the program only ever sees the new copy of an object.
- Taiichi Yuasa (1990) published the deletion barrier that underlies **snapshot-at-the-beginning** marking.

## 1983–1984: generations

Henry Lieberman and Carl Hewitt (1983) observed that most objects die young and proposed collecting recent objects more
often. David Ungar's **Generation Scavenging** (1984), in Berkeley Smalltalk, made it practical: a small nursery is
copied often, survivors are promoted, and a remembered set records old-to-young pointers. Almost every production
collector since has been generational.

## 1988: conservative collection for C

Hans Boehm, Alan Demers and Mark Weiser built a collector for C and C++ programs that were never designed for one. It
scans stacks, registers and data segments **conservatively**: any word that looks like a heap address keeps that object
alive. It never moves objects. The Boehm GC is still used by GNU Guile, Mono (in the past), Inkscape and many language
runtimes.

## 1990s–2000s: towards the modern design

- **Parallel collectors** used several threads during the pause, with work stealing to balance load (Endo et al., 1997;
  Flood et al., 2001).
- **Mostly concurrent** mark-sweep (Boehm et al., 1991; Printezis & Detlefs, 2000) became the basis of the JVM's CMS.
- **Region-based heaps** let a collector reclaim or evacuate parts of the heap independently. They lead to G1 in the
  JVM.
- **Immix** (Stephen Blackburn and Kathryn McKinley, 2008) introduced the **mark-region** heap: blocks cut into lines,
  bump allocation into free lines, and opportunistic copying of sparse blocks. paragc's heap is an Immix heap.
- **LXR** (Wenyu Zhao, Stephen Blackburn and Kathryn McKinley, 2022) combined Immix with reference counting for old
  objects and occasional concurrent tracing, and showed low latency without giving up throughput. paragc follows its
  first three stages.

## The textbook

Richard Jones, Antony Hosking and Eliot Moss, *The Garbage Collection Handbook* (2nd edition, 2023), covers all of the
above in depth and is the standard reference.
