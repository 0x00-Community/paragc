# 弱引用、签名与信箱

这三样东西把引用带到别的进程，而不让任何进程去等另一个进程。

## 出生戳

每个跨堆对象头里有一个 32 位的**出生戳**：堆的化身号和分配纪元经 SplitMix64 混合而成。不同时间占据同一个地址的两个对象，
出生戳几乎一定不同。

## `CrossHeapWeak`

`CrossHeapWeak<B>` 是一个字：高位是对象偏移（以 8 字节为单位），低 27 位是出生戳。

- 它不在目标堆里登记任何东西，也不让任何东西存活。
- 没有回收会看它，所以跨进程时它也拖不住任何人。

在目标堆的 `mutate` 里读它时，依次检查：

1. 块已经开出来，块表里这一项有内存；
2. 这个偏移上起点位是置位的，或者它是某个大对象第一块的起点；
3. 出生戳对得上。

任何一项不过就得到 `None`。这涵盖了对象已死、同一位置换了新对象、堆号被新堆复用这几种情况。出生戳撞上的概率约为 2⁻²⁷。

## 签名

`Signature` 是 32 字节：弱引用字、堆的化身号、类型键。一个进程发给另一个进程的就是它。

```rust
// 发送方
let sig: Signature = cross.mutate(|cm| cm.sign(obj));
let bytes: [u8; SIGNATURE_BYTES] = sig.into();
send(&bytes, cross.fd());                  // 例如经 Unix socket 带 SCM_RIGHTS 发出

// 接收方
let sig = Signature::from(&bytes);
let heap = space.open_fd(fd, threads)?;
let value = heap.mutate(|cm| cm.resolve::<NodeB>(&sig).map(|n| n.value));
```

| 调用 | 用途 |
| --- | --- |
| `sign(gc)` | 给跨堆里的对象签名 |
| `sign_promoted(gc)` | 不用品牌，直接凭私有 `Gc` 给晋升过去的对象签名 |
| `resolve::<B>(&sig)` | 对象还活着且类型是 `B` 时返回它 |
| `verify(&sig)` | 不知道类型时检查签名 |

不可信一方给的签名只会被读取。伪造的或过期的签名得到 `None`，不会让读的一方崩溃。

## 信箱：请主人代办

有些资源没法共享：文件描述符、`Box`，以及一切只属于主人进程的东西。别的进程请主人代办：

1. `CrossHeapMutator::post(obj)` 领一个不归任何进程的入口，所以发信方死了请求也还在；
2. 把入口压进超级块里的信箱栈；
3. 来信计数（一个 futex 字）加一，唤醒主人。

主人查看 `CrossHeap::mail()`（到目前为止收到的信数），或者在 `wait_mail(seen, timeout)` 里睡到它变化。不要在这个堆自己的
`mutate` 里等，否则回收会一直等你离开。`take_mail` 按发信先后取走全部；用 `find` 取出每个对象，处理完用 `release` 归还入口。

## 收缩与描述符

`CrossHeap::shrink` 做一次一个备用块都不留的回收，在一大波垃圾之后把文件的实际用量降下来。`CrossHeap::fd` 给出要传给
别的进程的文件描述符。接收方不需要路径，而且每个进程都把这个堆映射在同一个窗口号上。
