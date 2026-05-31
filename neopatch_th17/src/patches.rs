//! Patches and hooks for th17.exe v1.00b.

use neopatch_core::d3d9::install_call_site_rewrite;
use neopatch_core::patches::{Patch, patch_jmp};
use neopatch_core::screenshot::save_screenshot_live;
use std::arch::naked_asm;

/// Live `Direct3DCreate9` call site, rewritten to defend against downstream IAT hijacks.
/// This is the only call site in th17.
const DIRECT3DCREATE9_CALL_ADDR: usize = 0x0046_0f1c;
const DIRECT3DCREATE9_CALL_BYTES: [u8; 6] = [0xff, 0x15, 0x80, 0xa2, 0x49, 0x00];

pub(crate) unsafe fn install_d3d9_call_site_rewrite() {
    unsafe {
        install_call_site_rewrite(DIRECT3DCREATE9_CALL_ADDR, &DIRECT3DCREATE9_CALL_BYTES);
    }
}

/// "UpdateFast skip": unconditional `jmp +0x53` past the `Sleep(1)`, deadline comparison,
/// and deadline-advance spin inside `CWindowManager::UpdateFast` (`fcn.00462190`), so our
/// pacer is the sole timing source.
///
/// "force fast input latency": th17's input-mode dispatch is one block rather than the two
/// short cond jumps th14/th15/th16 flip, so this `jmp`s over it onto the `UpdateFast` call,
/// skipping the slow path's frame limiter and the "automatic"-mode variant (`fcn.00462370`).
/// OILP also does this under "Force fast input latency mode."
///
/// "replay speed control skip": NOPs the viewer-mode branch in `fcn.0044e610` so the game's
/// own replay-speed control doesn't fight our pacer.
const PATCHES: &[Patch] = &[
    Patch::new(0x0046_21e5, &[0x72, 0x18], &[0xeb, 0x53], "UpdateFast skip"),
    Patch::new(
        0x0046_119e,
        &[0x0f, 0x84, 0x10, 0x01, 0x00, 0x00],
        &[0xe9, 0x2f, 0x01, 0x00, 0x00, 0x90],
        "force fast input latency",
    ),
    Patch::new(
        0x0044_e633,
        &[0x75, 0x3a],
        &[0x90, 0x90],
        "replay speed control skip",
    ),
];

pub(crate) unsafe fn apply_basic() {
    unsafe { Patch::apply_all(PATCHES) };
}

/// Splice over `movss dword [ebp-0x64], xmm3` (5 bytes) inside `fcn.0046e560`, the
/// `AnmManager` modes 5/7 position helper. X and Y correctly accumulate `matrix.t*`;
/// Z doesn't. `[esi + 0x448]` is `matrix.tz` (scratch matrix at `vm + 0x410`).
/// `[ebp - 0x64]` is the Z frame slot that the displaced `movss` writes to.
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

/// th17 screenshot save (stdcall; filename pointer pushed on the stack).
/// The game calls this from the render thread before `Present`.
const SCREENSHOT_SAVE_FN: usize = 0x0044_18c0;
const SCREENSHOT_SAVE_FN_PROLOGUE: [u8; 5] = [0x53, 0x8b, 0xdc, 0x83, 0xec];

#[unsafe(naked)]
unsafe extern "C" fn screenshot_trampoline() -> u32 {
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
            "screenshot save (fcn.004418c0)",
        );
    }
}
