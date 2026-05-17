//! Static byte patches and hooks for th15.exe v1.00b.

use crate::patches::{Patch, patch_jmp};
use std::arch::naked_asm;
use std::ffi::c_void;
use std::mem::transmute;
use std::ptr::{null_mut, read_unaligned, with_exposed_provenance};
use tracing::info;
use windows_sys::Win32::Foundation::{HANDLE, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::WaitForSingleObject;

/// "UpdateFast skip": unconditional `jmp +0x4A` past the game's `Sleep`,
/// spin, and deadline-advance. Without it, the game's own pacer
/// holds the inter-Present interval at >=33ms in slow replay.
///
/// "fast input latency #1/#2": flip the cond jumps to `EB`, forcing the input preamble
/// to "fast" mode. OILP also does this under "Force fast input latency mode."
///
/// "replay speed control skip": skip the game's own replay-speed control
/// so it doesn't fight our pacer.
///
/// See `dialog_dismiss.rs` for dialog-flow byte patches.
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

/// Byte offset within the loader-context (`fcn.0044bed0`'s `this`) of the loader thread's `HANDLE`.
/// `fcn.0044be50` stores the `_beginthreadex` return value here.
/// `fcn.00403f30`, called from the destructor with `loader_ctx + 0xc`,
/// reads `[+4]` of that pointer, i.e. `loader_ctx + 0x10`.
const LOADER_CTX_HANDLE_OFFSET: usize = 0x10;

/// Length of the original prologue at `fcn.0044bed0` that the entry-jmp displaces.
/// The bytes are `55  8b ec  6a ff` (push ebp; mov ebp, esp; push -1);
/// the trampoline below replays the same instructions.
const PROLOGUE_LEN: usize = 5;
const FCN_0044BED0_AFTER_PROLOGUE: usize = FCN_0044BED0 + PROLOGUE_LEN;

pub(crate) unsafe fn apply_basic() {
    unsafe {
        for patch in PATCHES {
            patch.apply();
        }
    }
}

/// Replays `fcn.0044bed0`'s overwritten prologue, then resumes past the patch site.
/// None of the replayed instructions touch ECX, so `this` survives the trampoline.
#[unsafe(naked)]
unsafe extern "thiscall" fn fcn_0044bed0_trampoline(_this: *mut c_void) -> i32 {
    naked_asm!(
        "push ebp",
        "mov ebp, esp",
        "push -1",
        // Absolute jmp so the ASLR-relocated trampoline address doesn't matter.
        "mov eax, {after_prologue}",
        "jmp eax",
        after_prologue = const FCN_0044BED0_AFTER_PROLOGUE,
    )
}

unsafe extern "thiscall" fn hooked_fcn_0044bed0(this: *mut c_void) -> i32 {
    unsafe {
        let loader_handle: HANDLE = if this.is_null() {
            null_mut()
        } else {
            read_unaligned(
                this.cast::<u8>()
                    .add(LOADER_CTX_HANDLE_OFFSET)
                    .cast::<HANDLE>(),
            )
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
                transmute(with_exposed_provenance::<()>(FCN_004865F0));
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
/// that resolves a deadlock between the destructor's thread-join and a worker spinning in `preloadAnim`.
/// The worker spins on `[anim+0x128]`, which is cleared only by the anim driver `fcn.004865f0`,
/// reachable only from `main`'s per-frame state-tick. Once the destructor takes `main`,
/// the state-tick stops, the flag never clears, the worker never exits,
/// and the destructor's `WaitForSingleObject` waits forever.
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
