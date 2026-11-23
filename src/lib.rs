#![doc = include_str!("../README.md")]
extern crate self as paragc;

mod handle;
mod heap;
mod layout;
mod mark;
#[cfg(crossheap)]
mod shm;
mod sweep;
mod sync;

pub use handle::{Brand, Far, ScopeLocal, Handle, HandleSlot, ScopeGlobal, Slot, Weak, WordSlot, word_pointer, RAW_TAG, WORD_PAYLOAD, WORD_POINTER_MIN};
#[cfg(crossheap)]
pub use handle::{EdgeMap, CrossHeapGlobal, CrossHeapBrand, CrossHeapLocal, CrossHeapRef, CrossHeapSlot, CrossHeapWeak, SIGNATURE_BYTES, Shared, Signature};
pub use heap::{GroupMember, Heap, Scope, Stats, Tlab, YoungEvacuation};
#[cfg(crossheap)]
pub use heap::{HeapDead, CrossHeap, CrossHeapScope, CrossHeapStats};
pub use mark::{Trace, Tracer};
pub use paragc_macros::{Shared, Trace};
#[cfg(crossheap)]
pub use shm::CrossHeapSpace;
