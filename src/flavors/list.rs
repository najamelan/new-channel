//! Unbounded channel implemented as a linked list.

use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem::{self, ManuallyDrop};
use std::ptr;
use std::sync::atomic::{self, AtomicPtr, AtomicUsize, Ordering};
use std::time::Instant;

use err::{RecvTimeoutError, SendTimeoutError, TryRecvError, TrySendError};
use notify::{Context, Selected, SyncWaker};
use utils::{Backoff, CachePadded};

// Bits indicating the state of a slot:
// * If a message has been written into the slot, `WRITE` is set.
// * If a message has been read from the slot, `READ` is set.
// * If the block is being destroyed, `DESTROY` is set.
const WRITE: usize = 1;
const READ: usize = 2;
const DESTROY: usize = 4;

// Each block covers one "lap" of indices.
const LAP: usize = 32;
// The maximum number of messages a block can hold.
const BLOCK_CAP: usize = LAP - 1;
// How many lower bits are reserved for metadata.
const SHIFT: usize = 1;
// Has two different purposes:
// * If set in head, indicates that the block is not the last one.
// * If set in tail, indicates that the channel is closed.
const MARK_BIT: usize = 1;

/// A slot in a block.
struct Slot<T> {
    /// The message.
    msg: UnsafeCell<ManuallyDrop<T>>,

    /// The state of the slot.
    state: AtomicUsize,
}

impl<T> Slot<T> {
    /// Waits until a message is written into the slot.
    fn wait_write(&self) {
        let backoff = Backoff::new();
        while self.state.load(Ordering::Acquire) & WRITE == 0 {
            backoff.snooze();
        }
    }
}

/// A block in a linked list.
///
/// Each block in the list can hold up to `BLOCK_CAP` messages.
struct Block<T> {
    /// The next block in the linked list.
    next: AtomicPtr<Block<T>>,

    /// Slots for messages.
    slots: [Slot<T>; BLOCK_CAP],
}

impl<T> Block<T> {
    /// Creates an empty block.
    fn new() -> Block<T> {
        unsafe { mem::zeroed() }
    }

    /// Waits until the next pointer is set.
    fn wait_next(&self) -> *mut Block<T> {
        let backoff = Backoff::new();
        loop {
            let next = self.next.load(Ordering::Acquire);
            if !next.is_null() {
                return next;
            }
            backoff.snooze();
        }
    }

    /// Sets the `DESTROY` bit in slots starting from `start` and destroys the block.
    unsafe fn destroy(this: *mut Block<T>, start: usize) {
        // It is not necessary to set the `DESTROY bit in the last slot because that slot has begun
        // destruction of the block.
        for i in start..BLOCK_CAP - 1 {
            let slot = (*this).slots.get_unchecked(i);

            // Mark the `DESTROY` bit if a thread is still using the slot.
            if slot.state.load(Ordering::Acquire) & READ == 0
                && slot.state.fetch_or(DESTROY, Ordering::AcqRel) & READ == 0
            {
                // If a thread is still using the slot, it will continue destruction of the block.
                return;
            }
        }

        // No thread is using the block, now it is safe to destroy it.
        drop(Box::from_raw(this));
    }
}

/// A position in a channel.
#[derive(Debug)]
struct Position<T> {
    /// The index in the channel.
    index: AtomicUsize,

    /// The block in the linked list.
    block: AtomicPtr<Block<T>>,
}

/// Unbounded channel implemented as a linked list.
///
/// Each message sent into the channel is assigned a sequence number, i.e. an index. Indices are
/// represented as numbers of type `usize` and wrap on overflow.
///
/// Consecutive messages are grouped into blocks in order to put less pressure on the allocator and
/// improve cache efficiency.
pub struct Channel<T> {
    /// The head of the channel.
    head: CachePadded<Position<T>>,

    /// The tail of the channel.
    tail: CachePadded<Position<T>>,

    /// Receivers waiting while the channel is empty and not closed.
    receivers: SyncWaker,

    /// Indicates that dropping a `Channel<T>` may drop messages of type `T`.
    _marker: PhantomData<T>,
}

impl<T> Channel<T> {
    /// Creates a new unbounded channel.
    pub fn new() -> Self {
        Channel {
            head: CachePadded::new(Position {
                block: AtomicPtr::new(ptr::null_mut()),
                index: AtomicUsize::new(0),
            }),
            tail: CachePadded::new(Position {
                block: AtomicPtr::new(ptr::null_mut()),
                index: AtomicUsize::new(0),
            }),
            receivers: SyncWaker::new(),
            _marker: PhantomData,
        }
    }

