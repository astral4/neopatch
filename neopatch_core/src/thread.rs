//! Constructs for render-thread identity, access, and mutable statics.
//!
//! `MAIN_TID` records the render thread's TID. It is claimed by the first thread
//! to construct a `MainToken`, atomically, and verified on every subsequent construction.
//! `MainToken` is a ZST witness that the holding thread is the render thread.
//! It is `!Send + !Sync`, so rustc rejects any safe code that tries to move it or share it
//! across threads. `MainCell<T>` accessors take `&MainToken`, propagating the
//! "render-thread only" requirement to every call site at the type level.
//!
//! Render thread identity is established at first-hook entry, not at `DllMain`. The thread
//! running `DllMain` and the thread running the render loop are usually the same, but they
//! diverge when something (e.g. the thprac launcher) injects neopatch via `CreateRemoteThread`
//! into a `CREATE_SUSPENDED` process before resuming the real initial thread.

use crate::log::flush;
use crate::process::register_mmcss;
use std::cell::Cell;
use std::marker::PhantomData;
use std::process::abort;
use std::sync::atomic::{AtomicU32, Ordering};
use tracing::{error, info};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

static MAIN_TID: AtomicU32 = AtomicU32::new(0);

pub(crate) fn main_id() -> u32 {
    MAIN_TID.load(Ordering::Acquire)
}

/// ZST witness that the constructing thread is the render thread. Holding `&MainToken`
/// is the compile-time proof required to call `MainCell::get` and `MainCell::set`,
/// as well as any other render-thread-only function in the crate (or in a per-game
/// crate via the `pub use` at the crate root).
///
/// This type is `!Send + !Sync`. Combined with the atomic claim in the constructor,
/// this means: if the constructor returns, then every downstream cell access through
/// the resulting token is on the same thread that first constructed one.
pub struct MainToken(PhantomData<*const ()>);

impl MainToken {
    /// Creates an instance of `MainToken`. The first call claims `MAIN_TID` atomically.
    /// A subsequent call must come from the same thread; otherwise, it aborts the process.
    #[allow(clippy::new_without_default)]
    pub(crate) fn new() -> Self {
        let current = unsafe { GetCurrentThreadId() };
        match MAIN_TID.compare_exchange(0, current, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                info!(kind = "main_thread_claimed");
                let tok = Self(PhantomData);
                register_mmcss(&tok);
                tok
            }
            Err(existing) if existing == current => Self(PhantomData),
            Err(existing) => {
                error!(kind = "main_token_off_main", current, main = existing);
                flush();
                abort();
            }
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
