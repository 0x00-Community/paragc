//! 内存布局：尺寸常量、对象头（私有堆 [`Header`] 与公开堆 [`PHeader`] 共用 [`Core`]）、块与行、公开堆窗口里的各张表。
//!
//! 堆的尺寸常量：块 / 行 / 粒度 / 大对象阈值。
//!
//! 对象头：每个堆对象前面一个，记着怎么访问（追踪 / 重定位）、怎么析构、多大、哪一轮被标记过。
//!
//! 标记字平时存纪元；疏散时被搬走的对象用最高两位：[`BUSY`]（某个回收线程正在复制）、[`FORWARDED`]（已搬走，
//! 新地址写在原来放析构函数的那个字里）。纪元不会用到这两位。
//!
//! 块：按自身大小对齐的一段内存，开头是元数据（行标记 + 对象起点位图），后面按行切给小对象。
//!
//! - 行标记存纪元：标记阶段每个活对象把自己跨的行写成本轮纪元，于是「行标记 ≠ 当前纪元」就是空行
//! - 起点位图每粒度一位：清扫时据此逐个找到对象，析构死掉的
//! - 一个块在两次回收之间只归一个分配缓冲（[`crate::Tlab`]）所有，所以分配时写元数据没有竞争
//!
//! 公开堆窗口里的布局：位置全部定死，任何进程按窗口起点 + 常量偏移就能找到。
//!
//! ```text
//! 0          超级块 + 挂接表（1 页）
//! types      类型表：每个类型一项（类型键、大小、指针图在哪）
//! map        指针图：各类型里指针槽的偏移，接着往后排
//! entries    入口表：只有这一张，别处指进这个堆的引用各占一项
//! states     块状态表：每块一项（种类、可用栈里的下一个）
//! blocks     块区：从这里往后按需领
//! ```
//!
//! 文件刚建时一页都没有；每一段用到哪里才把文件延长到哪里（[`crate::shm::Region::commit`]），没写过的地方是空洞。
use core::mem::needs_drop;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::alloc::{Layout, handle_alloc_error};
#[cfg(any(not(target_os = "linux"), miri))]
use std::alloc::{alloc, dealloc};

#[cfg(all(target_os = "linux", not(miri)))]
use crossbeam_deque::{Injector, Steal};
use crossbeam_utils::Backoff;

use crate::Trace;
use crate::mark::{Tracer, TracerInternal};
#[cfg(target_os = "linux")]
use crate::shm::WINDOW;

/// 块大小：块按自身大小对齐，对象地址抹掉低位就是所在块的起点
pub const BLOCK: usize = 32 * 1024;
/// 行大小：回收的最小单位，空洞以行为单位找
pub const LINE: usize = 256;
/// 每块的行数
pub const LINES: usize = BLOCK / LINE;
/// 粒度：对象起点与大小都按它对齐，也是对象数据允许的最大对齐
pub const GRANULE: usize = 8;
/// 每块的粒度数（对象起点位图的位数）
pub const GRANULES: usize = BLOCK / GRANULE;
/// 起点位图每个字的位数
pub const BITS: usize = u64::BITS as usize;
/// 大对象阈值（含头的字节数）：超过就单独分配、不进块
pub const LARGE: usize = BLOCK / 2;
/// 不小于它的大对象（Linux）单独 mmap：释放时 munmap 立刻还给系统（全局分配器释放的大块常常留着不还）
#[cfg(all(target_os = "linux", not(miri)))]
pub const LARGE_MMAP: usize = 64 * 1024;

/// 回收线程数的估算：每个线程至少分到这么多个块 / 大对象才值得多开一个
pub const UNITS_PER_WORKER: usize = 8;

/// 触发回收的下限：上次回收后新领用的字节数不到它就不回收
pub const MIN_TRIGGER: usize = 4 * 1024 * 1024;
/// 触发回收的倍数：新领用的字节数超过「上次存活字节 × 它」就回收
pub const GROWTH: usize = 2;
/// collect_if_needed：引用计数不可信时，每这么多次年轻回收穿插一次完整回收（收掉死掉的老对象、重新计数）
pub const YOUNG_PER_FULL: usize = 4;
/// collect_if_needed：引用计数可信时（死掉的老对象由计数收），每这么多次年轻回收才一次完整回收（收环与计数粘住的）
pub const YOUNG_PER_FULL_RC: usize = 16;
/// 疏散候选：上次清扫后存活不到这么多字节的块（块的一半）
pub const EVACUATE_LIVE: usize = BLOCK / 2;
/// 疏散候选至少这么多个才值得疏散
pub const EVACUATE_MIN: usize = 4;
/// 一次至多疏散这么多个块（限住这次停顿）
pub const EVACUATE_MAX: usize = usize::MAX;
/// collect_if_needed：块占的字节比「存活 + 一轮允许新分配的」多出 FRAG_NUM / FRAG_DEN 倍就算碎了，做完整回收（带疏散）
pub const FRAG_NUM: usize = 3;
pub const FRAG_DEN: usize = 2;
/// 回收后至少留着复用的全空块数
pub const MIN_SPARE_BLOCKS: usize = 4;
/// 溢出分配领块时最多看几个可用块（找有放得下中对象的空洞的），都不行就开新块
pub const OVERFLOW_TRIES: usize = 8;

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
pub const MAX_EPOCH: u32 = BUSY - 1;

