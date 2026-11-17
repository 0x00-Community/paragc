#![doc = include_str!("../README.md")]
extern crate self as paragc;

mod block;
mod copy;
mod far;
mod gc;
mod header;
mod heap;
mod layout;
mod lockfree;
mod mark;
#[cfg(target_os = "linux")]
mod public;
mod release;
mod roots;
mod sweep;
mod trace;
mod workers;

pub use far::Far;
pub use gc::{Gc, Slot};
pub use heap::{Heap, Mutator, Stats, Tlab};
pub use paragc_macros::{Shared, Trace};
#[cfg(target_os = "linux")]
pub use public::{EdgeMap, HeapDead, Public, PublicBrand, PublicGc, PublicHeap, PublicMutator, PublicRef, PublicSlot, PublicSpace, PublicStats, Shared};
pub use roots::{Brand, Root};
pub use trace::{Trace, Tracer};
