//! Patches and hooks for th15.exe v1.00b.

use neopatch_core::d3d9::install_call_site_rewrite;
use neopatch_core::patches::{Patch, patch_call, patch_jmp};
use neopatch_core::screenshot::save_screenshot_live;
use neopatch_core::vtable::parse_fn_ptr;
use std::arch::naked_asm;
use std::ffi::c_void;
use std::ptr::{null_mut, read_unaligned, with_exposed_provenance, with_exposed_provenance_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use tracing::{info, warn};
use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

/// Live `Direct3DCreate9` call site, rewritten to defend against downstream IAT hijacks.
/// There is a second site at `0x00472e72` that seems to be dead error-recovery code.
const TH15_DIRECT3DCREATE9_CALL_ADDR: usize = 0x0047_158c;
const TH15_DIRECT3DCREATE9_CALL_BYTES: [u8; 6] = [0xff, 0x15, 0xb0, 0xe2, 0x4b, 0x00];

pub(crate) unsafe fn install_d3d9_call_site_rewrite() {
    unsafe {
        install_call_site_rewrite(
            TH15_DIRECT3DCREATE9_CALL_ADDR,
            &TH15_DIRECT3DCREATE9_CALL_BYTES,
        );
    }
}

/// "UpdateFast skip": unconditional `jmp +0x4A` past the game's `Sleep`, spin,
/// and deadline-advance. Without this, the game's pacer holds the inter-`Present`
/// interval at >=33ms in replay slowdown mode.
///
/// "fast input latency #1/#2": flips the cond jumps to `EB`, forcing the input preamble
/// to "fast" mode. OILP also does this under "Force fast input latency mode."
///
/// "replay speed control skip": skips the game's own replay-speed control
/// so it doesn't fight our pacer.
const PATCHES: &[Patch] = &[
    Patch::new(0x0047_27de, &[0x72, 0x08], &[0xeb, 0x4a], "UpdateFast skip"),
    Patch::new(
        0x0047_1a86,
        &[0x74, 0x0c],
        &[0xeb, 0x0c],
        "fast input latency #1",
    ),
    Patch::new(
        0x0047_1a9b,
        &[0x75, 0x15],
        &[0xeb, 0x15],
        "fast input latency #2",
    ),
    Patch::new(
        0x0045_ced2,
        &[0x75, 0x04],
        &[0xeb, 0x1d],
        "replay speed control skip",
    ),
];

pub(crate) unsafe fn apply_basic() {
    unsafe { Patch::apply_all(PATCHES) };
}

/// Splice over `movss dword [ebp-0x64], xmm3` (5 bytes) inside `fcn.0047fcd0`, the
/// `AnmManager` modes 5/7 position helper. X and Y correctly accumulate `matrix.t*`;
/// Z doesn't. `[esi + 0x454]` is `matrix.tz` (scratch matrix at `vm + 0x41c`).
/// `[ebp - 0x64]` is the Z frame slot that the displaced `movss` writes to.
const ANM_MODE57_SPLICE: usize = 0x0047_fed0;
const ANM_MODE57_DISPLACED_LEN: usize = 5;
static ANM_MODE57_AFTER_SPLICE: usize = ANM_MODE57_SPLICE + ANM_MODE57_DISPLACED_LEN;

#[unsafe(naked)]
unsafe extern "C" fn anm_mode57_z_trampoline() -> ! {
    naked_asm!(
        "addss xmm3, dword ptr [esi + 0x454]",
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

/// th15 screenshot save (stdcall; filename pointer pushed on the stack).
/// The game calls this from the render thread before `Present`.
const SCREENSHOT_SAVE_FN: usize = 0x0044_cbf0;
const SCREENSHOT_SAVE_FN_PROLOGUE: [u8; 5] = [0x55, 0x8b, 0xec, 0x83, 0xec];

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
            "screenshot save (fcn.0044cbf0)",
        );
    }
}

/// Fixes an anim loading deadlock.
///
/// Here are the loader-context destructor (`fcn.0044bed0`) and the anim driver (`fcn.004865f0`)
/// it must pump while waiting. The destructor joins a worker spinning in `preloadAnim`
/// on `[anim+0x128]`; that flag is only cleared by the anim driver, reachable only from
/// main's per-frame state-tick. Once the destructor takes main, the state-tick stops,
/// the flag never clears, and `WaitForSingleObject` waits forever. We pump the anim driver
/// inline while waiting. We hook the function entrypoint so all call sites are covered.
///
/// `fcn.004865f0`: cdecl, no args; iterates the 30-slot anim table and runs one
/// `fcn.00486110` work-step on the first slot needing it. Returns non-zero on work.
///
/// `[this + 0x10]`: loader thread `HANDLE`, written by `_beginthreadex` in `fcn.0044be50`.
/// `fcn.00403f30(loader_ctx + 0xc)` reads `[+4]` of that pointer.
const FCN_0044BED0: usize = 0x0044_bed0;
const FCN_004865F0: usize = 0x0048_65f0;
const LOADER_CTX_HANDLE_OFFSET: usize = 0x10;
const PROLOGUE_LEN: usize = 5;
static FCN_0044BED0_AFTER_PROLOGUE: usize = FCN_0044BED0 + PROLOGUE_LEN;

/// Replays the displaced prologue and resumes past the splice.
/// None of the replayed instructions touch ECX, so `this` survives the trampoline.
#[unsafe(naked)]
unsafe extern "thiscall" fn fcn_0044bed0_trampoline(_this: *mut c_void) -> i32 {
    naked_asm!(
        "push ebp",
        "mov ebp, esp",
        "push -1",
        "jmp dword ptr [{slot}]",
        slot = sym FCN_0044BED0_AFTER_PROLOGUE,
    )
}

