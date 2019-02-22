//! Miscellaneous utilities.

use std::cell::{Cell, UnsafeCell};
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, Ordering};

use std::sync::atomic;

const SPIN_LIMIT: u32 = 6;
const YIELD_LIMIT: u32 = 10;

/// Performs exponential backoff in spin loops.
///
/// Backing off in spin loops reduces contention and improves overall performance.
///
/// This primitive can execute *YIELD* and *PAUSE* instructions, yield the current thread to the OS
/// scheduler, and tell when is a good time to block the thread using a different synchronization
/// mechanism. Each step of the back off procedure takes roughly twice as long as the previous
/// step.
pub struct Backoff {
    step: Cell<u32>,
}

impl Backoff {
    /// Creates a new `Backoff`.
    #[inline]
    pub fn new() -> Self {
        Backoff { step: Cell::new(0) }
    }

    /// Backs off in a lock-free loop.
    ///
    /// This method should be used when we need to retry an operation because another thread made
    /// progress.
    ///
    /// The processor may yield using the *YIELD* or *PAUSE* instruction.
    #[inline]
    pub fn spin(&self) {
        for _ in 0..1 << self.step.get().min(SPIN_LIMIT) {
            atomic::spin_loop_hint();
        }

        if self.step.get() <= SPIN_LIMIT {
            self.step.set(self.step.get() + 1);
        }
    }

    /// Backs off in a blocking loop.
    ///
    /// This method should be used when we need to wait for another thread to make progress.
    ///
    /// The processor may yield using the *YIELD* or *PAUSE* instruction and the current thread
    /// may yield by giving up a timeslice to the OS scheduler.
    ///
    /// If possible, use `is_completed` to check when it is advised to stop using backoff and block
    /// the current thread using a different synchronization mechanism instead.
    #[inline]
    pub fn snooze(&self) {
        if self.step.get() <= SPIN_LIMIT {
            for _ in 0..1 << self.step.get() {
                atomic::spin_loop_hint();
            }
        } else {
            #[cfg(not(feature = "std"))]
            for _ in 0..1 << self.step.get() {
                atomic::spin_loop_hint();
            }

            #[cfg(feature = "std")]
            ::std::thread::yield_now();
        }

        if self.step.get() <= YIELD_LIMIT {
            self.step.set(self.step.get() + 1);
        }
    }

    /// Returns `true` if exponential backoff has completed and blocking the thread is advised.
    #[inline]
    pub fn is_completed(&self) -> bool {
        self.step.get() > YIELD_LIMIT
    }
}

/// Pads and aligns a value to the length of a cache line.
#[derive(Clone, Copy, Default, Hash, PartialEq, Eq)]
// Starting from Intel's Sandy Bridge, spatial prefetcher is now pulling pairs of 64-byte cache
// lines at a time, so we have to align to 128 bytes rather than 64.
//
// Sources:
// - https://www.intel.com/content/dam/www/public/us/en/documents/manuals/64-ia-32-architectures-optimization-manual.pdf
// - https://github.com/facebook/folly/blob/1b5288e6eea6df074758f877c849b6e73bbb9fbb/folly/lang/Align.h#L107
#[cfg_attr(target_arch = "x86_64", repr(align(128)))]
#[cfg_attr(not(target_arch = "x86_64"), repr(align(64)))]
pub struct CachePadded<T> {
    value: T,
}

impl<T> CachePadded<T> {
    /// Pads and aligns a value to the length of a cache line.
    pub fn new(t: T) -> CachePadded<T> {
        CachePadded::<T> { value: t }
    }
}

impl<T> Deref for CachePadded<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for CachePadded<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

impl<T: fmt::Debug> fmt::Debug for CachePadded<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("CachePadded")
            .field("value", &self.value)
            .finish()
    }
}

/// A simple spinlock-based mutex.
pub struct Mutex<T> {
    flag: AtomicBool,
    value: UnsafeCell<T>,
}

impl<T> Mutex<T> {
    /// Returns a new mutex initialized with `value`.
    pub fn new(value: T) -> Mutex<T> {
        Mutex {
            flag: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// Locks the mutex.
    pub fn lock(&self) -> MutexGuard<'_, T> {
        let backoff = Backoff::new();
        while self.flag.swap(true, Ordering::Acquire) {
            backoff.snooze();
        }
        MutexGuard { parent: self }
    }
}

/// A guard holding a mutex locked.
pub struct MutexGuard<'a, T: 'a> {
    parent: &'a Mutex<T>,
}

impl<'a, T> Drop for MutexGuard<'a, T> {
    fn drop(&mut self) {
        self.parent.flag.store(false, Ordering::Release);
    }
}

impl<'a, T> Deref for MutexGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.parent.value.get() }
    }
}

impl<'a, T> DerefMut for MutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.parent.value.get() }
    }
}