/// 两种堆共用的头部前缀：标记字与信息字。块、行、清扫只看这一段，所以私有堆与公开堆共用块的代码。
#[repr(C)]
pub struct Core {
    /// 最后一次被标记时的纪元，或 BUSY / FORWARDED
    mark: AtomicU32,
    /// 粒度数（含头）、引用计数、计数奇偶、大对象位（见 [`SIZE_BITS`] 等）
    info: AtomicU32,
}

#[repr(C)]
pub struct Header {
    core: Core,
    /// 访问对象数据里的边：追踪（只读）或重定位（独占改写），由 Tracer 的模式决定
    visit: unsafe fn(*mut u8, &mut Tracer<'_>),
    /// 析构对象数据；类型不需要析构时为 None。搬走后这个字改存新地址
    drop: Option<unsafe fn(*mut u8)>,
}

/// 头的字节数；对象数据紧跟在后面
pub const HEADER: usize = size_of::<Header>();

const _: () = assert!(HEADER.is_multiple_of(GRANULE), "对象数据要按粒度对齐");
const _: () = assert!(size_of::<Option<unsafe fn(*mut u8)>>() == size_of::<*const Header>(), "转发地址要放得进 drop 那个字");

/// 认领疏散的结果。
pub enum Claim {
    /// 归我复制
    Mine,
    /// 别人已经搬走了，新头在这
    Moved(*const Header),
}

impl core::ops::Deref for Header {
    type Target = Core;