unsafe extern "thiscall" fn hooked_fcn_0044bed0(this: *mut c_void) -> i32 {
    unsafe {
        let loader_handle: HANDLE = if this.is_null() {
            null_mut()
        } else {
            read_unaligned(this.cast::<u8>().add(LOADER_CTX_HANDLE_OFFSET).cast())
        };

        info!(
            kind = "destructor_entered",
            this = format_args!("{this:p}"),
            loader_handle = format_args!("{loader_handle:p}"),
        );

        // We pump the anim driver from main until the loader exits. We use an initial
        // probe with `timeout = 0` to skip the pump entirely in the no-bug case.
        if !loader_handle.is_null() {
            let driver: unsafe extern "C" fn() -> i32 =
                parse_fn_ptr(with_exposed_provenance_mut(FCN_004865F0))
                    .expect("FCN_004865F0 is a non-zero constant");
            let mut pump_iters: u32 = 0;
            let drained = loop {
                let timeout_ms = u32::from(pump_iters != 0);
                match WaitForSingleObject(loader_handle, timeout_ms) {
                    WAIT_OBJECT_0 => break true,
                    WAIT_TIMEOUT => {
                        driver();
                        pump_iters = pump_iters.saturating_add(1);
                    }
                    other => {
                        // The trampoline's `INFINITE` wait will also fail,
                        // so we don't try to pump while spinning.
                        warn!(
                            kind = "destructor_pump_aborted",
                            wait_result = format_args!("{other:#x}"),
                            pump_iters,
                        );
                        break false;
                    }
                }
            };
            if drained {
                info!(
                    kind = "destructor_pump_drained",
                    this = format_args!("{this:p}"),
                    pump_iters,
                );
            }
        }

        let result = fcn_0044bed0_trampoline(this);
        info!(
            kind = "destructor_returned",
            this = format_args!("{this:p}"),
            result,
        );
        result
    }
}

pub(crate) unsafe fn install_destructor_hook() {
    unsafe {
        patch_jmp(
            FCN_0044BED0,
            // Original prologue: `push ebp; mov ebp, esp; push -1`.
            &[0x55, 0x8b, 0xec, 0x6a, 0xff],
            hooked_fcn_0044bed0 as *mut (),
            "fcn.0044bed0 entry-jmp -> hooked_fcn_0044bed0",
        );
    }
}

/// Fixes a race between the main thread and a BGM-init thread.
///
/// Hook 1 (`updatefast_wrapper`): call-site rewrite at `0x471ab7` so the first
/// `UpdateFast` call (just past main's pre-loop init) drains both loaders before forwarding.
/// By construction, no sound consumer fires until the game-loop body runs.
///
/// Hook 2 (`io_error_abort_trampoline`): splices `signal = 2` into the I/O thread's
/// error-exit path so a missing-asset install force-aborts BGM-init
/// instead of leaving it busy-waiting on a NULL slot.
const UPDATEFAST_CALL_SITE: usize = 0x0047_1ab7;
const REAL_UPDATEFAST: usize = 0x0047_2790;
const BGM_HANDLE_SLOT: usize = 0x0052_1338;
const IO_HANDLE_SLOT: usize = 0x0052_133c;
/// Loader-control flag. `0` initial; non-zero releases the post-load `Sleep(1)` busy-wait
/// at the tail of both loader thread procs; `2` is also the only escape from the inner
/// `cmp [signal], 2` exit at the top of the I/O loop at `0x475970`, and is written by
/// `io_error_abort_trampoline`.
const LOADER_SIGNAL: usize = 0x0052_1344;
const LOADER_SIGNAL_EXIT_CLEAN: u32 = 1;
const LOADER_SIGNAL_ABORT: u32 = 2;

const UPDATEFAST_CALL_BYTES: [u8; 5] = [0xe8, 0xd4, 0x0c, 0x00, 0x00];

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

/// Splices into the I/O thread's error-exit path at `0x475970+0x4f`. The displaced 7-byte
/// `push dword [esi*4 + 0x4cb3c0]` (the filename for the "error : Sound %s" printf) is
/// replayed; the inserted `signal = 2` hits BGM-init's only escape from its NULL-slot
/// busy-wait. Note the displaced shape is `push` rather than TH10/11/12/13's `mov eax`;
/// the trampoline preserves the same single-push side effect on ESP.
const IO_ERROR_SPLICE: usize = 0x0047_59bf;
const IO_ERROR_DISPLACED_LEN: usize = 7;
static IO_ERROR_AFTER_SPLICE: usize = IO_ERROR_SPLICE + IO_ERROR_DISPLACED_LEN;

#[unsafe(naked)]
unsafe extern "C" fn io_error_abort_trampoline() -> ! {
    naked_asm!(
        "mov dword ptr [{signal}], {abort}",
        "push dword ptr [esi*4 + 0x4cb3c0]",
        "jmp dword ptr [{slot}]",
        signal = const LOADER_SIGNAL,
        abort = const LOADER_SIGNAL_ABORT,
        slot = sym IO_ERROR_AFTER_SPLICE,
    )
}

/// JMP opcode at `IO_ERROR_SPLICE` means our `patch_jmp` landed;
/// the original `push`'s `0xff` means the patch was rejected.
const HOOK2_INSTALLED_OPCODE: u8 = 0xe9;

pub(crate) unsafe fn install_loader_sync_hooks() {
    unsafe {
        patch_jmp(
            IO_ERROR_SPLICE,
            &[0xff, 0x34, 0xb5, 0xc0, 0xb3, 0x4c, 0x00],
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
