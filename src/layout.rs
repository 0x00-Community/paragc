//! 堆的尺寸常量：块 / 行 / 粒度 / 大对象阈值。

/// 块大小：块按自身大小对齐，对象地址抹掉低位就是所在块的起点
pub(crate) const BLOCK: usize = 32 * 1024;
/// 行大小：回收的最小单位，空洞以行为单位找
pub(crate) const LINE: usize = 256;
/// 每块的行数
pub(crate) const LINES: usize = BLOCK / LINE;
/// 粒度：对象起点与大小都按它对齐，也是对象数据允许的最大对齐
pub(crate) const GRANULE: usize = 8;
/// 每块的粒度数（对象起点位图的位数）
pub(crate) const GRANULES: usize = BLOCK / GRANULE;
/// 起点位图每个字的位数
pub(crate) const BITS: usize = u64::BITS as usize;
/// 大对象阈值（含头的字节数）：超过就单独分配、不进块
pub(crate) const LARGE: usize = 8 * 1024;

/// 回收线程数的估算：每个线程至少分到这么多个块 / 大对象才值得多开一个
pub(crate) const UNITS_PER_WORKER: usize = 8;

/// 触发回收的下限：上次回收后新领用的字节数不到它就不回收
pub(crate) const MIN_TRIGGER: usize = 4 * 1024 * 1024;
/// 触发回收的倍数：新领用的字节数超过「上次存活字节 × 它」就回收
pub(crate) const GROWTH: usize = 2;
/// collect_if_needed：引用计数不可信时，每这么多次年轻回收穿插一次完整回收（收掉死掉的老对象、重新计数）
pub(crate) const YOUNG_PER_FULL: usize = 4;
/// collect_if_needed：引用计数可信时（死掉的老对象由计数收），每这么多次年轻回收才一次完整回收（收环与计数粘住的）
pub(crate) const YOUNG_PER_FULL_RC: usize = 16;
/// 疏散候选：上次清扫后存活不到这么多字节的块（块的 1/4）
pub(crate) const EVACUATE_LIVE: usize = BLOCK / 4;
/// 疏散候选至少这么多个才值得疏散
pub(crate) const EVACUATE_MIN: usize = 4;
/// 一次至多疏散这么多个块（限住这次停顿）
pub(crate) const EVACUATE_MAX: usize = 1024;
/// 回收后至少留着复用的全空块数
pub(crate) const MIN_SPARE_BLOCKS: usize = 4;

