//! 对象头：每个堆对象前面一个，记着怎么访问（追踪 / 重定位）、怎么析构、多大、哪一轮被标记过。
//!
//! 标记字平时存纪元；疏散时被搬走的对象用最高两位：[`BUSY`]（某个回收线程正在复制）、[`FORWARDED`]（已搬走，
//! 新地址写在原来放析构函数的那个字里）。纪元不会用到这两位。
use core::mem::needs_drop;
use core::sync::atomic::{AtomicU32, Ordering};

use crossbeam_utils::Backoff;

use crate::Trace;
use crate::layout::GRANULE;
use crate::trace::Tracer;

/// 头里 `info` 的布局：低 [`SIZE_BITS`] 位是粒度数，往上 [`RC_BITS`] 位是引用计数，再往上一位是计数的奇偶，最高位是大对象
const SIZE_BITS: u32 = 24;
const SIZE_MASK: u32 = (1 << SIZE_BITS) - 1;
const RC_BITS: u32 = 6;
const RC_SHIFT: u32 = SIZE_BITS;
/// 引用计数的上限：到了就粘住（不再加减），留给追踪回收
const RC_MAX: u32 = (1 << RC_BITS) - 1;
const RC_MASK: u32 = RC_MAX << RC_SHIFT;
/// 计数属于哪一「代」：完整回收重新计数时翻转，旧计数一次性作废
const PARITY_BIT: u32 = 1 << (RC_SHIFT + RC_BITS);
/// 大对象（单独分配，不在块里）
const LARGE_BIT: u32 = 1 << 31;
/// 标记字：已搬走，新地址在 `drop` 那个字里
const FORWARDED: u32 = 1 << 31;
/// 标记字：某个回收线程认领了它、正在复制
const BUSY: u32 = 1 << 30;
/// 纪元的上限（不能碰到 BUSY / FORWARDED 位）
pub(crate) const MAX_EPOCH: u32 = BUSY - 1;

#[repr(C)]
pub(crate) struct Header {
    /// 访问对象数据里的边：追踪（只读）或重定位（独占改写），由 Tracer 的模式决定
    visit: unsafe fn(*mut u8, &mut Tracer<'_>),
    /// 析构对象数据；类型不需要析构时为 None。搬走后这个字改存新地址
    drop: Option<unsafe fn(*mut u8)>,
    /// 最后一次被标记时的纪元，或 BUSY / FORWARDED
    mark: AtomicU32,
    /// 粒度数（含头）、引用计数、计数奇偶、大对象位（见 [`SIZE_BITS`] 等）
    info: AtomicU32,
}

/// 头的字节数；对象数据紧跟在后面
pub(crate) const HEADER: usize = size_of::<Header>();

const _: () = assert!(HEADER.is_multiple_of(GRANULE), "对象数据要按粒度对齐");
const _: () = assert!(size_of::<Option<unsafe fn(*mut u8)>>() == size_of::<*const Header>(), "转发地址要放得进 drop 那个字");

/// 认领疏散的结果。
pub(crate) enum Claim {
    /// 归我复制
    Mine,
    /// 别人已经搬走了，新头在这
    Moved(*const Header),
}

impl Header {
    /// `mark`：初始标记纪元。
    pub(crate) fn new<T: Trace>(bytes: usize, large: bool, mark: u32) -> Self {
        let granules = u32::try_from(bytes / GRANULE).expect("对象太大");
        assert!(granules <= SIZE_MASK, "对象太大（单个对象至多 128 MiB）");
        Self {
            visit: Self::visit_as::<T>,
            drop: needs_drop::<T>().then_some(Self::drop_as::<T> as unsafe fn(*mut u8)),
            mark: AtomicU32::new(mark),
            info: AtomicU32::new(granules | if large { LARGE_BIT } else { 0 }),
        }
    }

    /// SAFETY（调用方）：`data` 指向一个活着的 `T`；重定位模式时调用方独占它（没有 mutator、别的回收线程不碰它的数据）
    unsafe fn visit_as<T: Trace>(data: *mut u8, t: &mut Tracer<'_>) {
        if t.relocating() {
            // SAFETY: 见上
            unsafe { &mut *data.cast::<T>() }.relocate(t);
        } else {
            // SAFETY: 见上
            unsafe { &*data.cast::<T>() }.trace(t);
        }
    }

    /// SAFETY（调用方）：`data` 指向一个 `T`，之后不再使用
    unsafe fn drop_as<T>(data: *mut u8) {
        // SAFETY: 见上
        unsafe { data.cast::<T>().drop_in_place() };
    }

    /// 数据指针（头之后）。
    pub(crate) fn data(this: *const Self) -> *mut u8 {
        this.cast::<u8>().cast_mut().wrapping_add(HEADER)
    }

    /// 由数据指针找回头。
    pub(crate) fn of(data: *const u8) -> *const Self {
        data.wrapping_sub(HEADER).cast()
    }

    pub(crate) fn bytes(&self) -> usize {
        (self.info.load(Ordering::Relaxed) & SIZE_MASK) as usize * GRANULE
    }

    pub(crate) fn is_large(&self) -> bool {
        self.info.load(Ordering::Relaxed) & LARGE_BIT != 0
    }

