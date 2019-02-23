///! Reference counter for channels.
use std::isize;
use std::ops;
use std::process;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::channel::Channel;

/// Reference counter internals.
struct Counter<C: ?Sized> {
    /// The number of senders associated with the channel.
    senders: AtomicUsize,

    /// The number of receivers associated with the channel.
    receivers: AtomicUsize,

    /// The internal channel.
    chan: C,
}

/// Wraps a channel into the reference counter.
pub fn new<'a, T, C: crate::channel::Channel<T> + 'a>(chan: C) -> (Sender<T>, Receiver<T>) {
    let counter = Box::new(Counter {
        senders: AtomicUsize::new(1),
        receivers: AtomicUsize::new(1),
        chan,
    });
    // let counter: Box<Counter<C>> = counter;
    // let counter: Box<Counter<Channel<T>>> = counter;
    let counter: Box<Counter<crate::channel::Channel<T> + 'a>> = counter;
    let counter: Box<Counter<crate::channel::Channel<T> + 'static>> =
        unsafe { std::mem::transmute(counter) };
    let counter = Box::into_raw(counter);
    let s = Sender { counter };
    let r = Receiver { counter };
    (s, r)
}

/// The sending side.
pub struct Sender<T> {
    counter: *mut Counter<crate::channel::Channel<T>>,
}

impl<C> Sender<C> {
    /// Returns the internal `Counter`.
    fn counter(&self) -> &Counter<Channel<C>> {
        unsafe { &*self.counter }
    }

    /// Acquires another sender reference.
    pub fn acquire(&self) -> Sender<C> {
        let count = self.counter().senders.fetch_add(1, Ordering::Relaxed);

        // Cloning senders and calling `mem::forget` on the clones could potentially overflow the
        // counter. It's very difficult to recover sensibly from such degenerate scenarios so we
        // just abort when the count becomes very large.
        if count > isize::MAX as usize {
            process::abort();
        }

        Sender {
            counter: self.counter,
        }
    }

    /// Releases the sender reference.
    ///
    /// Function `disconnect` will be called if this is the last sender reference.
    pub unsafe fn release<F: FnOnce(&Channel<C>) -> bool>(&self, disconnect: F) {
        if self.counter().senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            if !disconnect(&self.counter().chan) {
                drop(Box::from_raw(self.counter));
            }
        }
    }
}

impl<C> ops::Deref for Sender<C> {
    type Target = Channel<C> + 'static;

    fn deref(&self) -> &Self::Target {
        &self.counter().chan
    }
}

/// The receiving side.
pub struct Receiver<T> {
    counter: *mut Counter<crate::channel::Channel<T>>,
}

impl<C> Receiver<C> {
    /// Returns the internal `Counter`.
    fn counter(&self) -> &Counter<Channel<C>> {
        unsafe { &*self.counter }
    }

    /// Acquires another receiver reference.
    pub fn acquire(&self) -> Receiver<C> {
        let count = self.counter().receivers.fetch_add(1, Ordering::Relaxed);

        // Cloning receivers and calling `mem::forget` on the clones could potentially overflow the
        // counter. It's very difficult to recover sensibly from such degenerate scenarios so we
        // just abort when the count becomes very large.
        if count > isize::MAX as usize {
            process::abort();
        }

        Receiver {
            counter: self.counter,
        }
    }

    /// Releases the receiver reference.
    ///
    /// Function `disconnect` will be called if this is the last receiver reference.
    pub unsafe fn release<F: FnOnce(&Channel<C>) -> bool>(&self, disconnect: F) {
        if self.counter().receivers.fetch_sub(1, Ordering::AcqRel) == 1 {
            if !disconnect(&self.counter().chan) {
                drop(Box::from_raw(self.counter));
            }
        }
    }
}

impl<C> ops::Deref for Receiver<C> {
    type Target = Channel<C> + 'static;

    fn deref(&self) -> &Self::Target {
        &self.counter().chan
    }
}
