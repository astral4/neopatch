//! Utility for transiently making a memory region writable.
//!
//! We always open `PAGE_READWRITE` and restore the saved protection on exit.
//! The OS enforces NX only at instruction fetch, which doesn't happen during
//! the protect/write/restore window, so this works for `.text`, `.rdata`, and `.idata` alike.

use std::ffi::c_void;
use windows_sys::Win32::System::Memory::{PAGE_PROTECTION_FLAGS, PAGE_READWRITE, VirtualProtect};

// We don't use a RAII guard around the restore because we have `panic = "abort"`.
// `f` either returns or aborts the process; it never unwinds.
pub(crate) unsafe fn with_writable<R>(
    addr: *mut u8,
    len: usize,
    f: impl FnOnce(*mut u8) -> R,
) -> Option<R> {
    unsafe {
        let target: *mut c_void = addr.cast();
        let mut saved: PAGE_PROTECTION_FLAGS = 0;
        if VirtualProtect(target, len, PAGE_READWRITE, &raw mut saved) == 0 {
            return None;
        }
        let result = f(addr);
        let mut tmp: PAGE_PROTECTION_FLAGS = 0;
        VirtualProtect(target, len, saved, &raw mut tmp);
        Some(result)
    }
}
