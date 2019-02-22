//! Channel flavors.
//!
//! There are three flavors:
//!
//! 1. `array` - Bounded channel based on a preallocated array.
//! 2. `list` - Unbounded channel implemented as a linked list.
//! 3. `zero` - Zero-capacity channel.

pub mod array;
pub mod list;
pub mod zero;
