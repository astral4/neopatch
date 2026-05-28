//! Patches and hooks for th15.exe v1.00b.

use neopatch_core::d3d9::install_call_site_rewrite;
use neopatch_core::destructor_pump::{self, Hook};
use neopatch_core::loader_sync::{self, LOADER_SIGNAL_ABORT};
use neopatch_core::patches::{Patch, patch_jmp};
use neopatch_core::screenshot::save_screenshot_live;
use std::arch::naked_asm;
use std::ffi::c_void;

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

/// AsciiInf destructor pump for th15. See `core::destructor_pump` for more details.
///
/// Destructor: `fcn.0044bed0` (`sprtlib.h:750`).
/// Worker thread: `fcn.0044bd00`, spawned by `AsciiInf::start` (`fcn.0044be50`).
/// The worker preloads `.anm` assets (including one of `ascii.anm` / `ascii_960.anm` /
/// `ascii_1280.anm`, chosen by `[0x4e79c3] % 3`, the display-mode byte) into a 30-slot
/// table at `[DAT_00503c18 + 0x187f4d8]`.
/// Spin flag: `[anim+0x12c]`. Anim driver: `fcn.004865f0`. Join helper: `fcn.00403f30`.
/// Handle slot: `[this+0x10]`.
const FCN_0044BED0: usize = 0x0044_bed0;
static FCN_0044BED0_AFTER_PROLOGUE: usize = FCN_0044BED0 + 5;

/// Replays the displaced 5-byte prologue (`push ebp; mov ebp, esp; push -1`) and resumes past the splice.
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

pub(crate) unsafe fn install_destructor_hook() {
    unsafe {
        destructor_pump::install(destructor_pump::Config {
            dtor_addr: FCN_0044BED0,
            hook: Hook::EbpFrameThiscall(fcn_0044bed0_trampoline),
            anim_driver_addr: 0x0048_65f0,
            loader_handle_offset: 0x10,
            dtor_label: "fcn.0044bed0",
        });
    }
}

/// Fixes a race between the main thread and the BGM-init / I/O loader threads.
///
/// `LOADER_SIGNAL` semantics for th15: non-zero releases the post-load waits
/// in BGM and I/O thread procs. `2` (written by `io_error_abort_trampoline`)
/// is the only escape from the inner `cmp [signal], 2` exit at the top of
/// the I/O loop at `fcn.00475970`.
const LOADER_SIGNAL: usize = 0x0052_1344;
const IO_ERROR_SPLICE: usize = 0x0047_59bf;
const IO_ERROR_DISPLACED_LEN: usize = 7;
static IO_ERROR_AFTER_SPLICE: usize = IO_ERROR_SPLICE + IO_ERROR_DISPLACED_LEN;

/// Splices into the I/O thread's error-exit path at `fcn.00475970+0x4f`.
/// The displaced 7-byte `push dword [esi*4 + 0x4cb3c0]` (the filename for the
/// "error : Sound %s" `printf` call) is replayed.
/// The inserted `signal = 2` hits BGM-init's only escape from its NULL-slot busy-wait.
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
