//! Poison-tolerant accessors for the shared `Mutex` / `RwLock` state.
//!
//! `std` locks become *poisoned* if a thread panics while holding the guard;
//! every subsequent `.lock()` / `.read()` / `.write()` then returns `Err`, and
//! the codebase's `.unwrap()` on that turns one worker-thread panic into a hard
//! crash of the whole app on the next UI frame.
//!
//! For this viewer the data behind these locks is a plain log model / filter
//! spec — a poisoned guard means "some other thread panicked", not "the data is
//! structurally corrupt and unsafe to touch". Recovering the guard
//! (`into_inner`) and carrying on keeps the UI alive and is strictly better than
//! crashing. These extension traits provide `*_recover()` accessors that do
//! exactly that, so call sites read almost the same as before.

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub trait MutexExt<T> {
    /// Lock, recovering the guard if the mutex was poisoned by a panicking
    /// thread instead of propagating the poison as an `Err`/`unwrap` panic.
    fn lock_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub trait RwLockExt<T> {
    /// Acquire a read guard, recovering from poisoning.
    fn read_recover(&self) -> RwLockReadGuard<'_, T>;
    /// Acquire a write guard, recovering from poisoning.
    fn write_recover(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T> RwLockExt<T> for RwLock<T> {
    fn read_recover(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(|e| e.into_inner())
    }
    fn write_recover(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn mutex_recovers_after_poison() {
        let m = Arc::new(Mutex::new(7));
        let m2 = m.clone();
        // Poison the mutex by panicking while holding the guard.
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison it");
        })
        .join();
        assert!(m.lock().is_err(), "mutex should be poisoned");
        // Recovering accessor still yields the value instead of panicking.
        assert_eq!(*m.lock_recover(), 7);
    }

    #[test]
    fn rwlock_recovers_after_poison() {
        let rw = Arc::new(RwLock::new(String::from("ok")));
        let rw2 = rw.clone();
        let _ = std::thread::spawn(move || {
            let _g = rw2.write().unwrap();
            panic!("poison it");
        })
        .join();
        assert!(rw.read().is_err(), "rwlock should be poisoned");
        assert_eq!(&*rw.read_recover(), "ok");
        rw.write_recover().push('!');
        assert_eq!(&*rw.read_recover(), "ok!");
    }
}
