//! Utility for temporarily making a memory region writable.
//!
//! We always open `PAGE_READWRITE` and restore the saved protection on exit. The OS enforces NX only at instruction fetch,
//! which never targets `.rdata` or `.idata`. For `.text`, dropping execute is safe only because no thread fetches from the page
//! during the window: all code patches run in `DllMain`, while the process is effectively single-threaded.
//! A runtime `.text` patch would need to revisit this.

use std::ffi::c_void;
use windows_sys::Win32::System::Memory::{PAGE_PROTECTION_FLAGS, PAGE_READWRITE, VirtualProtect};

/// Temporarily makes a memory region writable for the duration of `f`, then restores the original protection.
/// `VirtualProtect` reports only the first page's previous protection, so the restore applies that across the whole range.
/// Therefore, callers must not span pages with differing protections.
///
/// Returns `Some(f(addr))` on success, or `None` if the initial protection change fails (in which case `f` is not called).
/// If restoring the original protection fails, that failure is silently ignored and `Some(_)` is still returned.
///
/// # Safety
/// `addr` and `len` must describe a committed memory region whose protection `VirtualProtect` can change to `PAGE_READWRITE` and back.
/// The function only opens the window; whatever `f` writes through `addr` is the caller's responsibility.
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
