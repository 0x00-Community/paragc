# 目录

[前言](README.md)

# 垃圾回收

- [什么是垃圾收集器](gc.md)
- [最早的垃圾收集器](history.md)
- [JVM 的收集器](jvm.md)
- [cppgc：V8 的 C++ 堆](cppgc.md)
- [从它们到 paragc](lineage.md)

# 入门

- [paragc 是什么](intro.md)
- [核心宗旨](principles.md)
- [快速上手](quickstart.md)
- [句柄与品牌](handles.md)

# 私有堆

- [块、行与对象头](layout.md)
- [比特布局](bits.md)
- [分配](allocation.md)
- [标记与线程池](marking.md)
- [清扫](sweeping.md)
- [并发标记与并发清扫](concurrent.md)
- [疏散](evacuation.md)
- [年轻回收](young.md)
- [引用计数](refcount.md)
- [弱引用与字槽](weak.md)

# 堆之间

- [Far 引用与一起回收](far.md)

# 跨堆（Linux，feature `crossheap`）

- [概览](crossheap.md)
- [一个 JS 对象怎么跨进程](walkthrough.md)
- [地址空间与文件布局](crossheap-layout.md)
- [对象、类型与指针图](crossheap-objects.md)
- [进程、崩溃与寿命](crossheap-processes.md)
- [弱引用、签名与信箱](crossheap-weak.md)
- [晋升：把跨堆当老年代](promotion.md)

# 质量与路线

- [测试与 Miri](testing.md)
- [性能对比](benchmarks.md)
- [代码规矩](conventions.md)
- [限制与未解决的问题](limits.md)
- [路线图](roadmap.md)

---

[术语表](glossary.md)
