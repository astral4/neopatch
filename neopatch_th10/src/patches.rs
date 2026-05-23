//! Static byte patches for th10.exe v1.00a.

use neopatch_core::patches::{Patch, patch_jmp};
use std::arch::naked_asm;

/// "Sleep-path branch nop": NOPs the `jne 0x439527` inside `fcn.00439390`
/// (`CWindowManager::Update`). The branch target contains the game's `Sleep`
/// at `0x0043952B`, so killing the branch keeps the loop from reaching it.
///
/// "frame limiter unconditional skip": flips a post-FPU (x87) `jne` to `jmp` so the loop
/// always lands past the deadline check. Together with the above, this disengages
/// the game's pacer in favor of ours.
///
/// "AnmManager mode 2 y -> z": fixes a typo in `fcn.00443290`. The Y component
/// of `parentPos` (`0x350`) is used in a sum instead of the Z component (`0x354`).
/// This bug is reached in render mode 2, and modes 1 and 3 when rotation is exactly 0.
pub(crate) const PATCHES: &[Patch] = &[
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
];

/// Stack offset of the `matrix.tz` slot in `fcn.00444240`'s frame.
const MATRIX_TZ_SLOT: i32 = 0x74;
/// Offset of the `AnmVm` flags field, read by the displaced `mov ebx, [ebx + ...]`.
const VM_FLAGS_OFFSET: i32 = 0x35c;
/// Resume target past the displaced mov at the splice.
const ANM_MODE57_AFTER_SPLICE: usize = 0x0044_4394;

/// Adds the missing matrix.tz `fadd` before the Z `fstp` in `fcn.00444240`,
/// the position helper used by `AnmManager` render modes 5 and 7.
/// X and Y correctly accumulate their `matrix.t*`, unlike Z.
#[unsafe(naked)]
unsafe extern "C" fn anm_mode57_z_trampoline() {
    naked_asm!(
        "fadd dword ptr [esp + {tz_slot}]",
        "mov  ebx, [ebx + {flags_off}]",
        "mov  eax, {after_splice}",
        "jmp  eax",
        tz_slot      = const MATRIX_TZ_SLOT,
        flags_off    = const VM_FLAGS_OFFSET,
        after_splice = const ANM_MODE57_AFTER_SPLICE,
    )
}

pub(crate) unsafe fn apply_basic() {
    unsafe { Patch::apply_all(PATCHES) };
}

pub(crate) unsafe fn install_anm_matrix_tz_fix() {
    unsafe {
        patch_jmp(
            0x0044_438e,
            &[0x8b, 0x9b, 0x5c, 0x03, 0x00],
            anm_mode57_z_trampoline as *mut (),
            "AnmManager mode 5/7 z + matrix.tz",
        );
    }
}
