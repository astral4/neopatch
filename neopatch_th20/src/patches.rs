//! Patches and hooks for th20.exe v1.00a.

use crate::aslr::{rebased_call_site_rewrite, rebased_patch, rebased_patch_jmp};
use neopatch_core::screenshot::save_screenshot_live_png;
use std::arch::naked_asm;
use std::ffi::c_char;

/// Live `Direct3DCreate9` call site, rewritten to defend against downstream IAT hijacks.
/// This is the only call site in th20.
const DIRECT3DCREATE9_CALL_VA: usize = 0x0041_c335;
const DIRECT3DCREATE9_CALL_BYTES: [u8; 5] = [0xe8, 0x32, 0x43, 0x12, 0x00];

pub(crate) unsafe fn install_d3d9_call_site_rewrite(slide: usize) {
    unsafe {
        rebased_call_site_rewrite(slide, DIRECT3DCREATE9_CALL_VA, &DIRECT3DCREATE9_CALL_BYTES);
    }
}

/// "fast input latency #1/#2": flips both `jne` gates in main's input-mode dispatch to
/// unconditional `jmp`, routing onto `CWindowManager::UpdateFast`. OILP also does this
/// under "Force fast input latency mode."
///
/// "modal body skip": main's modal gate at `0x0041eb48` falls through to a separate self-paced
/// frame routine (`fcn.00419c20`) when bit 2 of `DAT_005b87e8` is set (e.g. replay viewer).
/// Flipping the `jz` to `jmp` keeps `UpdateFast` running; without it, that routine's pacer
/// (`fcn.004193e0`) fights ours.
///
/// "UpdateFast skip": unconditional `jmp +0x65` past the `Sleep(1)`, deadline comparison, and
/// deadline-advance spin inside `CWindowManager::UpdateFast` (`fcn.00419de0`), so our pacer is the
/// sole timing source.
///
/// "replay speed control skip": flips the viewer-mode `jnz` in `fcn.005081e0` to `jmp` so the
/// game's own replay-speed control doesn't fight our pacer.
pub(crate) unsafe fn apply_basic(slide: usize) {
    unsafe {
        rebased_patch(
            slide,
            0x0041_eb71,
            &[0x75, 0x2b],
            &[0xeb, 0x2b],
            "fast input latency #1",
        );
        rebased_patch(
            slide,
            0x0041_eb7c,
            &[0x75, 0x20],
            &[0xeb, 0x20],
            "fast input latency #2",
        );
        rebased_patch(
            slide,
            0x0041_eb48,
            &[0x74, 0x20],
            &[0xeb, 0x20],
            "modal body skip",
        );
        rebased_patch(
            slide,
            0x0041_9e64,
            &[0x72, 0x09],
            &[0xeb, 0x65],
            "UpdateFast skip",
        );
        rebased_patch(
            slide,
            0x0050_8203,
            &[0x0f, 0x85, 0xf4, 0x00, 0x00, 0x00],
            &[0xe9, 0xf5, 0x00, 0x00, 0x00, 0x90],
            "replay speed control skip",
        );
    }
}

/// th20 screenshot save (thiscall; `this` in ECX, filename pointer pushed on the stack).
/// The game calls this from the render thread before `Present`.
/// Vanilla uses `this` for its device and deferred saver; our `stdcall` trampoline
/// discards it and saves via the tracked device in the core crate.
const SCREENSHOT_SAVE_FN_VA: usize = 0x004d_e040;
const SCREENSHOT_SAVE_FN_PROLOGUE: [u8; 5] = [0x55, 0x8b, 0xec, 0x83, 0xec];

#[unsafe(naked)]
unsafe extern "stdcall" fn screenshot_trampoline(_filename: *const c_char) -> u32 {
    naked_asm!(
        "push dword ptr [esp + 4]",
        "call {save}",
        "add esp, 4",
        "ret 4",
        save = sym save_screenshot_live_png,
    );
}

pub(crate) unsafe fn install_screenshot_hook(slide: usize) {
    unsafe {
        rebased_patch_jmp(
            slide,
            SCREENSHOT_SAVE_FN_VA,
            &SCREENSHOT_SAVE_FN_PROLOGUE,
            screenshot_trampoline as *mut (),
            "screenshot save (fcn.004de040)",
        );
    }
}
