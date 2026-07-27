//! Runtime rebasing for Touhou 20.
//!
//! Constants are stored as preferred-base VAs. Installation code adds the result of [`host_slide`] to reach the runtime VA.

use neopatch_core::game_addr::GameAddr;
use std::ptr::null;
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

pub(crate) const PREFERRED_IMAGE_BASE: usize = 0x0040_0000;

pub(crate) fn host_slide(host: HMODULE) -> usize {
    host.expose_provenance().wrapping_sub(PREFERRED_IMAGE_BASE)
}

pub(crate) fn current_slide() -> usize {
    host_slide(unsafe { GetModuleHandleW(null()) })
}

/// Returns `GameAddr<T>` for a th20 global at `va + slide`.
///
/// # Safety
/// `va + slide` must point to a value with layout `T`.
pub(crate) unsafe fn rebased_addr<T: Copy>(va: usize, slide: usize) -> GameAddr<T> {
    unsafe { GameAddr::new(va.wrapping_add(slide)) }
}
