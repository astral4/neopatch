//! Patches and hooks for th10.exe v1.00a.

use neopatch_core::d3d9::install_call_site_rewrite;
use neopatch_core::patches::{Patch, patch_jmp};
use neopatch_core::screenshot::save_screenshot_deferred_bmp;
use std::arch::naked_asm;

const DIRECT3DCREATE9_CALL_VA: usize = 0x0043_8bc3;
const DIRECT3DCREATE9_CALL_BYTES: [u8; 5] = [0xe8, 0xae, 0x95, 0x01, 0x00];

const PATCHES: &[Patch] = &[
    Patch::new(
        0x0043_93b7,
        &[0x0f, 0x85, 0x6a, 0x01, 0x00, 0x00],
        &[0x90, 0x90, 0x90, 0x90, 0x90, 0x90],
        "Sleep-path branch nop",
    ),
    Patch::new(
        0x0043_93c5,
        &[0x75, 0x22],
        &[0xeb, 0x22],
        "frame limiter unconditional skip",
    ),
    Patch::new(
        0x0044_343b,
        &[0xd9, 0x80, 0x50, 0x03, 0x00, 0x00],
        &[0xd9, 0x80, 0x54, 0x03, 0x00, 0x00],
        "AnmManager mode 2 y -> z",
    ),
    Patch::new(
        0x0043_98d4,
        &[0x74, 0x11],
        &[0xeb, 0x11],
        "32-bit color skip force-16-bit branch",
    ),
    Patch::new(
        0x0043_9916,
        &[0x0f, 0x95, 0xc1],
        &[0x90, 0x90, 0x90],
        "32-bit color ignore persistent choice",
    ),
];

const ANM_MODE57_SPLICE: usize = 0x0044_438e;
const ANM_MODE57_DISPLACED_LEN: usize = 6;
static ANM_MODE57_AFTER_SPLICE: usize = ANM_MODE57_SPLICE + ANM_MODE57_DISPLACED_LEN;

#[unsafe(naked)]
unsafe extern "C" fn anm_mode57_z_trampoline() -> ! {
    naked_asm!(
        "fadd dword ptr [esp + 0x74]",
        "mov ebx, [ebx + 0x35c]",
        "jmp dword ptr [{slot}]",
        slot = sym ANM_MODE57_AFTER_SPLICE,
    )
}

const SCREENSHOT_SAVE_FN: usize = 0x0042_0670;
const SCREENSHOT_SAVE_FN_PROLOGUE: [u8; 5] = [0x83, 0xec, 0x0c, 0x53, 0x55];

#[unsafe(naked)]
unsafe extern "C" fn screenshot_trampoline() -> u32 {
    naked_asm!(
        "push eax",
        "call {stash}",
        "add esp, 4",
        "ret",
        stash = sym save_screenshot_deferred_bmp,
    );
}

pub(crate) unsafe fn install() {
    unsafe {
        install_call_site_rewrite(DIRECT3DCREATE9_CALL_VA, &DIRECT3DCREATE9_CALL_BYTES);
        Patch::apply_all(PATCHES);
        patch_jmp(
            ANM_MODE57_SPLICE,
            &[0x8b, 0x9b, 0x5c, 0x03, 0x00, 0x00],
            anm_mode57_z_trampoline as *mut (),
            "AnmManager mode 5/7 z + matrix.tz",
        );
        patch_jmp(
            SCREENSHOT_SAVE_FN,
            &SCREENSHOT_SAVE_FN_PROLOGUE,
            screenshot_trampoline as *mut (),
            "screenshot save (fcn.00420670)",
        );
    }
}
