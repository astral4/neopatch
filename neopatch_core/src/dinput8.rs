//! Generic `dinput8.dll` proxy that loads the real System32 export and forwards calls.
//!
//! Every game crate that ships as `dinput8.dll` should use this to keep the proxy export
//! working even if hook installation fails: call `init` once from `DllMain`
//! and re-export `DirectInput8Create` via the [`dinput8_export!`] macro.

use crate::vtable::{FnSlot, parse_fn_ptr};
use std::ffi::c_void;
use windows_sys::Win32::Foundation::{E_FAIL, HINSTANCE, MAX_PATH};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::core::{GUID, HRESULT};

pub type DirectInput8CreateFn = unsafe extern "system" fn(
    HINSTANCE,
    u32,
    *const GUID,
    *mut *mut c_void,
    *mut c_void,
) -> HRESULT;

static REAL: FnSlot<DirectInput8CreateFn> = FnSlot::new("REAL_DIRECT_INPUT_8_CREATE");

/// Loads System32's `dinput8.dll` by full path so the bare name doesn't resolve back to us
/// via the same DLL search order that put us here, and caches the real `DirectInput8Create`.
/// Idempotent; subsequent calls are no-ops.
pub fn init() {
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
        && let Some(real) = parse_fn_ptr::<DirectInput8CreateFn>(f as *mut ())
    {
        REAL.store(real);
    }
}

/// Forwards to the cached real `DirectInput8Create`. Returns `E_FAIL` if `init` hasn't run
/// or System32's `dinput8.dll` cannot be resolved.
///
/// # Safety
/// The caller must obey the dinput8 export's published contract for the pointer arguments.
pub unsafe fn forward(
    hinst: HINSTANCE,
    dw_version: u32,
    riidltf: *const GUID,
    ppv_out: *mut *mut c_void,
    punk_outer: *mut c_void,
) -> HRESULT {
    let Some(real) = REAL.try_get() else {
        return E_FAIL;
    };
    unsafe { real(hinst, dw_version, riidltf, ppv_out, punk_outer) }
}

/// Emits the `DirectInput8Create` export.
#[macro_export]
macro_rules! dinput8_export {
    () => {
        #[unsafe(no_mangle)]
        #[allow(non_snake_case)]
        pub unsafe extern "system" fn DirectInput8Create(
            hinst: ::windows_sys::Win32::Foundation::HINSTANCE,
            dw_version: u32,
            riidltf: *const ::windows_sys::core::GUID,
            ppv_out: *mut *mut ::std::ffi::c_void,
            punk_outer: *mut ::std::ffi::c_void,
        ) -> ::windows_sys::core::HRESULT {
            unsafe { $crate::dinput8::forward(hinst, dw_version, riidltf, ppv_out, punk_outer) }
        }
    };
}
