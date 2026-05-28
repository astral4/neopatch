//! Patches and hooks for th14.exe v1.00b.

use neopatch_core::d3d9::install_call_site_rewrite;
use neopatch_core::destructor_pump::{self, Hook};
use neopatch_core::loader_sync::{self, LOADER_SIGNAL_ABORT};
use neopatch_core::patches::{Patch, patch_jmp};
use neopatch_core::screenshot::save_screenshot_live;
use std::arch::naked_asm;
use std::ffi::c_void;

/// Live `Direct3DCreate9` call site, rewritten to defend against downstream IAT hijacks.
const TH14_DIRECT3DCREATE9_CALL_ADDR: usize = 0x0046_952c;
const TH14_DIRECT3DCREATE9_CALL_BYTES: [u8; 6] = [0xff, 0x15, 0xb0, 0x12, 0x4b, 0x00];

pub(crate) unsafe fn install_d3d9_call_site_rewrite() {
    unsafe {
        install_call_site_rewrite(
            TH14_DIRECT3DCREATE9_CALL_ADDR,
            &TH14_DIRECT3DCREATE9_CALL_BYTES,
        );
    }
}

/// "UpdateFast skip": flips `jb 0x46a778` to `jmp +0x4A`, landing past the `Sleep(1)`
/// and the FPU catch-up loop inside `CWindowManager::UpdateFast` at `0x0046A720`.
///
/// "fast input latency #1/#2": flips two cond jumps so the per-frame driver dispatch
/// is always reached on `fcn.0046a720` (`UpdateFast`). Skips the alternative
/// "automatic" and "normal" paths. OILP also does this under "Force fast input latency mode."
///
/// "replay speed control skip": skips the game's own Ctrl-key fast-forward branch.
/// Without this, the game's internal speed control fights our pacer's replay-speed modes.
const PATCHES: &[Patch] = &[
    Patch::new(0x0046_a76e, &[0x72, 0x08], &[0xeb, 0x4a], "UpdateFast skip"),
    Patch::new(
        0x0046_9a20,
        &[0x74, 0x0c],
        &[0xeb, 0x0c],
        "fast input latency #1",
    ),
    Patch::new(
        0x0046_9a35,
        &[0x75, 0x15],
        &[0xeb, 0x15],
        "fast input latency #2",
    ),
    Patch::new(
        0x0045_5e82,
        &[0x75, 0x04],
        &[0xeb, 0x1d],
        "replay speed control skip",
    ),
];

pub(crate) unsafe fn apply_basic() {
    unsafe { Patch::apply_all(PATCHES) };
}

/// Splice over `movss dword [ebp - 0x5c], xmm3` (5 bytes) inside `fcn.00477730`, the
/// `AnmManager` modes 5/7 position helper. X and Y correctly accumulate `matrix.t*`;
/// Z doesn't. `[ebp - 0x5c]` is the stack matrix's `tz` slot, pre-loaded with the
/// scratch matrix's `tz` by the `rep movsd` at `0x00477857` (which copies
/// `[ebx + 0x420 .. ebx + 0x460]` into the stack frame before this splice runs).
/// The fix adds that pre-loaded `tz` back into xmm3 before the displaced `movss`
/// would have overwritten it. Equivalent to th15's `addss xmm3, [esi + 0x454]`
/// but reads from the stack slot since `rep movsd` has already deposited the
/// value there; th15 doesn't pre-copy and reads directly from the scratch matrix.
const ANM_MODE57_SPLICE: usize = 0x0047_78f9;
const ANM_MODE57_DISPLACED_LEN: usize = 5;
static ANM_MODE57_AFTER_SPLICE: usize = ANM_MODE57_SPLICE + ANM_MODE57_DISPLACED_LEN;

#[unsafe(naked)]
unsafe extern "C" fn anm_mode57_z_trampoline() -> ! {
    naked_asm!(
        "addss xmm3, dword ptr [ebp - 0x5c]",
        "movss dword ptr [ebp - 0x5c], xmm3",
        "jmp   dword ptr [{slot}]",
        slot = sym ANM_MODE57_AFTER_SPLICE,
    )
}

pub(crate) unsafe fn install_anm_matrix_tz_fix() {
    unsafe {
        patch_jmp(
            ANM_MODE57_SPLICE,
            &[0xf3, 0x0f, 0x11, 0x5d, 0xa4],
            anm_mode57_z_trampoline as *mut (),
            "AnmManager mode 5/7 z + matrix.tz",
        );
    }
}

