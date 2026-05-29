//! Patches and hooks for th11.exe v1.00a.

use neopatch_core::d3d9::install_call_site_rewrite;
use neopatch_core::destructor_pump::{self, Hook};
use neopatch_core::loader_sync::{self, LOADER_SIGNAL_ABORT};
use neopatch_core::patches::{Patch, patch_jmp};
use neopatch_core::screenshot::save_screenshot_live;
use std::arch::naked_asm;
use std::ffi::c_void;

/// Live `Direct3DCreate9` call site, rewritten to defend against downstream IAT hijacks.
/// There is a second call site at `0x00446ab2`, a dead standalone init helper that nothing calls.
const DIRECT3DCREATE9_CALL_ADDR: usize = 0x0044_570e;
const DIRECT3DCREATE9_CALL_BYTES: [u8; 5] = [0xe8, 0xa3, 0xa2, 0x01, 0x00];

pub(crate) unsafe fn install_d3d9_call_site_rewrite() {
    unsafe {
        install_call_site_rewrite(DIRECT3DCREATE9_CALL_ADDR, &DIRECT3DCREATE9_CALL_BYTES);
    }
}

/// "UpdateFast skip": flips `jne 0x44645e` to `jmp +0x43`, landing past
/// the `Sleep(1)` and the FPU catch-up loop in `CWindowManager::UpdateFast`.
///
/// "fast input latency #1/#2": flips two cond jumps so the per-frame dispatch
/// always reaches `fcn.00446420` (`UpdateFast`) instead of the slow/normal paths.
/// OILP also does this under "Force fast input latency mode."
///
/// "replay speed control skip": skips the game's own Ctrl-key fast-forward.
/// Without this, the game's internal speed control fights our pacer's replay-speed modes.
const PATCHES: &[Patch] = &[
    Patch::new(0x0044_6454, &[0x75, 0x08], &[0xeb, 0x43], "UpdateFast skip"),
    Patch::new(
        0x0044_5877,
        &[0x74, 0x0c],
        &[0xeb, 0x0c],
        "fast input latency #1",
    ),
    Patch::new(
        0x0044_588b,
        &[0x75, 0x15],
        &[0xeb, 0x15],
        "fast input latency #2",
    ),
    Patch::new(
        0x0043_6d5f,
        &[0x74, 0x14],
        &[0xeb, 0x14],
        "replay speed control skip",
    ),
];

pub(crate) unsafe fn apply_basic() {
    unsafe { Patch::apply_all(PATCHES) };
}

/// Splice over `mov ebx, [ebx + 0x404]` (6 bytes) inside `fcn.00450e20`, the `AnmManager`
/// modes 5/7 position helper. X and Y correctly accumulate `matrix.t*`; Z doesn't.
/// `[esp + 0x78]` is the `matrix.tz` frame slot; the displaced `mov` loads
/// the `AnmVm` flags field and is replayed.
const ANM_MODE57_SPLICE: usize = 0x0045_0f83;
const ANM_MODE57_DISPLACED_LEN: usize = 6;
static ANM_MODE57_AFTER_SPLICE: usize = ANM_MODE57_SPLICE + ANM_MODE57_DISPLACED_LEN;

#[unsafe(naked)]
unsafe extern "C" fn anm_mode57_z_trampoline() -> ! {
    naked_asm!(
        "fadd dword ptr [esp + 0x78]",
        "mov  ebx, [ebx + 0x404]",
        "jmp  dword ptr [{slot}]",
        slot = sym ANM_MODE57_AFTER_SPLICE,
    )
}

pub(crate) unsafe fn install_anm_matrix_tz_fix() {
    unsafe {
        patch_jmp(
            ANM_MODE57_SPLICE,
            &[0x8b, 0x9b, 0x04, 0x04, 0x00, 0x00],
            anm_mode57_z_trampoline as *mut (),
            "AnmManager mode 5/7 z + matrix.tz",
        );
    }
}

