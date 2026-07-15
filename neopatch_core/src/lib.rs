//! neopatch_core: game-agnostic foundation for neopatch.
//!
//! Provides patching, hooking, and trampoline primitives; D3D9/D3D9Ex shims; the frame pacer; per-session logging;
//! crash and watchdog instrumentation; and Win32 process tunables. Game-specific crates depend on this crate
//! and wire game-specific behavior through the registered callbacks documented on each module.

#[cfg(all(not(target_arch = "x86"), not(test), not(doc)))]
compile_error!("neopatch is x86-only");

pub mod config;
pub mod crash;
pub mod d3d9;
pub mod d3dx9;
pub mod dinput8;
pub mod exit_hooks;
pub mod game_addr;
pub mod gdi_caps;
pub mod iat;
pub mod input;
pub mod log;
mod modules;
pub mod pacer;
pub mod patches;
pub mod process;
mod protect;
pub mod replay;
pub mod screenshot;
pub mod thread;
pub mod timer_period;
mod untrusted;
pub mod vtable;
pub mod watchdog;
pub mod window;

/// Match `$v` against a list of `const` identifiers, returning the literal identifier name
/// (via `stringify!`) on hit and `"?"` on miss. This lets the printed name and value share a single source.
macro_rules! match_named {
    ($v:expr, $($name:ident),* $(,)?) => {
        match $v {
            $($name => stringify!($name),)*
            _ => "?",
        }
    };
}
pub(crate) use match_named;

/// Renders an `HRESULT` as `0xNNNNNNNN`.
macro_rules! fmt_hr {
    ($hr:expr) => {
        ::core::format_args!("{:#010x}", $hr.0.cast_unsigned())
    };
}
pub(crate) use fmt_hr;

// Stub; see `build.rs`.
#[cfg(needs_unwind_resume_stub)]
mod unwind_resume_stub {
    use std::ffi::c_void;
    use windows_sys::Win32::System::Threading::ExitProcess;

    #[unsafe(no_mangle)]
    unsafe extern "C" fn _Unwind_Resume(_: *mut c_void) -> ! {
        unsafe { ExitProcess(0xDEAD) }
    }
}
