//! Fixes an anim loader deadlock at scene transition, shared across th11/th12/th13/th14/th15.
//!
//! Each of those games has an `AsciiInf` class (ZUN's ASCII/text renderer) whose constructor
//! spawns a worker thread (via `_beginthreadex`) to preload the game's global UI anims in the
//! background. The worker calls a `preloadAnim` function for various `.anm` assets.
//! The destructor must join that worker before freeing the buffers it's still writing into.
//!
//! The worker calls `preloadAnim`, which sets a "loading" flag at `[anim+0x128]` (th11/th12)
//! or `[anim+0x12c]` (th13/th14/th15) and spins waiting for it to clear. The main thread's
//! per-frame loop advances the flag by calling the anim driver during the state-tick;
//! the driver iterates the slot table, processes one work-step per call, and eventually clears
//! the flag, freeing the worker.
//!
//! When the destructor runs on main, the per-frame loop is paused. The driver stops running,
//! the flag never advances, the worker never exits, and the destructor's join helper (which
//! polls the handle with `WaitForSingleObject(handle, 200ms)` + `Sleep(1)` until signaled)
//! spins forever. Deadlock!
//!
//! We fix this by pumping the anim driver inline from within the destructor, substituting for
//! the paused per-frame tick. We hook at the destructor's entrypoint so every call site is covered.
//! The hook reads the worker `HANDLE` from the destructor's `this`, drives the driver from main
//! until `WaitForSingleObject` reports the worker has exited, then tail-calls the per-game
//! trampoline which replays the displaced prologue and resumes the destructor body unchanged.
//!
//! Each `preloadAnim` call sets the flag once and waits, and each driver call clears at most
//! one slot's flag, so pumping is guaranteed to terminate.
//!
//! The destructor's calling convention varies by game; see [`Hook`]. Game-specific crates
//! should provide a naked-asm trampoline that replays the displaced prologue and tail-jumps
//! past the splice; [`install`] derives the expected prologue bytes from the `Hook` variant.

use crate::patches::patch_jmp;
use crate::vtable::parse_fn_ptr;
use std::ffi::c_void;
use std::ptr::{null_mut, with_exposed_provenance_mut};
use std::sync::OnceLock;
use tracing::{info, warn};
use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::WaitForSingleObject;

/// Per-game (ABI, prologue structure) pairing for the destructor entry hook.
#[non_exhaustive]
pub enum Hook {
    /// Ebp-frame prologue `push ebp; mov ebp, esp; push -1` (5 bytes);
    /// thiscall (`this` in ECX). Used by th14, th15.
    EbpFrameThiscall(unsafe extern "thiscall" fn(*mut c_void) -> i32),
    /// Ebp-frame prologue (5 bytes); thiscall (`this` in ECX) but the factory
    /// pushes one stack arg the destructor cleans with `ret 4`. The trampoline
    /// takes the extra arg so its `ret 4` balances. Used by th17.
    EbpFrameThiscallRet4(unsafe extern "thiscall" fn(*mut c_void, u32) -> i32),
    /// Ebp-frame prologue `push ebp; mov ebp, esp; push -1` (5 bytes);
    /// stdcall (`this` on stack). Used by th13.
    EbpFrameStdcall(unsafe extern "stdcall" fn(*mut c_void) -> i32),
    /// FPO-inlined SEH prologue `push -1; push imm32 (handler)` (7 bytes);
    /// stdcall. The destructor sets up its SEH frame in the prologue
    /// rather than via `push ebp; mov ebp, esp`. Used by th11, th12.
    FpoStdcall {
        trampoline: unsafe extern "stdcall" fn(*mut c_void) -> i32,
        seh_handler: u32,
    },
}

