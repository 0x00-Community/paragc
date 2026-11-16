#![doc = include_str!("../README.md")]
extern crate self as paragc;

mod block;
mod copy;
mod gc;
mod header;
mod heap;
mod layout;
mod mark;
mod roots;
mod sweep;
mod trace;
mod workers;

pub use gc::{Gc, Slot};
pub use heap::{Heap, Mutator, Stats, Tlab};
pub use paragc_macros::Trace;
pub use roots::{Brand, Root};
pub use trace::{Trace, Tracer};