    /// Attempts to send a message into the channel.
    pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        self.send(msg, None).map_err(|err| match err {
            SendTimeoutError::Closed(msg) => TrySendError::Closed(msg),
            SendTimeoutError::Timeout(_) => unreachable!(),
        })
    }

    /// Sends a message into the channel.
    pub fn send(&self, msg: T, _deadline: Option<Instant>) -> Result<(), SendTimeoutError<T>> {
        let backoff = Backoff::new();
        let mut tail = self.tail.index.load(Ordering::Acquire);
        let mut block = self.tail.block.load(Ordering::Acquire);
        let mut next_block = None;

        loop {
            // Check if the channel is closed.
            if tail & MARK_BIT != 0 {
                return Err(SendTimeoutError::Closed(msg));
            }

            // Calculate the offset of the index into the block.
            let offset = (tail >> SHIFT) % LAP;

            // If we reached the end of the block, wait until the next one is installed.
            if offset == BLOCK_CAP {
                backoff.snooze();
                tail = self.tail.index.load(Ordering::Acquire);
                block = self.tail.block.load(Ordering::Acquire);
                continue;
            }

            // If we're going to have to install the next block, allocate it in advance in order to
            // make the wait for other threads as short as possible.
            if offset + 1 == BLOCK_CAP && next_block.is_none() {
                next_block = Some(Box::new(Block::<T>::new()));
            }

            // If this is the first message to be sent into the channel, we need to allocate the
            // first block and install it.
            if block.is_null() {
                let new = Box::into_raw(Box::new(Block::<T>::new()));

                if self
                    .tail
                    .block
                    .compare_and_swap(block, new, Ordering::Release)
                    == block
                {
                    self.head.block.store(new, Ordering::Release);
                    block = new;
                } else {
                    next_block = unsafe { Some(Box::from_raw(new)) };
                    tail = self.tail.index.load(Ordering::Acquire);
                    block = self.tail.block.load(Ordering::Acquire);
                    continue;
                }
            }

            let new_tail = tail + (1 << SHIFT);

            // Try advancing the tail forward.
            match self.tail.index.compare_exchange_weak(
                tail,
                new_tail,
                Ordering::SeqCst,
                Ordering::Acquire,
            ) {
                Ok(_) => unsafe {
                    // If we've reached the end of the block, install the next one.
                    if offset + 1 == BLOCK_CAP {
                        let next_block = Box::into_raw(next_block.unwrap());
                        self.tail.block.store(next_block, Ordering::Release);
                        self.tail.index.fetch_add(1 << SHIFT, Ordering::Release);
                        (*block).next.store(next_block, Ordering::Release);
                    }

                    // Write the message into the slot.
                    let slot = (*block).slots.get_unchecked(offset);
                    slot.msg.get().write(ManuallyDrop::new(msg));
                    slot.state.fetch_or(WRITE, Ordering::Release);

                    // Wake a sleeping receiver.
                    self.receivers.notify();
                    return Ok(());
                },
                Err(t) => {
                    tail = t;
                    block = self.tail.block.load(Ordering::Acquire);
                    backoff.spin();
                }
            }
        }
    }

    /// Attempts to receive a message without blocking.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        let backoff = Backoff::new();
        let mut head = self.head.index.load(Ordering::Acquire);
        let mut block = self.head.block.load(Ordering::Acquire);

        loop {
            // Calculate the offset of the index into the block.
            let offset = (head >> SHIFT) % LAP;

            // If we reached the end of the block, wait until the next one is installed.
            if offset == BLOCK_CAP {
                backoff.snooze();
                head = self.head.index.load(Ordering::Acquire);
                block = self.head.block.load(Ordering::Acquire);
                continue;
            }

            let mut new_head = head + (1 << SHIFT);

            if new_head & MARK_BIT == 0 {
                atomic::fence(Ordering::SeqCst);
                let tail = self.tail.index.load(Ordering::Relaxed);

                // If the tail equals the head, that means the channel is empty.
                if head >> SHIFT == tail >> SHIFT {
                    // If the channel is closed...
                    if tail & MARK_BIT != 0 {
                        // ...then receive an error.
                        return Err(TryRecvError::Closed);
                    } else {
                        // Otherwise, the receive operation is not ready.
                        return Err(TryRecvError::Empty);
                    }
                }

                // If head and tail are not in the same block, set `MARK_BIT` in head.
                if (head >> SHIFT) / LAP != (tail >> SHIFT) / LAP {
                    new_head |= MARK_BIT;
                }
            }

            // The block can be null here only if the first message is being sent into the channel.
            // In that case, just wait until it gets initialized.
            if block.is_null() {
                backoff.snooze();
                head = self.head.index.load(Ordering::Acquire);
                block = self.head.block.load(Ordering::Acquire);
                continue;
            }

            // Try moving the head index forward.
            match self.head.index.compare_exchange_weak(
                head,
                new_head,
                Ordering::SeqCst,
                Ordering::Acquire,
            ) {
                Ok(_) => unsafe {
                    // If we've reached the end of the block, move to the next one.
                    if offset + 1 == BLOCK_CAP {
                        let next = (*block).wait_next();
                        let mut next_index = (new_head & !MARK_BIT).wrapping_add(1 << SHIFT);
                        if !(*next).next.load(Ordering::Relaxed).is_null() {
                            next_index |= MARK_BIT;
                        }

                        self.head.block.store(next, Ordering::Release);
                        self.head.index.store(next_index, Ordering::Release);
                    }

                    // Read the message.
                    let slot = (*block).slots.get_unchecked(offset);
                    slot.wait_write();
                    let m = slot.msg.get().read();
                    let msg = ManuallyDrop::into_inner(m);

                    // Destroy the block if we've reached the end, or if another thread wanted to
                    // destroy but couldn't because we were busy reading from the slot.
                    if offset + 1 == BLOCK_CAP {
                        Block::destroy(block, 0);
                    } else if slot.state.fetch_or(READ, Ordering::AcqRel) & DESTROY != 0 {
                        Block::destroy(block, offset + 1);
                    }

                    return Ok(msg);
                },
                Err(h) => {
                    head = h;
                    block = self.head.block.load(Ordering::Acquire);
                    backoff.spin();
                }
            }
        }
    }

    /// Receives a message from the channel.
    pub fn recv(&self, deadline: Option<Instant>) -> Result<T, RecvTimeoutError> {
        loop {
            // Try receiving a message several times.
            let backoff = Backoff::new();
            loop {
                match self.try_recv() {
                    Ok(msg) => return Ok(msg),
                    Err(TryRecvError::Closed) => return Err(RecvTimeoutError::Closed),
                    Err(TryRecvError::Empty) => {}
                }

                if backoff.is_completed() {
                    break;
                } else {
                    backoff.snooze();
                }
            }

            // Prepare for blocking until a sender wakes us up.
            Context::with(|cx| {
                self.receivers.register(cx);

                // Has the channel become ready just now?
                if !self.is_empty() || self.is_closed() {
                    let _ = cx.try_select(Selected::Aborted);
                }

                // Block the current thread.
                let sel = cx.wait_until(deadline);

                match sel {
                    Selected::Waiting => unreachable!(),
                    Selected::Aborted | Selected::Closed => {
                        self.receivers.unregister(cx).unwrap();
                        // If the channel was closed, we still have to check for remaining
                        // messages.
                    }
                    Selected::Operation => {}
                }
            });

            if let Some(d) = deadline {
                if Instant::now() >= d {
                    return Err(RecvTimeoutError::Timeout);
                }
            }
        }
    }

    /// Closes the channel and wakes up all blocked receivers.
    ///
    /// Returns `true` if this call closed the channel.
    pub fn close(&self) -> bool {
        let tail = self.tail.index.fetch_or(MARK_BIT, Ordering::SeqCst);

        if tail & MARK_BIT == 0 {
            self.receivers.close();
            true
        } else {
            false
        }
    }

    /// Returns `true` if the channel is closed.
    pub fn is_closed(&self) -> bool {
        self.tail.index.load(Ordering::SeqCst) & MARK_BIT != 0
    }

    /// Returns `true` if the channel is empty.
    pub fn is_empty(&self) -> bool {
        let head = self.head.index.load(Ordering::SeqCst);
        let tail = self.tail.index.load(Ordering::SeqCst);
        head >> SHIFT == tail >> SHIFT
    }
}

impl<T> Drop for Channel<T> {
    fn drop(&mut self) {
        let mut head = self.head.index.load(Ordering::Relaxed);
        let mut tail = self.tail.index.load(Ordering::Relaxed);
        let mut block = self.head.block.load(Ordering::Relaxed);

        // Erase the lower bits.
        head &= !((1 << SHIFT) - 1);
        tail &= !((1 << SHIFT) - 1);

        unsafe {
            // Drop all messages between head and tail and deallocate the heap-allocated blocks.
            while head != tail {
                let offset = (head >> SHIFT) % LAP;

                if offset < BLOCK_CAP {
                    // Drop the message in the slot.
                    let slot = (*block).slots.get_unchecked(offset);
                    ManuallyDrop::drop(&mut *(*slot).msg.get());
                } else {
                    // Deallocate the block and move to the next one.
                    let next = (*block).next.load(Ordering::Relaxed);
                    drop(Box::from_raw(block));
                    block = next;
                }

                head = head.wrapping_add(1 << SHIFT);
            }

            // Deallocate the last remaining block.
            if !block.is_null() {
                drop(Box::from_raw(block));
            }
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

    fn close(&self) -> bool {
        self.close()
    }
}
