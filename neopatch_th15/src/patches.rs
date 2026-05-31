//! Patches and hooks for th15.exe v1.00b.

use neopatch_core::d3d9::install_call_site_rewrite;
use neopatch_core::patches::{Patch, patch_jmp};
use neopatch_core::screenshot::save_screenshot_live;
use std::arch::naked_asm;
use std::ffi::c_char;

/// Live `Direct3DCreate9` call site, rewritten to defend against downstream IAT hijacks.
/// There is a second call site at `0x00472e72`, a dead standalone init helper that nothing calls.
const DIRECT3DCREATE9_CALL_ADDR: usize = 0x0047_158c;
const DIRECT3DCREATE9_CALL_BYTES: [u8; 6] = [0xff, 0x15, 0xb0, 0xe2, 0x4b, 0x00];

pub(crate) unsafe fn install_d3d9_call_site_rewrite() {
    unsafe {
        install_call_site_rewrite(DIRECT3DCREATE9_CALL_ADDR, &DIRECT3DCREATE9_CALL_BYTES);
    }
}

/// "UpdateFast skip": unconditional `jmp +0x4A` past the game's `Sleep`, spin,
/// and deadline-advance. Without this, the game's pacer holds the inter-`Present`
/// interval at >=33ms in replay slowdown mode.
///
/// "fast input latency #1/#2": flips the cond jumps to `EB`, forcing the input preamble
/// to "fast" mode. OILP also does this under "Force fast input latency mode."
///
/// "replay speed control skip": skips the game's own replay-speed control
/// so it doesn't fight our pacer.
const PATCHES: &[Patch] = &[
    Patch::new(0x0047_27de, &[0x72, 0x08], &[0xeb, 0x4a], "UpdateFast skip"),
    Patch::new(
        0x0047_1a86,
        &[0x74, 0x0c],
        &[0xeb, 0x0c],
        "fast input latency #1",
    ),
    Patch::new(
        0x0047_1a9b,
        &[0x75, 0x15],
        &[0xeb, 0x15],
        "fast input latency #2",
    ),
    Patch::new(
        0x0045_ced2,
        &[0x75, 0x04],
        &[0xeb, 0x1d],
        "replay speed control skip",
    ),
];

pub(crate) unsafe fn apply_basic() {
    unsafe { Patch::apply_all(PATCHES) };
}

/// Splice over `movss dword [ebp-0x64], xmm3` (5 bytes) inside `fcn.0047fcd0`, the
/// `AnmManager` modes 5/7 position helper. X and Y correctly accumulate `matrix.t*`;
/// Z doesn't. `[esi + 0x454]` is `matrix.tz` (scratch matrix at `vm + 0x41c`).
/// `[ebp - 0x64]` is the Z frame slot that the displaced `movss` writes to.
const ANM_MODE57_SPLICE: usize = 0x0047_fed0;
const ANM_MODE57_DISPLACED_LEN: usize = 5;
static ANM_MODE57_AFTER_SPLICE: usize = ANM_MODE57_SPLICE + ANM_MODE57_DISPLACED_LEN;

#[unsafe(naked)]
unsafe extern "C" fn anm_mode57_z_trampoline() -> ! {
    naked_asm!(
        "addss xmm3, dword ptr [esi + 0x454]",
        "movss dword ptr [ebp - 0x64], xmm3",
        "jmp   dword ptr [{slot}]",
        slot = sym ANM_MODE57_AFTER_SPLICE,
    )
}

pub(crate) unsafe fn install_anm_matrix_tz_fix() {
    unsafe {
        patch_jmp(
            ANM_MODE57_SPLICE,
            &[0xf3, 0x0f, 0x11, 0x5d, 0x9c],
            anm_mode57_z_trampoline as *mut (),
            "AnmManager mode 5/7 z + matrix.tz",
        );
    }
}

/// th15 screenshot save (stdcall; filename pointer pushed on the stack).
/// The game calls this from the render thread before `Present`.
const SCREENSHOT_SAVE_FN: usize = 0x0044_cbf0;
const SCREENSHOT_SAVE_FN_PROLOGUE: [u8; 5] = [0x55, 0x8b, 0xec, 0x83, 0xec];

#[unsafe(naked)]
unsafe extern "stdcall" fn screenshot_trampoline(_filename: *const c_char) -> u32 {
    naked_asm!(
        "push dword ptr [esp + 4]",
        "call {save}",
        "add esp, 4",
        "ret 4",
        save = sym save_screenshot_live,
    );
}

pub(crate) unsafe fn install_screenshot_hook() {
    unsafe {
        patch_jmp(
            SCREENSHOT_SAVE_FN,
            &SCREENSHOT_SAVE_FN_PROLOGUE,
            screenshot_trampoline as *mut (),
            "screenshot save (fcn.0044cbf0)",
        );
    }
}
