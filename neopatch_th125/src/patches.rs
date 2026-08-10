//! Patches and hooks for th125.exe v1.00a.

use neopatch_core::cfg_pin::{ByteField, CfgCheck, CfgFile};
use neopatch_core::d3d9::call_site_rewrite;
use neopatch_core::patches::PatchSite;
use neopatch_core::screenshot::save_screenshot_live_bmp;
use std::arch::naked_asm;

pub(crate) const CFG_FILE: CfgFile = CfgFile {
    file_name: "th125.cfg",
    magic: 0x0012_0501,
    size: 0x3c,
    frameskip: ByteField {
        offset: 0x1e,
        bound: 3,
    },
    other_checks: &[
        CfgCheck::byte_max(0x1a, 2),
        CfgCheck::byte_max(0x1b, 3),
        CfgCheck::byte_max(0x1c, 2),
        CfgCheck::byte_max(0x1d, 4),
        CfgCheck::byte_max(0x1f, 3),
    ],
};
const _: () = CFG_FILE.validate();

const DIRECT3DCREATE9_CALL_VA: usize = 0x0044_e179;
const DIRECT3DCREATE9_CALL_BYTES: [u8; 5] = [0xe8, 0x78, 0xd0, 0x01, 0x00];

pub(crate) const PATCHES: &[PatchSite] = &[
    call_site_rewrite(DIRECT3DCREATE9_CALL_VA, &DIRECT3DCREATE9_CALL_BYTES),
    PatchSite::replace(0x0044_f113, &[0x75, 0x08], &[0xeb, 0x5b], "UpdateFast skip"),
    PatchSite::replace(
        0x0044_e2f7,
        &[0x74, 0x0c],
        &[0xeb, 0x0c],
        "fast input latency #1",
    ),
    PatchSite::replace(
        0x0044_e30b,
        &[0x75, 0x15],
        &[0xeb, 0x15],
        "fast input latency #2",
    ),
    PatchSite::replace(
        0x0043_c86c,
        &[0x74, 0x14],
        &[0xeb, 0x14],
        "replay speed control skip",
    ),
    PatchSite::jmp(
        ANM_MODE57_SPLICE,
        &[0xd8, 0x83, 0x4c, 0x04, 0x00, 0x00],
        anm_mode57_z_trampoline as *mut (),
        "AnmManager mode 5/7 z + matrix.tz",
    ),
    PatchSite::jmp(
        SCREENSHOT_SAVE_FN,
        &SCREENSHOT_SAVE_FN_PROLOGUE,
        screenshot_trampoline as *mut (),
        "screenshot save (fcn.004298d0)",
    ),
];

const ANM_MODE57_SPLICE: usize = 0x0045_a973;
const ANM_MODE57_DISPLACED_LEN: usize = 6;
static ANM_MODE57_AFTER_SPLICE: usize = ANM_MODE57_SPLICE + ANM_MODE57_DISPLACED_LEN;

#[unsafe(naked)]
unsafe extern "C" fn anm_mode57_z_trampoline() -> ! {
    naked_asm!(
        "fadd dword ptr [ebx + 0x44c]",
        "fadd dword ptr [esp + 0x48]",
        "jmp dword ptr [{slot}]",
        slot = sym ANM_MODE57_AFTER_SPLICE,
    )
}

const SCREENSHOT_SAVE_FN: usize = 0x0042_98d0;
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
