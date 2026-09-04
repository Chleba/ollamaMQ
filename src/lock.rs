//! Mutex locking that survives a poisoned lock.

use std::sync::{Mutex, MutexGuard};

/// Locking a [`Mutex`] without the poisoning failure mode.
///
/// `std::sync::Mutex` marks itself poisoned as soon as a thread panics while
/// holding the guard, and every later `.lock().unwrap()` on it panics too.
/// With ~150 lock sites over the shared counters, queues and backend status,
/// a single panic anywhere would leave the proxy accepting connections while
/// every task servicing them panicked — a wedged process that has to be
/// noticed and restarted, which is far worse than the alternative.
///
/// The alternative is carrying on with whatever state the panicking thread
/// left behind. That is acceptable *here specifically* because everything
/// behind these mutexes is bookkeeping — request counts, queues, per-backend
/// status — where a half-applied update leaves a number wrong, not an
/// invariant broken. It would not be acceptable for data whose consistency
/// other code depends on for correctness or safety.
pub trait LockExt<T> {
    /// Lock, recovering the guarded value if the mutex has been poisoned.
    fn lock_or_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lock_or_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_poisoned_mutex_is_still_usable() {
        let m = Arc::new(Mutex::new(vec![1, 2, 3]));

        // Panic while holding the guard, exactly as a panicking request task
        // would inside the scheduler.
        let poisoner = Arc::clone(&m);
        let panicked = std::thread::spawn(move || {
            let mut guard = poisoner.lock().expect("first lock");
            guard.push(4);
            panic!("boom");
        })
        .join();
        assert!(panicked.is_err(), "the helper thread must have panicked");

        // std would give every later caller an Err here...
        assert!(m.lock().is_err(), "the mutex should be poisoned");
        // ...while this recovers the value, including the write that landed
        // before the panic.
        assert_eq!(*m.lock_or_recover(), vec![1, 2, 3, 4]);
        m.lock_or_recover().push(5);
        assert_eq!(*m.lock_or_recover(), vec![1, 2, 3, 4, 5]);
    }
}
