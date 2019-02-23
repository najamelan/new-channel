//! Zero-capacity channel.
//!
//! This kind of channel is also known as *rendezvous* channel.

use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use err::{RecvTimeoutError, SendTimeoutError, TryRecvError, TrySendError};
use notify::{Context, Selected, Waker};
use utils::{Backoff, Spinlock};

/// A slot for passing one message from a sender to a receiver.
struct Packet<T> {
    /// Equals `true` once the packet is ready for reading or writing.
    ready: AtomicBool,

    /// The message.
    msg: UnsafeCell<Option<T>>,
}

impl<T> Packet<T> {
    /// Creates an empty packet on the stack.
    fn empty() -> Packet<T> {
        Packet {
            ready: AtomicBool::new(false),
            msg: UnsafeCell::new(None),
        }
    }

    /// Creates a packet on the stack, containing a message.
    fn new(msg: T) -> Packet<T> {
        Packet {
            ready: AtomicBool::new(false),
            msg: UnsafeCell::new(Some(msg)),
        }
    }

    /// Waits until the packet becomes ready for reading or writing.
    fn wait_ready(&self) {
        let backoff = Backoff::new();
        while !self.ready.load(Ordering::Acquire) {
            backoff.snooze();
        }
    }
}

/// Inner representation of a zero-capacity channel.
struct Inner {
    /// Senders waiting to pair up with a receive operation.
    senders: Waker,

    /// Receivers waiting to pair up with a send operation.
    receivers: Waker,

    /// Equals `true` when the channel is disconnected.
    is_disconnected: bool,
}

/// Zero-capacity channel.
pub struct Channel<T> {
    /// Inner representation of the channel.
    inner: Spinlock<Inner>,

    /// Indicates that dropping a `Channel<T>` may drop values of type `T`.
    _marker: PhantomData<T>,
}

impl<T> Channel<T> {
    /// Constructs a new zero-capacity channel.
    pub fn new() -> Self {
        Channel {
            inner: Spinlock::new(Inner {
                senders: Waker::new(),
                receivers: Waker::new(),
                is_disconnected: false,
            }),
            _marker: PhantomData,
        }
    }

    /// Attempts to send a message into the channel.
    pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        let mut inner = self.inner.lock();

        // If there's a waiting receiver, pair up with it.
        if let Some(operation) = inner.receivers.try_select() {
            drop(inner);
            unsafe {
                let packet = &*(operation.packet as *const Packet<T>);
                packet.msg.get().write(Some(msg));
                packet.ready.store(true, Ordering::Release);
            }
            Ok(())
        } else if inner.is_disconnected {
            Err(TrySendError::Disconnected(msg))
        } else {
            Err(TrySendError::Full(msg))
        }
    }

    /// Sends a message into the channel.
    pub fn send(&self, msg: T, deadline: Option<Instant>) -> Result<(), SendTimeoutError<T>> {
        let mut inner = self.inner.lock();

        // If there's a waiting receiver, pair up with it.
        if let Some(operation) = inner.receivers.try_select() {
            drop(inner);
            unsafe {
                let packet = &*(operation.packet as *const Packet<T>);
                packet.msg.get().write(Some(msg));
                packet.ready.store(true, Ordering::Release);
            }
            return Ok(());
        }

        if inner.is_disconnected {
            return Err(SendTimeoutError::Disconnected(msg));
        }

        Context::with(|cx| {
            // Prepare for blocking until a receiver wakes us up.
            let packet = Packet::<T>::new(msg);
            inner
                .senders
                .register_with_packet(&packet as *const Packet<T> as usize, cx);
            drop(inner);

            // Block the current thread.
            let sel = cx.wait_until(deadline);

            match sel {
                Selected::Waiting => unreachable!(),
                Selected::Aborted => {
                    self.inner.lock().senders.unregister(cx).unwrap();
                    let msg = unsafe { packet.msg.get().replace(None).unwrap() };
                    Err(SendTimeoutError::Timeout(msg))
                }
                Selected::Disconnected => {
                    self.inner.lock().senders.unregister(cx).unwrap();
                    let msg = unsafe { packet.msg.get().replace(None).unwrap() };
                    Err(SendTimeoutError::Disconnected(msg))
                }
                Selected::Operation => {
                    // Wait until the message is read, then drop the packet.
                    packet.wait_ready();
                    Ok(())
                }
            }
        })
    }

    /// Attempts to receive a message without blocking.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        let mut inner = self.inner.lock();

        // If there's a waiting sender, pair up with it.
        if let Some(operation) = inner.senders.try_select() {
            drop(inner);
            unsafe {
                let packet = &*(operation.packet as *const Packet<T>);
                let msg = packet.msg.get().replace(None).unwrap();
                packet.ready.store(true, Ordering::Release);
                Ok(msg)
            }
        } else if inner.is_disconnected {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    /// Receives a message from the channel.
    pub fn recv(&self, deadline: Option<Instant>) -> Result<T, RecvTimeoutError> {
        let mut inner = self.inner.lock();

        // If there's a waiting sender, pair up with it.
        if let Some(operation) = inner.senders.try_select() {
            drop(inner);
            unsafe {
                let packet = &*(operation.packet as *const Packet<T>);
                let msg = packet.msg.get().replace(None).unwrap();
                packet.ready.store(true, Ordering::Release);
                return Ok(msg);
            }
        }

        if inner.is_disconnected {
            return Err(RecvTimeoutError::Disconnected);
        }

        Context::with(|cx| {
            // Prepare for blocking until a sender wakes us up.
            let packet = Packet::<T>::empty();
            inner
                .receivers
                .register_with_packet(&packet as *const Packet<T> as usize, cx);
            drop(inner);

            // Block the current thread.
            let sel = cx.wait_until(deadline);

            match sel {
                Selected::Waiting => unreachable!(),
                Selected::Aborted => {
                    self.inner.lock().receivers.unregister(cx).unwrap();
                    Err(RecvTimeoutError::Timeout)
                }
                Selected::Disconnected => {
                    self.inner.lock().receivers.unregister(cx).unwrap();
                    Err(RecvTimeoutError::Disconnected)
                }
                Selected::Operation => {
                    // Wait until the message is provided, then read it.
                    packet.wait_ready();
                    unsafe { Ok(packet.msg.get().replace(None).unwrap()) }
                }
            }
        })
    }

    /// Disconnects the channel and wakes up all blocked senders and receivers.
    ///
    /// Returns `true` if this call disconnected the channel.
    pub fn disconnect(&self) -> bool {
        let mut inner = self.inner.lock();

        if !inner.is_disconnected {
            inner.is_disconnected = true;
            inner.senders.disconnect();
            inner.receivers.disconnect();
            true
        } else {
            false
        }
    }
}

impl<T> crate::channel::Channel<T> for Channel<T> {
    fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        self.try_send(msg)
    }

    fn send(&self, msg: T, deadline: Option<Instant>) -> Result<(), SendTimeoutError<T>> {
        self.send(msg, deadline)
    }

    fn try_recv(&self) -> Result<T, TryRecvError> {
        self.try_recv()
    }

    fn recv(&self, deadline: Option<Instant>) -> Result<T, RecvTimeoutError> {
        self.recv(deadline)
    }

    fn disconnect(&self) -> bool {
        self.disconnect()
    }
}
