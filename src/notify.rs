//! Waking mechanism for threads blocked on channel operations.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, Thread, ThreadId};
use std::time::Instant;

use flavors;
use utils::{Backoff, Mutex};

/// Temporary data that gets initialized during select or a blocking operation, and is consumed by
/// `read` or `write`.
///
/// Each field contains data associated with a specific channel flavor.
#[derive(Default)]
pub struct Token {
    pub array: flavors::array::ArrayToken,
    pub list: flavors::list::ListToken,
    pub zero: flavors::zero::ZeroToken,
}

/// Identifier associated with an operation by a specific thread on a specific channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operation(usize);

impl Operation {
    /// Creates an operation identifier from a mutable reference.
    ///
    /// This function essentially just turns the address of the reference into a number. The
    /// reference should point to a variable that is specific to the thread and the operation,
    /// and is alive for the entire duration of select or blocking operation.
    #[inline]
    pub fn hook<T>(r: &mut T) -> Operation {
        let val = r as *mut T as usize;
        // Make sure that the pointer address doesn't equal the numerical representation of
        // `Selected::{Waiting, Aborted, Disconnected}`.
        assert!(val > 2);
        Operation(val)
    }
}

/// Current state of a select or a blocking operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selected {
    /// Still waiting for an operation.
    Waiting,

    /// The attempt to block the current thread has been aborted.
    Aborted,

    /// An operation became ready because a channel is disconnected.
    Disconnected,

    /// An operation became ready because a message can be sent or received.
    Operation(Operation),
}

impl From<usize> for Selected {
    #[inline]
    fn from(val: usize) -> Selected {
        match val {
            0 => Selected::Waiting,
            1 => Selected::Aborted,
            2 => Selected::Disconnected,
            oper => Selected::Operation(Operation(oper)),
        }
    }
}

impl Into<usize> for Selected {
    #[inline]
    fn into(self) -> usize {
        match self {
            Selected::Waiting => 0,
            Selected::Aborted => 1,
            Selected::Disconnected => 2,
            Selected::Operation(Operation(val)) => val,
        }
    }
}

/// Represents a thread blocked on a specific channel operation.
pub struct Entry {
    /// The operation.
    pub oper: Operation,

    /// Optional packet.
    pub packet: usize,

    /// Context associated with the thread owning this operation.
    pub cx: Context,
}

/// A queue of threads blocked on channel operations.
///
/// This data structure is used by threads to register blocking operations and get woken up once
/// an operation becomes ready.
pub struct Waker {
    /// A list of select operations.
    selectors: Vec<Entry>,

    /// A list of operations waiting to be ready.
    observers: Vec<Entry>,
}

impl Waker {
    /// Creates a new `Waker`.
    #[inline]
    pub fn new() -> Self {
        Waker {
            selectors: Vec::new(),
            observers: Vec::new(),
        }
    }

    /// Registers a select operation.
    #[inline]
    pub fn register(&mut self, oper: Operation, cx: &Context) {
        self.register_with_packet(oper, 0, cx);
    }

    /// Registers a select operation and a packet.
    #[inline]
    pub fn register_with_packet(&mut self, oper: Operation, packet: usize, cx: &Context) {
        self.selectors.push(Entry {
            oper,
            packet,
            cx: cx.clone(),
        });
    }

    /// Unregisters a select operation.
    #[inline]
    pub fn unregister(&mut self, oper: Operation) -> Option<Entry> {
        if let Some((i, _)) = self
            .selectors
            .iter()
            .enumerate()
            .find(|&(_, entry)| entry.oper == oper)
        {
            let entry = self.selectors.remove(i);
            Some(entry)
        } else {
            None
        }
    }

