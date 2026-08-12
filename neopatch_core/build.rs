//! Rust's precompiled standard library for `i686-pc-windows-gnu` references the DWARF/SEH unwind symbol `_Unwind_Resume`.
//! If your build host's mingw-w64 toolchain uses SJLJ exceptions (e.g. Homebrew mingw on macOS),
//! specify `NEOPATCH_UNWIND_RESUME_STUB=1` so the build links.

use std::env::{VarError, var};

const VALUE_ERR_MSG: &str = "unrecognized NEOPATCH_UNWIND_RESUME_STUB value";

fn main() {
    println!("cargo:rustc-check-cfg=cfg(needs_unwind_resume_stub)");
    println!("cargo:rerun-if-env-changed=NEOPATCH_UNWIND_RESUME_STUB");
    println!("cargo:rerun-if-changed=build.rs");

    let needs_stub = match var("NEOPATCH_UNWIND_RESUME_STUB") {
        Err(VarError::NotPresent) => false,
        Err(VarError::NotUnicode(value)) => panic!("{VALUE_ERR_MSG}; got {}", value.display()),
        Ok(value) => parse_bool(&value).unwrap_or_else(|| panic!("{VALUE_ERR_MSG}; got {value:?}")),
    };

    if needs_stub {
        assert_eq!(
            var("CARGO_CFG_TARGET_ARCH").as_deref(),
            Ok("x86"),
            "NEOPATCH_UNWIND_RESUME_STUB is for 32-bit SJLJ mingw hosts only",
        );
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
