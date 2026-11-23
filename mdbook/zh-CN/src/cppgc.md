# cppgc：V8 的 C++ 堆

JVM 掌控着语言、编译器和运行时，所以能在各处放屏障和栈图。给 **C++** 用的垃圾收集器做不到：代码是人写的，指针是裸的，栈是
C++ 编译器生成的样子。cppgc（又名 **Oilpan**）是这个问题最知名的现代答案，也是 paragc 最近的亲戚。

## 来历

Chrome 的渲染引擎 Blink 原来用引用计数管理 DOM 对象，C++ DOM 节点和 JavaScript 包装对象之间的环会泄漏。大约 2013 年，Blink
团队为 Blink 的 C++ 对象做了一个追踪式垃圾收集器 **Oilpan**。2020 年它作为独立库 **cppgc** 搬进 V8，其他嵌入方也能用。今天
Blink 的 DOM、V8 的 JavaScript 堆和 cppgc 的 C++ 堆组成一个**统一堆**，一起追踪。

## 编程模型

```cpp
class Node final : public cppgc::GarbageCollected<Node> {
 public:
  void Trace(cppgc::Visitor* visitor) const {
    visitor->Trace(next_);
  }
 private:
  cppgc::Member<Node> next_;
};

auto* node = cppgc::MakeGarbageCollected<Node>(heap->GetAllocationHandle());
cppgc::Persistent<Node> root(node);
```

| cppgc | 作用 | paragc 里对应的 |
| --- | --- | --- |
| `GarbageCollected<T>` | 堆对象的基类 | `#[derive(Trace)]` |
| `Trace(Visitor*)` | 手写，报告每个指针字段 | 由派生生成 |
| `Member<T>` | 堆对象里的指针字段，带写屏障 | `Slot<'heap, 'scope, T>` |
| `WeakMember<T>` | 弱字段 | 对象里的 `Weak` |
| `Persistent<T>` | 从堆外持有的根 | `ScopeGlobal<'heap, 'heap, B>` |
| `WeakPersistent<T>` | 弱根 | `Weak<B>` |
| 栈上的裸 `T*` | 靠保守扫栈找到 | `ScopeLocal<'heap, 'scope, T>`，对所属 scope 的借用，任何一次回收前由借用检查器让它失效 |

## 怎么回收

- **标记-清扫**，按大小分类的页里用空闲链表分配，另有线性分配缓冲里的递增分配。普通对象不搬。
- **保守扫栈**：线程栈上任何指向堆的字都让那个对象存活。正因如此，C++ 代码可以安全地在局部变量里持有裸指针，也正因如此，
  对象不能随意搬动。
- **增量和并发标记**，`Member` 写入上有写屏障；**并发清扫**。
- **只压缩后备存储**：Blink 的 vector 和哈希表背后的缓冲可以压缩，因为 cppgc 知道每个指向它们的槽。
- **预析构**（pre-finalizer）在清扫前运行，那时所有对象都还完好，用于必须碰其他堆对象的清理。普通析构在清扫时运行，没有固定
  顺序。
- **年轻代**（带记忆集的 minor 回收）在一个构建开关后面，需要**笼式堆**（caged heap）：所有对象在一块大的保留区里，用掩码就能
  判断一个指针是否在堆里。

## cppgc 带来的启示

- C++ 收集器必须容忍栈上的裸指针，所以只能保守扫描，这限制了搬动。
- 手写的 `Trace` 方法是常见的 bug 来源：漏掉一个字段，活对象就会被释放。
- 跨语言（C++ 和 JS）的统一堆消除了两个各自为政、互相引用的收集器造成的泄漏。

## paragc 的不同

| | cppgc | paragc |
| --- | --- | --- |
| 栈 | 保守扫描 | 不扫；`ScopeLocal` 被 Rust 生命周期限制在所属 scope 里，根（`ScopeGlobal`）是显式的 |
| 全停 | 需要每个线程停在已知位置 | `&mut Heap`：借用检查器证明没人在里面 |
| 搬动 | 普通对象从不搬 | 疏散和年轻复制，因为不存在收集器不知道的裸指针 |
| 追踪代码 | 手写 `Trace` | 派生生成；编译期拒绝被追踪类型实现 `Drop` |
| 分配线程 | 每线程一个堆 | 一个堆，多个线程同时递增分配 |
| 年轻对象 | 可选的 minor 回收 | 缺省就有粘性标记位的年轻回收和引用计数 |
| 进程间共享 | 无 | 共享内存上的跨堆（Linux） |

cppgc 要靠保守扫描换来的东西，paragc 由 Rust 直接给了：编译器知道什么时候不可能有指针在收集器视野之外。