/// `AsciiInf` destructor pump for th11. See `core::destructor_pump` for more details.
///
/// Destructor: `fcn.00428c30` (`.\src\game\ascii.cpp:89`). Worker thread: `fcn.004289e0`,
/// spawned by `AsciiInf::start` (`fcn.00428b40`). The worker preloads `.anm` assets into
/// a 32-slot table at `[[0x4c3268] + 0x4350c0]`. Spin flag: `[anim+0x128]`.
/// Anim driver: `fcn.004548d0`. Join helper: `fcn.00459430`. Handle slot: `[this+0x14]`.
///
/// The destructor uses `__stdcall` with FPO: instead of `push ebp; mov ebp, esp`,
/// it does 9 raw pushes including `push -1; push 0x48a686` for the SEH frame, then reads
/// `this` from `[esp+0x28]` into EBP (repurposed as a scratch register).
/// The trampoline replays the two pushes so the SEH frame is in place
/// before the destructor body resumes at byte 7.
const FCN_00428C30: usize = 0x0042_8c30;
const DTOR_SEH_HANDLER: u32 = 0x0048_a686;
static FCN_00428C30_AFTER_PROLOGUE: usize = FCN_00428C30 + 7;

/// Replays the displaced 7-byte prologue (`push -1; push imm32`) and resumes past the splice.
#[unsafe(naked)]
unsafe extern "stdcall" fn fcn_00428c30_trampoline(_this: *mut c_void) -> i32 {
    naked_asm!(
        "push -1",
        "push {seh}",
        "jmp dword ptr [{slot}]",
        seh = const DTOR_SEH_HANDLER,
        slot = sym FCN_00428C30_AFTER_PROLOGUE,
    )
}

pub(crate) unsafe fn install_destructor_hook() {
    unsafe {
        destructor_pump::install(destructor_pump::Config {
            dtor_addr: FCN_00428C30,
            hook: Hook::FpoStdcall {
                trampoline: fcn_00428c30_trampoline,
                seh_handler: DTOR_SEH_HANDLER,
            },
            anim_driver_addr: 0x0045_48d0,
            loader_handle_offset: 0x14,
            dtor_label: "fcn.00428c30",
        });
    }
}

/// th11 screenshot save (eax-convention; filename pointer in EAX).
/// The game calls this from the render thread before `Present`.
const SCREENSHOT_SAVE_FN: usize = 0x0042_9ca0;
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
            "screenshot save (fcn.00429ca0)",
        );
    }
}

/// Fixes a race between the main thread and the BGM-init / I/O loader threads.
///
/// `LOADER_SIGNAL` semantics for th11: non-zero releases the post-load waits
/// in BGM and I/O thread procs. `2` (written by `io_error_abort_trampoline`)
/// is the only escape from the inner busy-wait in `fcn.00449140` and the outer loop.
const LOADER_SIGNAL: usize = 0x004c_90a4;
const IO_ERROR_SPLICE: usize = 0x0044_94cf;
const IO_ERROR_DISPLACED_LEN: usize = 7;
static IO_ERROR_AFTER_SPLICE: usize = IO_ERROR_SPLICE + IO_ERROR_DISPLACED_LEN;

/// Splices into the I/O thread's error-exit path at `fcn.00449480+0x4f`.
/// The displaced 7-byte `mov eax, [esi*4 + 0x4a36b0]` is replayed.
/// The inserted `signal = 2` hits BGM-init's only escape from its NULL-slot busy-wait.
#[unsafe(naked)]
unsafe extern "C" fn io_error_abort_trampoline() -> ! {
    naked_asm!(
        "mov dword ptr [{signal}], {abort}",
        "mov eax, dword ptr [esi*4 + 0x4a36b0]",
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
                bgm_handle_addr: 0x004c_9098,
                io_handle_addr: 0x004c_909c,
                call_site: 0x0044_58a7,
                call_bytes: [0xe8, 0x74, 0x0b, 0x00, 0x00],
                real_fn: 0x0044_6420,
                splice_addr: IO_ERROR_SPLICE,
                splice_expected: [0x8b, 0x04, 0xb5, 0xb0, 0x36, 0x4a, 0x00],
            },
            io_error_abort_trampoline as *mut (),
        );
    }
}
