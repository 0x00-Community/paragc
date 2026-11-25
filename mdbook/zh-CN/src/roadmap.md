# 路线图

## 已完成

| 版本 | 内容 |
| --- | --- |
| 0.4 及以前 | 并行全停回收与常驻线程池；并发标记；并发与惰性清扫；机会式疏散；年轻回收；引用计数与后台释放；每 16 次年轻回收一次追踪；并发重算计数；`Far` 与一起回收 |
| 0.5 | 共享内存上的跨堆，多进程与崩溃恢复；`CrossHeapRef` 与跨堆一起回收；堆号与重启；堆随主人死；`WordSlot` 与 `Weak` |
| 0.6.0 | `crossheap` feature（只在 Linux）；`Public*` 接口改名 `CrossHeap*`；`CrossHeapWeak`、出生戳与签名；`open_fd`；信箱；跨堆收缩；晋升进挂接的跨堆、只归主人回收；写进跨堆槽的私有对象延后晋升；`sign_promoted` |
| 0.6.1 | `alloc_tail` / `alloc_shared_tail` |
| 0.6.2 | `Handle` / `HandleSlot` / `alloc_handle`：经 Handle 表的引用 |
| 0.6.3 | `Handle::from_id` |
| 0.7.0 | scope：`Heap<'heap>` / `Scope<'heap, 'life>` 嵌套并在 scope 里回收，`ScopeLocal`（原 `Gc`）、`ScopeGlobal`（原 `Root`）、`'heap` / `'xheap` Brand |
| 0.7.1 | 分配上限：`Heap::used`（堆占用的字节数）、`Heap::set_limit` / `Heap::on_limit` 与 `Scope::set_limit` / `Scope::used`，堆占用超过上限时调用钩子 |

## 接下来

- **j8 的跨进程 JS 对象**，分段做：
  1. isolate 挂跨堆，一轮结束时给共享对象签名（已完成）；
  2. 字符串进 GC 堆（已完成）；
  3. shape 和编译好的函数进 GC 堆，经 Handle 引用；
  4. 普通对象与数组；
  5. 函数，字节码放进跨堆；
  6. 宿主对象，经主人与信箱代办。
- **跨堆的并发标记、年轻回收和引用计数**，让大的共享老年代不再意味着长停顿。
- **挂着跨堆时的并发标记。**
- **并发年轻回收**，以及一个字大小的 `Slot`。
