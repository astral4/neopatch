//! Patches and hooks for th13.exe v1.00c.

use neopatch_core::d3d9::install_call_site_rewrite;
use neopatch_core::loader_sync::{self, LOADER_SIGNAL_ABORT};
use neopatch_core::patches::{Patch, patch_jmp};
use neopatch_core::screenshot::save_screenshot_live;
use std::arch::naked_asm;

/// Live `Direct3DCreate9` call site, rewritten to defend against downstream IAT hijacks.
/// There is a second site at `0x0045da12` that seems to be dead error-recovery code.
const TH13_DIRECT3DCREATE9_CALL_ADDR: usize = 0x0045_c42f;
const TH13_DIRECT3DCREATE9_CALL_BYTES: [u8; 6] = [0xff, 0x15, 0x98, 0x22, 0x4a, 0x00];

pub(crate) unsafe fn install_d3d9_call_site_rewrite() {
    unsafe {
        install_call_site_rewrite(
            TH13_DIRECT3DCREATE9_CALL_ADDR,
            &TH13_DIRECT3DCREATE9_CALL_BYTES,
        );
    }
}

/// "UpdateFast skip": flips `jne 0x45d33e` to `jmp +0x5b`, landing past the `Sleep(1)`
/// and the FPU catch-up loop inside `CWindowManager::UpdateFast` at `0x0045D2F0`.
///
/// "fast input latency #1/#2": flips two cond jumps so the per-frame driver dispatch is
/// always reached on `fcn.0045d2f0` (`UpdateFast`). Skips the alternative
/// "slow" (`fcn.0045cef0`) and "automatic" (`fcn.0045d570`) paths.
/// OILP also does this under "Force fast input latency mode."
///
/// "replay speed control skip": skips the game's own Ctrl-key fast-forward branch.
/// Without this, the game's internal speed control fights our pacer's replay-speed modes.
const PATCHES: &[Patch] = &[
    Patch::new(0x0045_d334, &[0x75, 0x08], &[0xeb, 0x5b], "UpdateFast skip"),
    Patch::new(
        0x0045_c5d7,
        &[0x74, 0x0c],
        &[0xeb, 0x0c],
        "fast input latency #1",
    ),
    Patch::new(
        0x0045_c5eb,
        &[0x75, 0x15],
        &[0xeb, 0x15],
        "fast input latency #2",
    ),
    Patch::new(
        0x0044_8e6f,
        &[0x75, 0x04],
        &[0xeb, 0x1d],
        "replay speed control skip",
    ),
];

pub(crate) unsafe fn apply_basic() {
    unsafe { Patch::apply_all(PATCHES) };
}

/// Splice over `mov ebx, [ebx + 0x570]` (6 bytes) inside `fcn.00468e50`, the `AnmManager`
/// modes 5/7 position helper. X and Y correctly accumulate `matrix.t*`; Z doesn't.
/// `[ebp - 0x5c]` is `matrix.tz`. The displaced `mov` loads the parent `AnmVm` pointer
/// and is replayed so the parent-recursion path runs unchanged.
const ANM_MODE57_SPLICE: usize = 0x0046_8fc9;
const ANM_MODE57_DISPLACED_LEN: usize = 6;
static ANM_MODE57_AFTER_SPLICE: usize = ANM_MODE57_SPLICE + ANM_MODE57_DISPLACED_LEN;

#[unsafe(naked)]
unsafe extern "C" fn anm_mode57_z_trampoline() -> ! {
    naked_asm!(
        "fadd dword ptr [ebp - 0x5c]",
        "mov  ebx, [ebx + 0x570]",
        "jmp  dword ptr [{slot}]",
        slot = sym ANM_MODE57_AFTER_SPLICE,
    )
}

pub(crate) unsafe fn install_anm_matrix_tz_fix() {
    unsafe {
        patch_jmp(
            ANM_MODE57_SPLICE,
            &[0x8b, 0x9b, 0x70, 0x05, 0x00, 0x00],
            anm_mode57_z_trampoline as *mut (),
            "AnmManager mode 5/7 z + matrix.tz",
        );
    }
}

/// th13 screenshot save (eax-convention; filename pointer in EAX).
/// The game calls this from the render thread before `Present`.
const SCREENSHOT_SAVE_FN: usize = 0x0043_a950;
const SCREENSHOT_SAVE_FN_PROLOGUE: [u8; 5] = [0x55, 0x8b, 0xec, 0x83, 0xec];

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
            "screenshot save (fcn.0043a950)",
        );
    }
}

/// Fixes a race between the main thread and the BGM-init / I/O loader threads.
///
/// `LOADER_SIGNAL` semantics for th13: non-zero releases the post-load waits
/// in BGM and I/O thread procs. `2` (written by `io_error_abort_trampoline`)
/// is the only escape from the inner busy-wait in `fcn.00461490` and the outer loop
/// in `fcn.0045fea0`. Handle closure is left to the scene-transition teardown
/// (`fcn.0045fdb0`/`fcn.00460a60`), which no-ops on an exited thread.
const LOADER_SIGNAL: usize = 0x004e_4760;
const IO_ERROR_SPLICE: usize = 0x0046_01ff;
const IO_ERROR_DISPLACED_LEN: usize = 7;
static IO_ERROR_AFTER_SPLICE: usize = IO_ERROR_SPLICE + IO_ERROR_DISPLACED_LEN;

/// Splices into the I/O thread's error-exit path at `fcn.004601b0+0x4f`.
/// The displaced 7-byte `mov eax, [esi*4 + 0x4bb320]` is replayed.
/// The inserted `signal = 2` hits BGM-init's only escape from its NULL-slot busy-wait.
#[unsafe(naked)]
unsafe extern "C" fn io_error_abort_trampoline() -> ! {
    naked_asm!(
        "mov dword ptr [{signal}], {abort}",
        "mov eax, dword ptr [esi*4 + 0x4bb320]",
        "jmp dword ptr [{slot}]",
        signal = const LOADER_SIGNAL,
        abort = const LOADER_SIGNAL_ABORT,
        slot = sym IO_ERROR_AFTER_SPLICE,
    )
}

pub(crate) unsafe fn install_loader_sync_hooks() {
    unsafe {
        loader_sync::install(
            &loader_sync::Config {
                signal_addr: LOADER_SIGNAL,
                bgm_handle_addr: 0x004e_4754,
                io_handle_addr: 0x004e_4758,
                call_site: 0x0045_c607,
                call_bytes: [0xe8, 0xe4, 0x0c, 0x00, 0x00],
                real_fn: 0x0045_d2f0,
                splice_addr: IO_ERROR_SPLICE,
                splice_expected: [0x8b, 0x04, 0xb5, 0x20, 0xb3, 0x4b, 0x00],
            },
            io_error_abort_trampoline as *mut (),
        );
    }
}
