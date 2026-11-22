//! 并发原语：回收线程池 [`Pool`]、只追加的分段向量 [`SegVec`] 与只压入链 [`PushList`]、公开堆文件里的无锁栈 [`Stack`] 与跨进程 futex 等待。
//!
//! 回收用的常驻线程池：堆第一次回收时起 `threads − 1` 个工人线程，活到堆析构；调用线程自己当 0 号工人。
//!
//! - 分派：先写好任务与调用方，再把「代号 + 本轮参与人数」一个原子字加一代，只叫醒参与的工人
//! - 工人空闲时先退避自旋（crossbeam `Backoff`），转完一轮就 `thread::park` 挂起（futex，不是锁），不白烧 CPU
//! - 本轮参与者全部做完后由最后一个叫醒调用方；工人里的 panic 在调用方重新抛出
//!
//! 堆内部用的两个无锁结构：只追加的分段向量 [`SegVec`]、只压入 / 整体取走的链 [`PushList`]。
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::num::NonZeroUsize;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
#[cfg(target_os = "linux")]
use core::sync::atomic::AtomicU32;
#[cfg(target_os = "linux")]
use core::time::Duration;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread::{self, JoinHandle, Thread};

use crossbeam_utils::{Backoff, CachePadded};

use crate::layout::UNITS_PER_WORKER;

/// 任务字的低位：本轮参与人数（含调用方）
const ACTIVE_BITS: u32 = 16;
const ACTIVE_MASK: u64 = (1 << ACTIVE_BITS) - 1;
/// 任务字每加一代加这么多
const GENERATION: u64 = 1 << ACTIVE_BITS;

/// 一轮的任务：类型擦除过的 `&dyn Fn(工号)`，只在 [`Pool::run`] 等待期间有效。
type Job = *const (dyn Fn(usize) + Sync);

struct Shared {
    /// 高位代号、低位参与人数；工人一次读出，二者一致
    task: CachePadded<AtomicU64>,
    /// 本轮还没做完的工人数（不含调用方）
    remaining: CachePadded<AtomicUsize>,
    job: UnsafeCell<Job>,
    /// 本轮的调用方（做完的最后一个工人叫醒它）
    caller: UnsafeCell<Option<Thread>>,
    /// 本轮有工人 panic 了
    panicked: AtomicBool,
    /// 正有一轮在跑（防重入：并发标记经 &Heap 调用）
    busy: AtomicBool,
    stop: AtomicBool,
}

// SAFETY: job / caller 只在没有工人读它们的时候写（上一轮全部做完、下一代还没发出）；其余是原子量
unsafe impl Sync for Shared {}
// SAFETY: 同上
unsafe impl Send for Shared {}

impl Shared {
    fn task(&self) -> (u64, usize) {
        let word = self.task.load(Ordering::Acquire);
        (word & !ACTIVE_MASK, usize::try_from(word & ACTIVE_MASK).unwrap_or(usize::MAX))
    }

    /// 工人 `me` 的主循环。
    fn serve(&self, me: usize) {
        // 从第 0 代起算：线程起得晚、第一轮已经发出时也不会漏掉
        let mut seen = 0;
        loop {
            let (generation, active) = self.wait(seen);
            if self.stop.load(Ordering::Acquire) {
                return;
            }
            seen = generation;
            if me >= active {
                continue;
            }
            // SAFETY: 参与本轮的工人做完之前，调用方一直等着，job 指向的闭包活着、不会被改写
            let job = unsafe { &**self.job.get() };
            if catch_unwind(AssertUnwindSafe(|| job(me))).is_err() {
                self.panicked.store(true, Ordering::Release);
            }
            // 先抄下调用方再减计数：减到 0 的瞬间调用方就可能返回、开始下一轮并改写 caller
            // SAFETY: 本工人还没减计数，本轮没做完，调用方不会改 caller
            let caller = unsafe { (*self.caller.get()).clone() };
            if self.remaining.fetch_sub(1, Ordering::AcqRel) == 1
                && let Some(caller) = caller
            {
                // 调用方可能已经走了，多一次 unpark 无害
                caller.unpark();
            }
        }
    }

    /// 等到代号不是 `seen`（或要停）。
    fn wait(&self, seen: u64) -> (u64, usize) {
        let backoff = Backoff::new();
        loop {
            let task = self.task();
            if task.0 != seen || self.stop.load(Ordering::Acquire) {
                return task;
            }
            if backoff.is_completed() {
                thread::park();
            } else {
                backoff.snooze();
            }
        }
    }
}

