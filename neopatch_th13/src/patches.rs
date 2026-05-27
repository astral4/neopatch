//! Patches and hooks for th13.exe v1.00c.

use neopatch_core::patches::{Patch, patch_call, patch_jmp};
use neopatch_core::screenshot::{log_failed, log_saved, sanitize_filename, save_live};
use neopatch_core::vtable::parse_fn_ptr;
use std::arch::naked_asm;
use std::ffi::c_void;
use std::ptr::{with_exposed_provenance, with_exposed_provenance_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use tracing::{info, warn};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

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
    Patch::new(0x0045_c5d7, &[0x74], &[0xeb], "fast input latency #1"),
    Patch::new(0x0045_c5eb, &[0x75], &[0xeb], "fast input latency #2"),
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

/// th13 screenshot save. Filename pointer in EAX.
const SCREENSHOT_SAVE_FN: usize = 0x0043_a950;
const SCREENSHOT_SAVE_FN_PROLOGUE: [u8; 5] = [0x55, 0x8b, 0xec, 0x83, 0xec];

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
            "screenshot save (fcn.0043a950)",
        );
    }
}

/// Fixes a race between the main thread and a BGM-init thread.
///
/// Hook 1 (`updatefast_wrapper`): call-site rewrite at `0x45c607` so the first
/// `UpdateFast` call (just past main's pre-loop init) drains both loaders before forwarding.
/// By construction, no sound consumer fires until the game-loop body runs.
///
/// Hook 2 (`io_error_abort_trampoline`): splices `signal = 2` intothe I/O thread's
/// error-exit path so a missing-asset install force-aborts BGM-init
/// instead of leaving it busy-waiting on a NULL slot.
const UPDATEFAST_CALL_SITE: usize = 0x0045_c607;
const REAL_UPDATEFAST: usize = 0x0045_d2f0;
const BGM_HANDLE_SLOT: usize = 0x004e_4754;
const IO_HANDLE_SLOT: usize = 0x004e_4758;
/// Loader-control flag. `0` initial; `1` releases the post-load waits in BGM thread proc
/// and I/O thread proc; `2` is the only escape from the inner busy-wait in `fcn.00461490`
/// and the outer loop in `fcn.0045fea0`, and is written by `io_error_abort_trampoline`.
const LOADER_SIGNAL: usize = 0x004e_4760;
const LOADER_SIGNAL_EXIT_CLEAN: u32 = 1;
const LOADER_SIGNAL_ABORT: u32 = 2;

const UPDATEFAST_CALL_BYTES: [u8; 5] = [0xe8, 0xe4, 0x0c, 0x00, 0x00];

static LOADERS_DRAINED: AtomicBool = AtomicBool::new(false);

type UpdateFastFn = unsafe extern "system" fn(*mut c_void) -> i32;

extern "system" fn updatefast_wrapper(arg: *mut c_void) -> i32 {
    if LOADERS_DRAINED
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        drain_loaders();
    }
    let real: UpdateFastFn = parse_fn_ptr(with_exposed_provenance_mut(REAL_UPDATEFAST))
        .expect("REAL_UPDATEFAST is a non-zero constant");
    unsafe { real(arg) }
}

fn drain_loaders() {
    // If the trampoline already wrote `2`, we preserve it. Losing that abort would leave
    // BGM-init busy-waiting on a NULL slot and deadlock the wait. Handle closure is left to
    // the game's scene-transition teardown (`fcn.0045fdb0`/`fcn.00460a60`);
    // both no-op on an exited thread.
    unsafe {
        let signal = AtomicU32::from_ptr(with_exposed_provenance_mut(LOADER_SIGNAL));
        let _ = signal.compare_exchange(
            0,
            LOADER_SIGNAL_EXIT_CLEAN,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        let bgm = with_exposed_provenance::<HANDLE>(BGM_HANDLE_SLOT).read_volatile();
        let io = with_exposed_provenance::<HANDLE>(IO_HANDLE_SLOT).read_volatile();
        info!(
            kind = "loader_sync_begin",
            bgm = format_args!("{bgm:p}"),
            io = format_args!("{io:p}"),
            signal = signal.load(Ordering::Acquire),
        );
        if !bgm.is_null() {
            WaitForSingleObject(bgm, INFINITE);
        }
        if !io.is_null() {
            WaitForSingleObject(io, INFINITE);
        }
        info!(kind = "loader_sync_end");
    }
}

/// Splices into the I/O thread's error-exit path at `fcn.004601b0+0x4f`.
/// The displaced 7-byte `mov eax, [esi*4 + 0x4bb320]` is replayed;
/// the inserted `signal = 2` hits BGM-init's only escape from its NULL-slot busy-wait.
const IO_ERROR_SPLICE: usize = 0x0046_01ff;
const IO_ERROR_DISPLACED_LEN: usize = 7;
static IO_ERROR_AFTER_SPLICE: usize = IO_ERROR_SPLICE + IO_ERROR_DISPLACED_LEN;

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

/// JMP opcode at `IO_ERROR_SPLICE` means our `patch_jmp` landed;
/// the original `mov`'s `0x8b` means the patch was rejected.
const HOOK2_INSTALLED_OPCODE: u8 = 0xe9;

pub(crate) unsafe fn install_loader_sync_hooks() {
    unsafe {
        patch_jmp(
            IO_ERROR_SPLICE,
            &[0x8b, 0x04, 0xb5, 0x20, 0xb3, 0x4b, 0x00],
            io_error_abort_trampoline as *mut (),
            "I/O error -> BGM-init abort",
        );
        let hook2_byte = with_exposed_provenance::<u8>(IO_ERROR_SPLICE).read_volatile();
        if hook2_byte != HOOK2_INSTALLED_OPCODE {
            // Without Hook 2, the drain barrier could deadlock
            // on a missing-asset install, so we skip Hook 1.
            warn!(
                kind = "loader_sync_aborted",
                addr = format_args!("{IO_ERROR_SPLICE:#010x}"),
                opcode = format_args!("{hook2_byte:#04x}"),
            );
            return;
        }
        patch_call(
            UPDATEFAST_CALL_SITE,
            &UPDATEFAST_CALL_BYTES,
            updatefast_wrapper as *mut (),
            "loader sync barrier (main -> UpdateFast)",
        );
    }
}
