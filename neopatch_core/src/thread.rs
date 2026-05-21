//! Constructs for main-thread identity, access, and mutable statics.
//!
//! `MAIN_TID` is set once from `DllMain` when hooks are installed.
//! `MainToken` is a ZST witness that the holding thread is the main render thread.
//! It is `!Send + !Sync`, so rustc rejects any code that tries to move it
//! or share it across threads. `MainCell<T>` accessors take `&MainToken`,
//! propagating the "main-thread only" requirement to every call site at the type level.

use crate::log::flush;
use std::cell::Cell;
use std::marker::PhantomData;
use std::process::abort;
use std::sync::atomic::{AtomicU32, Ordering};
use tracing::error;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

static MAIN_TID: AtomicU32 = AtomicU32::new(0);

pub fn set_main_id(tid: u32) {
    MAIN_TID.store(tid, Ordering::Release);
}

pub(crate) fn main_id() -> u32 {
    MAIN_TID.load(Ordering::Acquire)
}

/// ZST witness that the constructing thread is the main render thread.
/// Holding `&MainToken` is the compile-time proof required to call
/// `MainCell::get` and `MainCell::set`.
///
/// This type is `!Send + !Sync`, so `&MainToken` is also `!Send + !Sync`.
/// Trait-bound checks at `std::thread::spawn` and similar APIs reject any closure
/// that would carry the token or a reference to it onto another thread.
/// Combined with the runtime check at construction, this means: if the constructor returns,
/// then every downstream cell access through the resulting token is on the main thread.
pub(crate) struct MainToken {
    _marker: PhantomData<*const ()>,
}

impl MainToken {
    /// Creates an instance of `MainToken`.
    /// Aborts via `std::process::abort` if the caller isn't on the main thread.
    ///
    /// This must not be called before `set_main_id`. Until then, `MAIN_TID` is 0,
    /// and `GetCurrentThreadId` never returns 0, so this will abort.
    #[allow(clippy::new_without_default)]
    pub(crate) fn new() -> Self {
        let current = unsafe { GetCurrentThreadId() };
        let main = main_id();
        if current != main {
            error!(kind = "main_token_off_main", current, main);
            flush();
            abort();
        }
        Self {
            _marker: PhantomData,
        }
    }
}

/// Interior-mutable cell for state that is single-thread by construction
/// but lives in a `Sync`-required slot (e.g. a `static`, or inside a `OnceLock<...>`).
///
/// Prefer this over atomic types when there is no cross-thread sharing;
/// atomics would misleadingly signal lock-free synchronization that isn't present.
// The `T: Copy` bound is required by `Cell::get`. It also has the useful side effect of
// forbidding `Drop` on `T`, since `Copy` and `Drop` are mutually exclusive. So, even a
// hypothetical off-thread drop of a `MainCell` (if one ever lived outside a `static`)
// runs no thread-affine destructor.
pub(crate) struct MainCell<T: Copy>(Cell<T>);

// SAFETY: cross-thread access is prevented at the type level. `get` and `set` require
// `&MainToken` with `MainToken: !Send + !Sync` and `&MainToken: !Send + !Sync`.
// So, neither the token nor a reference to it can reach another thread.
//
// `Sync` lets `MainCell` live inside `static` and `OnceLock<...>`.
// `Send` is needed transitively because `OnceLock<T>: Sync` requires `T: Send + Sync`.
unsafe impl<T: Copy> Sync for MainCell<T> {}
unsafe impl<T: Copy> Send for MainCell<T> {}

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
