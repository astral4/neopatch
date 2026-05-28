//! Patches and hooks for th12.exe v1.00b.

use neopatch_core::d3d9::install_call_site_rewrite;
use neopatch_core::destructor_pump::{self, Hook};
use neopatch_core::loader_sync::{self, LOADER_SIGNAL_ABORT};
use neopatch_core::patches::{Patch, patch_jmp};
use neopatch_core::screenshot::save_screenshot_live;
use std::arch::naked_asm;
use std::ffi::c_void;

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

/// AsciiInf destructor pump for th12. See `core::destructor_pump` for more details.
///
/// Destructor: `fcn.0042ec00` (`.\src\game\ascii.cpp:79`). Worker thread: `fcn.0042e9b0`,
/// spawned by `AsciiInf::start` (`fcn.0042eb10`). The worker preloads `.anm` assets into
/// a 32-slot table at `[[0x4ce8cc] + 0x4b50c0]`. Spin flag: `[anim+0x128]`.
/// Anim driver: `fcn.004603d0`. Join helper: `fcn.00464c40`. Handle slot: `[this+0x14]`.
///
/// The destructor uses `__stdcall` with FPO: instead of `push ebp; mov ebp, esp`,
/// it does 9 raw pushes including `push -1; push 0x497336` for the SEH frame, then reads
/// `this` from `[esp+0x28]` into EBP (repurposed as a scratch register).
/// The trampoline replays the two pushes so the SEH frame is in place
/// before the destructor body resumes at byte 7.
const FCN_0042EC00: usize = 0x0042_ec00;
const DTOR_SEH_HANDLER: u32 = 0x0049_7336;
static FCN_0042EC00_AFTER_PROLOGUE: usize = FCN_0042EC00 + 7;

/// Replays the displaced 7-byte prologue (`push -1; push imm32`) and resumes past the splice.
#[unsafe(naked)]
unsafe extern "stdcall" fn fcn_0042ec00_trampoline(_this: *mut c_void) -> i32 {
    naked_asm!(
        "push -1",
        "push {seh}",
        "jmp dword ptr [{slot}]",
        seh = const DTOR_SEH_HANDLER,
        slot = sym FCN_0042EC00_AFTER_PROLOGUE,
    )
}

pub(crate) unsafe fn install_destructor_hook() {
    unsafe {
        destructor_pump::install(destructor_pump::Config {
            dtor_addr: FCN_0042EC00,
            hook: Hook::FpoStdcall {
                trampoline: fcn_0042ec00_trampoline,
                seh_handler: DTOR_SEH_HANDLER,
            },
            anim_driver_addr: 0x0046_03d0,
            loader_handle_offset: 0x14,
            dtor_label: "fcn.0042ec00",
        });
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

/// Fixes a race between the main thread and the BGM-init / I/O loader threads.
///
/// `LOADER_SIGNAL` semantics for th12: non-zero releases the post-load waits
/// in BGM and I/O thread procs. `2` (written by `io_error_abort_trampoline`)
/// is the only escape from the inner busy-wait in `fcn.00453120` and the outer loop.
const LOADER_SIGNAL: usize = 0x004d_4770;
const IO_ERROR_SPLICE: usize = 0x0045_34af;
const IO_ERROR_DISPLACED_LEN: usize = 7;
static IO_ERROR_AFTER_SPLICE: usize = IO_ERROR_SPLICE + IO_ERROR_DISPLACED_LEN;

/// Splices into the I/O thread's error-exit path at `fcn.00453460+0x4f`.
/// The displaced 7-byte `mov eax, [esi*4 + 0x4aea50]` is replayed.
/// The inserted `signal = 2` hits BGM-init's only escape from its NULL-slot busy-wait.
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

pub(crate) unsafe fn install_loader_sync_hooks() {
    unsafe {
        loader_sync::install(
            &loader_sync::Config {
                signal_addr: LOADER_SIGNAL,
                bgm_handle_addr: 0x004d_4764,
                io_handle_addr: 0x004d_4768,
                call_site: 0x0044_f8aa,
                call_bytes: [0xe8, 0x41, 0x0b, 0x00, 0x00],
                real_fn: 0x0045_03f0,
                splice_addr: IO_ERROR_SPLICE,
                splice_expected: [0x8b, 0x04, 0xb5, 0x50, 0xea, 0x4a, 0x00],
            },
            io_error_abort_trampoline as *mut (),
        );
    }
}
