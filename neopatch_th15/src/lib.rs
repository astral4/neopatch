//! neopatch_th15: latency optimizations, frame pacing, and other fixes for Touhou 15.
//!
//! Shipped as `dinput8.dll` next to `th15.exe`.
//! Windows's DLL search order makes us load as part of th15's static-import resolution,
//! and `DllMain` runs before any game code. The exported `DirectInput8Create`
//! forwards to the real System32 DLL we load by full path; everything else is hooks.

mod iat;
mod modules;
mod patches;
mod protect;
mod thread;
mod vtable;

use std::ffi::c_void;
use std::mem::transmute;
use std::sync::OnceLock;
use vtable::FnSlot;
use windows_sys::Win32::Foundation::{E_FAIL, HINSTANCE, HMODULE, MAX_PATH};
use windows_sys::Win32::System::LibraryLoader::{
    DisableThreadLibraryCalls, GetProcAddress, LoadLibraryW,
};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::core::{GUID, HRESULT};

// Throughout the codebase, we assume x86.
#[cfg(all(not(target_arch = "x86"), not(test)))]
compile_error!("neopatch_th15 is i686-only");

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

static REAL_DIRECT_INPUT_8_CREATE: OnceLock<FnSlot> = OnceLock::new();

#[unsafe(no_mangle)]
#[allow(non_snake_case, clippy::missing_safety_doc)]
pub unsafe extern "system" fn DllMain(
    hinst: HINSTANCE,
    reason: u32,
    _reserved: *mut c_void,
) -> i32 {
    if reason != DLL_PROCESS_ATTACH {
        return 1;
    }
    unsafe {
        DisableThreadLibraryCalls(hinst as HMODULE);
        // Lets the vtable patcher distinguish "already our hook" (idempotent re-entry)
        // from a shim-layer chain like `apphelp.dll`'s `CreateDevice` hijack.
        vtable::set_our_dll_handle(hinst as HMODULE);
        // We cache the real `DirectInput8Create` first because
        // the proxy export must work even if hook installation fails.
        load_real_dinput8();
        install_hooks();
    }
    1
}

/// Loads by full path so the bare name doesn't resolve back to us
/// via the same DLL search order that put us here.
fn load_real_dinput8() {
    const SUFFIX: [u16; 13] = {
        let s = b"\\dinput8.dll";
        let mut out = [0u16; 13];
        let mut i = 0;
        while i < s.len() {
            assert!(s[i] < 0x80);
            out[i] = s[i] as u16;
            i += 1;
        }
        out
    };
    let mut buf = [0u16; MAX_PATH as usize];
    let len = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), MAX_PATH) };
    if len == 0 || (len as usize) + SUFFIX.len() > buf.len() {
        return;
    }
    let path_end = len as usize;
    buf[path_end..path_end + SUFFIX.len()].copy_from_slice(&SUFFIX);
    let dll = unsafe { LoadLibraryW(buf.as_ptr()) };
    if dll.is_null() {
        return;
    }
    if let Some(f) = unsafe { GetProcAddress(dll, c"DirectInput8Create".as_ptr().cast()) }
        && let Some(nn) = FnSlot::new(f as *mut ())
    {
        // `OnceLock::set` returns `Err` only if already set.
        // This function runs once from `DllMain`, so a second call is a programmer error.
        assert!(
            REAL_DIRECT_INPUT_8_CREATE.set(nn).is_ok(),
            "load_real_dinput8 called twice",
        );
    }
}

/// Proxy export. Forwards to the cached real `DirectInput8Create`.
///
/// # Safety
///
/// Called by th15's import resolver (or another caller of `dinput8.dll`'s `DirectInput8Create` export).
/// Pointer arguments must obey the dinput8 export's published contract.
#[unsafe(no_mangle)]
#[allow(non_snake_case)]
pub unsafe extern "system" fn DirectInput8Create(
    hinst: HINSTANCE,
    dw_version: u32,
    riidltf: *const GUID,
    ppv_out: *mut *mut c_void,
    punk_outer: *mut c_void,
) -> HRESULT {
    type F = unsafe extern "system" fn(
        HINSTANCE,
        u32,
        *const GUID,
        *mut *mut c_void,
        *mut c_void,
    ) -> HRESULT;
    let Some(cached) = REAL_DIRECT_INPUT_8_CREATE.get() else {
        return E_FAIL;
    };
    let real: F = unsafe { transmute::<*mut (), F>(cached.as_ptr()) };
    unsafe { real(hinst, dw_version, riidltf, ppv_out, punk_outer) }
}

unsafe fn install_hooks() {
    unsafe {
        // `DllMain` runs on the `LoadLibrary` caller.
        // For a static-imported, DLL this is the process' main thread.
        thread::set_main_id(GetCurrentThreadId());
    }
}

// This is a stub for SJLJ-built mingw-w64 toolchains whose `libgcc_eh.a`
// doesn't provide `_Unwind_Resume`. Rust's precompiled standard library
// for `i686-pc-windows-gnu` still references it. See `build.rs` for more details.
// The body is unreachable at runtime since we have `panic = "abort"`.
#[cfg(needs_unwind_resume_stub)]
mod unwind_resume_stub {
    use std::ffi::c_void;
    use windows_sys::Win32::System::Threading::ExitProcess;

    #[unsafe(no_mangle)]
    unsafe extern "C" fn _Unwind_Resume(_: *mut c_void) -> ! {
        unsafe { ExitProcess(0xDEAD) }
    }
}