    fn deref(&self) -> &Core {
        &self.core
    }
}

impl Header {
    /// `mark`：初始标记纪元。
    pub fn new<T: Trace>(bytes: usize, large: bool, mark: u32) -> Self {
        Self {
            core: Core::new(bytes, large, mark),
            visit: Self::visit_as::<T>,
            drop: needs_drop::<T>().then_some(Self::drop_as::<T> as unsafe fn(*mut u8)),
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
    pub fn data(this: *const Self) -> *mut u8 {
        this.cast::<u8>().cast_mut().wrapping_add(HEADER)
    }

    /// 由数据指针找回头。
    pub fn of(data: *const u8) -> *const Self {
        data.wrapping_sub(HEADER).cast()
    }
}

impl Core {
    pub fn new(bytes: usize, large: bool, mark: u32) -> Self {
        let granules = u32::try_from(bytes / GRANULE).expect("对象太大");
        assert!(granules <= SIZE_MASK, "对象太大（单个对象至多 128 MiB）");
        Self { mark: AtomicU32::new(mark), info: AtomicU32::new(granules | if large { LARGE_BIT } else { 0 }) }
    }

    pub fn bytes(&self) -> usize {
        (self.info.load(Ordering::Relaxed) & SIZE_MASK) as usize * GRANULE
    }

    pub fn is_large(&self) -> bool {
        self.info.load(Ordering::Relaxed) & LARGE_BIT != 0
    }

    /// 这一轮第一次标记它时返回 true（只有一个线程会拿到 true）。
    pub fn mark(&self, epoch: u32) -> bool {
        self.mark.load(Ordering::Relaxed) != epoch && self.mark.swap(epoch, Ordering::AcqRel) != epoch
    }

    pub fn is_marked(&self, epoch: u32) -> bool {
        self.mark.load(Ordering::Acquire) == epoch
    }

    /// 引用计数加一（`parity` 是当前计数的奇偶：对不上说明是上一代的旧计数，从 1 起算）。粘住了就不变。
    pub fn rc_inc(&self, parity: bool) {
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

    /// 同 [`Core::rc_inc`]，但调用方保证此刻只有自己在改计数（单线程标记：mutator 不碰计数、减量队列为空）：
    /// 用普通的读、算、写，不用带锁的原子读改写（重新计数时每条边一次，单线程省下的就是这个）。
    pub fn rc_inc_exclusive(&self, parity: bool) {
        let parity = if parity { PARITY_BIT } else { 0 };
        let word = self.info.load(Ordering::Relaxed);
        let rc = if word & PARITY_BIT == parity { (word & RC_MASK) >> RC_SHIFT } else { 0 };
        if rc < RC_MAX {
            self.info.store((word & !(RC_MASK | PARITY_BIT)) | parity | ((rc + 1) << RC_SHIFT), Ordering::Relaxed);
        }
    }

    /// 引用计数减一；减到 0 返回 true。粘住的、旧一代的、已经是 0 的都不变，返回 false。
    pub fn rc_dec(&self, parity: bool) -> bool {
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
    pub fn is_forwarded(&self) -> bool {
        self.mark.load(Ordering::Acquire) & FORWARDED != 0
    }

    /// 复制出来的新头：标成本轮。
    pub fn set_mark(&self, epoch: u32) {
        self.mark.store(epoch, Ordering::Release);
    }
}

impl Header {
    /// 疏散：认领复制权；别人在复制就等，别人搬完了就给新地址。
    pub fn claim(&self) -> Claim {
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
    pub unsafe fn forward(this: *const Self, to: *const Self) {
        // SAFETY: 认领后只有本线程写这个字；别的线程在看到 FORWARDED（Release / Acquire）之前不读它
        unsafe { (&raw const (*this).drop).cast_mut().cast::<*const Self>().write(to) };
        // SAFETY: this 有效
        unsafe { &*this }.core.mark.store(FORWARDED, Ordering::Release);
    }

    fn forwardee(&self) -> *const Self {
        // SAFETY: 只在看到 FORWARDED 之后调用，新地址已经写好
        unsafe { (&raw const self.drop).cast::<*const Self>().read() }
    }

    /// 访问对象数据（追踪或重定位）。SAFETY（调用方）：对象活着；重定位时调用方独占它。
    pub unsafe fn visit(this: *const Self, t: &mut Tracer<'_>) {
        // SAFETY: 见上；visit 是按对象自己的类型生成的
        unsafe { ((*this).visit)(Self::data(this), t) };
    }

    /// 析构对象数据。SAFETY（调用方）：对象已死、之后不再使用，而且没被搬走。
    pub unsafe fn finalize(this: *const Self) {
        // SAFETY: 见上；drop 是按对象自己的类型生成的
        if let Some(drop) = unsafe { (*this).drop } {
            unsafe { drop(Self::data(this)) };
        }
    }
}

#[repr(C)]
struct Meta {
    /// 每行最后一次被活对象占用时的纪元
    lines: [AtomicU32; LINES],
    /// 对象起点：每粒度一位
    starts: [AtomicU64; GRANULES / BITS],
    /// 上次清扫时的存活字节数；交给分配缓冲（或刚开出来）后是 [`REUSED`]（不知道，不当疏散候选）
    live: AtomicU32,
    /// 这一轮要疏散：里面的活对象被追踪到时搬走
    evacuate: AtomicBool,
    /// 在可用队列里（防止同一块进队两次）
    listed: AtomicBool,
}

/// 清扫时怎么析构死对象：私有堆调对象自己的析构函数，公开堆的对象没有析构。
pub trait Finalize {
    /// SAFETY（调用方）：`object` 是死对象的头，之后不再使用。
    unsafe fn finalize(object: *const Core);
}

impl Finalize for Header {
    unsafe fn finalize(object: *const Core) {
        // SAFETY: 私有堆块里的对象头都是 Header（Core 在它开头）；见上
        unsafe { Header::finalize(object.cast()) };
    }
}

/// [`Meta::live`]：块被重新拿去分配了，上次清扫的存活量不再作数
const REUSED: u32 = u32::MAX;

/// 清扫一个块的结果。
#[derive(Clone, Copy)]
pub enum State {
    /// 没有活对象
    Empty,
    /// 还有空行
    Recyclable,
    /// 没有空行
    Full,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Block(NonNull<Meta>);

// SAFETY: 块是一段独立的堆内存；元数据都是原子量，对象数据的跨线程访问由 Trace: Send + Sync 担保
unsafe impl Send for Block {}
// SAFETY: 同上
unsafe impl Sync for Block {}

impl Block {
    const LAYOUT: Layout = match Layout::from_size_align(BLOCK, BLOCK) {
        Ok(layout) => layout,
        Err(_) => panic!("块的大小 / 对齐不合法"),
    };
    /// 元数据之后第一个能放对象的行
    pub const FIRST_LINE: usize = size_of::<Meta>().div_ceil(LINE);

    /// 新块（从 `pages` 领）：只有元数据清零（行标记 0 早于任何纪元，全部是空行）；对象区不清——分配从不依赖内存是零，
    /// 不碰的页也就不缺页。
    pub fn new(pages: &Pages) -> Self {
        let b = Self(pages.take());
        // SAFETY: 刚领到、独有；元数据全是原子量，全零是合法值
        unsafe { b.0.as_ptr().write_bytes(0, 1) };
        b.reused();
        b
    }

    #[cfg(target_os = "linux")]
    /// 公开堆窗口里的块。SAFETY（调用方）：`p` 按块大小对齐、有 [`BLOCK`] 字节可读写；
    /// 第一次用时内存是零（新延长的文件页）或已经 [`Block::reset`] 过。
    pub unsafe fn at(p: *mut u8) -> Self {
        // SAFETY: 见上
        Self(unsafe { NonNull::new_unchecked(p.cast()) })
    }

    #[cfg(target_os = "linux")]
    /// 元数据清零（大对象占过的块还回来时）。SAFETY（调用方）：块里没有对象，没人在用它。
    pub unsafe fn reset(self) {
        // SAFETY: 见上；元数据全是原子量，全零是合法值
        unsafe { self.0.as_ptr().write_bytes(0, 1) };
    }

    /// 交给分配缓冲：上次清扫的存活量不再作数。
    pub fn reused(self) {
        self.meta().live.store(REUSED, Ordering::Relaxed);
    }

    /// 上次清扫后没再分配过时的存活字节数。
    pub fn live_since_sweep(self) -> Option<usize> {
        let live = self.meta().live.load(Ordering::Relaxed);
        (live != REUSED).then_some(live as usize)
    }

    /// 要放进可用队列：之前不在里面才返回 true。
    pub fn list(self) -> bool {
        !self.meta().listed.swap(true, Ordering::AcqRel)
    }

    /// 从可用队列里拿出来了。
    pub fn unlist(self) {
        self.meta().listed.store(false, Ordering::Release);
    }

    /// 清掉 `p` 处的对象起点（引用计数释放了它）。
    pub fn clear_start(self, p: *const u8) {
        let g = self.offset(p) / GRANULE;
        self.meta().starts[g / BITS].fetch_and(!(1 << (g % BITS)), Ordering::Relaxed);
    }

    /// 按还在的老对象（标记纪元 = `epoch`）重算行标记（引用计数释放过对象的块）：它们占的行标成 `epoch`，其余行空出来；
    /// 年轻对象的行不标（同一次停顿里年轻追踪会给活下来的标上，死掉的由清扫收）。
    /// 块上次清扫后被拿去分配过（里面可能有还没扫的死年轻对象）：保留这个状态、返回 false，交给年轻清扫分类；
    /// 否则记下存活字节数，返回是否有空行（可以直接放回可用队列）。
    /// SAFETY（调用方）：没有 mutator（独占堆），起点位标着的对象都还没析构。
    pub unsafe fn recompute_lines(self, epoch: u32) -> bool {
        let meta = self.meta();
        let reused = meta.live.load(Ordering::Relaxed) == REUSED;
        for line in &meta.lines {
            line.store(0, Ordering::Relaxed);
        }
        let mut live = 0;
        for (w, word) in meta.starts.iter().enumerate() {
            let mut bits = word.load(Ordering::Relaxed);
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let p = self.base().wrapping_add((w * BITS + bit) * GRANULE);
                // SAFETY: 起点位标着的地方是还没析构的对象
                let h = unsafe { &*p.cast::<Core>() };
                if h.is_marked(epoch) {
                    self.mark_lines(p, h.bytes(), epoch);
                    live += h.bytes();
                }
            }
        }
        if reused {
            return false;
        }
        meta.live.store(u32::try_from(live).unwrap_or(REUSED - 1), Ordering::Relaxed);
        meta.lines[Self::FIRST_LINE..].iter().any(|l| l.load(Ordering::Relaxed) != epoch)
    }

    pub fn set_evacuate(self) {
        self.meta().evacuate.store(true, Ordering::Relaxed);
    }

    /// 不疏散这个块（里面有别的堆直接指着的对象）；原来要疏散返回 true。
    pub fn pin(self) -> bool {
        self.meta().evacuate.swap(false, Ordering::Relaxed)
    }

    pub fn is_evacuating(self) -> bool {
        self.meta().evacuate.load(Ordering::Relaxed)
    }

    /// 还给 `pages`（物理内存还给系统）。SAFETY（调用方）：块来自同一个 `pages`，里面没有活对象（死的已析构），之后不再使用。
    pub unsafe fn release(self, pages: &Pages) {
        // SAFETY: 见上
        unsafe { pages.give(self.0) };
    }

    /// 地址所在的块（地址必须在某个块里）。
    pub fn containing(p: *const u8) -> Self {
        let base = p.map_addr(|a| a & !(BLOCK - 1)).cast_mut().cast::<Meta>();
        // SAFETY: 块起点不会是 0
        Self(unsafe { NonNull::new_unchecked(base) })
    }

    fn meta(&self) -> &Meta {
        // SAFETY: 块活着时元数据一直有效
        unsafe { self.0.as_ref() }
    }

    fn base(self) -> *mut u8 {
        self.0.as_ptr().cast()
    }

    /// 第 `i` 行的起点（`i == LINES` 是块尾）。
    pub fn line(self, i: usize) -> *mut u8 {
        self.base().wrapping_add(i * LINE)
    }

    fn offset(self, p: *const u8) -> usize {
        p.addr() - self.base().addr()
    }

    /// 标记 `[p, p + bytes)` 跨过的行。
    pub fn mark_lines(self, p: *const u8, bytes: usize, epoch: u32) {
        let first = self.offset(p) / LINE;
        let last = (self.offset(p) + bytes - 1) / LINE;
        for line in &self.meta().lines[first..=last] {
            line.store(epoch, Ordering::Relaxed);
        }
    }

    /// 记下 `p` 处有一个对象。
    pub fn set_start(self, p: *const u8) {
        let g = self.offset(p) / GRANULE;
        self.meta().starts[g / BITS].fetch_or(1 << (g % BITS), Ordering::Relaxed);
    }

    /// 有没有放得下 `bytes` 字节的空洞（连续空行）。
    pub fn has_hole(self, bytes: usize, epoch: u32) -> bool {
        let need = bytes.div_ceil(LINE);
        let mut from = Self::FIRST_LINE;
        while let Some((start, end)) = self.hole(from, epoch) {
            if end - start >= need {
                return true;
            }
            from = end;
        }
        false
    }

    /// 从第 `from` 行起的第一个空洞（一段连续空行），返回 `[起, 止)` 行号。
    /// 空行 = 行标记早于上次完成的纪元 `epoch`：并发标记期间标记方会把活行写成更新的纪元，那些也不是空行。
    pub fn hole(self, from: usize, epoch: u32) -> Option<(usize, usize)> {
        let lines = &self.meta().lines;
        let free = |i: usize| lines[i].load(Ordering::Relaxed) < epoch;
        let start = (from..LINES).find(|&i| free(i))?;
        let end = (start..LINES).find(|&i| !free(i)).unwrap_or(LINES);
        Some((start, end))
    }

    /// 清扫：析构（按 `K`）没标上本轮纪元的对象（搬走了的不析构）、清掉它们的起点位，返回块的状态与存活字节数。
    /// 给了 `deferred` 就不当场析构，把死对象的头地址放进去（调用方析构完才能把块交出去）。
    /// SAFETY（调用方）：标记已经做完，没有 mutator 在用这个块；块里对象的头是 `K` 认识的那种。
    pub unsafe fn sweep<K: Finalize>(self, epoch: u32, mut deferred: Option<&mut Vec<usize>>) -> (State, usize) {
        let mut live = 0;
        for (w, word) in self.meta().starts.iter().enumerate() {
            let mut bits = word.load(Ordering::Relaxed);
            let mut dead = 0u64;
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let header = self.base().wrapping_add((w * BITS + bit) * GRANULE).cast::<Core>().cast_const();
                // SAFETY: 起点位标着的地方都是已初始化的对象
                let h = unsafe { &*header };
                if h.is_forwarded() {
                    // 活着的是搬过去的新副本；旧的只清起点位
                    dead |= 1 << bit;
                } else if h.is_marked(epoch) {
                    live += h.bytes();
                } else {
                    match deferred.as_deref_mut() {
                        Some(list) => list.push(header.expose_provenance()),
                        // SAFETY: 没标记 ⇒ 从根不可达，之后没人再用
                        None => unsafe { K::finalize(header) },
                    }
                    dead |= 1 << bit;
                }
            }
            if dead != 0 {
                word.fetch_and(!dead, Ordering::Relaxed);
            }
        }
        let lines = &self.meta().lines[Self::FIRST_LINE..];
        let used = lines.iter().filter(|l| l.load(Ordering::Relaxed) == epoch).count();
        let state = match used {
            0 => State::Empty,
            n if n == lines.len() => State::Full,
            _ => State::Recyclable,
        };
        self.meta().live.store(u32::try_from(live).unwrap_or(REUSED - 1), Ordering::Relaxed);
        self.meta().evacuate.store(false, Ordering::Relaxed);
        (state, live)
    }
}

/// 一次向系统映射多大的一段，切成块（Linux）
#[cfg(all(target_os = "linux", not(miri)))]
const CHUNK: usize = 2 * 1024 * 1024;

/// 私有堆的块从这里领、还回这里（每个堆一份）。
///
/// Linux：自己 mmap 一段段（[`CHUNK`]），切成按 [`BLOCK`] 对齐的块放进空闲池；还回来的块先 `madvise(MADV_DONTNEED)`
/// 把物理内存还给系统，地址进池复用（不经 glibc：对齐分配有额外开销，释放的内存它也不还给系统）。
/// 映射的段在堆析构时一起解除。其他平台与 Miri 下走全局分配器。
#[derive(Default)]
pub struct Pages {
    /// 空闲的块（地址）
    #[cfg(all(target_os = "linux", not(miri)))]
    free: Injector<usize>,
    /// 映射过的段（起点地址），析构时解除
    #[cfg(all(target_os = "linux", not(miri)))]
    chunks: Injector<usize>,
}

impl Pages {
    #[cfg(all(target_os = "linux", not(miri)))]
    fn take(&self) -> NonNull<Meta> {
        loop {
            match self.free.steal() {
                // SAFETY: 池里只有本对象切出来、非零的块地址
                Steal::Success(addr) => return unsafe { NonNull::new_unchecked(core::ptr::with_exposed_provenance_mut(addr)) },
                Steal::Retry => {}
                Steal::Empty => break,
            }
        }
        let base = self.map_chunk();
        // 第一块自己用，其余进池
        for i in 1..CHUNK / BLOCK {
            self.free.push(base.wrapping_add(i * BLOCK).expose_provenance());
        }
        // SAFETY: mmap 成功的地址非零
        unsafe { NonNull::new_unchecked(base.cast()) }
    }

    /// 映射一段按 [`BLOCK`] 对齐的 [`CHUNK`] 字节（多映射一块，对齐后把头尾解除）。
    #[cfg(all(target_os = "linux", not(miri)))]
    fn map_chunk(&self) -> *mut u8 {
        let len = CHUNK + BLOCK;
        // SAFETY: 匿名私有映射，参数合法
        let raw = unsafe { libc::mmap(core::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE, -1, 0) };
        if raw == libc::MAP_FAILED {
            handle_alloc_error(Block::LAYOUT);
        }
        let raw = raw.cast::<u8>();
        let head = raw.align_offset(BLOCK);
        let base = raw.wrapping_add(head);
        // SAFETY: 两段都在刚才的映射里、互不重叠；长度为 0 时不调
        unsafe {
            if head > 0 {
                libc::munmap(raw.cast(), head);
            }
            let tail = BLOCK - head;
            if tail > 0 {
                libc::munmap(base.wrapping_add(CHUNK).cast(), tail);
            }
        }
        self.chunks.push(base.expose_provenance());
        base
    }

    /// SAFETY（调用方）：`p` 来自本对象的 [`Pages::take`]，之后不再使用。
    #[cfg(all(target_os = "linux", not(miri)))]
    unsafe fn give(&self, p: NonNull<Meta>) {
        // SAFETY: 块在本对象映射的段里；DONTNEED 只丢内容，地址仍可用
        unsafe { libc::madvise(p.as_ptr().cast(), BLOCK, libc::MADV_DONTNEED) };
        self.free.push(p.as_ptr().expose_provenance());
    }

    #[cfg(any(not(target_os = "linux"), miri))]
    #[expect(clippy::unused_self, reason = "与 Linux 版同签名")]
    fn take(&self) -> NonNull<Meta> {
        // SAFETY: LAYOUT 大小非零
        let p = unsafe { alloc(Block::LAYOUT) };
        NonNull::new(p.cast()).unwrap_or_else(|| handle_alloc_error(Block::LAYOUT))
    }

    #[cfg(any(not(target_os = "linux"), miri))]
    #[expect(clippy::unused_self, reason = "与 Linux 版同签名")]
    unsafe fn give(&self, p: NonNull<Meta>) {
        // SAFETY: 来自 take 的同一 LAYOUT
        unsafe { dealloc(p.as_ptr().cast(), Block::LAYOUT) };
    }
}

#[cfg(all(target_os = "linux", not(miri)))]
impl Drop for Pages {
    fn drop(&mut self) {
        loop {
            match self.chunks.steal() {
                // SAFETY: 段是 map_chunk 映射的 CHUNK 字节；堆析构时块都已还回，没人再用
                Steal::Success(addr) => unsafe {
                    libc::munmap(core::ptr::with_exposed_provenance_mut::<libc::c_void>(addr), CHUNK);
                },
                Steal::Retry => {}
                Steal::Empty => return,
            }
        }
    }
}

/// 超级块里的魔数：认出这是 paragc 的公开堆、布局版本对得上
#[cfg(target_os = "linux")]
pub const MAGIC: u64 = u64::from_le_bytes(*b"paragcP4");
/// 页大小：各段按页对齐、按页延长文件
#[cfg(target_os = "linux")]
pub const PAGE: usize = 4096;
/// 类型表容量
#[cfg(target_os = "linux")]
pub const TYPE_CAP: usize = 1024;
/// 指针图容量（全部类型的指针槽偏移加起来）
#[cfg(target_os = "linux")]
pub const MAP_CAP: usize = 64 * 1024;
/// 窗口里每这么多字节给一个入口
#[cfg(target_os = "linux")]
pub const BYTES_PER_ENTRY: usize = 16 * 1024;
/// 挂接表容量：同时映射这个堆的地方（进程，或同一进程里的多个地址区）最多这么多
#[cfg(target_os = "linux")]
pub const ATTACH_CAP: usize = 32;
/// 挂接表在超级块那一页里的偏移
#[cfg(target_os = "linux")]
pub const ATTACH_OFF: usize = 1024;

/// 超级块（窗口偏移 0）。跨进程共享：除了建堆时写一次的字段，全是原子量。
#[repr(C)]
#[cfg(target_os = "linux")]
pub struct Super {
    pub magic: AtomicU64,
    /// 窗口大小（布局由它算出）
    pub window: AtomicU64,
    /// 堆号 = 窗口号
    pub heap: AtomicU64,
    /// 回收请求（futex 字）：0 = 没人在回收，否则是回收方的挂接槽号 + 1；非 0 时新的 mutate 先等
    pub request: AtomicU32,
    /// 最近一次开始的回收用的纪元（回收开始时就写）：回收方被杀、别人接手时用比它新的纪元从头标
    pub marking: AtomicU32,
    /// 最后一次完成的标记的纪元
    pub epoch: AtomicU32,
    /// 开过的块数（块区里下一个新块的序号）
    pub blocks: AtomicU32,
    pub collections: AtomicU64,
    /// 回收方被杀、别人接手重做的次数
    pub takeovers: AtomicU64,
    /// 可用块栈的头（见 [`crate::sync::Stack`]）
    pub available: AtomicU64,
    /// 类型表里下一项
    pub types: AtomicU32,
    /// 指针图里下一个空位
    pub map: AtomicU32,
    /// 入口表里下一个没用过的项
    pub entries: AtomicU32,
    _pad: u32,
    /// 还回来的入口栈的头
    pub free_entries: AtomicU64,
    /// 文件已经延长到的地方（窗口偏移）：类型表、指针图、入口表、块状态表各一个
    pub committed: [AtomicU64; Segment::COUNT],
    /// 上次回收后存活的字节数
    pub live: AtomicU64,
    /// 上次回收后新领用的字节数
    pub allocated: AtomicU64,
    /// 堆身份：建堆时随机取的 64 位数。堆号会被重用，身份不会：指向这个堆的引用、登记都带着它，对不上就是指向一个已经死掉的旧堆
    pub incarnation: AtomicU64,
    /// 主人（建堆进程）的进程号与启动时间（见 [`crate::shm::Process`]）：主人不在了、又没有 [`Super::persist`]，堆就死了
    pub owner_pid: AtomicU32,
    /// 非 0：主人不在了堆也留着（[`crate::PublicHeap::persist`]）
    pub persist: AtomicU32,
    pub owner_start: AtomicU64,
    /// 非 0：堆已经死了（主人走了），文件已经或正在删除；还映射着它的进程只能放手
    pub dead: AtomicU32,
    _pad_dead: u32,
}

#[cfg(target_os = "linux")]
impl Super {
    /// 新堆的超级块：纪元从 1 开始（新对象的标记 0、新块的行标记 0 都早于它）。
    pub fn new(heap: u64, incarnation: u64, owner: crate::shm::Process) -> Self {
        Self {
            magic: AtomicU64::new(MAGIC),
            window: AtomicU64::new(WINDOW as u64),
            heap: AtomicU64::new(heap),
            request: AtomicU32::new(0),
            marking: AtomicU32::new(0),
            epoch: AtomicU32::new(1),
            blocks: AtomicU32::new(0),
            collections: AtomicU64::new(0),
            takeovers: AtomicU64::new(0),
            available: AtomicU64::new(0),
            types: AtomicU32::new(0),
            map: AtomicU32::new(0),
            entries: AtomicU32::new(0),
            _pad: 0,
            free_entries: AtomicU64::new(0),
            committed: core::array::from_fn(|_| AtomicU64::new(0)),
            live: AtomicU64::new(0),
            allocated: AtomicU64::new(0),
            incarnation: AtomicU64::new(incarnation),
            owner_pid: AtomicU32::new(owner.pid),
            persist: AtomicU32::new(0),
            owner_start: AtomicU64::new(owner.start),
            dead: AtomicU32::new(0),
            _pad_dead: 0,
        }
    }
}

/// 挂接表的一项（一条缓存行，免得各进程的计数互相挤）。
#[repr(C, align(64))]
#[cfg(target_os = "linux")]
pub struct Attachment {
    /// 进程号；0 = 空槽
    pub pid: AtomicU32,
    /// 这个挂接里在 mutate 中的线程数（futex 字：回收方等它归零）
    pub inside: AtomicU32,
    /// 进程启动时间（见 [`crate::shm::Process`]）；0 = 还在登记
    pub start: AtomicU64,
    /// 这个挂接里因为回收请求挡在门口、还没进去的线程数（futex 字）：回收方先让它们进去再抢下一次，免得把 mutator 饿死
    pub waiting: AtomicU32,
}

#[cfg(target_os = "linux")]
const _: () = assert!(size_of::<Super>() <= ATTACH_OFF && ATTACH_OFF + ATTACH_CAP * size_of::<Attachment>() <= PAGE, "挂接表放在超级块那一页里");

/// 按需延长文件的几段。
#[derive(Clone, Copy)]
#[cfg(target_os = "linux")]
pub enum Segment {
    Types,
    Map,
    Entries,
    States,
}

#[cfg(target_os = "linux")]
impl Segment {
    pub const COUNT: usize = 4;

    pub fn index(self) -> usize {
        self as usize
    }
}

/// 类型表的一项。
#[repr(C)]
#[cfg(target_os = "linux")]
pub struct TypeEntry {
    /// 写完其余字段后置 1（之后才可以按它查、按它追踪）
    pub ready: AtomicU32,
    /// 指针槽个数
    pub edges: AtomicU32,
    /// 指针图里从第几个开始
    pub map: AtomicU32,
    /// 对象数据的字节数
    pub size: AtomicU32,
    /// 类型键：类型名、大小、指针图一起算的 128 位哈希
    pub key: [AtomicU64; 2],
}

#[cfg(target_os = "linux")]
impl TypeEntry {
    /// 登记完了、类型键是 `key`。
    pub fn has_key(&self, key: [u64; 2]) -> bool {
        self.ready.load(core::sync::atomic::Ordering::Acquire) == 1 && self.key.iter().zip(key).all(|(k, v)| k.load(core::sync::atomic::Ordering::Relaxed) == v)
    }
}

/// 入口表的一项。
#[repr(C)]
#[cfg(target_os = "linux")]
pub struct Entry {
    /// 目标对象数据的公开偏移；0 = 空
    pub ptr: AtomicU64,
    /// 持有方（0 = 不归任何人；挂接槽号 + 1；或 [`crate::heap::HEAP_HOLDER`] | 堆号）
    pub holder: AtomicU32,
    /// 在还回来的入口栈里时：下一个（见 [`crate::sync::Stack`]）
    pub next: AtomicU32,
    /// 持有方是另一个公开堆时，那个堆的身份（[`Super::incarnation`]）：认出它已经死了、号被别人拿去了
    pub holder_incarnation: AtomicU64,
}

/// 块状态表的一项。
#[repr(C)]
#[cfg(target_os = "linux")]
pub struct BlockState {
    /// [`Kind`]
    pub kind: AtomicU32,
    /// 在可用块栈里时：下一个
    pub next: AtomicU32,
}

/// 块的种类。
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg(target_os = "linux")]
pub enum Kind {
    /// 还没开过
    Unused = 0,
    /// 按行切给小对象
    Small = 1,
    /// 一个大对象的第一块（对象头在块的开头）
    LargeHead = 2,
    /// 大对象后面的块
    LargeTail = 3,
}

#[cfg(target_os = "linux")]
impl From<u32> for Kind {
    fn from(v: u32) -> Self {
        match v {
            1 => Self::Small,
            2 => Self::LargeHead,
            3 => Self::LargeTail,
            _ => Self::Unused,
        }
    }
}

/// 各段在窗口里的偏移（由窗口大小算出，所以挂上来的进程算出的一样）。
#[derive(Clone, Copy)]
#[cfg(target_os = "linux")]
pub struct WindowLayout {
    pub types: usize,
    pub map: usize,
    pub entries: usize,
    pub entry_cap: usize,
    pub states: usize,
    pub blocks: usize,
    pub max_blocks: usize,
}

#[cfg(target_os = "linux")]
impl WindowLayout {
    pub fn new() -> Self {
        let types = size_of::<Super>().next_multiple_of(PAGE);
        let map = types + TYPE_CAP * size_of::<TypeEntry>();
        let entries = (map + MAP_CAP * size_of::<u32>()).next_multiple_of(PAGE);
        let entry_cap = WINDOW / BYTES_PER_ENTRY;
        let states = (entries + entry_cap * size_of::<Entry>()).next_multiple_of(PAGE);
        let blocks = (states + WINDOW / BLOCK * size_of::<BlockState>()).next_multiple_of(BLOCK);
        let max_blocks = (WINDOW - blocks) / BLOCK;
        assert!(max_blocks > 0, "窗口放不下任何块");
        Self { types, map, entries, entry_cap, states, blocks, max_blocks }
    }

    /// 某一段的起点与终点（窗口偏移）。
    pub fn segment(&self, s: Segment) -> (usize, usize) {
        match s {
            Segment::Types => (self.types, self.map),
            Segment::Map => (self.map, self.entries),
            Segment::Entries => (self.entries, self.states),
            Segment::States => (self.states, self.blocks),
        }
    }
}

/// 公开堆的对象头（16 B）：两种堆共用的标记字 / 信息字，加类型号。
#[repr(C)]
#[cfg(target_os = "linux")]
pub struct PHeader {
    pub core: Core,
    /// 类型表里的序号
    pub ty: u32,
    _pad: u32,
}

/// 公开堆对象头的字节数；数据紧跟在后面
#[cfg(target_os = "linux")]
pub const PHEADER: usize = size_of::<PHeader>();

#[cfg(target_os = "linux")]
const _: () = assert!(PHEADER == 16, "公开堆的对象头是 16 字节");

#[cfg(target_os = "linux")]
impl PHeader {
    pub fn new(bytes: usize, large: bool, ty: u32) -> Self {
        // 新对象的标记 0 不等于任何纪元（纪元从 1 开始）
        Self { core: Core::new(bytes, large, 0), ty, _pad: 0 }
    }

    pub fn data(this: *const Self) -> *mut u8 {
        this.cast::<u8>().cast_mut().wrapping_add(PHEADER)
    }

    pub fn of(data: *const u8) -> *const Self {
        data.wrapping_sub(PHEADER).cast()
    }
}

// 公开堆的对象没有析构
#[cfg(target_os = "linux")]
impl Finalize for PHeader {
    unsafe fn finalize(_: *const Core) {}
}
