//! Utility for temporarily making a memory region writable.
//!
//! We always open `PAGE_READWRITE` and restore the saved protection on exit.
//! The OS enforces NX only at instruction fetch, which doesn't happen during
//! the protect/write/restore window, so this works for `.text`, `.rdata`, and `.idata` alike.

use std::ffi::c_void;
use windows_sys::Win32::System::Memory::{PAGE_PROTECTION_FLAGS, PAGE_READWRITE, VirtualProtect};

/// Temporarily makes a memory region writable for the duration of `f`,
/// then restores its original page protection.
///
/// Returns `Some(f(addr))` on success, or `None` if the initial protection change fails
/// (in which case `f` is not called). If restoring the original protection fails,
/// that failure is silently ignored and `Some(_)` is still returned.
#[must_use]
pub(crate) unsafe fn with_writable<R>(
    addr: *mut u8,
    len: usize,
    f: impl FnOnce(*mut u8) -> R,
) -> Option<R> {
    unsafe {
        let target: *mut c_void = addr.cast();
        let mut saved: PAGE_PROTECTION_FLAGS = 0;
        // We don't use a RAII guard around the restore because we have `panic = "abort"`.
        // `f` either returns or aborts the process; it never unwinds.
        if VirtualProtect(target, len, PAGE_READWRITE, &raw mut saved) == 0 {
            return None;
        }
        let result = f(addr);
        let mut tmp: PAGE_PROTECTION_FLAGS = 0;
        VirtualProtect(target, len, saved, &raw mut tmp);
        Some(result)
    }
}
