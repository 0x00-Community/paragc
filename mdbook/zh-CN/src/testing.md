# 测试与 Miri

## 测试文件

| 文件 | 覆盖 |
| --- | --- |
| `tests/gc.rs` | 私有堆：存活、析构计数、多线程、大对象、深结构、并发标记与清扫、疏散、年轻回收、引用计数、收缩、串行析构 |
| `tests/weak.rs` | 各种回收下的弱引用、并发标记期间的读屏障 |
| `tests/word.rs` | `WordSlot` 经历完整、年轻、并发回收 |
| `tests/handle.rs` | Handle：各种年轻回收模式下对象在它下面搬动、老对象不经屏障持有、表项重用、并发标记、id 变回 Handle |
| `tests/scope.rs` | 嵌套 scope：在 scope 里回收、在两个小 scope 之间把对象带出去 |
| `tests/far.rs` | `Far`、入口不搬、一起回收、私有堆之间的环 |
| `tests/cross_heap.rs` | 单进程里的跨堆：指针图、大对象、映射两次、`CrossHeapRef`、一起回收、弱引用、签名、信箱、收缩、晋升、尾巴（需要 `--features crossheap`） |
| `tests/processes.rs` | 多个进程共用一个堆：杀 mutator、杀回收方、随机杀、重启、堆号、8 个进程杀一半（需要 `--features crossheap`） |

README 的验证表列了每一条测试和它检查什么。`Scope` 上的两个 `compile_fail` 文档测试检查借用规则：回收之后还用
`ScopeLocal`，以及把一个堆的对象写进另一个堆。

## 结果（0.7.0）

| 文件 | 测试数 | 结果 |
| --- | ---: | --- |
| `gc.rs` | 26 | 通过 |
| `weak.rs` | 8 | 通过 |
| `far.rs` | 7 | 通过 |
| `word.rs` | 5 | 通过 |
| `handle.rs` | 5 | 通过 |
| `scope.rs` | 2 | 通过 |
| `cross_heap.rs` | 18 | 通过 |
| `processes.rs` | 9 | 通过 |
| 文档测试（README 示例、两个 `compile_fail`） | 3 | 通过 |

开不开 `crossheap`，clippy 都没有警告。

## 怎么跑

```sh
cargo test --test gc -- --exact <name>
cargo test --features crossheap --test cross_heap
cargo test --features crossheap --test processes
```

`promotion_into_attached_cross_heap` 在一个测试里依次跑好几个场景，因为一个进程同时只能占一次固定地址区，而同一文件的测试
在并行线程上跑。

## Miri

每条测试也在 Miri 下跑，Stacked Borrows 和 Tree Borrows 各一遍（`cfg(miri)` 下缩小规模）：

```sh
MIRIFLAGS="-Zmiri-permissive-provenance -Zmiri-ignore-leaks" cargo +nightly miri test --test gc -- --exact <name>
MIRIFLAGS="-Zmiri-permissive-provenance -Zmiri-ignore-leaks -Zmiri-tree-borrows" cargo +nightly miri test --test gc -- --exact <name>
```

- Miri 不能映射文件。Miri 下跨堆地址区是一块普通分配（4 个 16 MiB 的窗口）；不带文件的测试在它上面跑，带文件的跳过。
- 要 `-Zmiri-ignore-leaks`，因为 crossbeam-epoch 的全局回收器退出时还拿着延迟释放的垃圾。
- 4 个回收线程时 Stacked Borrows 会在 crossbeam-epoch 内部报违规，所以 Miri 下测试用 2 个。少数并发测试在 Stacked Borrows
  下仍会碰到它，在 Tree Borrows 下干净。

## 已知不稳定的测试

`gc::background_release_while_mutating` 在 release 构建下可能失败，debug 下通过。这个问题在 0.6.0 之前就有。
