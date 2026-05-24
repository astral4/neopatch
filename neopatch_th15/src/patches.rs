//! Static byte patches and hooks for th15.exe v1.00b.

use neopatch_core::patches::{Patch, patch_jmp};
use neopatch_core::vtable::parse_fn_ptr;
use std::arch::naked_asm;
use std::ffi::c_void;
use std::ptr::{null_mut, read_unaligned, with_exposed_provenance_mut};
use tracing::info;
use windows_sys::Win32::Foundation::{HANDLE, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::WaitForSingleObject;

/// "UpdateFast skip": unconditional `jmp +0x4A` past the game's `Sleep`,
/// spin, and deadline-advance. Without it, the game's own pacer
/// holds the inter-`Present` interval at >=33ms in slow replay.
///
/// "fast input latency #1/#2": flip the cond jumps to `EB`, forcing the input preamble
/// to "fast" mode. OILP also does this under "Force fast input latency mode."
///
/// "replay speed control skip": skip the game's own replay-speed control
/// so it doesn't fight our pacer.
pub(crate) const PATCHES: &[Patch] = &[
    Patch::new(0x0047_27de, &[0x72, 0x08], &[0xeb, 0x4a], "UpdateFast skip"),
    Patch::new(0x0047_1a86, &[0x74], &[0xeb], "fast input latency #1"),
    Patch::new(0x0047_1a9b, &[0x75], &[0xeb], "fast input latency #2"),
    Patch::new(
        0x0045_ced2,
        &[0x75, 0x04],
        &[0xeb, 0x1d],
        "replay speed control skip",
    ),
];

const FCN_0044BED0: usize = 0x0044_bed0;
/// `fcn.004865f0` is th15's anim driver. It iterates the 30-slot anim table
/// and dispatches one `fcn.00486110` work-step on the first slot that needs it.
/// It returns non-zero if work was done. The calling convention is cdecl with no args;
/// it only uses globals.
const FCN_004865F0: usize = 0x0048_65f0;

/// Byte offset within the loader-context (`fcn.0044bed0`'s `this`) of the loader thread's
/// `HANDLE`. `fcn.0044be50` stores the `_beginthreadex` return value here.
/// `fcn.00403f30`, called from the destructor with `loader_ctx + 0xc`,
/// reads `[+4]` of that pointer, i.e. `loader_ctx + 0x10`.
const LOADER_CTX_HANDLE_OFFSET: usize = 0x10;

/// Length of the original prologue at `fcn.0044bed0` that the entry-jmp displaces.
const PROLOGUE_LEN: usize = 5;
static FCN_0044BED0_AFTER_PROLOGUE: usize = FCN_0044BED0 + PROLOGUE_LEN;

pub(crate) unsafe fn apply_basic() {
    unsafe { Patch::apply_all(PATCHES) };
}

/// Replays `fcn.0044bed0`'s overwritten prologue, then resumes past the patch site.
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

        // Drive the anim driver from main until the loader exits.
        // The initial probe with `timeout = 0` lets the no-bug case,
        // where the loader is already done, skip the pump entirely.
        if !loader_handle.is_null() {
            let driver: unsafe extern "C" fn() -> i32 =
                parse_fn_ptr(with_exposed_provenance_mut::<()>(FCN_004865F0))
                    .expect("FCN_004865F0 is a non-zero constant");
            let mut pump_iters: u32 = 0;
            let mut r = WaitForSingleObject(loader_handle, 0);
            while r == WAIT_TIMEOUT {
                driver();
                pump_iters = pump_iters.saturating_add(1);
                r = WaitForSingleObject(loader_handle, 1);
            }
            info!(
                kind = "destructor_pump_drained",
                this = format_args!("{this:p}"),
                pump_iters,
            );
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

/// Function-entry hook on `fcn.0044bed0` (the loader-context destructor)
/// that resolves a deadlock between the destructor's thread-join and a worker
/// spinning in `preloadAnim`. The worker spins on `[anim+0x128]`, which is cleared
/// only by the anim driver `fcn.004865f0`, reachable only from `main`'s per-frame state-tick.
/// Once the destructor takes `main`, the state-tick stops, the flag never clears,
/// the worker never exits, and the destructor's `WaitForSingleObject` waits forever.
/// We hook at the function entry (rather than at every call site) just to be sure.
pub(crate) unsafe fn install_destructor_hook() {
    unsafe {
        patch_jmp(
            FCN_0044BED0,
            // Original 5-byte prologue (`push ebp; mov ebp, esp; push -1`)
            // that the entry-jmp displaces.
            &[0x55, 0x8b, 0xec, 0x6a, 0xff],
            hooked_fcn_0044bed0 as *mut (),
            "fcn.0044bed0 entry-jmp -> hooked_fcn_0044bed0",
        );
    }
}

/// Location of the `movss dword [ebp-0x64], xmm3` we displace with `e9 disp32`.
const ANM_MODE57_SPLICE: usize = 0x0047_fed0;
/// Length of the displaced instruction (5 bytes for `F3 0F 11 5D 9C`).
const ANM_MODE57_DISPLACED_LEN: usize = 5;
/// Resume target past the displaced `movss` at the splice.
static ANM_MODE57_AFTER_SPLICE: usize = ANM_MODE57_SPLICE + ANM_MODE57_DISPLACED_LEN;

/// Adds the missing `addss xmm3, matrix.tz` before the Z `movss` in `fcn.0047fcd0`, the
/// position helper used by `AnmManager` render modes 5 and 7. X and Y correctly accumulate
/// their `matrix.t*`, unlike Z. `[esi + 0x454]` is the matrix.tz offset within the scratch
/// matrix at `vm + 0x41c`. `[ebp - 0x64]` is the Z frame slot the displaced `movss` writes to.
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
