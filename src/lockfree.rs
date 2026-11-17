//! 堆内部用的两个无锁结构：只追加的分段向量 [`SegVec`]、只压入 / 整体取走的链 [`PushList`]。
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

/// 第一段的容量（2 的幂）；之后每段翻倍
const FIRST: usize = 32;
/// 段数上限：容量加起来覆盖整个 usize
const SEGMENTS: usize = usize::BITS as usize - FIRST.trailing_zeros() as usize;

/// 一个格子：写完值才置 `ready`，读的一侧只认 ready 的。
struct Cell<T> {
    ready: AtomicBool,
    value: UnsafeCell<MaybeUninit<T>>,
}

/// 只追加的分段向量：段一旦分配就不动，所以元素地址不变、`push` 与读可以并发，不加锁。
pub struct SegVec<T> {
    segments: [AtomicPtr<Cell<T>>; SEGMENTS],
    /// 领出去的格子数（含还没写完的）
    len: AtomicUsize,
}

// SAFETY: 格子写完才对别人可见（ready 的 Release / Acquire）；之后只给出 &T
unsafe impl<T: Send + Sync> Sync for SegVec<T> {}
// SAFETY: 元素随容器转移
unsafe impl<T: Send> Send for SegVec<T> {}

impl<T> Default for SegVec<T> {
    fn default() -> Self {
        Self { segments: core::array::from_fn(|_| AtomicPtr::new(null_mut())), len: AtomicUsize::new(0) }
    }
}

impl<T> SegVec<T> {
    /// 第 `k` 段的容量。
    fn capacity(k: usize) -> usize {
        FIRST << k
    }

    /// 下标 → （段号, 段内下标）。
    fn locate(i: usize) -> (usize, usize) {
        let j = i + FIRST;
        let k = (usize::BITS - 1 - j.leading_zeros()) as usize - FIRST.trailing_zeros() as usize;
        (k, j - Self::capacity(k))
    }

    /// 第 `k` 段，没有就分配（两个线程同时分配时输的那个把自己的还掉）。
    fn segment(&self, k: usize) -> *mut Cell<T> {
        let seg = self.segments[k].load(Ordering::Acquire);
        if !seg.is_null() {
            return seg;
        }
        let fresh: Box<[Cell<T>]> = (0..Self::capacity(k)).map(|_| Cell { ready: AtomicBool::new(false), value: UnsafeCell::new(MaybeUninit::uninit()) }).collect();
        let fresh = Box::into_raw(fresh).cast::<Cell<T>>();
        match self.segments[k].compare_exchange(null_mut(), fresh, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => fresh,
            Err(won) => {
                // SAFETY: 刚分配、没给出去
                drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(fresh, Self::capacity(k))) });
                won
            }
        }
    }

    fn cell(&self, i: usize) -> Option<&Cell<T>> {
        let (k, at) = Self::locate(i);
        let seg = self.segments.get(k)?.load(Ordering::Acquire);
        // SAFETY: 段非空就有 capacity(k) 个格子，at 在范围内；段不会被释放（除非 &mut self）
        (!seg.is_null()).then(|| unsafe { &*seg.add(at) })
    }

    /// 追加，返回下标。
    pub fn push(&self, value: T) -> usize {
        let i = self.len.fetch_add(1, Ordering::AcqRel);
        let (k, at) = Self::locate(i);
        // SAFETY: 段有 capacity(k) 个格子；下标 i 只归本线程写
        let cell = unsafe { &*self.segment(k).add(at) };
        // SAFETY: 同上，还没置 ready，没人读
        unsafe { (*cell.value.get()).write(value) };
        cell.ready.store(true, Ordering::Release);
        i
    }

    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 第 `i` 个；还没写完返回 None。
    pub fn get(&self, i: usize) -> Option<&T> {
        let cell = self.cell(i)?;
        // SAFETY: ready 之后值已初始化、不再改
        cell.ready.load(Ordering::Acquire).then(|| unsafe { (*cell.value.get()).assume_init_ref() })
    }

    /// 已经写完的元素（跳过还在写的）。
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        (0..self.len()).filter_map(|i| self.get(i))
    }

    /// 独占时取走全部元素，容器变空（段保留下来接着用）。
    pub fn take(&mut self) -> Vec<T> {
        let n = core::mem::replace(self.len.get_mut(), 0);
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let (k, at) = Self::locate(i);
            let seg = *self.segments[k].get_mut();
            // SAFETY: 独占：push 都已返回，前 n 个格子都写完了
            let cell = unsafe { &mut *seg.add(at) };
            *cell.ready.get_mut() = false;
            // SAFETY: 写完过、这里取走后 ready 已清，不会再读或析构
            out.push(unsafe { cell.value.get_mut().assume_init_read() });
        }
        out
    }
}

impl<T> Extend<T> for SegVec<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for v in iter {
            self.push(v);
        }
    }
}

impl<T> Drop for SegVec<T> {
    fn drop(&mut self) {
        drop(self.take());
        for (k, seg) in self.segments.iter_mut().enumerate() {
            let p = *seg.get_mut();
            if !p.is_null() {
                // SAFETY: 段来自 segment() 的 Box<[Cell<T>]>，元素都已取走
                drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(p, Self::capacity(k))) });
            }
        }
    }
}

struct Node<T> {
    value: T,
    next: *mut Node<T>,
}

/// 只压入、整体取走的无锁链（Treiber 栈的头只被换成空，所以没有 ABA）。
pub struct PushList<T> {
    head: AtomicPtr<Node<T>>,
}

// SAFETY: 节点从压入方整体转给取走方
unsafe impl<T: Send> Sync for PushList<T> {}
// SAFETY: 同上
unsafe impl<T: Send> Send for PushList<T> {}

impl<T> Default for PushList<T> {
    fn default() -> Self {
        Self { head: AtomicPtr::new(null_mut()) }
    }
}

impl<T> PushList<T> {
    pub fn push(&self, value: T) {
        let node = Box::into_raw(Box::new(Node { value, next: null_mut() }));
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            // SAFETY: 节点还没发布，只有本线程碰
            unsafe { (*node).next = head };
            match self.head.compare_exchange_weak(head, node, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(now) => head = now,
            }
        }
    }

    /// 取走全部（后压入的在前）。
    pub fn take(&self) -> Vec<T> {
        let mut p = self.head.swap(null_mut(), Ordering::AcqRel);
        let mut out = Vec::new();
        while !p.is_null() {
            // SAFETY: 整条链刚被换下来，归本线程；节点来自 push 的 Box
            let node = unsafe { Box::from_raw(p) };
            p = node.next;
            out.push(node.value);
        }
        out
    }
}

impl<T> Drop for PushList<T> {
    fn drop(&mut self) {
        drop(self.take());
    }
}
