//! Patches and hooks for th11.exe v1.00a.

use neopatch_core::d3d9::install_call_site_rewrite;
use neopatch_core::patches::{Patch, patch_jmp};
use neopatch_core::screenshot::save_screenshot_live_bmp;
use std::arch::naked_asm;

const DIRECT3DCREATE9_CALL_VA: usize = 0x0044_570e;
const DIRECT3DCREATE9_CALL_BYTES: [u8; 5] = [0xe8, 0xa3, 0xa2, 0x01, 0x00];

const PATCHES: &[Patch] = &[
    Patch::new(0x0044_6454, &[0x75, 0x08], &[0xeb, 0x43], "UpdateFast skip"),
    Patch::new(
        0x0044_5877,
        &[0x74, 0x0c],
        &[0xeb, 0x0c],
        "fast input latency #1",
    ),
    Patch::new(
        0x0044_588b,
        &[0x75, 0x15],
        &[0xeb, 0x15],
        "fast input latency #2",
    ),
    Patch::new(
        0x0043_6d5f,
        &[0x74, 0x14],
        &[0xeb, 0x14],
        "replay speed control skip",
    ),
];

const ANM_MODE57_SPLICE: usize = 0x0045_0f83;
const ANM_MODE57_DISPLACED_LEN: usize = 6;
static ANM_MODE57_AFTER_SPLICE: usize = ANM_MODE57_SPLICE + ANM_MODE57_DISPLACED_LEN;

#[unsafe(naked)]
unsafe extern "C" fn anm_mode57_z_trampoline() -> ! {
    naked_asm!(
        "fadd dword ptr [esp + 0x78]",
        "mov  ebx, [ebx + 0x404]",
        "jmp  dword ptr [{slot}]",
        slot = sym ANM_MODE57_AFTER_SPLICE,
    )
}

const SCREENSHOT_SAVE_FN: usize = 0x0042_9ca0;
const SCREENSHOT_SAVE_FN_PROLOGUE: [u8; 5] = [0x83, 0xec, 0x10, 0x83, 0x3d];

#[unsafe(naked)]
unsafe extern "C" fn screenshot_trampoline() -> u32 {
    naked_asm!(
        "push eax",
        "call {save}",
        "add esp, 4",
        "ret",
        save = sym save_screenshot_live_bmp,
    );
}

pub(crate) unsafe fn install() {
    unsafe {
        install_call_site_rewrite(DIRECT3DCREATE9_CALL_VA, &DIRECT3DCREATE9_CALL_BYTES);
        Patch::apply_all(PATCHES);
        patch_jmp(
            ANM_MODE57_SPLICE,
            &[0x8b, 0x9b, 0x04, 0x04, 0x00, 0x00],
            anm_mode57_z_trampoline as *mut (),
            "AnmManager mode 5/7 z + matrix.tz",
        );
        patch_jmp(
            SCREENSHOT_SAVE_FN,
            &SCREENSHOT_SAVE_FN_PROLOGUE,
            screenshot_trampoline as *mut (),
            "screenshot save (fcn.00429ca0)",
        );
    }
}