/// Per-game configuration for [`install`]. Addresses are absolute in the host executable.
pub struct Config {
    /// Address of the `AsciiInf` destructor's first byte.
    pub dtor_addr: usize,
    /// Trampoline.
    pub hook: Hook,
    /// Address of the per-game anim driver (`unsafe extern "C" fn() -> i32`),
    /// which iterates the slot table and clears the loader's spin flag.
    pub anim_driver_addr: usize,
    /// Offset on the destructor's `this` where the worker thread `HANDLE` is located.
    /// For th11..th14, the destructor passes `&this[0x10]` to the join helper which reads
    /// `[ecx+4]`, putting the handle at `+0x14`.
    /// th15's destructor passes `&this[0xc]` instead, putting it at `+0x10`.
    pub loader_handle_offset: usize,
    /// Human-readable label for the destructor (e.g., `"fcn.0044bed0"`).
    pub dtor_label: &'static str,
}

struct State {
    hook: Hook,
    anim_driver: unsafe extern "C" fn() -> i32,
    loader_handle_offset: usize,
}

static STATE: OnceLock<State> = OnceLock::new();

/// Encodes the FPO destructor's 7-byte SEH-frame prologue (`push -1; push imm32 (handler)`).
const fn fpo_seh_prologue(seh_handler: u32) -> [u8; 7] {
    let b = seh_handler.to_le_bytes();
    [0x6a, 0xff, 0x68, b[0], b[1], b[2], b[3]]
}

/// Installs the destructor pump for the given game. This function should be called
/// exactly once per process on the render thread before the destructor can fire.
///
/// # Safety
/// `cfg` must be valid for the game.
///
/// # Panics
/// Aborts if called more than once in a process or if `cfg.anim_driver_addr` is 0.
pub unsafe fn install(cfg: Config) {
    let anim_driver: unsafe extern "C" fn() -> i32 =
        parse_fn_ptr(with_exposed_provenance_mut(cfg.anim_driver_addr))
            .expect("anim_driver_addr is a non-zero constant");
    let Config {
        dtor_addr,
        hook,
        anim_driver_addr: _,
        loader_handle_offset,
        dtor_label,
    } = cfg;
    // We can't verify alignment of `this` at installation since the object doesn't exist yet,
    // but we can ensure the offset preserves alignment of a properly-aligned `this`.
    assert!(
        loader_handle_offset.is_multiple_of(align_of::<HANDLE>()),
        "loader_handle_offset {loader_handle_offset:#x} not a multiple of align_of::<HANDLE>() = {}",
        align_of::<HANDLE>(),
    );

    let (entry_hook, entry_label, fpo_seh) = match &hook {
        Hook::EbpFrameThiscall(_) => (pump_entry_thiscall as *mut (), "pump_entry_thiscall", None),
        Hook::EbpFrameThiscallRet4(_) => (
            pump_entry_thiscall_ret4 as *mut (),
            "pump_entry_thiscall_ret4",
            None,
        ),
        Hook::EbpFrameStdcall(_) => (pump_entry_stdcall as *mut (), "pump_entry_stdcall", None),
        Hook::FpoStdcall { seh_handler, .. } => (
            pump_entry_stdcall as *mut (),
            "pump_entry_stdcall",
            Some(*seh_handler),
        ),
    };
    let patch_name = format!("{dtor_label} entry-jmp -> destructor_pump::{entry_label}");

    // We stash `State` before installing the patch so the pump has
    // everything it needs at the moment the entry-jmp goes live.
    STATE
        .set(State {
            hook,
            anim_driver,
            loader_handle_offset,
        })
        .ok()
        .expect("destructor_pump::install called more than once per process");

    // Install the entry-jmp.
    unsafe {
        match fpo_seh {
            None => patch_jmp(
                dtor_addr,
                &[0x55, 0x8b, 0xec, 0x6a, 0xff],
                entry_hook,
                &patch_name,
            ),
            Some(seh) => patch_jmp(dtor_addr, &fpo_seh_prologue(seh), entry_hook, &patch_name),
        }
    }
}

unsafe extern "thiscall" fn pump_entry_thiscall(this: *mut c_void) -> i32 {
    unsafe { pump(this) }
}

