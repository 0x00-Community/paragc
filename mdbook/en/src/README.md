# Preface

This book explains how [paragc](https://github.com/0x00-Community/paragc) works, from the first `alloc` to a heap shared
by several processes. It is written for two kinds of readers:

- **Users** who embed paragc in a Rust program (a language runtime such as [j8](https://j8.app238.com), a database, a
  graph library) and want to know what each API promises and what it costs.
- **Contributors** who want to change the collector and need the invariants that hold it together.

paragc is a tracing garbage collector with an Immix-style mark-region heap. Many threads allocate at once, collections
mark and sweep on every core, and nothing in it takes a lock. On Linux, the `crossheap` feature adds heaps that live in
shared memory: other processes map them, read and write their objects, and find objects by a signature.

## How to read it

| Part | What it covers |
| --- | --- |
| Garbage collection | What a garbage collector is and the concepts every collector is built from; the first collectors (1960 onwards); the JVM's collectors; cppgc, V8's C++ heap; and how paragc descends from them |
| Getting started | What paragc is, a first program, the handle types and why they carry brands |
| The private heap | The in-process heap: layout, allocation, marking, sweeping, concurrent work, evacuation, young collections, reference counting, weak references |
| Between heaps | Direct references from one private heap into another, and collecting cycles that span them |
| Cross-heap | Heaps on shared memory: address space, file layout, objects without code, processes that die at any moment, weak references and signatures across processes, promotion of private objects into a cross-heap |
| Quality and roadmap | Tests, Miri, benchmarks, code rules, known limits and what comes next |

Every page states the current behaviour of the code on `main`. When a page and the code disagree, the code wins; please
open an issue or use the edit link at the top of the page.

## Versions

This edition describes paragc **0.6.1**:

- 0.6.0 added cross-heap weak references, signatures, the mailbox, shrinking and promotion;
- 0.6.1 added tail allocation.

The crate is on [crates.io](https://crates.io/crates/paragc) and its API reference is on [docs.rs](https://docs.rs/paragc).
