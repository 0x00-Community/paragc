//! 公开堆：放在共享内存文件上、有文件权限的进程都能挂的堆（设计见 `docs/cross-heap.zh-CN.md`）。
mod heap;
mod layout;
mod mark;
mod process;
mod ptr;
mod region;
mod space;

pub use heap::{PublicHeap, PublicMutator, PublicStats};
pub use ptr::{EdgeMap, Public, PublicBrand, PublicGc, PublicSlot, Shared};
pub use space::PublicSpace;
