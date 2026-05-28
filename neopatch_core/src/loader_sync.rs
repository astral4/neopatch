//! I/O loader sync barrier.
//!
//! Some games spawn a BGM-init thread and an I/O loader thread from the main thread
//! before entering the main loop. If a sound consumer fires before both loaders
//! have drained, the game races. We use two hooks to fix this:
//!
//! Hook 1 (call-site rewrite at the first per-frame update call) drains both loaders
//! before forwarding. Once the wrapper has drained on the first call, every subsequent frame
//! hits a single byte compare and tail-jumps straight to the real function.
//!
//! Hook 2 (per-game trampoline) splices `signal = 2` into the I/O thread's
//! error-exit path so a missing asset during installation force-aborts BGM-init
//! instead of leaving it busy-waiting on a NULL slot.
//!
//! The wrapper is naked-asm and ABI-agnostic. It preserves caller-saved registers
//! across the drain call, then tail-jumps to the real function
//! so the original calling convention is preserved.

use crate::patches::{patch_call, patch_jmp};
use std::arch::naked_asm;
use std::ptr::{with_exposed_provenance, with_exposed_provenance_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use tracing::{info, warn};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

/// Signal value written by [`install`] to release the loader threads' post-load busy-waits.
/// Per-game escape semantics are documented at each game's `LOADER_SIGNAL` declaration.
const LOADER_SIGNAL_EXIT_CLEAN: u32 = 1;

/// Signal value written by each game's `io_error_abort_trampoline`
/// to force-abort BGM-init when the I/O loader hits a missing-asset path.
pub const LOADER_SIGNAL_ABORT: u32 = 2;

/// Byte at `cfg.splice_addr` after a successful `patch_jmp`.
/// Used to detect installation refusals which leave the original opcode in place.
const HOOK2_INSTALLED_OPCODE: u8 = 0xe9;

/// Per-game parameters for [`install`]. Addresses are absolute in the host executable.
pub struct Config {
    /// Address of the `dword` flag polled by the game's loader threads.
    pub signal_addr: usize,
    /// Address of the `dword` slot holding the BGM thread's `HANDLE`.
    pub bgm_handle_addr: usize,
    /// Address of the `dword` slot holding the I/O thread's `HANDLE`.
    pub io_handle_addr: usize,
    /// Address of the per-frame update call site to rewrite.
    pub call_site: usize,
    /// 5-byte `E8 disp32` expected at `call_site` before rewriting.
    pub call_bytes: [u8; 5],
    /// Address of the real per-frame update function (the original call target).
    pub real_fn: usize,
    /// Address inside the I/O thread's error-exit path to splice `signal = 2` into.
    pub splice_addr: usize,
    /// 7-byte displaced instruction expected at `splice_addr` before splicing.
    pub splice_expected: [u8; 7],
}

static LOADERS_DRAINED: AtomicBool = AtomicBool::new(false);
static SIGNAL_ADDR: AtomicUsize = AtomicUsize::new(0);
static BGM_HANDLE_ADDR: AtomicUsize = AtomicUsize::new(0);
static IO_HANDLE_ADDR: AtomicUsize = AtomicUsize::new(0);
static REAL_FN: AtomicUsize = AtomicUsize::new(0);

/// ABI-agnostic wrapper installed at each game's per-frame update call site.
///
/// Fast path: byte-compare `LOADERS_DRAINED`; tail-jmp to the real function.
///
/// Slow path (first call only): save EAX/ECX/EDX (volatile; ECX carries `this` for thiscall,
/// ECX/EDX carry args for fastcall); atomic-gate via `xchg byte ptr` (implicit LOCK on x86
/// for memory operands); call `drain_loaders` if we won the race; restore; tail-jmp.
///
/// The tail-jmp passes the caller's stack/registers through to the real function
/// unchanged so stdcall/thiscall/fastcall all work uniformly.
#[unsafe(naked)]
unsafe extern "C" fn updatefast_wrapper() -> ! {
    naked_asm!(
        "cmp byte ptr [{drained}], 0",
        "jne 2f",
        "push eax",
        "push ecx",
        "push edx",
        "mov al, 1",
        "xchg byte ptr [{drained}], al",
        "test al, al",
        "jnz 1f",
        "call {drain}",
        "1:",
        "pop edx",
        "pop ecx",
        "pop eax",
        "2:",
        "jmp dword ptr [{real_fn}]",
        drained = sym LOADERS_DRAINED,
        drain = sym drain_loaders,
        real_fn = sym REAL_FN,
    )
}

extern "C" fn drain_loaders() {
    let signal_addr = SIGNAL_ADDR.load(Ordering::Relaxed);
    let bgm_addr = BGM_HANDLE_ADDR.load(Ordering::Relaxed);
    let io_addr = IO_HANDLE_ADDR.load(Ordering::Relaxed);
    unsafe {
        // We use CAS to preserve a trampoline-written `2`; overwriting this would leave
        // BGM-init busy-waiting on a NULL slot. Handle closure is left to
        // the game's scene-transition teardown, which no-ops on an exited thread.
        //
        // Atomics are technically wrong because the game's BGM and I/O threads write the slot
        // via a plain `mov`. We rely on x86 TSO for aligned dword stores. `compare_exchange`
        // lowers to `lock cmpxchg`, just like MSVC's `_InterlockedCompareExchange` intrinsic.
        let signal = AtomicU32::from_ptr(with_exposed_provenance_mut(signal_addr));
        let _ = signal.compare_exchange(
            0,
            LOADER_SIGNAL_EXIT_CLEAN,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        let bgm = with_exposed_provenance::<HANDLE>(bgm_addr).read_volatile();
        let io = with_exposed_provenance::<HANDLE>(io_addr).read_volatile();
        info!(
            kind = "loader_sync_begin",
            bgm = format_args!("{bgm:p}"),
            io = format_args!("{io:p}"),
            signal = signal.load(Ordering::Acquire),
        );
        if !bgm.is_null() {
            WaitForSingleObject(bgm, INFINITE);
        }
        if !io.is_null() {
            WaitForSingleObject(io, INFINITE);
        }
        info!(kind = "loader_sync_end");
    }
}

/// Installs the loader-sync barrier according to `cfg`.
/// `io_error_trampoline` is the game-specific splice target that writes
/// `LOADER_SIGNAL_ABORT` and replays the per-game displaced instruction.
///
/// # Safety
/// `cfg` addresses must point at the documented sites in the host EXE.
/// This call must run on the same thread that will later execute
/// the patched call site (i.e., from `DllMain` on the render thread).
pub unsafe fn install(cfg: &Config, io_error_trampoline: *mut ()) {
    // These `Relaxed` stores are fine because the wrapper is unreachable until
    // `patch_call` returns, so we have same-thread program order and x86 TSO guarantees.
    SIGNAL_ADDR.store(cfg.signal_addr, Ordering::Relaxed);
    BGM_HANDLE_ADDR.store(cfg.bgm_handle_addr, Ordering::Relaxed);
    IO_HANDLE_ADDR.store(cfg.io_handle_addr, Ordering::Relaxed);
    REAL_FN.store(cfg.real_fn, Ordering::Relaxed);

    unsafe {
        // If installing Hook 2 fails, we skip installing Hook 1
        // so the drain barrier can't deadlock.
        patch_jmp(
            cfg.splice_addr,
            &cfg.splice_expected,
            io_error_trampoline,
            "I/O error -> BGM-init abort",
        );
        let hook2_byte = with_exposed_provenance::<u8>(cfg.splice_addr).read_volatile();
        if hook2_byte != HOOK2_INSTALLED_OPCODE {
            warn!(
                kind = "loader_sync_aborted",
                addr = format_args!("{:#010x}", cfg.splice_addr),
                opcode = format_args!("{hook2_byte:#04x}"),
            );
            return;
        }
        patch_call(
            cfg.call_site,
            &cfg.call_bytes,
            updatefast_wrapper as *mut (),
            "loader sync barrier (main -> per-frame update)",
        );
    }
}