/// AsciiInf destructor pump for th14. See `core::destructor_pump` for more details.
///
/// Destructor: `fcn.00444340` (`src\game\ascii.cpp:82`). Worker thread: `fcn.00444170`,
/// spawned by `AsciiInf::start` (`fcn.004442c0`). The worker preloads `.anm` assets
/// (including one of `ascii.anm` / `ascii_960.anm` / `ascii_1280.anm`, chosen by
/// `[0x4d9153] % 3`, the display-mode byte) into a 26-slot table at `[[0x4f56cc] + 0xbc7b0c]`.
/// Spin flag: `[anim+0x12c]`. Anim driver: `fcn.0047d720`. Join helper: `fcn.00403bb0`.
/// Handle slot: `[this+0x14]` (the destructor passes `&edi[0x10]` to the join helper,
/// which reads `[ecx+4]`).
const FCN_00444340: usize = 0x0044_4340;
static FCN_00444340_AFTER_PROLOGUE: usize = FCN_00444340 + 5;

/// Replays the displaced 5-byte prologue (`push ebp; mov ebp, esp; push -1`) and resumes past the splice.
/// None of the replayed instructions touch ECX, so `this` survives the trampoline.
#[unsafe(naked)]
unsafe extern "thiscall" fn fcn_00444340_trampoline(_this: *mut c_void) -> i32 {
    naked_asm!(
        "push ebp",
        "mov ebp, esp",
        "push -1",
        "jmp dword ptr [{slot}]",
        slot = sym FCN_00444340_AFTER_PROLOGUE,
    )
}

pub(crate) unsafe fn install_destructor_hook() {
    unsafe {
        destructor_pump::install(destructor_pump::Config {
            dtor_addr: FCN_00444340,
            hook: Hook::EbpFrameThiscall(fcn_00444340_trampoline),
            anim_driver_addr: 0x0047_d720,
            loader_handle_offset: 0x14,
            dtor_label: "fcn.00444340",
        });
    }
}

/// th14 screenshot save (stdcall; filename pointer pushed on the stack).
/// The game calls this from the render thread before `Present`.
const SCREENSHOT_SAVE_FN: usize = 0x0044_5000;
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
            "screenshot save (fcn.00445000)",
        );
    }
}

/// Fixes a race between the main thread and the BGM-init / I/O loader threads.
///
/// `LOADER_SIGNAL` semantics for th14: non-zero releases the post-load waits
/// in BGM and I/O thread procs. `2` (written by `io_error_abort_trampoline`)
/// is the only escape from the inner busy-wait in `fcn.0046d620` and the
/// top-of-loop check in `fcn.0046d980`. Handle closure is left to the
/// scene-transition teardown, which no-ops on an exited thread.
const LOADER_SIGNAL: usize = 0x004f_d1a0;
const IO_ERROR_SPLICE: usize = 0x0046_d9cf;
const IO_ERROR_DISPLACED_LEN: usize = 7;
static IO_ERROR_AFTER_SPLICE: usize = IO_ERROR_SPLICE + IO_ERROR_DISPLACED_LEN;

/// Splices into the I/O thread's error-exit path at `fcn.0046d980+0x4f`.
/// The displaced 7-byte `push dword [esi*4 + 0x4d60b0]` (the filename for the
/// "error : Sound %s" `printf` call) is replayed.
/// The inserted `signal = 2` hits BGM-init's only escape from its NULL-slot busy-wait.
#[unsafe(naked)]
unsafe extern "C" fn io_error_abort_trampoline() -> ! {
    naked_asm!(
        "mov dword ptr [{signal}], {abort}",
        "push dword ptr [esi*4 + 0x4d60b0]",
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
                bgm_handle_addr: 0x004f_d194,
                io_handle_addr: 0x004f_d198,
                call_site: 0x0046_9a51,
                call_bytes: [0xe8, 0xca, 0x0c, 0x00, 0x00],
                real_fn: 0x0046_a720,
                splice_addr: IO_ERROR_SPLICE,
                splice_expected: [0xff, 0x34, 0xb5, 0xb0, 0x60, 0x4d, 0x00],
            },
            io_error_abort_trampoline as *mut (),
        );
    }
}
