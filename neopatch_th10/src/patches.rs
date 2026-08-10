//! Patches and hooks for th10.exe v1.00a.

use neopatch_core::cfg_pin::{ByteField, CfgCheck, CfgFile};
use neopatch_core::d3d9::call_site_rewrite;
use neopatch_core::patches::PatchSite;
use neopatch_core::screenshot::save_screenshot_deferred_bmp;
use std::arch::naked_asm;

pub(crate) const CFG_FILE: CfgFile = CfgFile {
    file_name: "th10.cfg",
    magic: 0x0010_0003,
    size: 0x34,
    frameskip: ByteField {
        offset: 0x1e,
        bound: 3,
    },
    other_checks: &[
        CfgCheck::byte_max(0x1a, 2),
        CfgCheck::byte_max(0x1b, 3),
        CfgCheck::byte_max(0x1c, 2),
        CfgCheck::byte_max(0x1d, 2),
        CfgCheck::byte_max(0x1f, 3),
    ],
};
const _: () = CFG_FILE.validate();

const DIRECT3DCREATE9_CALL_VA: usize = 0x0043_8bc3;
const DIRECT3DCREATE9_CALL_BYTES: [u8; 5] = [0xe8, 0xae, 0x95, 0x01, 0x00];

pub(crate) const PATCHES: &[PatchSite] = &[
    call_site_rewrite(DIRECT3DCREATE9_CALL_VA, &DIRECT3DCREATE9_CALL_BYTES),
    PatchSite::nop(
        0x0043_93b7,
        &[0x0f, 0x85, 0x6a, 0x01, 0x00, 0x00],
        "Sleep-path branch nop",
    ),
    PatchSite::replace(
        0x0043_93c5,
        &[0x75, 0x22],
        &[0xeb, 0x22],
        "frame limiter unconditional skip",
    ),
    PatchSite::replace(
        0x0044_343b,
        &[0xd9, 0x80, 0x50, 0x03, 0x00, 0x00],
        &[0xd9, 0x80, 0x54, 0x03, 0x00, 0x00],
        "AnmManager mode 2 y -> z",
    ),
    PatchSite::replace(
        0x0043_98d4,
        &[0x74, 0x11],
        &[0xeb, 0x11],
        "32-bit color skip force-16-bit branch",
    ),
    PatchSite::nop(
        0x0043_9916,
        &[0x0f, 0x95, 0xc1],
        "32-bit color ignore persistent choice",
    ),
    PatchSite::jmp(
        ANM_MODE57_SPLICE,
        &[0x8b, 0x9b, 0x5c, 0x03, 0x00, 0x00],
        anm_mode57_z_trampoline as *mut (),
        "AnmManager mode 5/7 z + matrix.tz",
    ),
    PatchSite::jmp(
        SCREENSHOT_SAVE_FN,
        &SCREENSHOT_SAVE_FN_PROLOGUE,
        screenshot_trampoline as *mut (),
        "screenshot save (fcn.00420670)",
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
        "call {save}",
        "add esp, 4",
        "ret",
        save = sym save_screenshot_deferred_bmp,
    );
}
