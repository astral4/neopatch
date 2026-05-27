//! Patches and hooks for th10.exe v1.00a.

use neopatch_core::d3d9::install_call_site_rewrite;
use neopatch_core::patches::{Patch, patch_call, patch_jmp};
use neopatch_core::screenshot::save_screenshot_deferred;
use neopatch_core::vtable::parse_fn_ptr;
use std::arch::naked_asm;
use std::ffi::c_void;
use std::ptr::{with_exposed_provenance, with_exposed_provenance_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use tracing::{info, warn};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

/// Live `Direct3DCreate9` call site, rewritten to defend against downstream IAT hijacks.
/// There is a second site at `0x00439702` that seems to be dead error-recovery code.
const TH10_DIRECT3DCREATE9_CALL_ADDR: usize = 0x0043_8bc3;
const TH10_DIRECT3DCREATE9_CALL_BYTES: [u8; 5] = [0xe8, 0xae, 0x95, 0x01, 0x00];

pub(crate) unsafe fn install_d3d9_call_site_rewrite() {
    unsafe {
        install_call_site_rewrite(
            TH10_DIRECT3DCREATE9_CALL_ADDR,
            &TH10_DIRECT3DCREATE9_CALL_BYTES,
        );
    }
}

/// "Sleep-path branch nop" + "frame limiter unconditional skip": disengages the game's
/// own pacer in `CWindowManager::Update` (the `Sleep` at `0x0043952B`, the FPU deadline check).
///
/// "AnmManager mode 2 y -> z": fixes a typo in `fcn.00443290`. The Y component of
/// `parentPos` (`+0x350`) is summed where the Z component (`+0x354`) was meant to be.
/// Reached in render mode 2, and modes 1/3 when rotation is exactly 0.
///
/// "32-bit color skip force-16-bit branch" + "32-bit color ignore persistent choice":
/// forces `pp.BackBufferFormat = X8R8G8B8` in `fcn.00439890`'s fullscreen path regardless of
/// `custom.exe`. First skips the `[0x491d78] & 1` 16-bit fallback; second NOPs the `setne cl`
/// so the persistent `[0x491d62]` choice can't push `0x16` (X8R8G8B8) up to `0x17` (R5G6B5).
/// Windowed mode pulls the format from the desktop and needs no patches.
const PATCHES: &[Patch] = &[
    Patch::new(
        0x0043_93b7,
        &[0x0f, 0x85, 0x6a, 0x01, 0x00, 0x00],
        &[0x90, 0x90, 0x90, 0x90, 0x90, 0x90],
        "Sleep-path branch nop",
    ),
    Patch::new(
        0x0043_93c5,
        &[0x75, 0x22],
        &[0xeb, 0x22],
        "frame limiter unconditional skip",
    ),
    Patch::new(
        0x0044_343b,
        &[0xd9, 0x80, 0x50, 0x03, 0x00, 0x00],
        &[0xd9, 0x80, 0x54, 0x03, 0x00, 0x00],
        "AnmManager mode 2 y -> z",
    ),
    Patch::new(
        0x0043_98d4,
        &[0x74, 0x11],
        &[0xeb, 0x11],
        "32-bit color skip force-16-bit branch",
    ),
    Patch::new(
        0x0043_9916,
        &[0x0f, 0x95, 0xc1],
        &[0x90, 0x90, 0x90],
        "32-bit color ignore persistent choice",
    ),
];

pub(crate) unsafe fn apply_basic() {
    unsafe { Patch::apply_all(PATCHES) };
}

/// Splice over `mov ebx, [ebx + 0x35c]` (6 bytes) inside `fcn.00444240`, the `AnmManager`
/// modes 5/7 position helper. X and Y correctly accumulate `matrix.t*`; Z doesn't.
/// `[esp + 0x74]` is the `matrix.tz` frame slot; the displaced `mov` loads
/// the `AnmVm` flags field and is replayed.
const ANM_MODE57_SPLICE: usize = 0x0044_438e;
const ANM_MODE57_DISPLACED_LEN: usize = 6;
static ANM_MODE57_AFTER_SPLICE: usize = ANM_MODE57_SPLICE + ANM_MODE57_DISPLACED_LEN;

#[unsafe(naked)]
unsafe extern "C" fn anm_mode57_z_trampoline() -> ! {
    naked_asm!(
        "fadd dword ptr [esp + 0x74]",
        "mov  ebx, [ebx + 0x35c]",
        "jmp  dword ptr [{slot}]",
        slot = sym ANM_MODE57_AFTER_SPLICE,
    )
}

pub(crate) unsafe fn install_anm_matrix_tz_fix() {
    unsafe {
        patch_jmp(
            ANM_MODE57_SPLICE,
            &[0x8b, 0x9b, 0x5c, 0x03, 0x00, 0x00],
            anm_mode57_z_trampoline as *mut (),
            "AnmManager mode 5/7 z + matrix.tz",
        );
    }
}

/// th10 screenshot save (eax-convention; filename pointer in EAX). The game calls this
/// after `Present`, where the live back buffer is undefined under D3D9Ex flip-model, so we
/// stash the path and capture in the next `on_pre_present` instead of saving immediately.
const SCREENSHOT_SAVE_FN: usize = 0x0042_0670;
const SCREENSHOT_SAVE_FN_PROLOGUE: [u8; 5] = [0x83, 0xec, 0x0c, 0x53, 0x55];

#[unsafe(naked)]
unsafe extern "C" fn screenshot_trampoline() -> u32 {
    naked_asm!(
        "push eax",
        "call {stash}",
        "add esp, 4",
        "ret",
        stash = sym save_screenshot_deferred,
    );
}

pub(crate) unsafe fn install_screenshot_hook() {
    unsafe {
        patch_jmp(
            SCREENSHOT_SAVE_FN,
            &SCREENSHOT_SAVE_FN_PROLOGUE,
            screenshot_trampoline as *mut (),
            "screenshot save (fcn.00420670)",
        );
    }
}

/// Fixes a race between the main thread and a BGM-init thread.
///
/// Hook 1 (`update_wrapper`): call-site rewrite at `0x438d31` so the first per-frame
/// update call (just past main's pre-loop init) drains both loaders before forwarding.
/// By construction, no sound consumer fires until the game-loop body runs.
///
/// Hook 2 (`io_error_abort_trampoline`): splices `signal = 2` into the I/O thread's
/// error-exit path so a missing-asset install force-aborts BGM-init
/// instead of leaving it busy-waiting on a NULL slot.
const UPDATE_CALL_SITE: usize = 0x0043_8d31;
const REAL_UPDATE: usize = 0x0043_9390;
const BGM_HANDLE_SLOT: usize = 0x0049_77a8;
const IO_HANDLE_SLOT: usize = 0x0049_77ac;
/// Loader-control flag. `0` initial; non-zero releases the post-load `Sleep(1)` busy-wait
/// in the I/O loader at `0x43d080`; `2` is also the only escape from the inner
/// `cmp [signal], 2` exit at the top of that loop, and is written by
/// `io_error_abort_trampoline`.
const LOADER_SIGNAL: usize = 0x0049_77b4;
const LOADER_SIGNAL_EXIT_CLEAN: u32 = 1;
const LOADER_SIGNAL_ABORT: u32 = 2;

const UPDATE_CALL_BYTES: [u8; 5] = [0xe8, 0x5a, 0x06, 0x00, 0x00];

static LOADERS_DRAINED: AtomicBool = AtomicBool::new(false);

type UpdateFn = unsafe extern "system" fn(*mut c_void) -> i32;

extern "system" fn update_wrapper(arg: *mut c_void) -> i32 {
    if LOADERS_DRAINED
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        drain_loaders();
    }
    let real: UpdateFn = parse_fn_ptr(with_exposed_provenance_mut(REAL_UPDATE))
        .expect("REAL_UPDATE is a non-zero constant");
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

/// Splices into the I/O thread's error-exit path at `fcn.0043d080+0x4f`.
/// The displaced 7-byte `mov eax, [esi*4 + 0x474b40]` is replayed;
/// the inserted `signal = 2` hits BGM-init's only escape from its NULL-slot busy-wait.
const IO_ERROR_SPLICE: usize = 0x0043_d0cf;
const IO_ERROR_DISPLACED_LEN: usize = 7;
static IO_ERROR_AFTER_SPLICE: usize = IO_ERROR_SPLICE + IO_ERROR_DISPLACED_LEN;

#[unsafe(naked)]
unsafe extern "C" fn io_error_abort_trampoline() -> ! {
    naked_asm!(
        "mov dword ptr [{signal}], {abort}",
        "mov eax, dword ptr [esi*4 + 0x474b40]",
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
            &[0x8b, 0x04, 0xb5, 0x40, 0x4b, 0x47, 0x00],
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
            UPDATE_CALL_SITE,
            &UPDATE_CALL_BYTES,
            update_wrapper as *mut (),
            "loader sync barrier (main -> per-frame update)",
        );
    }
}
