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
///
/// "32-bit color skip force-16-bit branch" and "32-bit color ignore persistent choice":
/// forces `pp.BackBufferFormat = X8R8G8B8` in `fcn.00439890`'s fullscreen path regardless of
/// the `custom.exe` color-depth selection. The first flips a `je` to `jmp` so the
/// `[0x491d78] & 1` "force 16-bit" fallback is bypassed; the second NOPs the `setne cl`
/// between `xor ecx, ecx` and `add ecx, 0x16`, so the persistent `[0x491d62]` choice
/// can never push the result from `0x16` (X8R8G8B8) to `0x17` (R5G6B5). Windowed mode pulls
/// the format from the desktop display mode, so no patches are needed there.
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

/// Location of the `mov ebx, [ebx + 0x35c]` we displace with `e9 disp32`.
const ANM_MODE57_SPLICE: usize = 0x0044_438e;
/// Length of the displaced instruction (6 bytes for `8B 9B disp32`).
const ANM_MODE57_DISPLACED_LEN: usize = 6;
/// Resume target past the displaced `mov` at the splice.
static ANM_MODE57_AFTER_SPLICE: usize = ANM_MODE57_SPLICE + ANM_MODE57_DISPLACED_LEN;

/// Adds the missing `matrix.tz` `fadd` before the Z `fstp` in `fcn.00444240`, the position
/// helper used by `AnmManager` render modes 5 and 7. X and Y correctly accumulate their
/// `matrix.t*`, unlike Z. `[esp + 0x74]` is the `matrix.tz` slot in the function's frame.
/// `[ebx + 0x35c]` is the `AnmVm` flags field (replayed from the displaced `mov`).
#[unsafe(naked)]
unsafe extern "C" fn anm_mode57_z_trampoline() -> ! {
    naked_asm!(
        "fadd dword ptr [esp + 0x74]",
        "mov  ebx, [ebx + 0x35c]",
        "jmp  dword ptr [{slot}]",
        slot = sym ANM_MODE57_AFTER_SPLICE,
    )
}

pub(crate) unsafe fn apply_basic() {
    unsafe { Patch::apply_all(PATCHES) };
}

pub(crate) unsafe fn install_anm_matrix_tz_fix() {
    unsafe {
        patch_jmp(
            ANM_MODE57_SPLICE,
            &[0x8b, 0x9b, 0x5c, 0x03, 0x00],
            anm_mode57_z_trampoline as *mut (),
            "AnmManager mode 5/7 z + matrix.tz",
        );
    }
}
