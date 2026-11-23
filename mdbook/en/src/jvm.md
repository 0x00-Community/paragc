# The JVM's collectors

The Java Virtual Machine is where garbage collection was industrialised. A JVM runs many threads, allocates at enormous
rates, and serves both batch jobs that want throughput and servers that want short pauses. Instead of one collector,
HotSpot (OpenJDK's JVM) offers several, chosen with a command-line flag. Together they show the whole design space.

## Common machinery

| Mechanism | What it is |
| --- | --- |
| **Precise roots** | the JIT emits stack maps; at a safepoint every pointer on every stack is known |
| **Safepoints** | compiled code polls at loop back-edges and method returns; a collection stops all threads at safepoints |
| **TLABs** | each thread bump-allocates in its own thread-local allocation buffer inside the young generation |
| **Card table** | the old generation is divided into 512-byte cards; the write barrier dirties a card when a pointer is stored into it, so young collections only scan dirty cards |
| **Generations** | a young generation (Eden + two survivor spaces, copied) and an old generation |

## Serial

One thread does everything while all mutators stop. The young generation is copied (Eden and one survivor space into the
other survivor space or the old generation); the old generation is mark-compacted. It is the right choice for small
heaps and single-core machines, and it still runs in containers with little CPU.

## Parallel (the throughput collector)

The same algorithms as Serial, but the pause uses every core, with work stealing. It gives the best total throughput,
at the cost of pauses that grow with the heap. It was the default collector up to JDK 8.

## CMS: Concurrent Mark Sweep

CMS (JDK 1.4) marked the old generation **concurrently** with the program, using an incremental-update write barrier,
with short pauses at the start (initial mark) and end (remark), and then swept concurrently. It never compacted the old
generation, so it suffered from fragmentation and had to fall back to a long full collection when allocation failed. It
was deprecated in JDK 9 and removed in JDK 14.

## G1: Garbage First

G1 (experimental in JDK 6u14, default since JDK 9) splits the heap into equal **regions** (1–32 MiB).

- Regions take the role of Eden, survivor or old as needed; humongous objects take runs of regions.
- Each region has a **remembered set** of incoming pointers, so any set of regions can be evacuated on its own.
- Marking is concurrent with **snapshot-at-the-beginning** (SATB) barriers.
- Each pause evacuates a **collection set**: all young regions plus the old regions with the most garbage (hence the
  name), chosen to fit a **pause-time goal** (`-XX:MaxGCPauseMillis`, 200 ms by default).
- Compaction happens incrementally by evacuation, so fragmentation stays under control.

G1 is a balance between throughput and latency, and it is the default for most workloads.

## ZGC

ZGC (experimental in JDK 11, production in JDK 15, generational since JDK 21) aims for pauses under a millisecond,
independent of heap size, up to multi-terabyte heaps.

- **Coloured pointers**: metadata bits inside each 64-bit reference record its marking and relocation state.
- **Load barriers**: every pointer loaded from the heap is checked. If it is stale, the barrier fixes it on the spot by
  finding or making the relocated copy.
- Marking, relocation and reference updating are all **concurrent**. Pauses only handle roots, and their length does
  not depend on heap size.

## Shenandoah

Shenandoah (Red Hat, JDK 12) also evacuates **concurrently** with the program. It started with Brooks forwarding pointers
(an extra word per object pointing at its current copy) and moved to load-reference barriers like ZGC. It became
generational in recent releases.

## Epsilon

Epsilon (JDK 11) allocates and never collects. It exists for performance testing and for very short-lived programs.

## What the JVM teaches

- There is no single best collector; throughput, latency and footprint pull in different directions.
- Generations, TLABs, card tables and remembered sets are the standard toolkit.
- Low pause times need concurrent **moving**, which needs read barriers on every load. The JIT can afford those because
  it compiles every access.

paragc borrows the toolkit: TLAB-style buffers, remembered sets, SATB marking, parallel work stealing. It does not
borrow read barriers. It is a library, not a JIT, and cannot instrument every pointer load, so it only moves objects while
the mutators are stopped.
