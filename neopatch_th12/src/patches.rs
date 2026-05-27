//! Patches and hooks for th12.exe v1.00b.

use neopatch_core::patches::{Patch, patch_jmp};
use neopatch_core::screenshot::{log_failed, log_saved, sanitize_filename, save_live};
use std::arch::naked_asm;

/// "UpdateFast skip": flips `jne 0x45042e` to `jmp +0x43`, landing past
/// the `Sleep(1)` and the FPU catch-up loop in `CWindowManager::UpdateFast`.
///
/// "fast input latency #1/#2": flips two cond jumps so the per-frame dispatch
/// always reaches `fcn.004503f0` (`UpdateFast`) instead of the slow/normal paths.
/// OILP also does this under "Force fast input latency mode."
///
/// "replay speed control skip": skips the game's own Ctrl-key fast-forward.
/// Without this, the game's internal speed control fights our pacer's replay-speed modes.
const PATCHES: &[Patch] = &[
    Patch::new(0x0045_0424, &[0x75, 0x08], &[0xeb, 0x43], "UpdateFast skip"),
    Patch::new(0x0044_f87a, &[0x74], &[0xeb], "fast input latency #1"),
    Patch::new(0x0044_f88e, &[0x75], &[0xeb], "fast input latency #2"),
    Patch::new(0x0043_c54f, &[0x74], &[0xeb], "replay speed control skip"),
];

/// Splice over `fadd dword [ebx + 0x444]` (6 bytes) inside `fcn.0045b930`,
/// the `AnmManager` modes 5/7 position helper. X and Y correctly accumulate `matrix.t*`;
/// Z doesn't. `[esp + 0x48]` is the `matrix.tz` frame slot;
/// the displaced `fadd` (the third Z addend) is replayed.
const ANM_MODE57_SPLICE: usize = 0x0045_ba6d;
const ANM_MODE57_DISPLACED_LEN: usize = 6;
static ANM_MODE57_AFTER_SPLICE: usize = ANM_MODE57_SPLICE + ANM_MODE57_DISPLACED_LEN;

#[unsafe(naked)]
unsafe extern "C" fn anm_mode57_z_trampoline() -> ! {
    naked_asm!(
        "fadd dword ptr [ebx + 0x444]",
        "fadd dword ptr [esp + 0x48]",
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
            &[0xd8, 0x83, 0x44, 0x04, 0x00, 0x00],
            anm_mode57_z_trampoline as *mut (),
            "AnmManager mode 5/7 z + matrix.tz",
        );
    }
}

/// th12 screenshot save. Filename pointer in EAX.
const SCREENSHOT_SAVE_FN: usize = 0x0042_fca0;
const SCREENSHOT_SAVE_FN_PROLOGUE: [u8; 5] = [0x83, 0xec, 0x10, 0x83, 0x3d];

#[unsafe(naked)]
unsafe extern "C" fn screenshot_trampoline() -> u32 {
    naked_asm!(
        "push eax",
        "call {save}",
        "add esp, 4",
        "ret",
        save = sym save_screenshot,
    );
}

unsafe extern "C" fn save_screenshot(filename_ptr: *const u8) -> u32 {
    let Some(path) = sanitize_filename(filename_ptr) else {
        return 1;
    };
    let bytes = path.as_slice();
    match save_live(bytes) {
        Ok((w, h)) => {
            log_saved(bytes, w, h, "live");
            0
        }
        Err(e) => {
            log_failed(bytes, &e);
            1
        }
    }
}

pub(crate) unsafe fn install_screenshot_hook() {
    unsafe {
        patch_jmp(
            SCREENSHOT_SAVE_FN,
            &SCREENSHOT_SAVE_FN_PROLOGUE,
            screenshot_trampoline as *mut (),
            "screenshot save (fcn.0042fca0)",
        );
    }
}