    /// Attempts to find another thread's entry, select the operation, and wake it up.
    #[inline]
    pub fn try_select(&mut self) -> Option<Entry> {
        let mut entry = None;

        if !self.selectors.is_empty() {
            let thread_id = current_thread_id();

            for i in 0..self.selectors.len() {
                // Does the entry belong to a different thread?
                if self.selectors[i].cx.thread_id() != thread_id {
                    // Try selecting this operation.
                    let sel = Selected::Operation(self.selectors[i].oper);
                    let res = self.selectors[i].cx.try_select(sel);

                    if res.is_ok() {
                        // Provide the packet.
                        self.selectors[i].cx.store_packet(self.selectors[i].packet);
                        // Wake the thread up.
                        self.selectors[i].cx.unpark();

                        // Remove the entry from the queue to keep it clean and improve
                        // performance.
                        entry = Some(self.selectors.remove(i));
                        break;
                    }
                }
            }
        }

        entry
    }

    /// Notifies all operations waiting to be ready.
    #[inline]
    pub fn notify(&mut self) {
        for entry in self.observers.drain(..) {
            if entry.cx.try_select(Selected::Operation(entry.oper)).is_ok() {
                entry.cx.unpark();
            }
        }
    }

    /// Notifies all registered operations that the channel is disconnected.
    #[inline]
    pub fn disconnect(&mut self) {
        for entry in self.selectors.iter() {
            if entry.cx.try_select(Selected::Disconnected).is_ok() {
                // Wake the thread up.
                //
                // Here we don't remove the entry from the queue. Registered threads must
                // unregister from the waker by themselves. They might also want to recover the
                // packet value and destroy it, if necessary.
                entry.cx.unpark();
            }
        }

        self.notify();
    }
}

impl Drop for Waker {
    #[inline]
    fn drop(&mut self) {
        debug_assert_eq!(self.selectors.len(), 0);
        debug_assert_eq!(self.observers.len(), 0);
    }
}

/// A waker that can be shared among threads without locking.
///
/// This is a simple wrapper around `Waker` that internally uses a mutex for synchronization.
pub struct SyncWaker {
    /// The inner `Waker`.
    inner: Mutex<Waker>,

    /// `true` if the waker is empty.
    is_empty: AtomicBool,
}

impl SyncWaker {
    /// Creates a new `SyncWaker`.
    #[inline]
    pub fn new() -> Self {
        SyncWaker {
            inner: Mutex::new(Waker::new()),
            is_empty: AtomicBool::new(false),
        }
    }

    /// Registers the current thread with an operation.
    #[inline]
    pub fn register(&self, oper: Operation, cx: &Context) {
        let mut inner = self.inner.lock();
        inner.register(oper, cx);
        self.is_empty.store(
            inner.selectors.is_empty() && inner.observers.is_empty(),
            Ordering::SeqCst,
        );
    }

    /// Unregisters an operation previously registered by the current thread.
    #[inline]
    pub fn unregister(&self, oper: Operation) -> Option<Entry> {
        let mut inner = self.inner.lock();
        let entry = inner.unregister(oper);
        self.is_empty.store(
            inner.selectors.is_empty() && inner.observers.is_empty(),
            Ordering::SeqCst,
        );
        entry
    }

    /// Attempts to find one thread (not the current one), select its operation, and wake it up.
    #[inline]
    pub fn notify(&self) {
        if !self.is_empty.load(Ordering::SeqCst) {
            let mut inner = self.inner.lock();
            inner.try_select();
            inner.notify();
            self.is_empty.store(
                inner.selectors.is_empty() && inner.observers.is_empty(),
                Ordering::SeqCst,
            );
        }
    }

    /// Notifies all threads that the channel is disconnected.
    #[inline]
    pub fn disconnect(&self) {
        let mut inner = self.inner.lock();
        inner.disconnect();
        self.is_empty.store(
            inner.selectors.is_empty() && inner.observers.is_empty(),
            Ordering::SeqCst,
        );
    }
}

impl Drop for SyncWaker {
    #[inline]
    fn drop(&mut self) {
        debug_assert_eq!(self.is_empty.load(Ordering::SeqCst), true);
    }
}

/// Returns the id of the current thread.
#[inline]
fn current_thread_id() -> ThreadId {
    thread_local! {
        /// Cached thread-local id.
        static THREAD_ID: ThreadId = thread::current().id();
    }

    THREAD_ID
        .try_with(|id| *id)
        .unwrap_or_else(|_| thread::current().id())
}

