//! Runtime rebasing for Touhou 20.
//!
//! Constants are stored as preferred-base VAs. Installation code adds [`host_slide()`] to reach the runtime VA.

use neopatch_core::d3d9::install_call_site_rewrite;
use neopatch_core::game_addr::GameAddr;
use neopatch_core::patches::{Patch, patch_jmp};
use windows_sys::Win32::Foundation::HMODULE;

pub(crate) const PREFERRED_IMAGE_BASE: usize = 0x0040_0000;

pub(crate) fn host_slide(host: HMODULE) -> usize {
    host.expose_provenance().wrapping_sub(PREFERRED_IMAGE_BASE)
}

/// # Safety
/// `va + slide` must be a writable code address.
pub(crate) unsafe fn rebased_patch<const N: usize>(
    slide: usize,
    va: usize,
    expected: &'static [u8; N],
    replacement: &'static [u8; N],
    name: &'static str,
) {
    unsafe { Patch::new(va.wrapping_add(slide), expected, replacement, name).apply() };
}

/// # Safety
/// `va + slide` must be a writable code address holding `expected`.
pub(crate) unsafe fn rebased_patch_jmp<const N: usize>(
    slide: usize,
    va: usize,
    expected: &[u8; N],
    hook: *mut (),
    name: &str,
) {
    unsafe { patch_jmp(va.wrapping_add(slide), expected, hook, name) };
}

/// # Safety
/// `va + slide` must be a writable code address holding `expected`.
pub(crate) unsafe fn rebased_call_site_rewrite<const N: usize>(
    slide: usize,
    va: usize,
    expected: &[u8; N],
) {
    unsafe { install_call_site_rewrite(va.wrapping_add(slide), expected) };
}

/// Returns `GameAddr<T>` for a th20 global at `va + slide`.
///
/// # Safety
/// `va + slide` must point to a value with layout `T`.
pub(crate) unsafe fn rebased_addr<T: Copy>(slide: usize, va: usize) -> GameAddr<T> {
    unsafe { GameAddr::new(va.wrapping_add(slide)) }
}
