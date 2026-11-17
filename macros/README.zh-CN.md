# paragc-macros

[English](README.md) · **简体中文**

[paragc](https://github.com/0x00-Community/paragc) 的过程宏。通过 `paragc` 使用（`paragc::Trace`），不要直接依赖。

- `#[derive(Trace)]`：逐个字段追踪（`trace`）与重定位（`relocate`，疏散回收时把边改成搬走后的新地址）；`NO_EDGES` 是全部字段
  `NO_EDGES` 的与。结构体、枚举都可以。
- 缺省还检查类型没有实现 `Drop`（析构顺序不定，`Drop` 里可能碰到已经析构的对象）。`#[trace(unsafe_drop)]` 关掉检查，
  由你保证 `Drop` 里不碰任何 `Gc`。
- `#[derive(Shared)]`：能放进公开堆的类型（共享内存上、可能被别的进程读和回收）。要求 `#[repr(C)]`（每个进程里布局才一致），
  只支持结构体；生成指针图：每个 `PublicSlot` 的字节偏移，嵌套类型与定长数组展开。没有析构由 `PublicMutator::alloc` 在编译期检查。

MIT © Jabberwocky238
