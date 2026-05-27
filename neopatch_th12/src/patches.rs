//! Patches and hooks for th12.exe v1.00b.

use neopatch_core::d3d9::install_call_site_rewrite;
use neopatch_core::patches::{Patch, patch_call, patch_jmp};
use neopatch_core::screenshot::save_screenshot_live;
use neopatch_core::vtable::parse_fn_ptr;
use std::arch::naked_asm;
use std::ffi::c_void;
use std::ptr::{with_exposed_provenance, with_exposed_provenance_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use tracing::{info, warn};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

/// Live `Direct3DCreate9` call site, rewritten to defend against downstream IAT hijacks.
/// There is a second site at `0x00450a42` that seems to be dead error-recovery code.
const TH12_DIRECT3DCREATE9_CALL_ADDR: usize = 0x0044_f6fc;
const TH12_DIRECT3DCREATE9_CALL_BYTES: [u8; 5] = [0xe8, 0xb5, 0xcf, 0x01, 0x00];

pub(crate) unsafe fn install_d3d9_call_site_rewrite() {
    unsafe {
        install_call_site_rewrite(
            TH12_DIRECT3DCREATE9_CALL_ADDR,
            &TH12_DIRECT3DCREATE9_CALL_BYTES,
        );
    }
}

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
    Patch::new(
        0x0044_f87a,
        &[0x74, 0x0c],
        &[0xeb, 0x0c],
        "fast input latency #1",
    ),
    Patch::new(
        0x0044_f88e,
        &[0x75, 0x15],
        &[0xeb, 0x15],
        "fast input latency #2",
    ),
    Patch::new(
        0x0043_c54f,
        &[0x74, 0x14],
        &[0xeb, 0x14],
        "replay speed control skip",
    ),
];

pub(crate) unsafe fn apply_basic() {
    unsafe { Patch::apply_all(PATCHES) };
}

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

/// th12 screenshot save (eax-convention; filename pointer in EAX).
/// The game calls this from the render thread before `Present`.
const SCREENSHOT_SAVE_FN: usize = 0x0042_fca0;
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
            "screenshot save (fcn.0042fca0)",
        );
    }
}

/// Fixes a race between the main thread and a BGM-init thread.
///
/// Hook 1 (`updatefast_wrapper`): call-site rewrite at `0x44f8aa` so the first
/// `UpdateFast` call (just past main's pre-loop init) drains both loaders before forwarding.
/// By construction, no sound consumer fires until the game-loop body runs.
///
/// Hook 2 (`io_error_abort_trampoline`): splices `signal = 2` into the I/O thread's
/// error-exit path so a missing-asset install force-aborts BGM-init
/// instead of leaving it busy-waiting on a NULL slot.
const UPDATEFAST_CALL_SITE: usize = 0x0044_f8aa;
const REAL_UPDATEFAST: usize = 0x0045_03f0;
const BGM_HANDLE_SLOT: usize = 0x004d_4764;
const IO_HANDLE_SLOT: usize = 0x004d_4768;
/// Loader-control flag. `0` initial; `1` releases the post-load waits in BGM thread proc
/// and I/O thread proc; `2` is the only escape from the inner busy-wait in `fcn.00453120`
/// and the outer loop, and is written by `io_error_abort_trampoline`.
const LOADER_SIGNAL: usize = 0x004d_4770;
const LOADER_SIGNAL_EXIT_CLEAN: u32 = 1;
const LOADER_SIGNAL_ABORT: u32 = 2;

const UPDATEFAST_CALL_BYTES: [u8; 5] = [0xe8, 0x41, 0x0b, 0x00, 0x00];

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
    // We use CAS to preserve a trampoline-written `2`; overwriting this would leave BGM-init
    // busy-waiting on a NULL slot. Handle closure is left to the game's scene-transition
    // teardown which no-ops on an exited thread.
    unsafe {
        // Atomics are technically wrong because the game's BGM and I/O threads write the slot
        // via a plain `mov`. We rely on x86 TSO for aligned dword stores. `compare_exchange`
        // lowers to `lock cmpxchg`, just like MSVC's `_InterlockedCompareExchange` intrinsic.
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

/// Splices into the I/O thread's error-exit path at `fcn.00453460+0x4f`.
/// The displaced 7-byte `mov eax, [esi*4 + 0x4aea50]` is replayed;
/// the inserted `signal = 2` hits BGM-init's only escape from its NULL-slot busy-wait.
const IO_ERROR_SPLICE: usize = 0x0045_34af;
const IO_ERROR_DISPLACED_LEN: usize = 7;
static IO_ERROR_AFTER_SPLICE: usize = IO_ERROR_SPLICE + IO_ERROR_DISPLACED_LEN;

#[unsafe(naked)]
unsafe extern "C" fn io_error_abort_trampoline() -> ! {
    naked_asm!(
        "mov dword ptr [{signal}], {abort}",
        "mov eax, dword ptr [esi*4 + 0x4aea50]",
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
            &[0x8b, 0x04, 0xb5, 0x50, 0xea, 0x4a, 0x00],
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