pub struct Pool {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

impl Pool {
    /// 起 `threads − 1` 个工人。
    pub fn new(threads: NonZeroUsize) -> Self {
        let shared = Arc::new(Shared {
            task: CachePadded::new(AtomicU64::new(0)),
            remaining: CachePadded::new(AtomicUsize::new(0)),
            job: UnsafeCell::new(core::ptr::null::<fn(usize)>() as Job),
            caller: UnsafeCell::new(None),
            panicked: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let max = u64::try_from(threads.get()).unwrap_or(ACTIVE_MASK).min(ACTIVE_MASK);
        let workers = (1..usize::try_from(max).unwrap_or(1))
            .map(|me| {
                let shared = shared.clone();
                thread::Builder::new().name(format!("paragc-{me}")).spawn(move || shared.serve(me)).expect("起回收线程")
            })
            .collect();
        Self { shared, workers }
    }

    /// 线程总数（含调用方）。
    pub fn threads(&self) -> usize {
        self.workers.len() + 1
    }

    /// 这么多活（块 + 大对象）该用几个线程：活少时少用。
    pub fn sized(&self, units: usize) -> usize {
        (units / UNITS_PER_WORKER).clamp(1, self.threads())
    }

    /// `n` 个线程（含调用方，工号 0..n）各跑一次 `f(工号)`，按工号收集结果。
    pub fn run<R: Send>(&self, n: usize, f: impl Fn(usize) -> R + Sync) -> Vec<R> {
        let n = n.clamp(1, self.threads());
        if n == 1 {
            return vec![f(0)];
        }
        let _busy = Busy::enter(&self.shared.busy);
        let slots: Vec<UnsafeCell<Option<R>>> = (0..n).map(|_| UnsafeCell::new(None)).collect();
        let slots = Slots(&slots);
        let job = |me: usize| slots.put(me, f(me));
        let job: &(dyn Fn(usize) + Sync) = &job;
        let shared = &*self.shared;
        // SAFETY: 上一轮已全部做完（Busy 保证 run 不重入），此刻没有工人读 job / caller；
        // 擦掉的生命周期由下面「等到全部做完才返回」保证
        #[expect(clippy::transmute_ptr_to_ptr, reason = "要擦掉 dyn 的生命周期，as 转换做不到")]
        unsafe {
            *shared.job.get() = core::mem::transmute::<*const (dyn Fn(usize) + Sync + '_), Job>(job);
            *shared.caller.get() = Some(thread::current());
        }
        shared.panicked.store(false, Ordering::Relaxed);
        shared.remaining.store(n - 1, Ordering::Relaxed);
        let active = u64::try_from(n).unwrap_or(ACTIVE_MASK);
        let word = shared.task.load(Ordering::Relaxed);
        shared.task.store(((word & !ACTIVE_MASK) + GENERATION) | active, Ordering::Release);
        for w in &self.workers[..n - 1] {
            w.thread().unpark();
        }
        let mine = catch_unwind(AssertUnwindSafe(|| job(0)));
        let backoff = Backoff::new();
        while shared.remaining.load(Ordering::Acquire) != 0 {
            if backoff.is_completed() {
                thread::park();
            } else {
                backoff.snooze();
            }
        }
        if let Err(e) = mine {
            std::panic::resume_unwind(e);
        }
        assert!(!shared.panicked.load(Ordering::Acquire), "回收线程 panic 了");
        slots.take()
    }
}

/// 一轮的占用标记：同一时刻只能有一轮。
struct Busy<'a>(&'a AtomicBool);

impl<'a> Busy<'a> {
    /// 等到没有别的一轮在跑（并发标记与并发清扫可能从不同线程同时来）。
    fn enter(flag: &'a AtomicBool) -> Self {
        let backoff = Backoff::new();
        while flag.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            backoff.snooze();
        }
        Self(flag)
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// 结果槽：每个工号一个，跨线程只写自己的。
struct Slots<'a, R>(&'a [UnsafeCell<Option<R>>]);

// SAFETY: 每个槽只被一个线程写，读在全部写完之后
unsafe impl<R: Send> Sync for Slots<'_, R> {}

impl<R> Slots<'_, R> {
    fn put(&self, me: usize, r: R) {
        // SAFETY: 每个工号只有一个线程写自己的槽，读在全部做完之后
        unsafe { *self.0[me].get() = Some(r) };
    }

    /// 全部做完之后取出结果。
    fn take(&self) -> Vec<R> {
        // SAFETY: 全部工人已经做完，没有并发写
        self.0.iter().map(|s| unsafe { (*s.get()).take() }.expect("每个工号都写了结果")).collect()
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.task.fetch_add(GENERATION, Ordering::AcqRel);
        for w in self.workers.drain(..) {
            w.thread().unpark();
            // 工人里的 panic 已经在 run 里报过
            let _ = w.join();
        }
    }
}

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

/// 栈头低 32 位：栈顶序号 + 1（0 = 空）；高 32 位是改动次数（防 ABA）
#[cfg(target_os = "linux")]
const STACK_TOP: u64 = u32::MAX as u64;
/// 栈头里改动次数的位移
#[cfg(target_os = "linux")]
const STACK_TAG_SHIFT: u32 = u32::BITS;

/// 窗口里的无锁栈（可用块、还回来的入口）：头在超级块里，每个元素的「下一个」在它自己的表项里。
#[cfg(target_os = "linux")]
pub struct Stack<'a> {
    head: &'a AtomicU64,
}

#[cfg(target_os = "linux")]
impl<'a> Stack<'a> {
    /// 头在 `head` 的栈。
    pub fn on(head: &'a AtomicU64) -> Self {
        Self { head }
    }

