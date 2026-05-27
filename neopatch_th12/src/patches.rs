//! Patches and hooks for th12.exe v1.00b.

use neopatch_core::patches::{Patch, patch_jmp};
use neopatch_core::screenshot::{log_failed, log_saved, sanitize_filename, save_live};
use std::arch::naked_asm;

/// "UpdateFast skip": flips `jne 0x45042e` to `jmp +0x43`, landing past the `Sleep(1)` gate
/// and the FPU (x87) catch-up loop inside `CWindowManager::UpdateFast` at `0x004503F0`.
/// The catch-up loop iterates the game step without rendering when behind schedule.
/// We let the D3D9 `Present` pacer in core be the sole timing source.
///
/// "fast input latency #1/#2": flips two cond jumps so the per-frame driver dispatch is
/// always reached on `fcn.004503f0` (`CWindowManager::UpdateFast`). Skips the alternative
/// "slow" and "normal" paths. OILP also does this under "Force fast input latency mode."
///
/// "replay speed control skip": skips the game's own Ctrl-key fast-forward branch.
/// Without it the game's internal speed control fights our pacer's replay-skip / replay-slow
/// modes (see `state::replay_mode`).
const PATCHES: &[Patch] = &[
    Patch::new(0x0045_0424, &[0x75, 0x08], &[0xeb, 0x43], "UpdateFast skip"),
    Patch::new(0x0044_f87a, &[0x74], &[0xeb], "fast input latency #1"),
    Patch::new(0x0044_f88e, &[0x75], &[0xeb], "fast input latency #2"),
    Patch::new(0x0043_c54f, &[0x74], &[0xeb], "replay speed control skip"),
];

/// Location of the `fadd dword [ebx + 0x444]` we displace with `e9 disp32`.
const ANM_MODE57_SPLICE: usize = 0x0045_ba6d;
/// Length of the displaced instruction (6 bytes for `D8 83 disp32`).
const ANM_MODE57_DISPLACED_LEN: usize = 6;
/// Resume target past the displaced fadd at the splice.
static ANM_MODE57_AFTER_SPLICE: usize = ANM_MODE57_SPLICE + ANM_MODE57_DISPLACED_LEN;

/// Adds the missing `matrix.tz` `fadd` before the Z `fstp` in `fcn.0045b930`, the position
/// helper used by `AnmManager` render modes 5 and 7. X and Y correctly accumulate their
/// `matrix.t*`, unlike Z. `[ebx + 0x444]` is the displaced operand (the third `fadd` for Z),
/// replayed in the trampoline. `[esp + 0x48]` is the `matrix.tz` slot in the function's frame.
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

/// `fcn.0042fca0`: th12 screenshot save.
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
