use crate::cell::UnsafeCell;
use crate::sync::atomic::{
    AtomicI32, AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};
use crate::sys::futex::{futex_wait, futex_wake, futex_wake_all};
use crate::time::Duration;

pub type MovableMutex = Mutex;
pub type MovableCondvar = Condvar;

pub struct Mutex {
    /// 0: unlocked
    /// 1: locked, no other threads waiting
    /// 2: locked, and other threads waiting (contended)
    futex: AtomicI32,
}

impl Mutex {
    #[inline]
    pub const fn new() -> Self {
        Self { futex: AtomicI32::new(0) }
    }

    #[inline]
    pub unsafe fn init(&mut self) {}

    #[inline]
    pub unsafe fn destroy(&self) {}

    #[inline]
    pub unsafe fn try_lock(&self) -> bool {
        self.futex.compare_exchange(0, 1, Acquire, Relaxed).is_ok()
    }

    #[inline]
    pub unsafe fn lock(&self) {
        if self.futex.compare_exchange(0, 1, Acquire, Relaxed).is_err() {
            self.lock_contended();
        }
    }

    #[cold]
    fn lock_contended(&self) {
        // Spin first to speed things up if the lock is released quickly.
        let mut state = self.spin();

        // If it's unlocked now, attempt to take the lock
        // without marking it as contended.
        if state == 0 {
            match self.futex.compare_exchange(0, 1, Acquire, Relaxed) {
                Ok(_) => return, // Locked!
                Err(s) => state = s,
            }
        }

        loop {
            // Put the lock in contended state.
            // We avoid an unnecessary write if it as already set to 2,
            // to be friendlier for the caches.
            if state != 2 && self.futex.swap(2, Acquire) == 0 {
                // We changed it from 0 to 2, so we just succesfully locked it.
                return;
            }

            // Wait for the futex to change state, assuming it is still 2.
            futex_wait(&self.futex, 2, None);

            // Spin again after waking up.
            state = self.spin();
        }
    }

    fn spin(&self) -> i32 {
        let mut spin = 100;
        loop {
            // We only use `load` (and not `swap` or `compare_exchange`)
            // while spinning, to be easier on the caches.
            let state = self.futex.load(Relaxed);

            // We stop spinning when the mutex is unlocked (0),
            // but also when it's contended (2).
            if state != 1 || spin == 0 {
                return state;
            }

            crate::hint::spin_loop();
            spin -= 1;
        }
    }

    #[inline]
    pub unsafe fn unlock(&self) {
        if self.futex.swap(0, Release) == 2 {
            // We only wake up one thread. When that thread locks the mutex, it
            // will mark the mutex as contended (2) (see lock_contended above),
            // which makes sure that any other waiting threads will also be
            // woken up eventually.
            self.wake();
        }
    }

    #[cold]
    fn wake(&self) {
        futex_wake(&self.futex);
    }
}

pub struct Condvar {
    // The value of this atomic is simply incremented on every notification.
    // This is used by `.wait()` to not miss any notifications after
    // unlocking the mutex and before waiting for notifications.
    futex: AtomicI32,
}

impl Condvar {
    #[inline]
    pub const fn new() -> Self {
        Self { futex: AtomicI32::new(0) }
    }

    #[inline]
    pub unsafe fn init(&mut self) {}

    #[inline]
    pub unsafe fn destroy(&self) {}

    // All the memory orderings here are `Relaxed`,
    // because synchronization is done by unlocking and locking the mutex.

    pub unsafe fn notify_one(&self) {
        self.futex.fetch_add(1, Relaxed);
        futex_wake(&self.futex);
    }

    pub unsafe fn notify_all(&self) {
        self.futex.fetch_add(1, Relaxed);
        futex_wake_all(&self.futex);
    }

    pub unsafe fn wait(&self, mutex: &Mutex) {
        self.wait_optional_timeout(mutex, None);
    }

    pub unsafe fn wait_timeout(&self, mutex: &Mutex, timeout: Duration) -> bool {
        self.wait_optional_timeout(mutex, Some(timeout))
    }

    unsafe fn wait_optional_timeout(&self, mutex: &Mutex, timeout: Option<Duration>) -> bool {
        // Examine the notification counter _before_ we unlock the mutex.
        let futex_value = self.futex.load(Relaxed);

        // Unlock the mutex before going to sleep.
        mutex.unlock();

        // Wait, but only if there hasn't been any
        // notification since we unlocked the mutex.
        let r = futex_wait(&self.futex, futex_value, timeout);

        // Lock the mutex again.
        mutex.lock();

        r
    }
}

/// A reentrant mutex. Used by stdout().lock() and friends.
///
/// The 'owner' field tracks which thread has locked the mutex.
///
/// We use current_thread_unique_ptr() as the thread identifier,
/// which is just the address of a thread local variable.
///
/// If `owner` is set to the identifier of the current thread,
/// we assume the mutex is already locked and instead of locking it again,
/// we increment `lock_count`.
///
/// When unlocking, we decrement `lock_count`, and only unlock the mutex when
/// it reaches zero.
///
/// `lock_count` is protected by the mutex and only accessed by the thread that has
/// locked the mutex, so needs no synchronization.
///
/// `owner` can be checked by other threads that want to see if they already
/// hold the lock, so needs to be atomic. If it compares equal, we're on the
/// same thread that holds the mutex and memory access can use relaxed ordering
/// since we're not dealing with multiple threads. If it compares unequal,
/// synchronization is left to the mutex, making relaxed memory ordering for
/// the `owner` field fine in all cases.
pub struct ReentrantMutex {
    mutex: Mutex,
    owner: AtomicUsize,
    lock_count: UnsafeCell<u32>,
}

unsafe impl Send for ReentrantMutex {}
unsafe impl Sync for ReentrantMutex {}

impl ReentrantMutex {
    #[inline]
    pub const unsafe fn uninitialized() -> Self {
        Self { mutex: Mutex::new(), owner: AtomicUsize::new(0), lock_count: UnsafeCell::new(0) }
    }

    #[inline]
    pub unsafe fn init(&self) {}

    #[inline]
    pub unsafe fn destroy(&self) {}

    pub unsafe fn try_lock(&self) -> bool {
        let this_thread = current_thread_unique_ptr();
        if self.owner.load(Relaxed) == this_thread {
            self.increment_lock_count();
            true
        } else if self.mutex.try_lock() {
            self.owner.store(this_thread, Relaxed);
            *self.lock_count.get() = 1;
            true
        } else {
            false
        }
    }

    pub unsafe fn lock(&self) {
        let this_thread = current_thread_unique_ptr();
        if self.owner.load(Relaxed) == this_thread {
            self.increment_lock_count();
        } else {
            self.mutex.lock();
            self.owner.store(this_thread, Relaxed);
            *self.lock_count.get() = 1;
        }
    }

    unsafe fn increment_lock_count(&self) {
        *self.lock_count.get() = (*self.lock_count.get())
            .checked_add(1)
            .expect("lock count overflow in reentrant mutex");
    }

    pub unsafe fn unlock(&self) {
        *self.lock_count.get() -= 1;
        if *self.lock_count.get() == 0 {
            self.owner.store(0, Relaxed);
            self.mutex.unlock();
        }
    }
}

/// Get an address that is unique per running thread.
///
/// This can be used as a non-null usize-sized ID.
pub fn current_thread_unique_ptr() -> usize {
    // Use a non-drop type to make sure it's still available during thread destruction.
    thread_local! { static X: u8 = 0 }
    X.with(|x| <*const _>::addr(x))
}