unsafe extern "thiscall" fn pump_entry_thiscall_ret4(this: *mut c_void, _flags: u32) -> i32 {
    unsafe { pump(this) }
}

unsafe extern "stdcall" fn pump_entry_stdcall(this: *mut c_void) -> i32 {
    unsafe { pump(this) }
}

/// Shared pump body. Reads the worker handle from `this`, drives the anim driver until
/// the worker exits (or the wait fails), then calls the per-game trampoline which replays
/// the prologue and jumps to the destructor body.
///
/// # Safety
/// Must be entered via one of the `pump_entry_*` functions,
/// which guarantee the stack/regs match the destructor's expected entry.
/// [`install`] must have been called before this function is reachable.
unsafe fn pump(this: *mut c_void) -> i32 {
    let state = STATE
        .get()
        .expect("destructor_pump::install must run first");

    // SAFETY: The worker writes the `HANDLE` slot via a plain mov in `AsciiInf::start`
    // after `_beginthreadex` returns; we read it via `read_volatile`. The slot is
    // dword-aligned in every supported game (offset 0x10 or 0x14 from a 4-byte-aligned `this`),
    // so the cast through `*u8` is sound.
    let loader_handle: HANDLE = if this.is_null() {
        null_mut()
    } else {
        #[allow(clippy::cast_ptr_alignment)]
        unsafe {
            this.cast::<u8>()
                .add(state.loader_handle_offset)
                .cast::<HANDLE>()
                .read_volatile()
        }
    };

    info!(
        kind = "destructor_entered",
        this = format_args!("{this:p}"),
        loader_handle = format_args!("{loader_handle:p}"),
    );

    // We pump the anim driver from main until the loader exits. The initial probe
    // with `timeout = 0` short-circuits the pump entirely in the no-bug case
    // where the worker already exited by the time we got here.
    if !loader_handle.is_null() {
        let mut pump_iters: u32 = 0;
        let drained = loop {
            let timeout_ms = u32::from(pump_iters != 0);
            match unsafe { WaitForSingleObject(loader_handle, timeout_ms) } {
                WAIT_OBJECT_0 => break true,
                WAIT_TIMEOUT => {
                    unsafe { (state.anim_driver)() };
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

    let result = unsafe {
        match state.hook {
            Hook::EbpFrameThiscall(tramp) => tramp(this),
            Hook::EbpFrameThiscallRet4(tramp) => tramp(this, 0),
            Hook::EbpFrameStdcall(tramp) => tramp(this),
            Hook::FpoStdcall { trampoline, .. } => trampoline(this),
        }
    };
    info!(
        kind = "destructor_returned",
        this = format_args!("{this:p}"),
        result,
    );

    result
}

#[cfg(test)]
mod tests {
    use super::fpo_seh_prologue;

    #[test]
    fn fpo_seh_prologue_encodes_push_minus_1_then_push_imm32_le() {
        // th11 and th12's actual SEH handler addresses.
        assert_eq!(
            fpo_seh_prologue(0x0048_a686),
            [0x6a, 0xff, 0x68, 0x86, 0xa6, 0x48, 0x00],
            "th11 SEH handler",
        );
        assert_eq!(
            fpo_seh_prologue(0x0049_7336),
            [0x6a, 0xff, 0x68, 0x36, 0x73, 0x49, 0x00],
            "th12 SEH handler",
        );
        // Byte 0..3 is the fixed `push -1; push imm32` opcode prefix.
        // Byte 3..7 is the input's little-endian encoding.
        for h in [0u32, 1, 0x1234_5678, 0xabab_abab, 0xffff_ffff] {
            let p = fpo_seh_prologue(h);
            assert_eq!(&p[0..3], &[0x6a, 0xff, 0x68], "opcode prefix for {h:#x}");
            assert_eq!(&p[3..7], &h.to_le_bytes(), "imm32 LE for {h:#x}");
        }
    }
}