    /// 这一轮第一次标记它时返回 true（只有一个线程会拿到 true）。
    pub(crate) fn mark(&self, epoch: u32) -> bool {
        self.mark.load(Ordering::Relaxed) != epoch && self.mark.swap(epoch, Ordering::AcqRel) != epoch
    }

    pub(crate) fn is_marked(&self, epoch: u32) -> bool {
        self.mark.load(Ordering::Acquire) == epoch
    }

    /// 引用计数加一（`parity` 是当前计数的奇偶：对不上说明是上一代的旧计数，从 1 起算）。粘住了就不变。
    pub(crate) fn rc_inc(&self, parity: bool) {
        let parity = if parity { PARITY_BIT } else { 0 };
        let mut word = self.info.load(Ordering::Relaxed);
        loop {
            let rc = if word & PARITY_BIT == parity { (word & RC_MASK) >> RC_SHIFT } else { 0 };
            if rc == RC_MAX {
                return;
            }
            let new = (word & !(RC_MASK | PARITY_BIT)) | parity | ((rc + 1) << RC_SHIFT);
            match self.info.compare_exchange_weak(word, new, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return,
                Err(now) => word = now,
            }
        }
    }

    /// 同 [`Header::rc_inc`]，但调用方保证此刻只有自己在改计数（单线程标记：mutator 不碰计数、减量队列为空）：
    /// 用普通的读、算、写，不用带锁的原子读改写（重新计数时每条边一次，单线程省下的就是这个）。
    pub(crate) fn rc_inc_exclusive(&self, parity: bool) {
        let parity = if parity { PARITY_BIT } else { 0 };
        let word = self.info.load(Ordering::Relaxed);
        let rc = if word & PARITY_BIT == parity { (word & RC_MASK) >> RC_SHIFT } else { 0 };
        if rc < RC_MAX {
            self.info.store((word & !(RC_MASK | PARITY_BIT)) | parity | ((rc + 1) << RC_SHIFT), Ordering::Relaxed);
        }
    }

    /// 引用计数减一；减到 0 返回 true。粘住的、旧一代的、已经是 0 的都不变，返回 false。
    pub(crate) fn rc_dec(&self, parity: bool) -> bool {
        let parity = if parity { PARITY_BIT } else { 0 };
        let mut word = self.info.load(Ordering::Relaxed);
        loop {
            if word & PARITY_BIT != parity {
                return false;
            }
            let rc = (word & RC_MASK) >> RC_SHIFT;
            if rc == RC_MAX || rc == 0 {
                return false;
            }
            let new = (word & !RC_MASK) | ((rc - 1) << RC_SHIFT);
            match self.info.compare_exchange_weak(word, new, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return rc == 1,
                Err(now) => word = now,
            }
        }
    }

    /// 已经被搬走（清扫时不析构、不算存活）。
    pub(crate) fn is_forwarded(&self) -> bool {
        self.mark.load(Ordering::Acquire) & FORWARDED != 0
    }

    /// 疏散：认领复制权；别人在复制就等，别人搬完了就给新地址。
    pub(crate) fn claim(&self) -> Claim {
        let backoff = Backoff::new();
        loop {
            let word = self.mark.load(Ordering::Acquire);
            if word & FORWARDED != 0 {
                return Claim::Moved(self.forwardee());
            }
            if word & BUSY != 0 {
                backoff.snooze();
                continue;
            }
            if self.mark.compare_exchange_weak(word, BUSY, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return Claim::Mine;
            }
        }
    }

    /// 复制完：写下新地址并发布。SAFETY（调用方）：本线程认领了它（[`Claim::Mine`]）。
    pub(crate) unsafe fn forward(this: *const Self, to: *const Self) {
        // SAFETY: 认领后只有本线程写这个字；别的线程在看到 FORWARDED（Release / Acquire）之前不读它
        unsafe { (&raw const (*this).drop).cast_mut().cast::<*const Self>().write(to) };
        // SAFETY: this 有效
        unsafe { &*this }.mark.store(FORWARDED, Ordering::Release);
    }

    fn forwardee(&self) -> *const Self {
        // SAFETY: 只在看到 FORWARDED 之后调用，新地址已经写好
        unsafe { (&raw const self.drop).cast::<*const Self>().read() }
    }

    /// 复制出来的新头：标成本轮。
    pub(crate) fn set_mark(&self, epoch: u32) {
        self.mark.store(epoch, Ordering::Release);
    }

    /// 访问对象数据（追踪或重定位）。SAFETY（调用方）：对象活着；重定位时调用方独占它。
    pub(crate) unsafe fn visit(this: *const Self, t: &mut Tracer<'_>) {
        // SAFETY: 见上；visit 是按对象自己的类型生成的
        unsafe { ((*this).visit)(Self::data(this), t) };
    }

    /// 析构对象数据。SAFETY（调用方）：对象已死、之后不再使用，而且没被搬走。
    pub(crate) unsafe fn finalize(this: *const Self) {
        // SAFETY: 见上；drop 是按对象自己的类型生成的
        if let Some(drop) = unsafe { (*this).drop } {
            unsafe { drop(Self::data(this)) };
        }
    }
}
