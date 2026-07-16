//! Patches and hooks for th20.exe v1.00a.

use crate::aslr::{rebased_call_site_rewrite, rebased_patch, rebased_patch_jmp};
use neopatch_core::screenshot::save_screenshot_live_png;
use std::arch::naked_asm;
use std::ffi::c_char;

const DIRECT3DCREATE9_CALL_VA: usize = 0x0041_c335;
const DIRECT3DCREATE9_CALL_BYTES: [u8; 5] = [0xe8, 0x32, 0x43, 0x12, 0x00];

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

pub(crate) unsafe fn install(slide: usize) {
    unsafe {
        rebased_call_site_rewrite(slide, DIRECT3DCREATE9_CALL_VA, &DIRECT3DCREATE9_CALL_BYTES);
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
        rebased_patch_jmp(
            slide,
            SCREENSHOT_SAVE_FN_VA,
            &SCREENSHOT_SAVE_FN_PROLOGUE,
            screenshot_trampoline as *mut (),
            "screenshot save (fcn.004de040)",
        );
    }
}
