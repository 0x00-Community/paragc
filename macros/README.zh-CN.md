# paragc-macros

[English](README.md) · **简体中文**

[paragc](https://github.com/Jabberwocky238/paragc) 的过程宏。通过 `paragc` 使用（`paragc::Trace`），不要直接依赖。

- `#[derive(Trace)]`：逐个字段追踪；`NO_EDGES` 是全部字段 `NO_EDGES` 的与。结构体、枚举都可以。
- 缺省还检查类型没有实现 `Drop`（析构顺序不定，`Drop` 里可能碰到已经析构的对象）。`#[trace(unsafe_drop)]` 关掉检查，
  由你保证 `Drop` 里不碰任何 `Gc`。

MIT © Jabberwocky238
