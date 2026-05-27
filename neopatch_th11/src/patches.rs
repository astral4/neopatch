//! Patches and hooks for th11.exe v1.00a.

use neopatch_core::d3d9::install_call_site_rewrite;
use neopatch_core::patches::{Patch, patch_jmp};
use neopatch_core::screenshot::save_screenshot_live;
use std::arch::naked_asm;

/// Live `Direct3DCreate9` call site, rewritten to defend against downstream IAT hijacks.
/// There is a second site at `0x00446ab2` that seems to be dead error-recovery code.
const TH11_DIRECT3DCREATE9_CALL_ADDR: usize = 0x0044_570e;
const TH11_DIRECT3DCREATE9_CALL_BYTES: [u8; 5] = [0xe8, 0xa3, 0xa2, 0x01, 0x00];

pub(crate) unsafe fn install_d3d9_call_site_rewrite() {
    unsafe {
        install_call_site_rewrite(
            TH11_DIRECT3DCREATE9_CALL_ADDR,
            &TH11_DIRECT3DCREATE9_CALL_BYTES,
        );
    }
}

/// "UpdateFast skip": flips `jne 0x44645e` to `jmp +0x43`, landing past
/// the `Sleep(1)` and the FPU catch-up loop in `CWindowManager::UpdateFast`.
///
/// "fast input latency #1/#2": flips two cond jumps so the per-frame dispatch
/// always reaches `fcn.00446420` (`UpdateFast`) instead of the slow/normal paths.
/// OILP also does this under "Force fast input latency mode."
///
/// "replay speed control skip": skips the game's own Ctrl-key fast-forward.
/// Without this, the game's internal speed control fights our pacer's replay-speed modes.
const PATCHES: &[Patch] = &[
    Patch::new(0x0044_6454, &[0x75, 0x08], &[0xeb, 0x43], "UpdateFast skip"),
    Patch::new(0x0044_5877, &[0x74], &[0xeb], "fast input latency #1"),
    Patch::new(0x0044_588b, &[0x75], &[0xeb], "fast input latency #2"),
    Patch::new(0x0043_6d5f, &[0x74], &[0xeb], "replay speed control skip"),
];

/// Splice over `mov ebx, [ebx + 0x404]` (6 bytes) inside `fcn.00450e20`, the `AnmManager`
/// modes 5/7 position helper. X and Y correctly accumulate `matrix.t*`; Z doesn't.
/// `[esp + 0x78]` is the `matrix.tz` frame slot; the displaced `mov` loads
/// the `AnmVm` flags field and is replayed.
const ANM_MODE57_SPLICE: usize = 0x0045_0f83;
const ANM_MODE57_DISPLACED_LEN: usize = 6;
static ANM_MODE57_AFTER_SPLICE: usize = ANM_MODE57_SPLICE + ANM_MODE57_DISPLACED_LEN;

#[unsafe(naked)]
unsafe extern "C" fn anm_mode57_z_trampoline() -> ! {
    naked_asm!(
        "fadd dword ptr [esp + 0x78]",
        "mov  ebx, [ebx + 0x404]",
        "jmp  dword ptr [{slot}]",
        slot = sym ANM_MODE57_AFTER_SPLICE,
    )
}

pub(crate) unsafe fn install_anm_matrix_tz_fix() {
    unsafe {
        patch_jmp(
            ANM_MODE57_SPLICE,
            &[0x8b, 0x9b, 0x04, 0x04, 0x00, 0x00],
            anm_mode57_z_trampoline as *mut (),
            "AnmManager mode 5/7 z + matrix.tz",
        );
    }
}

/// th11 screenshot save (eax-convention; filename pointer in EAX).
/// The game calls this from the render thread before `Present`.
const SCREENSHOT_SAVE_FN: usize = 0x0042_9ca0;
const SCREENSHOT_SAVE_FN_PROLOGUE: [u8; 5] = [0x83, 0xec, 0x10, 0x83, 0x3d];

#[unsafe(naked)]
unsafe extern "C" fn screenshot_trampoline() -> u32 {
    naked_asm!(
        "push eax",
        "call {save}",
        "add esp, 4",
        "ret",
        save = sym save_screenshot_live,
    );
}

pub(crate) unsafe fn install_screenshot_hook() {
    unsafe {
        patch_jmp(
            SCREENSHOT_SAVE_FN,
            &SCREENSHOT_SAVE_FN_PROLOGUE,
            screenshot_trampoline as *mut (),
            "screenshot save (fcn.00429ca0)",
        );
    }
}
