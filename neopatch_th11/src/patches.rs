//! Patches and hooks for th11.exe v1.00a.

use neopatch_core::patches::{Patch, patch_jmp};
use neopatch_core::screenshot::{log_failed, log_saved, sanitize_filename, save_live};
use std::arch::naked_asm;

/// "UpdateFast skip": flips `jne 0x44645e` to `jmp +0x43`, landing past the `Sleep(1)`
/// at `0x00446458` and the FPU (x87) catch-up loop (`0x0044645e..0x00446497`) inside
/// `CWindowManager::UpdateFast`. The catch-up loop iterates the game step without rendering
/// when behind schedule. We let the D3D9 `Present` pacer in core be the sole timing source,
/// so neither the yield nor the multi-step matters.
///
/// "fast input latency #1/#2": flips two cond jumps so the per-frame driver dispatch
/// at `0x004458a7` is always reached. Skips the alternative `fcn.00446080` ("slow")
/// and `fcn.00446650` ("normal") paths in favor of `fcn.00446420` (`CWindowManager::UpdateFast`).
/// OILP also does this under "Force fast input latency mode."
///
/// "replay speed control skip": skips the game's own Ctrl-key fast-forward
/// branch at `0x00436d5f`. Without it the game's internal speed control fights
/// our pacer's replay-skip / replay-slow modes (see `state::replay_mode`).
pub(crate) const PATCHES: &[Patch] = &[
    Patch::new(0x0044_6454, &[0x75, 0x08], &[0xeb, 0x43], "UpdateFast skip"),
    Patch::new(0x0044_5877, &[0x74], &[0xeb], "fast input latency #1"),
    Patch::new(0x0044_588b, &[0x75], &[0xeb], "fast input latency #2"),
    Patch::new(0x0043_6d5f, &[0x74], &[0xeb], "replay speed control skip"),
];

/// Location of the `mov ebx, [ebx + 0x404]` we displace with `e9 disp32`.
const ANM_MODE57_SPLICE: usize = 0x0045_0f83;
/// Length of the displaced instruction (6 bytes for `8B 9B disp32`).
const ANM_MODE57_DISPLACED_LEN: usize = 6;
/// Resume target past the displaced mov at the splice.
static ANM_MODE57_AFTER_SPLICE: usize = ANM_MODE57_SPLICE + ANM_MODE57_DISPLACED_LEN;

/// Adds the missing `matrix.tz` `fadd` before the Z `fstp` in `fcn.00450e20`, the position
/// helper used by `AnmManager` render modes 5 and 7. X and Y correctly accumulate their
/// `matrix.t*`, unlike Z. `[esp + 0x78]` is the `matrix.tz` slot in the function's frame.
/// `[ebx + 0x404]` is the `AnmVm` flags field (replayed from the displaced `mov`).
#[unsafe(naked)]
unsafe extern "C" fn anm_mode57_z_trampoline() -> ! {
    naked_asm!(
        "fadd dword ptr [esp + 0x78]",
        "mov  ebx, [ebx + 0x404]",
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
            &[0x8b, 0x9b, 0x04, 0x04, 0x00],
            anm_mode57_z_trampoline as *mut (),
            "AnmManager mode 5/7 z + matrix.tz",
        );
    }
}

/// `fcn.00429ca0`: th11 screenshot save.
const SCREENSHOT_SAVE_FN: usize = 0x0042_9ca0;
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
            "screenshot save (fcn.00429ca0)",
        );
    }
}
