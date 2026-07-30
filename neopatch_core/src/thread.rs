//! Constructs for render-thread identity, access, and mutable statics.
//!
//! [`MainToken`] is a ZST witness that the holding thread is the render thread. It is `!Send + !Sync`,
//! so rustc rejects any safe code that tries to move it or share it across threads. [`MainCell<T>`] accessors take `&MainToken`,
//! propagating the "render-thread only" requirement to every call site at the type level.
//!
//! [`MAIN_TID`] records the render thread's TID. It is claimed atomically by [`MainToken::claim`] at the game's
//! first device creation, not at `DllMain`. The thread running `DllMain` and the thread running the render loop
//! are usually the same, but they diverge when something (e.g. the thprac launcher) injects neopatch via `CreateRemoteThread`
//! into a `CREATE_SUSPENDED` process before resuming the real initial thread.

use crate::process::register_mmcss;
use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU32, Ordering};
use tracing::{info, warn};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

static MAIN_TID: AtomicU32 = AtomicU32::new(0);

/// ZST witness that the holding thread is the render thread. Holding `&MainToken` is the compile-time proof required to call
/// [`MainCell::get`] and [`MainCell::set`], as well as any other render-thread-only function in the codebase.
///
/// This type is `!Send + !Sync`. Combined with the atomic claim in [`MainToken::claim`] and the check in [`MainToken::current`],
/// this means: if a constructor returns `Some`, then every downstream cell access through the resulting token is on the claimed thread.
/// (This assumes the claiming thread is never destroyed with its TID recycled onto a fresh thread.)
pub struct MainToken(PhantomData<*const ()>);

impl MainToken {
    /// Claims the render thread for the calling thread, or confirms an existing claim by it.
    /// Returns `None` if another thread already holds the claim.
    pub(crate) fn claim() -> Option<Self> {
        let current = unsafe { GetCurrentThreadId() };
        match MAIN_TID.compare_exchange(0, current, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                info!(kind = "main_thread_claimed");
                let tok = Self(PhantomData);
                register_mmcss(&tok);
                Some(tok)
            }
            Err(existing) if existing == current => Some(Self(PhantomData)),
            Err(existing) => {
                warn!(kind = "main_token_off_main", current, main = existing);
                None
            }
        }
    }

    /// Returns `Some` iff [`MAIN_TID`] is claimed by the calling thread.
    pub(crate) fn current() -> Option<Self> {
        let claimed = MAIN_TID.load(Ordering::Acquire);
        (claimed != 0 && claimed == unsafe { GetCurrentThreadId() }).then_some(Self(PhantomData))
    }
}

/// Interior-mutable cell for state that is single-thread by construction but lives in a `Sync`-required slot
/// (e.g. a `static`, or inside a `OnceLock`). Prefer this over atomic types when there is no cross-thread sharing;
/// atomics would misleadingly signal lock-free synchronization that isn't present.
// `Copy` and `Drop` are mutually exclusive, so nothing stored here can carry a thread-affine destructor.
// Therefore, even a hypothetical off-thread drop of a `MainCell` (if one ever lived outside a `static`) runs nothing.
pub(crate) struct MainCell<T: Copy>(Cell<T>);

// SAFETY: Cross-thread access is prevented at the type level. `get` and `set` require `&MainToken` with `MainToken: !Send + !Sync`
// and `&MainToken: !Send + !Sync`. So, neither the token nor a reference to it can reach another thread.
//
// `Send` is needed transitively because `OnceLock<T>: Sync` requires `T: Send + Sync`.
// `Sync` lets `MainCell` live inside `static` and `OnceLock<...>`.

unsafe impl<T: Copy> Send for MainCell<T> {}
unsafe impl<T: Copy> Sync for MainCell<T> {}

impl<T: Copy> MainCell<T> {
    pub(crate) const fn new(v: T) -> Self {
        Self(Cell::new(v))
    }

    #[inline]
    pub(crate) fn get(&self, _tok: &MainToken) -> T {
        self.0.get()
    }

    #[inline]
    pub(crate) fn set(&self, _tok: &MainToken, v: T) {
        self.0.set(v);
    }
}

impl<T: Copy + Default> MainCell<T> {
    #[inline]
    pub(crate) fn take(&self, _tok: &MainToken) -> T {
        self.0.take()
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::MAIN_TID;
    use std::sync::atomic::Ordering;
    use std::sync::{Mutex, MutexGuard, PoisonError};

    /// Exclusive access to [`MAIN_TID`].
    ///
    /// A real process claims `MAIN_TID` once and never releases it, so any two tests that need a claim would race for it.
    /// Acquiring this guard serializes such tests against each other and hands the calling thread an unclaimed `MAIN_TID`,
    /// released again on drop. (`-Zpanic-abort-tests` means each test gets its own process, but we still use this just in case.)
    pub(crate) struct MainClaim(#[allow(dead_code)] MutexGuard<'static, ()>);

    impl MainClaim {
        pub(crate) fn acquire() -> Self {
            static LOCK: Mutex<()> = Mutex::new(());
            // Poisoning only records that some earlier holder panicked.
            // The state that this guard establishes is unconditional, so the lock is still usable afterwards.
            let guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
            MAIN_TID.store(0, Ordering::Release);
            Self(guard)
        }
    }

    impl Drop for MainClaim {
        fn drop(&mut self) {
            MAIN_TID.store(0, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MainToken;
    use super::test_support::MainClaim;
    use std::thread::spawn;

    #[test]
    fn main_claim_serializes_concurrent_claims() {
        let handles: Vec<_> = (0..8)
            .map(|i| {
                spawn(move || {
                    let _claim = MainClaim::acquire();
                    assert!(
                        MainToken::current().is_none(),
                        "thread {i} saw a stale claim"
                    );
                    // `MainToken` is a ZST with no destructor, so dropping it does not end the claim.
                    assert!(MainToken::claim().is_some(), "thread {i} could not claim");
                    assert!(MainToken::current().is_some(), "thread {i} lost its claim");
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn claim_and_current_thread_semantics() {
        let _claim = MainClaim::acquire();

        assert!(MainToken::current().is_none());

        let _tok = MainToken::claim().expect("the guard hands over an unclaimed MAIN_TID");
        assert!(MainToken::claim().is_some());
        assert!(MainToken::current().is_some());

        spawn(|| {
            assert!(MainToken::claim().is_none());
            assert!(MainToken::current().is_none());
        })
        .join()
        .unwrap();

        assert!(MainToken::current().is_some());
    }
}
