//! Generic `dinput8.dll` proxy that loads the real System32 export and forwards calls.
//!
//! Game-specific crates that ship as `dinput8.dll` should re-export `DirectInput8Create` via the [`crate::dinput8_export!`] macro
//! to keep the proxy export working even if hook installation fails. The real DLL is resolved on the first forwarded call.

use crate::vtable::raw_to_fn_ptr;
use std::ffi::c_void;
use std::mem::offset_of;
use std::sync::OnceLock;
use tracing::info;
use windows::Win32::Devices::HumanInterfaceDevice::{
    IDirectInput8A, IDirectInput8A_Vtbl, IDirectInput8W, IDirectInput8W_Vtbl,
    IDirectInputDevice8A_Vtbl, IDirectInputDevice8W_Vtbl,
};
use windows::core::{GUID as WinGUID, Interface};
use windows_sys::Win32::Foundation::{E_FAIL, HINSTANCE, MAX_PATH};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::core::{GUID, HRESULT};

// `forward` depends on patched slots (`CreateDevice`, `GetDeviceState`) being at the same vtable offset in both the A and W vtables.
const _: () = {
    assert!(
        offset_of!(IDirectInput8A_Vtbl, CreateDevice)
            == offset_of!(IDirectInput8W_Vtbl, CreateDevice)
    );
    assert!(
        offset_of!(IDirectInputDevice8A_Vtbl, GetDeviceState)
            == offset_of!(IDirectInputDevice8W_Vtbl, GetDeviceState)
    );
};

type DirectInput8CreateFn = unsafe extern "system" fn(
    HINSTANCE,
    u32,
    *const GUID,
    *mut *mut c_void,
    *mut c_void,
) -> HRESULT;

/// System32's `DirectInput8Create`, resolved once on first use.
static REAL: OnceLock<Option<DirectInput8CreateFn>> = OnceLock::new();

/// Optional callback run with the new `IDirectInput8` after each successful `DirectInput8Create`.
/// Set by [`set_on_created`]; first caller wins.
static ON_CREATED: OnceLock<unsafe fn(*mut c_void)> = OnceLock::new();

/// Registers a hook to run after `DirectInput8Create` returns a new `IDirectInput8`; first caller wins.
/// This must be called before any DirectInput call from the game.
pub(crate) fn set_on_created(f: unsafe fn(*mut c_void)) {
    let _ = ON_CREATED.set(f);
}

/// Loads System32's `dinput8.dll` by full path so the bare name doesn't resolve back to us
/// via the same DLL search order that put us here, and returns the real `DirectInput8Create`.
fn load_system_dinput8() -> Option<DirectInput8CreateFn> {
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
        return None;
    }
    let path_end = len as usize;
    buf[path_end..path_end + SUFFIX.len()].copy_from_slice(&SUFFIX);
    let dll = unsafe { LoadLibraryW(buf.as_ptr()) };
    if dll.is_null() {
        return None;
    }
    let proc = unsafe { GetProcAddress(dll, c"DirectInput8Create".as_ptr().cast()) }?;
    unsafe { raw_to_fn_ptr(proc as *mut ()) }
}

/// Forwards to the real `DirectInput8Create`, resolving System32's `dinput8.dll` on the first call.
/// Returns `E_FAIL` if it cannot be resolved. On success, hands the returned `IDirectInput8` to any callback
/// registered via [`set_on_created`]. If no callback is registered, the call simply passes through.
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
    let Some(real) = *REAL.get_or_init(load_system_dinput8) else {
        return E_FAIL;
    };
    let hr = unsafe { real(hinst, dw_version, riidltf, ppv_out, punk_outer) };
    if hr >= 0 && !ppv_out.is_null() && !riidltf.is_null() {
        // SAFETY: `windows_sys::core::GUID` and `windows::core::GUID` are both `#[repr(C)]` with identical fields, so they share layout.
        let iid: WinGUID = unsafe { *riidltf.cast() };
        // This is fine because A/W vtables coincide at the patched slots. th10–th18 provide the A IID; th20 provides the W IID.
        if iid == IDirectInput8A::IID || iid == IDirectInput8W::IID {
            if let Some(on_created) = ON_CREATED.get() {
                let di = unsafe { *ppv_out };
                unsafe { on_created(di) };
            }
        } else {
            info!(
                kind = "dinput8_on_created_skipped",
                reason = "non_idi8_iid",
                iid_data1 = format_args!("{:#010x}", iid.data1),
            );
        }
    }
    hr
}

/// Emits the `DirectInput8Create` export.
#[macro_export]
macro_rules! dinput8_export {
    () => {
        #[unsafe(no_mangle)]
        unsafe extern "system" fn DirectInput8Create(
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
