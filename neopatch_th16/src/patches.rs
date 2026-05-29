//! Patches and hooks for th16.exe v1.00a.

use neopatch_core::d3d9::install_call_site_rewrite;
use neopatch_core::destructor_pump::{self, Hook};
use neopatch_core::loader_sync::{self, LOADER_SIGNAL_ABORT};
use neopatch_core::patches::{Patch, patch_jmp};
use neopatch_core::screenshot::save_screenshot_live;
use std::arch::naked_asm;
use std::ffi::c_void;

/// Live `Direct3DCreate9` call site, rewritten to defend against downstream IAT hijacks.
/// This is the only call site in th16.
const DIRECT3DCREATE9_CALL_ADDR: usize = 0x0045_9a84;
const DIRECT3DCREATE9_CALL_BYTES: [u8; 6] = [0xff, 0x15, 0x5c, 0xb2, 0x48, 0x00];

pub(crate) unsafe fn install_d3d9_call_site_rewrite() {
    unsafe {
        install_call_site_rewrite(DIRECT3DCREATE9_CALL_ADDR, &DIRECT3DCREATE9_CALL_BYTES);
    }
}

/// "UpdateFast skip": unconditional `jmp +0x4B` past the game's `Sleep`, spin, and
/// deadline-advance, so our pacer is the sole timing source.
///
/// "fast input latency #1/#2": flips the cond jumps to `EB`, forcing the input preamble
/// to "fast" mode. OILP also does this under "Force fast input latency mode."
///
/// "replay speed control skip": skips the game's own replay-speed control so it doesn't
/// fight our pacer.
const PATCHES: &[Patch] = &[
    Patch::new(0x0045_ac9d, &[0x72, 0x08], &[0xeb, 0x4b], "UpdateFast skip"),
    Patch::new(
        0x0045_9f72,
        &[0x74, 0x0c],
        &[0xeb, 0x0c],
        "fast input latency #1",
    ),
    Patch::new(
        0x0045_9f87,
        &[0x75, 0x15],
        &[0xeb, 0x15],
        "fast input latency #2",
    ),
    Patch::new(
        0x0044_8e62,
        &[0x74, 0x19],
        &[0xeb, 0x19],
        "replay speed control skip",
    ),
];

pub(crate) unsafe fn apply_basic() {
    unsafe { Patch::apply_all(PATCHES) };
}

/// `AsciiInf` destructor pump for th16. See `core::destructor_pump` for more details.
///
/// Destructor: `fcn.0043afe0` (ebp-frame + SEH prologue, thiscall).
/// Worker thread: `fcn.0043adc0`, spawned by `AsciiInf::start` (`fcn.0043af60`) via `_beginthreadex`.
/// Spin flag: `[anim+0x128]`. Anim driver: `fcn.00465a30`. Handle slot: `[this+0x10]`.
const FCN_0043AFE0: usize = 0x0043_afe0;
static FCN_0043AFE0_AFTER_PROLOGUE: usize = FCN_0043AFE0 + 5;

/// Replays the displaced 5-byte prologue (`push ebp; mov ebp, esp; push -1`) and resumes
/// past the splice. None of the replayed instructions touch ECX, so `this` survives.
#[unsafe(naked)]
unsafe extern "thiscall" fn fcn_0043afe0_trampoline(_this: *mut c_void) -> i32 {
    naked_asm!(
        "push ebp",
        "mov ebp, esp",
        "push -1",
        "jmp dword ptr [{slot}]",
        slot = sym FCN_0043AFE0_AFTER_PROLOGUE,
    )
}

pub(crate) unsafe fn install_destructor_hook() {
    unsafe {
        destructor_pump::install(destructor_pump::Config {
            dtor_addr: FCN_0043AFE0,
            hook: Hook::EbpFrameThiscall(fcn_0043afe0_trampoline),
            anim_driver_addr: 0x0046_5a30,
            loader_handle_offset: 0x10,
            dtor_label: "fcn.0043afe0",
        });
    }
}

