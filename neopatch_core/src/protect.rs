//! Utility for temporarily making a memory region writable.
//!
//! We always open `PAGE_READWRITE` and restore the saved protection on exit. The OS enforces NX only at instruction fetch,
//! which never targets `.rdata` or `.idata`. For `.text`, dropping execute is safe only because no thread fetches from the page
//! during the window. `.text` and IAT patches run in `DllMain`, while the process is effectively single-threaded.
//! vtable patches run later, when the game creates the COM object, but still on that one startup thread.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use tracing::warn;
use windows_sys::Win32::System::Memory::{PAGE_PROTECTION_FLAGS, PAGE_READWRITE, VirtualProtect};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

/// The number of protection windows currently open across all threads.
static ACTIVE_WINDOWS: AtomicU32 = AtomicU32::new(0);

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
    if ACTIVE_WINDOWS.fetch_add(1, Ordering::Relaxed) > 0 {
        warn!(
            kind = "writable_window_overlap",
            tid = unsafe { GetCurrentThreadId() },
        );
    }

    let target: *mut c_void = addr.cast();
    let mut saved: PAGE_PROTECTION_FLAGS = 0;
    // We don't use a RAII guard around the restore (or the window count) because we have
    // `panic = "abort"`. `f` either returns or aborts the process; it never unwinds.
    if unsafe { VirtualProtect(target, len, PAGE_READWRITE, &raw mut saved) } == 0 {
        ACTIVE_WINDOWS.fetch_sub(1, Ordering::Relaxed);
        return None;
    }
    let result = f(addr);
    let mut tmp: PAGE_PROTECTION_FLAGS = 0;
    unsafe { VirtualProtect(target, len, saved, &raw mut tmp) };
    ACTIVE_WINDOWS.fetch_sub(1, Ordering::Relaxed);
    Some(result)
}