    pub fn push(&self, i: u32, next: &AtomicU32) {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            next.store((head & STACK_TOP) as u32, Ordering::Relaxed);
            let new = ((head >> STACK_TAG_SHIFT).wrapping_add(1) << STACK_TAG_SHIFT) | u64::from(i + 1);
            match self.head.compare_exchange_weak(head, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(now) => head = now,
            }
        }
    }

    pub fn pop<'n>(&self, next_of: impl Fn(u32) -> &'n AtomicU32) -> Option<u32> {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            let top = (head & STACK_TOP) as u32;
            let i = top.checked_sub(1)?;
            // 读到的可能是别人刚弹走又改过的「下一个」：那样头的改动次数也变了，下面的 CAS 会失败
            let next = next_of(i).load(Ordering::Relaxed);
            let new = ((head >> STACK_TAG_SHIFT).wrapping_add(1) << STACK_TAG_SHIFT) | u64::from(next);
            match self.head.compare_exchange_weak(head, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(i),
                Err(now) => head = now,
            }
        }
    }

    /// 整个栈一次拿走，先压进去的在前。拿走的元素只归调用方。
    pub fn take_all<'n>(&self, next_of: impl Fn(u32) -> &'n AtomicU32) -> Vec<u32> {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            if head & STACK_TOP == 0 {
                return Vec::new();
            }
            let new = (head >> STACK_TAG_SHIFT).wrapping_add(1) << STACK_TAG_SHIFT;
            match self.head.compare_exchange_weak(head, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(now) => head = now,
            }
        }
        let mut out = Vec::new();
        let mut top = (head & STACK_TOP) as u32;
        while let Some(i) = top.checked_sub(1) {
            out.push(i);
            top = next_of(i).load(Ordering::Relaxed);
        }
        out.reverse();
        out
    }
}

#[cfg(target_os = "linux")]
/// 共享内存上的 32 位字当 futex 用（不带 FUTEX_PRIVATE_FLAG，跨进程有效）。Miri 下退化成让出时间片。
pub trait FutexWord {
    /// 字还等于 `expected` 就睡，最多 `timeout`（醒来不保证字变了，调用方自己再查）。
    fn wait(&self, expected: u32, timeout: Duration);
    /// 叫醒在这个字上睡的全部线程（任何进程里的）。
    fn wake_all(&self);
}

#[cfg(all(target_os = "linux", not(miri)))]
impl FutexWord for AtomicU32 {
    fn wait(&self, expected: u32, timeout: Duration) {
        let ts = libc::timespec {
            tv_sec: libc::time_t::try_from(timeout.as_secs()).unwrap_or(libc::time_t::MAX),
            tv_nsec: timeout.subsec_nanos().into(),
        };
        // SAFETY: 字在共享内存里一直有效；FUTEX_WAIT 只读它
        unsafe { libc::syscall(libc::SYS_futex, self.as_ptr(), libc::FUTEX_WAIT, expected, &raw const ts) };
    }

    fn wake_all(&self) {
        // SAFETY: 同上
        unsafe { libc::syscall(libc::SYS_futex, self.as_ptr(), libc::FUTEX_WAKE, i32::MAX) };
    }
}

#[cfg(all(target_os = "linux", miri))]
impl FutexWord for AtomicU32 {
    fn wait(&self, _: u32, _: Duration) {
        std::thread::yield_now();
    }

    fn wake_all(&self) {}
}

/// 回收方放开回收权后，再抢之前的退让：时长随机（错开同时在抢的几个回收方，免得每次都一起撞上）、
/// 上限随次数翻倍，封顶 [`Retry::MAX`]。
#[cfg(target_os = "linux")]
pub struct Retry {
    round: u32,
    /// 每个回收方自己的随机源（std 的随机种子），不用全局状态
    seed: std::collections::hash_map::RandomState,
}

#[cfg(target_os = "linux")]
impl Retry {
    /// 第一次退让的上限
    const BASE: Duration = Duration::from_millis(2);
    /// 退让上限的封顶
    const MAX: Duration = Duration::from_millis(200);

    pub fn new() -> Self {
        Self { round: 0, seed: std::collections::hash_map::RandomState::new() }
    }

    pub fn pause(&mut self) {
        let cap = Self::BASE.saturating_mul(1 << self.round.min(16)).min(Self::MAX);
        let nanos = u64::try_from(cap.as_nanos()).unwrap_or(u64::MAX).max(1);
        let jitter = std::hash::BuildHasher::hash_one(&self.seed, self.round) % nanos;
        std::thread::sleep(Duration::from_nanos(jitter));
        self.round += 1;
    }
}
