//! Patches and hooks for th18.exe v1.00a.

use neopatch_core::d3d9::install_call_site_rewrite;
use neopatch_core::patches::{Patch, patch_jmp};
use neopatch_core::screenshot::save_screenshot_live;
use std::arch::naked_asm;
use std::ffi::c_char;

/// Live `Direct3DCreate9` call site, rewritten to defend against downstream IAT hijacks.
/// This is the only call site in th18.
const DIRECT3DCREATE9_CALL_ADDR: usize = 0x0047_1634;
const DIRECT3DCREATE9_CALL_BYTES: [u8; 6] = [0xff, 0x15, 0x88, 0xd2, 0x4a, 0x00];

pub(crate) unsafe fn install_d3d9_call_site_rewrite() {
    unsafe {
        install_call_site_rewrite(DIRECT3DCREATE9_CALL_ADDR, &DIRECT3DCREATE9_CALL_BYTES);
    }
}

/// "UpdateFast skip": unconditional `jmp +0x53` past the `Sleep(1)`, deadline comparison,
/// and deadline-advance spin inside `CWindowManager::UpdateFast` (`fcn.00472dd0`), so our
/// pacer is the sole timing source.
///
/// "force fast input latency": th18's input-mode dispatch is one block, like th17's: a single
/// `jmp` over the conditional sets us on the `CWindowManager::UpdateFast` call site directly,
/// skipping the "slow"/"automatic" alternatives.
///
/// "replay speed control skip": NOPs the viewer-mode branch in `fcn.00461db0` so the game's
/// own replay-speed control doesn't fight our pacer.
const PATCHES: &[Patch] = &[
    Patch::new(0x0047_2e25, &[0x72, 0x18], &[0xeb, 0x53], "UpdateFast skip"),
    Patch::new(
        0x0047_1a9e,
        &[0x0f, 0x89, 0x93, 0x01, 0x00, 0x00],
        &[0xe9, 0xb2, 0x01, 0x00, 0x00, 0x90],
        "force fast input latency",
    ),
    Patch::new(
        0x0046_1dd3,
        &[0x75, 0x3a],
        &[0x90, 0x90],
        "replay speed control skip",
    ),
];

pub(crate) unsafe fn apply_basic() {
    unsafe { Patch::apply_all(PATCHES) };
}

/// Splice over `movss dword [ebp-0x64], xmm3` (5 bytes) inside `fcn.0047feb0`,
/// th18's AnmManager position helper. X and Y accumulate `matrix.t*` via a
/// carried xmm0 chain; Z drops it. `[esi + 0x44c]` is `matrix.tz` (scratch
/// matrix at `vm + 0x414`, translation row at +0x444).
/// `[ebp - 0x64]` is the Z frame slot the displaced `movss` writes to.
const ANM_MODE57_SPLICE: usize = 0x0048_00b2;
const ANM_MODE57_DISPLACED_LEN: usize = 5;
static ANM_MODE57_AFTER_SPLICE: usize = ANM_MODE57_SPLICE + ANM_MODE57_DISPLACED_LEN;

#[unsafe(naked)]
unsafe extern "C" fn anm_mode57_z_trampoline() -> ! {
    naked_asm!(
        "addss xmm3, dword ptr [esi + 0x44c]",
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
            "AnmManager z + matrix.tz",
        );
    }
}

/// th18 screenshot save (stdcall; filename pointer pushed on the stack).
/// The game calls this from the render thread before `Present`.
const SCREENSHOT_SAVE_FN: usize = 0x0045_3f40;
const SCREENSHOT_SAVE_FN_PROLOGUE: [u8; 5] = [0x53, 0x8b, 0xdc, 0x83, 0xec];

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
            "screenshot save (fcn.00453f40)",
        );
    }
}
