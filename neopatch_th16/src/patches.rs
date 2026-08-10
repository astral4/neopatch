//! Patches and hooks for th16.exe v1.00a.

use neopatch_core::cfg_pin::{ByteField, CfgCheck, CfgFile};
use neopatch_core::d3d9::call_site_rewrite;
use neopatch_core::patches::PatchSite;
use neopatch_core::screenshot::save_screenshot_live_bmp;
use std::arch::naked_asm;
use std::ffi::c_char;

pub(crate) const CFG_FILE: CfgFile = CfgFile {
    file_name: "th16.cfg",
    magic: 0x0016_0002,
    size: 0x64,
    frameskip: ByteField {
        offset: 0x20,
        bound: 3,
    },
    other_checks: &[
        CfgCheck::byte_max(0x1c, 2),
        CfgCheck::byte_max(0x1d, 3),
        CfgCheck::byte_max(0x1e, 2),
        CfgCheck::byte_max(0x1f, 6),
        CfgCheck::byte_max(0x21, 3),
    ],
};
const _: () = CFG_FILE.validate();

const DIRECT3DCREATE9_CALL_VA: usize = 0x0045_9a84;
const DIRECT3DCREATE9_CALL_BYTES: [u8; 6] = [0xff, 0x15, 0x5c, 0xb2, 0x48, 0x00];

pub(crate) const PATCHES: &[PatchSite] = &[
    call_site_rewrite(DIRECT3DCREATE9_CALL_VA, &DIRECT3DCREATE9_CALL_BYTES),
    PatchSite::replace(0x0045_ac9d, &[0x72, 0x08], &[0xeb, 0x4b], "UpdateFast skip"),
    PatchSite::replace(
        0x0045_9f72,
        &[0x74, 0x0c],
        &[0xeb, 0x0c],
        "fast input latency #1",
    ),
    PatchSite::replace(
        0x0045_9f87,
        &[0x75, 0x15],
        &[0xeb, 0x15],
        "fast input latency #2",
    ),
    PatchSite::replace(
        0x0044_8e62,
        &[0x74, 0x19],
        &[0xeb, 0x19],
        "replay speed control skip",
    ),
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
        "screenshot save (fcn.0043bbd0)",
    ),
];

const ANM_MODE57_SPLICE: usize = 0x0046_70ff;
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

const SCREENSHOT_SAVE_FN: usize = 0x0043_bbd0;
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
