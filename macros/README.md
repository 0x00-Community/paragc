# paragc-macros

**English** · [简体中文](README.zh-CN.md)

Procedural macros for [paragc](https://github.com/0x00-Community/paragc). Use them through `paragc` (`paragc::Trace`), not directly.

- `#[derive(Trace)]`: traces every field (`trace`) and relocates every field (`relocate`, used by evacuating
  collections to rewrite edges to moved objects); `NO_EDGES` is the AND of the fields' `NO_EDGES`. Works on structs and enums.
- By default the derive also checks that the type does not implement `Drop` (destructors run in no particular order, so a
  `Drop` could see objects that were already dropped). `#[trace(unsafe_drop)]` turns the check off; you then guarantee
  that the `Drop` touches no `Gc`.
- `#[derive(Shared)]`: types that can live in a public heap (shared memory that other processes may read and collect).
  Requires `#[repr(C)]` so the layout is the same in every process; structs only. Generates the pointer map: the byte
  offset of every `PublicSlot`, through nested types and fixed-size arrays. That the type has no destructor is checked at
  compile time by `PublicMutator::alloc`.

MIT © Jabberwocky238