/// Thread-local context used in select.
#[derive(Clone)]
pub struct Context {
    inner: Arc<Inner>,
}

/// Inner representation of `Context`.
struct Inner {
    /// Selected operation.
    select: AtomicUsize,

    /// A slot into which another thread may store a pointer to its `Packet`.
    packet: AtomicUsize,

    /// Thread handle.
    thread: Thread,

    /// Thread id.
    thread_id: ThreadId,
}

impl Context {
    /// Creates a new context for the duration of the closure.
    #[inline]
    pub fn with<F, R>(f: F) -> R
    where
        F: FnOnce(&Context) -> R,
    {
        thread_local! {
            /// Cached thread-local context.
            static CONTEXT: Cell<Option<Context>> = Cell::new(Some(Context::new()));
        }

        let mut f = Some(f);
        let mut f = move |cx: &Context| -> R {
            let f = f.take().unwrap();
            f(cx)
        };

        CONTEXT
            .try_with(|cell| match cell.take() {
                None => f(&Context::new()),
                Some(cx) => {
                    cx.reset();
                    let res = f(&cx);
                    cell.set(Some(cx));
                    res
                }
            })
            .unwrap_or_else(|_| f(&Context::new()))
    }

    /// Creates a new `Context`.
    #[cold]
    fn new() -> Context {
        Context {
            inner: Arc::new(Inner {
                select: AtomicUsize::new(Selected::Waiting.into()),
                packet: AtomicUsize::new(0),
                thread: thread::current(),
                thread_id: thread::current().id(),
            }),
        }
    }

    /// Resets `select` and `packet`.
    #[inline]
    fn reset(&self) {
        self.inner
            .select
            .store(Selected::Waiting.into(), Ordering::Release);
        self.inner.packet.store(0, Ordering::Release);
    }

    /// Attempts to select an operation.
    ///
    /// On failure, the previously selected operation is returned.
    #[inline]
    pub fn try_select(&self, select: Selected) -> Result<(), Selected> {
        self.inner
            .select
            .compare_exchange(
                Selected::Waiting.into(),
                select.into(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|e| e.into())
    }

    /// Stores a packet.
    ///
    /// This method must be called after `try_select` succeeds and there is a packet to provide.
    #[inline]
    pub fn store_packet(&self, packet: usize) {
        if packet != 0 {
            self.inner.packet.store(packet, Ordering::Release);
        }
    }

    /// Waits until an operation is selected and returns it.
    ///
    /// If the deadline is reached, `Selected::Aborted` will be selected.
    #[inline]
    pub fn wait_until(&self, deadline: Option<Instant>) -> Selected {
        // Spin for a short time, waiting until an operation is selected.
        let backoff = Backoff::new();
        loop {
            let sel = Selected::from(self.inner.select.load(Ordering::Acquire));
            if sel != Selected::Waiting {
                return sel;
            }

            if backoff.is_completed() {
                break;
            } else {
                backoff.snooze();
            }
        }

        loop {
            // Check whether an operation has been selected.
            let sel = Selected::from(self.inner.select.load(Ordering::Acquire));
            if sel != Selected::Waiting {
                return sel;
            }

            // If there's a deadline, park the current thread until the deadline is reached.
            if let Some(end) = deadline {
                let now = Instant::now();

                if now < end {
                    thread::park_timeout(end - now);
                } else {
                    // The deadline has been reached. Try aborting select.
                    return match self.try_select(Selected::Aborted) {
                        Ok(()) => Selected::Aborted,
                        Err(s) => s,
                    };
                }
            } else {
                thread::park();
            }
        }
    }

    /// Unparks the thread this context belongs to.
    #[inline]
    pub fn unpark(&self) {
        self.inner.thread.unpark();
    }

    /// Returns the id of the thread this context belongs to.
    #[inline]
    pub fn thread_id(&self) -> ThreadId {
        self.inner.thread_id
    }
}
