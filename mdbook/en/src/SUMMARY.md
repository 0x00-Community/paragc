# Summary

[Preface](README.md)

# Garbage collection

- [What a garbage collector is](gc.md)
- [The first garbage collectors](history.md)
- [The JVM's collectors](jvm.md)
- [cppgc: V8's C++ heap](cppgc.md)
- [From there to paragc](lineage.md)

# Getting started

- [What paragc is](intro.md)
- [Core principles](principles.md)
- [Quick start](quickstart.md)
- [Handles and brands](handles.md)

# The private heap

- [Blocks, lines and headers](layout.md)
- [Bit layouts](bits.md)
- [Why object addresses change](moving.md)
- [Allocation](allocation.md)
- [Marking and the thread pool](marking.md)
- [Sweeping](sweeping.md)
- [Concurrent marking and sweeping](concurrent.md)
- [Evacuation](evacuation.md)
- [Young collections](young.md)
- [Reference counting](refcount.md)
- [Weak references and word slots](weak.md)

# Between heaps

- [Far references and group collection](far.md)

# Cross-heap (Linux, feature `crossheap`)

- [Overview](crossheap.md)
- [Sharing a JS object, step by step](walkthrough.md)
- [Address space and file layout](crossheap-layout.md)
- [Objects, types and pointer maps](crossheap-objects.md)
- [Processes, crashes and lifetime](crossheap-processes.md)
- [Weak references, signatures and the mailbox](crossheap-weak.md)
- [Promotion: a cross-heap as the old generation](promotion.md)

# Quality and roadmap

- [Testing and Miri](testing.md)
- [Benchmarks](benchmarks.md)
- [Code conventions](conventions.md)
- [Limits and open problems](limits.md)
- [Roadmap](roadmap.md)

---

[Glossary](glossary.md)
