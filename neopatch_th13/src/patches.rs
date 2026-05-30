//! Patches and hooks for th13.exe v1.00c.

use neopatch_core::d3d9::install_call_site_rewrite;
use neopatch_core::destructor_pump::{self, Hook};
use neopatch_core::patches::{Patch, patch_jmp};
use neopatch_core::screenshot::save_screenshot_live;
use std::arch::naked_asm;
use std::ffi::c_void;

/// Live `Direct3DCreate9` call site, rewritten to defend against downstream IAT hijacks.
/// There is a second call site at `0x0045da12`, a dead standalone init helper that nothing calls.
const DIRECT3DCREATE9_CALL_ADDR: usize = 0x0045_c42f;
const DIRECT3DCREATE9_CALL_BYTES: [u8; 6] = [0xff, 0x15, 0x98, 0x22, 0x4a, 0x00];

pub(crate) unsafe fn install_d3d9_call_site_rewrite() {
    unsafe {
        install_call_site_rewrite(DIRECT3DCREATE9_CALL_ADDR, &DIRECT3DCREATE9_CALL_BYTES);
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
    Patch::new(
        0x0045_c5d7,
        &[0x74, 0x0c],
        &[0xeb, 0x0c],
        "fast input latency #1",
    ),
    Patch::new(
        0x0045_c5eb,
        &[0x75, 0x15],
        &[0xeb, 0x15],
        "fast input latency #2",
    ),
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

/// `AsciiInf` destructor pump for th13. See `core::destructor_pump` for more details.
///
/// Destructor: `fcn.004399e0` (`src\game\ascii.cpp:79`). Worker thread: `fcn.00439730`,
/// spawned by `AsciiInf::start` (`fcn.004398e0`). The worker preloads `.anm` assets into
/// a 26-slot table at `[[0x4dc688] + 0xb77b34]`. Spin flag: `[anim+0x12c]`.
/// Anim driver: `fcn.0046e510`. Join helper: `fcn.00473590`. Handle slot: `[this+0x14]`.
///
/// The destructor uses `__stdcall`: the factory does `push esi; call dtor`,
/// the destructor reads `this` from `[ebp+8]`, and returns with `ret 4`.
const FCN_004399E0: usize = 0x0043_99e0;
static FCN_004399E0_AFTER_PROLOGUE: usize = FCN_004399E0 + 5;

/// Replays the displaced 5-byte prologue (`push ebp; mov ebp, esp; push -1`) and resumes past the splice.
#[unsafe(naked)]
unsafe extern "stdcall" fn fcn_004399e0_trampoline(_this: *mut c_void) -> i32 {
    naked_asm!(
        "push ebp",
        "mov ebp, esp",
        "push -1",
        "jmp dword ptr [{slot}]",
        slot = sym FCN_004399E0_AFTER_PROLOGUE,
    )
}

pub(crate) unsafe fn install_destructor_hook() {
    unsafe {
        destructor_pump::install(destructor_pump::Config {
            dtor_addr: FCN_004399E0,
            hook: Hook::EbpFrameStdcall(fcn_004399e0_trampoline),
            anim_driver_addr: 0x0046_e510,
            loader_handle_offset: 0x14,
            dtor_label: "fcn.004399e0",
        });
    }
}

/// th13 screenshot save (eax-convention; filename pointer in EAX).
/// The game calls this from the render thread before `Present`.
const SCREENSHOT_SAVE_FN: usize = 0x0043_a950;
const SCREENSHOT_SAVE_FN_PROLOGUE: [u8; 5] = [0x55, 0x8b, 0xec, 0x83, 0xec];

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
            "screenshot save (fcn.0043a950)",
        );
    }
}
