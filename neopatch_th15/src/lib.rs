//! neopatch_th15: latency optimizations, frame pacing, and other fixes for Touhou 15.
//!
//! Shipped as `dinput8.dll` next to `th15.exe`.

// Throughout the codebase, we assume x86.
#[cfg(all(not(target_arch = "x86"), not(test)))]
compile_error!("neopatch_th15 is i686-only");
