//! Multi-producer multi-consumer channels for message passing.

#![warn(missing_docs)]
#![warn(missing_debug_implementations)]

mod channel;
mod counter;
mod err;
mod flavors;
mod notify;
mod utils;

pub use channel::{bounded, unbounded};
pub use channel::{IntoIter, Iter, TryIter};
pub use channel::{Receiver, Sender};

pub use err::{RecvError, RecvTimeoutError, TryRecvError};
pub use err::{SendError, SendTimeoutError, TrySendError};
