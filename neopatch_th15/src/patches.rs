//! Patches and hooks for th15.exe v1.00b.

use neopatch_core::d3d9::install_call_site_rewrite;
use neopatch_core::loader_sync::{self, LOADER_SIGNAL_ABORT};
use neopatch_core::patches::{Patch, patch_jmp};
use neopatch_core::screenshot::save_screenshot_live;
use neopatch_core::vtable::parse_fn_ptr;
use std::arch::naked_asm;
use std::ffi::c_void;
use std::ptr::{null_mut, read_unaligned, with_exposed_provenance_mut};
use tracing::{info, warn};
use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::WaitForSingleObject;

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

/// Fixes an anim loader deadlock at scene transition.
///
/// `fcn.0044bed0` is the destructor of `AsciiInf`, ZUN's ASCII/text renderer
/// (`sprtlib.h:750`). It owns a worker thread that preloads font and global UI anims
/// (`ascii*.anm`, `sig.anm`, `text.anm`) in the background. The destructor must join
/// that worker before freeing the buffers it's writing into.
///
/// The worker loads each anim through a `preloadAnim` function, which sets a "loading" flag
/// at `[anim+0x128]` and spins waiting for it to clear. The main thread's per-frame loop
/// (peek-message -> state-tick -> render -> present) advances the flag by calling the
/// anim driver (`fcn.004865f0`) during the state-tick. The driver processes one work-step
/// per call and eventually clears the flag, freeing the worker.
///
/// When the destructor runs on main, the per-frame loop is paused. The driver stops running,
/// the flag never advances, the worker never exits, and the destructor's join helper
/// (`fcn.00403f30`, which polls the handle with `WaitForSingleObject(handle, 200ms)`
/// and `Sleep(1)` until signaled) spins forever. Deadlock!
///
/// We fix this by pumping the driver inline from within the destructor,
/// substituting for the per-frame tick. We hook at the destructor's entrypoint
/// so every call site is covered.
///
/// `fcn.004865f0`: cdecl, no args; iterates the 30-slot anim table
/// and runs one `fcn.00486110` work-step on the first slot needing it.
///
/// `[this + 0x10]`: worker thread `HANDLE`, written by `_beginthreadex` in `fcn.0044be50`.
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
                        // The original join sees the same result and falls through to
                        // `CloseHandle`, so pumping can't help.
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

/// Fixes a race between the main thread and the BGM-init / I/O loader threads.
///
/// `LOADER_SIGNAL` semantics for th15: non-zero releases the post-load waits
/// in BGM and I/O thread procs. `2` (written by `io_error_abort_trampoline`)
/// is the only escape from the inner `cmp [signal], 2` exit
/// at the top of the I/O loop at `0x475970`.
///
/// The per-frame `UpdateFast` (`fcn.00472790`) uses `__thiscall`.
/// `this` is passed in ECX and the call site at `0x471ab7` pushes nothing.
const LOADER_SIGNAL: usize = 0x0052_1344;
const IO_ERROR_SPLICE: usize = 0x0047_59bf;
const IO_ERROR_DISPLACED_LEN: usize = 7;
static IO_ERROR_AFTER_SPLICE: usize = IO_ERROR_SPLICE + IO_ERROR_DISPLACED_LEN;

/// Splices into the I/O thread's error-exit path at `0x475970+0x4f`. The displaced 7-byte
/// `push dword [esi*4 + 0x4cb3c0]` (the filename for the "error : Sound %s" `printf` call)
/// is replayed. The inserted `signal = 2` hits BGM-init's only escape from its NULL-slot
/// busy-wait. Note: th15 has `push` instead of th10/11/12/13's `mov eax`;
/// the trampoline preserves the same single-push side effect on ESP.
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

pub(crate) unsafe fn install_loader_sync_hooks() {
    unsafe {
        loader_sync::install(
            &loader_sync::Config {
                signal_addr: LOADER_SIGNAL,
                bgm_handle_addr: 0x0052_1338,
                io_handle_addr: 0x0052_133c,
                call_site: 0x0047_1ab7,
                call_bytes: [0xe8, 0xd4, 0x0c, 0x00, 0x00],
                real_fn: 0x0047_2790,
                splice_addr: IO_ERROR_SPLICE,
                splice_expected: [0xff, 0x34, 0xb5, 0xc0, 0xb3, 0x4c, 0x00],
            },
            io_error_abort_trampoline as *mut (),
        );
    }
}