/// Fixes a race between the main thread and the sound-file / I/O loader threads.
///
/// `LOADER_SIGNAL` semantics for th16: non-zero releases the loaders' post-load waits. `2`
/// (written by `sound_load_error_abort_trampoline`) is the only escape from the I/O thread's
/// NULL-slot busy-wait in `fcn.0045e990`, which consumes the `se_*.wav` table the sound-file
/// loader (`fcn.0045d7e0`) produces at `0x004dbfe4`.
const LOADER_SIGNAL: usize = 0x004d_f490;
const SOUND_LOAD_ERROR_SPLICE: usize = 0x0045_d82f;
const SOUND_LOAD_ERROR_DISPLACED_LEN: usize = 7;
static SOUND_LOAD_ERROR_AFTER_SPLICE: usize =
    SOUND_LOAD_ERROR_SPLICE + SOUND_LOAD_ERROR_DISPLACED_LEN;

/// Splices into the sound-file loader's error-exit path at `fcn.0045d7e0+0x4f`. The displaced
/// 7-byte `push dword [esi*4 + 0x491a00]` (the `.wav` path for the "error : Sound %s" `printf`)
/// is replayed. The inserted `signal = 2` releases the I/O thread's NULL-slot busy-wait.
#[unsafe(naked)]
unsafe extern "C" fn sound_load_error_abort_trampoline() -> ! {
    naked_asm!(
        "mov dword ptr [{signal}], {abort}",
        "push dword ptr [esi*4 + 0x491a00]",
        "jmp dword ptr [{slot}]",
        signal = const LOADER_SIGNAL,
        abort = const LOADER_SIGNAL_ABORT,
        slot = sym SOUND_LOAD_ERROR_AFTER_SPLICE,
    )
}

pub(crate) unsafe fn install_loader_sync_hooks() {
    unsafe {
        loader_sync::install(
            &loader_sync::Config {
                signal_addr: LOADER_SIGNAL,
                bgm_handle_addr: 0x004d_f488,
                io_handle_addr: 0x004d_f484,
                call_site: 0x0045_9fa3,
                call_bytes: [0xe8, 0xa8, 0x0c, 0x00, 0x00],
                real_fn: 0x0045_ac50,
                splice_addr: SOUND_LOAD_ERROR_SPLICE,
                splice_expected: [0xff, 0x34, 0xb5, 0x00, 0x1a, 0x49, 0x00],
            },
            sound_load_error_abort_trampoline as *mut (),
        );
    }
}

/// Splice over `movss dword [ebp-0x64], xmm3` (5 bytes) inside `fcn.00466f00`, the
/// `AnmManager` modes 5/7 position helper. X and Y correctly accumulate `matrix.t*`;
/// Z doesn't. `[esi + 0x448]` is `matrix.tz` (scratch matrix at `vm + 0x410`).
/// `[ebp - 0x64]` is the Z frame slot that the displaced `movss` writes to.
const ANM_MODE57_SPLICE: usize = 0x0046_70ff;
const ANM_MODE57_DISPLACED_LEN: usize = 5;
static ANM_MODE57_AFTER_SPLICE: usize = ANM_MODE57_SPLICE + ANM_MODE57_DISPLACED_LEN;

#[unsafe(naked)]
unsafe extern "C" fn anm_mode57_z_trampoline() -> ! {
    naked_asm!(
        "addss xmm3, dword ptr [esi + 0x448]",
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

/// th16 screenshot save (stdcall; filename pointer pushed on the stack).
/// The game calls this from the render thread before `Present`.
const SCREENSHOT_SAVE_FN: usize = 0x0043_bbd0;
const SCREENSHOT_SAVE_FN_PROLOGUE: [u8; 5] = [0x53, 0x8b, 0xdc, 0x83, 0xec];

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
            "screenshot save (fcn.0043bbd0)",
        );
    }
}
