//! 对象头：每个堆对象前面一个，记着怎么追踪、怎么析构、多大、哪一轮被标记过。
use core::mem::needs_drop;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::Trace;
use crate::layout::GRANULE;
use crate::trace::Tracer;

/// 头里 `granules` 的最高位：大对象（单独分配，不在块里）
const LARGE_BIT: u32 = 1 << 31;

#[repr(C)]
pub(crate) struct Header {
    /// 追踪对象数据里的边
    trace: unsafe fn(*const u8, &mut Tracer<'_>),
    /// 析构对象数据；类型不需要析构时为 None
    drop: Option<unsafe fn(*mut u8)>,
    /// 最后一次被标记时的纪元；新对象是 0（纪元从 1 开始）
    mark: AtomicU32,
    /// 对象占的粒度数（含头），最高位是 [`LARGE_BIT`]
    granules: u32,
}

/// 头的字节数；对象数据紧跟在后面
pub(crate) const HEADER: usize = size_of::<Header>();

const _: () = assert!(HEADER.is_multiple_of(GRANULE), "对象数据要按粒度对齐");

impl Header {
    /// `mark`：初始标记纪元（并发标记期间分配的对象直接标成本轮，平时是 0）。
    pub(crate) fn new<T: Trace>(bytes: usize, large: bool, mark: u32) -> Self {
        let granules = u32::try_from(bytes / GRANULE).expect("对象太大");
        assert!(granules & LARGE_BIT == 0, "对象太大");
        Self {
            trace: Self::trace_as::<T>,
            drop: needs_drop::<T>().then_some(Self::drop_as::<T> as unsafe fn(*mut u8)),
            mark: AtomicU32::new(mark),
            granules: granules | if large { LARGE_BIT } else { 0 },
        }
    }

    /// SAFETY（调用方）：`data` 指向一个活着的 `T`
    unsafe fn trace_as<T: Trace>(data: *const u8, t: &mut Tracer<'_>) {
        // SAFETY: 见上
        unsafe { &*data.cast::<T>() }.trace(t);
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
        (self.granules & !LARGE_BIT) as usize * GRANULE
    }

    pub(crate) fn is_large(&self) -> bool {
        self.granules & LARGE_BIT != 0
    }

    /// 这一轮第一次标记它时返回 true（只有一个线程会拿到 true）。
    pub(crate) fn mark(&self, epoch: u32) -> bool {
        self.mark.load(Ordering::Relaxed) != epoch && self.mark.swap(epoch, Ordering::AcqRel) != epoch
    }

    pub(crate) fn is_marked(&self, epoch: u32) -> bool {
        self.mark.load(Ordering::Acquire) == epoch
    }

    /// 追踪对象数据。SAFETY（调用方）：对象活着。
    pub(crate) unsafe fn trace(this: *const Self, t: &mut Tracer<'_>) {
        // SAFETY: 见上；trace 是按对象自己的类型生成的
        unsafe { ((*this).trace)(Self::data(this), t) };
    }

    /// 析构对象数据。SAFETY（调用方）：对象已死、之后不再使用。
    pub(crate) unsafe fn finalize(this: *const Self) {
        // SAFETY: 见上；drop 是按对象自己的类型生成的
        if let Some(drop) = unsafe { (*this).drop } {
            unsafe { drop(Self::data(this)) };
        }
    }
}
