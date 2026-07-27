//! Patches and hooks for th17.exe v1.00b.

use neopatch_core::d3d9::call_site_rewrite;
use neopatch_core::patches::PatchSite;
use neopatch_core::screenshot::save_screenshot_live_bmp;
use std::arch::naked_asm;
use std::ffi::c_char;

const DIRECT3DCREATE9_CALL_VA: usize = 0x0046_0f1c;
const DIRECT3DCREATE9_CALL_BYTES: [u8; 6] = [0xff, 0x15, 0x80, 0xa2, 0x49, 0x00];

pub(crate) const PATCHES: &[PatchSite] = &[
    call_site_rewrite(DIRECT3DCREATE9_CALL_VA, &DIRECT3DCREATE9_CALL_BYTES),
    PatchSite::replace(0x0046_21e5, &[0x72, 0x18], &[0xeb, 0x53], "UpdateFast skip"),
    PatchSite::replace(
        0x0046_119e,
        &[0x0f, 0x84, 0x10, 0x01, 0x00, 0x00],
        &[0xe9, 0x2f, 0x01, 0x00, 0x00, 0x90],
        "force fast input latency",
    ),
    PatchSite::nop(0x0044_e633, &[0x75, 0x3a], "replay speed control skip"),
    PatchSite::jmp(
        ANM_MODE57_SPLICE,
        &[0xf3, 0x0f, 0x11, 0x5d, 0x9c],
        anm_mode57_z_trampoline as *mut (),
        "AnmManager mode 5/7 z + matrix.tz",
    ),
    PatchSite::jmp(
        SCREENSHOT_SAVE_FN,
        &SCREENSHOT_SAVE_FN_PROLOGUE,
        screenshot_trampoline as *mut (),
        "screenshot save (fcn.004418c0)",
    ),
];

const ANM_MODE57_SPLICE: usize = 0x0046_e75f;
const ANM_MODE57_DISPLACED_LEN: usize = 5;
static ANM_MODE57_AFTER_SPLICE: usize = ANM_MODE57_SPLICE + ANM_MODE57_DISPLACED_LEN;

#[unsafe(naked)]
unsafe extern "C" fn anm_mode57_z_trampoline() -> ! {
    naked_asm!(
        "addss xmm3, dword ptr [esi + 0x448]",
        "movss dword ptr [ebp - 0x64], xmm3",
        "jmp   dword ptr [{slot}]",
        slot = sym ANM_MODE57_AFTER_SPLICE,
    )
}

const SCREENSHOT_SAVE_FN: usize = 0x0044_18c0;
const SCREENSHOT_SAVE_FN_PROLOGUE: [u8; 5] = [0x53, 0x8b, 0xdc, 0x83, 0xec];

#[unsafe(naked)]
unsafe extern "stdcall" fn screenshot_trampoline(_filename: *const c_char) -> u32 {
    naked_asm!(
        "push dword ptr [esp + 4]",
        "call {save}",
        "add esp, 4",
        "ret 4",
        save = sym save_screenshot_live_bmp,
    );
}
