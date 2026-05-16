//! Rust's precompiled standard library for `i686-pc-windows-gnu` references `_Unwind_Resume`,
//! the DWARF/SEH name. If your build host's mingw-w64 toolchain uses SJLJ exceptions
//! (e.g. Homebrew mingw on macOS), specify `NEOPATCH_UNWIND_RESUME_STUB=1` to have builds work.

use std::env::var;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(needs_unwind_resume_stub)");
    println!("cargo:rerun-if-env-changed=NEOPATCH_UNWIND_RESUME_STUB");
    println!("cargo:rerun-if-changed=build.rs");

    let needs_stub = var("NEOPATCH_UNWIND_RESUME_STUB")
        .ok()
        .and_then(|s| parse_bool(&s))
        .unwrap_or(false);

    if needs_stub {
        println!("cargo:rustc-cfg=needs_unwind_resume_stub");
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}
