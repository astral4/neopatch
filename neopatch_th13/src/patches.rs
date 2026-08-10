//! Patches and hooks for th13.exe v1.00c.

use neopatch_core::cfg_pin::{ByteField, CfgCheck, CfgFile};
use neopatch_core::d3d9::call_site_rewrite;
use neopatch_core::patches::PatchSite;
use neopatch_core::screenshot::save_screenshot_live_bmp;
use std::arch::naked_asm;

pub(crate) const CFG_FILE: CfgFile = CfgFile {
    file_name: "th13.cfg",
    magic: 0x0013_0001,
    size: 0x3c,
    frameskip: ByteField {
        offset: 0x20,
        bound: 3,
    },
    other_checks: &[
        CfgCheck::byte_max(0x1c, 2),
        CfgCheck::byte_max(0x1d, 3),
        CfgCheck::byte_max(0x1e, 2),
        CfgCheck::byte_max(0x1f, 4),
        CfgCheck::byte_max(0x21, 3),
    ],
};
const _: () = CFG_FILE.validate();

const DIRECT3DCREATE9_CALL_VA: usize = 0x0045_c42f;
const DIRECT3DCREATE9_CALL_BYTES: [u8; 6] = [0xff, 0x15, 0x98, 0x22, 0x4a, 0x00];

pub(crate) const PATCHES: &[PatchSite] = &[
    call_site_rewrite(DIRECT3DCREATE9_CALL_VA, &DIRECT3DCREATE9_CALL_BYTES),
    PatchSite::replace(0x0045_d334, &[0x75, 0x08], &[0xeb, 0x5b], "UpdateFast skip"),
    PatchSite::replace(
        0x0045_c5d7,
        &[0x74, 0x0c],
        &[0xeb, 0x0c],
        "fast input latency #1",
    ),
    PatchSite::replace(
        0x0045_c5eb,
        &[0x75, 0x15],
        &[0xeb, 0x15],
        "fast input latency #2",
    ),
    PatchSite::replace(
        0x0044_8e6f,
        &[0x75, 0x04],
        &[0xeb, 0x1d],
        "replay speed control skip",
    ),
    PatchSite::jmp(
        ANM_MODE57_SPLICE,
        &[0x8b, 0x9b, 0x70, 0x05, 0x00, 0x00],
        anm_mode57_z_trampoline as *mut (),
        "AnmManager mode 5/7 z + matrix.tz",
    ),
    PatchSite::jmp(
        SCREENSHOT_SAVE_FN,
        &SCREENSHOT_SAVE_FN_PROLOGUE,
        screenshot_trampoline as *mut (),
        "screenshot save (fcn.0043a950)",
    ),
];

const ANM_MODE57_SPLICE: usize = 0x0046_8fc9;
const ANM_MODE57_DISPLACED_LEN: usize = 6;
static ANM_MODE57_AFTER_SPLICE: usize = ANM_MODE57_SPLICE + ANM_MODE57_DISPLACED_LEN;

#[unsafe(naked)]
unsafe extern "C" fn anm_mode57_z_trampoline() -> ! {
    naked_asm!(
        "fadd dword ptr [ebp - 0x5c]",
        "mov ebx, [ebx + 0x570]",
        "jmp dword ptr [{slot}]",
        slot = sym ANM_MODE57_AFTER_SPLICE,
    )
}

const SCREENSHOT_SAVE_FN: usize = 0x0043_a950;
const SCREENSHOT_SAVE_FN_PROLOGUE: [u8; 5] = [0x55, 0x8b, 0xec, 0x83, 0xec];

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
