//! neopatch_core: game-agnostic foundation for neopatch.
//!
//! Provides patching, hooking, and trampoline primitives; D3D9/D3D9Ex shims; the frame pacer;
//! per-session logging; crash and watchdog instrumentation; and Win32 process tunables.
//! Game-specific crates depend on this crate and wire game-specific behavior
//! through the registered callbacks documented on each module.

pub mod config;
pub mod crash;
pub mod d3d9;
pub mod d3dx9;
pub mod exit_hooks;
pub mod game_addr;
pub mod gdi_caps;
pub mod iat;
pub mod log;
pub mod modules;
pub mod pacer;
pub mod patches;
pub mod process;
pub mod protect;
pub mod thread;
pub mod timer_period;
pub mod untrusted;
pub mod vtable;
pub mod watchdog;
pub mod window;

/// Match `$v` against a list of `const` identifiers, returning the literal identifier name
/// (via `stringify!`) on hit and `"?"` on miss. This lets the printed name and value
/// share a single source.
#[macro_export]
macro_rules! match_named {
    ($v:expr, $($name:ident),* $(,)?) => {
        match $v {
            $($name => stringify!($name),)*
            _ => "?",
        }
    };
}
