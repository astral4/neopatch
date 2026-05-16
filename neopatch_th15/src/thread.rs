//! Main-thread identity.
//!
//! Set once at DllMain by `lib.rs::install_hooks`; used for log labels and debug assertions.

use std::sync::atomic::{AtomicU32, Ordering};
#[cfg(debug_assertions)]
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

static MAIN_TID: AtomicU32 = AtomicU32::new(0);

pub(crate) fn set_main_id(tid: u32) {
    MAIN_TID.store(tid, Ordering::Release);
}

pub(crate) fn main_id() -> u32 {
    MAIN_TID.load(Ordering::Acquire)
}

#[inline]
pub(crate) fn debug_assert_main() {
    #[cfg(debug_assertions)]
    {
        let current = unsafe { GetCurrentThreadId() };
        let main = MAIN_TID.load(Ordering::Acquire);
        debug_assert_eq!(current, main, "must run on the main (Present) thread");
    }
}
