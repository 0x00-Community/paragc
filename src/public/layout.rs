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
//! 文件刚建时一页都没有；每一段用到哪里才把文件延长到哪里（[`super::region::Region::commit`]），没写过的地方是空洞。
use core::sync::atomic::{AtomicU32, AtomicU64};

use crate::layout::BLOCK;

use super::region::WINDOW;

/// 超级块里的魔数：认出这是 paragc 的公开堆、布局版本对得上
pub(crate) const MAGIC: u64 = u64::from_le_bytes(*b"paragcP2");
/// 页大小：各段按页对齐、按页延长文件
pub(crate) const PAGE: usize = 4096;
/// 类型表容量
pub(crate) const TYPE_CAP: usize = 1024;
/// 指针图容量（全部类型的指针槽偏移加起来）
pub(crate) const MAP_CAP: usize = 64 * 1024;
/// 窗口里每这么多字节给一个入口
pub(crate) const BYTES_PER_ENTRY: usize = 16 * 1024;
/// 挂接表容量：同时映射这个堆的地方（进程，或同一进程里的多个地址区）最多这么多
pub(crate) const ATTACH_CAP: usize = 32;
/// 挂接表在超级块那一页里的偏移
pub(crate) const ATTACH_OFF: usize = 1024;

/// 超级块（窗口偏移 0）。跨进程共享：除了建堆时写一次的字段，全是原子量。
#[repr(C)]
pub(crate) struct Super {
    pub(crate) magic: AtomicU64,
    /// 窗口大小（布局由它算出）
    pub(crate) window: AtomicU64,
    /// 堆号 = 窗口号
    pub(crate) heap: AtomicU64,
    /// 回收请求（futex 字）：0 = 没人在回收，否则是回收方的挂接槽号 + 1；非 0 时新的 mutate 先等
    pub(crate) request: AtomicU32,
    /// 最近一次开始的回收用的纪元（回收开始时就写）：回收方被杀、别人接手时用比它新的纪元从头标
    pub(crate) marking: AtomicU32,
    /// 最后一次完成的标记的纪元
    pub(crate) epoch: AtomicU32,
    /// 开过的块数（块区里下一个新块的序号）
    pub(crate) blocks: AtomicU32,
    pub(crate) collections: AtomicU64,
    /// 回收方被杀、别人接手重做的次数
    pub(crate) takeovers: AtomicU64,
    /// 可用块栈的头（见 [`super::heap::Stack`]）
    pub(crate) available: AtomicU64,
    /// 类型表里下一项
    pub(crate) types: AtomicU32,
    /// 指针图里下一个空位
    pub(crate) map: AtomicU32,
    /// 入口表里下一个没用过的项
    pub(crate) entries: AtomicU32,
    _pad: u32,
    /// 还回来的入口栈的头
    pub(crate) free_entries: AtomicU64,
    /// 文件已经延长到的地方（窗口偏移）：类型表、指针图、入口表、块状态表各一个
    pub(crate) committed: [AtomicU64; Segment::COUNT],
    /// 上次回收后存活的字节数
    pub(crate) live: AtomicU64,
    /// 上次回收后新领用的字节数
    pub(crate) allocated: AtomicU64,
}

impl Super {
    /// 新堆的超级块：纪元从 1 开始（新对象的标记 0、新块的行标记 0 都早于它）。
    pub(crate) fn new(heap: u64) -> Self {
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
        }
    }
}

/// 挂接表的一项（一条缓存行，免得各进程的计数互相挤）。
#[repr(C, align(64))]
pub(crate) struct Attachment {
    /// 进程号；0 = 空槽
    pub(crate) pid: AtomicU32,
    /// 这个挂接里在 mutate 中的线程数（futex 字：回收方等它归零）
    pub(crate) inside: AtomicU32,
    /// 进程启动时间（见 [`super::process::Process`]）；0 = 还在登记
    pub(crate) start: AtomicU64,
    /// 这个挂接里因为回收请求挡在门口、还没进去的线程数（futex 字）：回收方先让它们进去再抢下一次，免得把 mutator 饿死
    pub(crate) waiting: AtomicU32,
}

const _: () = assert!(size_of::<Super>() <= ATTACH_OFF && ATTACH_OFF + ATTACH_CAP * size_of::<Attachment>() <= PAGE, "挂接表放在超级块那一页里");

/// 按需延长文件的几段。
#[derive(Clone, Copy)]
pub(crate) enum Segment {
    Types,
    Map,
    Entries,
    States,
}

impl Segment {
    pub(crate) const COUNT: usize = 4;

    pub(crate) fn index(self) -> usize {
        self as usize
    }
}

/// 类型表的一项。
#[repr(C)]
pub(crate) struct TypeEntry {
    /// 写完其余字段后置 1（之后才可以按它查、按它追踪）
    pub(crate) ready: AtomicU32,
    /// 指针槽个数
    pub(crate) edges: AtomicU32,
    /// 指针图里从第几个开始
    pub(crate) map: AtomicU32,
    /// 对象数据的字节数
    pub(crate) size: AtomicU32,
    /// 类型键：类型名、大小、指针图一起算的 128 位哈希
    pub(crate) key: [AtomicU64; 2],
}

impl TypeEntry {
    /// 登记完了、类型键是 `key`。
    pub(crate) fn has_key(&self, key: [u64; 2]) -> bool {
        self.ready.load(core::sync::atomic::Ordering::Acquire) == 1 && self.key.iter().zip(key).all(|(k, v)| k.load(core::sync::atomic::Ordering::Relaxed) == v)
    }
}

/// 入口表的一项。
#[repr(C)]
pub(crate) struct Entry {
    /// 目标对象数据的公开偏移；0 = 空
    pub(crate) ptr: AtomicU64,
    /// 持有方（0 = 普通句柄）
    pub(crate) holder: AtomicU32,
    /// 在还回来的入口栈里时：下一个（见 [`super::heap::Stack`]）
    pub(crate) next: AtomicU32,
}

/// 块状态表的一项。
#[repr(C)]
pub(crate) struct BlockState {
    /// [`Kind`]
    pub(crate) kind: AtomicU32,
    /// 在可用块栈里时：下一个
    pub(crate) next: AtomicU32,
}

/// 块的种类。
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// 还没开过
    Unused = 0,
    /// 按行切给小对象
    Small = 1,
    /// 一个大对象的第一块（对象头在块的开头）
    LargeHead = 2,
    /// 大对象后面的块
    LargeTail = 3,
}

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
pub(crate) struct Layout {
    pub(crate) types: usize,
    pub(crate) map: usize,
    pub(crate) entries: usize,
    pub(crate) entry_cap: usize,
    pub(crate) states: usize,
    pub(crate) blocks: usize,
    pub(crate) max_blocks: usize,
}

impl Layout {
    pub(crate) fn new() -> Self {
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
    pub(crate) fn segment(&self, s: Segment) -> (usize, usize) {
        match s {
            Segment::Types => (self.types, self.map),
            Segment::Map => (self.map, self.entries),
            Segment::Entries => (self.entries, self.states),
            Segment::States => (self.states, self.blocks),
        }
    }
}
