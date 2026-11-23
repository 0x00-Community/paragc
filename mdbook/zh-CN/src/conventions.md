# 代码规矩

## 不用锁

paragc 不拿锁，回收里也不拿。共享状态的变化全靠原子操作、带标签栈头的无锁栈、只追加的分段向量和 futex 等待。工作窃取队列和
退避来自 crossbeam；只追加向量和只推入链表是 paragc 自己的（`src/sync.rs`）。

## 靠借用互斥

需要全停的操作都取 `&mut Heap`，借用检查器就是安全点协议。新代码应保持这一点，不要为私有堆加运行时协调。

## 源码布局

源码是平铺的，同类的东西放在一起：

| 文件 | 内容 |
| --- | --- |
| `layout.rs` | 块、行、对象头、跨堆窗口布局与超级块 |
| `heap.rs` | `Heap`、`Scope`、`Tlab`、各种回收、`CrossHeap`、`CrossHeapScope`、晋升 |
| `mark.rs` | `Tracer`、工作窃取工人组、复制器、跨堆标记器 |
| `sweep.rs` | 清扫与大对象 |
| `handle.rs` | `ScopeLocal`、`Slot`、`WordSlot`、`ScopeGlobal`、`Weak`、`Far`、跨堆 Handle、`Signature`、`Shared` |
| `shm.rs` | 跨堆地址区、窗口、堆号、进程死活（Linux） |
| `sync.rs` | 无锁构件与 futex 等待 |

跨堆的条目都挂在 `cfg(crossheap)` 上，这个 cfg 只在 Linux 且开了 `crossheap` feature 时由 `build.rs` 设置。

## 风格

- 代码、注释、消息、文档用英文；README 英文（缺省）加中文两份。
- `cargo clippy --all-targets` 在 `pedantic` 下干净，开不开 `--features crossheap` 都一样。
- 大小、阈值、位字段一律用具名常量，不写裸数字。
- 每个 `unsafe` 块都用 `// SAFETY:` 注释写明为什么安全。

## 发版

仓库的 `main` 受保护：所有改动都走 GitHub（0x00-Community/paragc）上的 PR。版本发到 crates.io（`paragc` 和 `paragc-macros`，
版本号相同），并打 `vX.Y.Z` 标签。
