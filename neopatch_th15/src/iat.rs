//! Utilities for walking a loaded module's import directory and replacing IAT slots.

use crate::protect::with_writable;
use crate::vtable::FnSlot;
use std::ffi::{CStr, c_char};
use std::mem::offset_of;
use std::ptr::{read_unaligned, write_unaligned};
use std::sync::OnceLock;
use tracing::{info, warn};
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::Diagnostics::Debug::{
    IMAGE_DATA_DIRECTORY, IMAGE_DIRECTORY_ENTRY_IMPORT, IMAGE_NT_HEADERS32, IMAGE_OPTIONAL_HEADER32,
};
use windows_sys::Win32::System::SystemServices::{
    IMAGE_DOS_HEADER, IMAGE_DOS_SIGNATURE, IMAGE_IMPORT_DESCRIPTOR, IMAGE_NT_SIGNATURE,
};
use windows_sys::Win32::System::WindowsProgramming::IMAGE_THUNK_DATA32;

/// Declares `static $real: Original` plus a typed `unsafe fn $trampoline(args) -> ret`
/// that calls the displaced pointer. Hook bodies invoke `$trampoline(args)`
/// instead of transmuting at every call site, so the signature is checked once at declaration
/// rather than per call. The ABI is hard-coded to `unsafe extern "system"` (Win32 stdcall).
///
/// ```text
/// iat_hook! {
///     REAL_GET_DEVICE_CAPS / real_get_device_caps : c"GetDeviceCaps"
///         as fn(hdc: HDC, index: i32) -> i32;
/// }
/// ```
#[macro_export]
macro_rules! iat_hook {
    (
        $real:ident / $trampoline:ident : $cstr:literal
            as fn($($arg:ident : $argty:ty),* $(,)?) -> $ret:ty;
    ) => {
        static $real: $crate::iat::Original = $crate::iat::Original::new($cstr);

        #[inline]
        #[allow(dead_code, clippy::too_many_arguments)]
        unsafe fn $trampoline($($arg : $argty),*) -> $ret {
            type __Sig = unsafe extern "system" fn($($argty),*) -> $ret;
            unsafe {
                let f: __Sig = ::std::mem::transmute($real.raw());
                f($($arg),*)
            }
        }
    };
}

#[repr(C)]
struct ImageImportByName {
    hint: u16,
    name: [u8; 1],
}

/// Set-once-with-non-null storage for an IAT hook's import name and the displaced original pointer.
/// Use through `iat_hook!`. `Original::raw` panics if `Original::install` was never called or missed,
/// so an uncaptured trampoline fires a named panic instead of dispatching through null.
pub(crate) struct Original {
    ptr: OnceLock<FnSlot>,
    name: &'static CStr,
}

impl Original {
    pub(crate) const fn new(name: &'static CStr) -> Self {
        Self {
            ptr: OnceLock::new(),
            name,
        }
    }
    pub(crate) fn raw(&self) -> *mut () {
        self.ptr
            .get()
            .unwrap_or_else(|| panic!("IAT hook {:?} not installed", self.name))
            .as_ptr()
    }
    // Logs OK/MISS so per-site install code doesn't have to.
    pub(crate) unsafe fn install(&self, host: HMODULE, hook: *mut ()) -> bool {
        unsafe {
            let name_str = self.name.to_str().unwrap();
            if patch_iat(host, self.name, hook, &self.ptr) {
                info!(kind = "iat_hook", name = name_str, status = "OK");
                true
            } else {
                warn!(kind = "iat_hook", name = name_str, status = "MISS");
                false
            }
        }
    }
}

unsafe fn data_directory(module: HMODULE, idx: usize) -> Option<(*const u8, u32)> {
    unsafe {
        let base = module.cast::<u8>().cast_const();
        let e_magic: u16 = read_unaligned(base.add(offset_of!(IMAGE_DOS_HEADER, e_magic)).cast());
        if e_magic != IMAGE_DOS_SIGNATURE {
            return None;
        }
        // Treat negative `e_lfanew` as malformed rather than wrapping.
        let e_lfanew: i32 = read_unaligned(base.add(offset_of!(IMAGE_DOS_HEADER, e_lfanew)).cast());
        let nt_base = base.add(usize::try_from(e_lfanew).ok()?);
        let signature: u32 = read_unaligned(
            nt_base
                .add(offset_of!(IMAGE_NT_HEADERS32, Signature))
                .cast(),
        );
        if signature != IMAGE_NT_SIGNATURE {
            return None;
        }
        // `offset_of!` only takes literal paths, so the array index is manual.
        let dd_offset = offset_of!(IMAGE_NT_HEADERS32, OptionalHeader)
            + offset_of!(IMAGE_OPTIONAL_HEADER32, DataDirectory)
            + idx * size_of::<IMAGE_DATA_DIRECTORY>();
        let va: u32 = read_unaligned(
            nt_base
                .add(dd_offset + offset_of!(IMAGE_DATA_DIRECTORY, VirtualAddress))
                .cast(),
        );
        let size: u32 = read_unaligned(
            nt_base
                .add(dd_offset + offset_of!(IMAGE_DATA_DIRECTORY, Size))
                .cast(),
        );
        if va == 0 || size == 0 {
            return None;
        }
        Some((base.add(va as usize), size))
    }
}

// Replaces `import_name`'s IAT slot (case-insensitive).
// The displaced original is stored into `out_old` inside the same `with_writable` window
// that writes the hook, so a concurrent caller can't observe the new pointer
// before the store completes. Returns `true` on hit.
// `module` specifies the target. This should always be the game (e.g. th15.exe)
// via `GetModuleHandleW(NULL)`, never `DllMain`'s `hinst` (our own DLL).
unsafe fn patch_iat(
    module: HMODULE,
    import_name: &CStr,
    new_fn: *mut (),
    out_old: &OnceLock<FnSlot>,
) -> bool {
    unsafe {
        let Some((imp_dir, _)) = data_directory(module, IMAGE_DIRECTORY_ENTRY_IMPORT as usize)
        else {
            return false;
        };
        let base_mut = module.cast::<u8>();
        let base = base_mut.cast_const();

        let mut desc_offset: usize = 0;
        loop {
            let dll_name_rva: u32 = read_unaligned(
                imp_dir
                    .add(desc_offset + offset_of!(IMAGE_IMPORT_DESCRIPTOR, Name))
                    .cast(),
            );
            if dll_name_rva == 0 {
                break;
            }

            // `OriginalFirstThunk` holds names and `FirstThunk` holds live pointers.
            // Some loaders strip `OriginalFirstThunk`, so we fall back to `FirstThunk`.
            // The `Anonymous` union aliases OFT.
            let oft: u32 = read_unaligned(
                imp_dir
                    .add(desc_offset + offset_of!(IMAGE_IMPORT_DESCRIPTOR, Anonymous))
                    .cast(),
            );
            let ft: u32 = read_unaligned(
                imp_dir
                    .add(desc_offset + offset_of!(IMAGE_IMPORT_DESCRIPTOR, FirstThunk))
                    .cast(),
            );
            let lookup_rva = if oft != 0 { oft } else { ft };

            let mut i: usize = 0;
            loop {
                let entry: u32 = read_unaligned(
                    base.add(lookup_rva as usize + i * size_of::<IMAGE_THUNK_DATA32>())
                        .cast(),
                );
                if entry == 0 {
                    break;
                }
                let ord_flag: u32 = 0x8000_0000;
                if entry & ord_flag == 0 {
                    // By-name import: `entry` is the RVA of `IMAGE_IMPORT_BY_NAME`.
                    let name_offset = entry as usize + offset_of!(ImageImportByName, name);
                    let name_ptr = base.add(name_offset).cast::<c_char>();
                    let imp_name = CStr::from_ptr(name_ptr).to_bytes();
                    if imp_name.eq_ignore_ascii_case(import_name.to_bytes()) {
                        let slot_offset = ft as usize + i * size_of::<IMAGE_THUNK_DATA32>();
                        let slot_ptr = base_mut.add(slot_offset);
                        return with_writable(slot_ptr, size_of::<*mut ()>(), |_| {
                            let old: *mut () = read_unaligned(base.add(slot_offset).cast());
                            // Set the original before publishing the hook so a caller that races in
                            // observes a populated `out_old` rather than
                            // the panic from an uncaptured read.
                            FnSlot::store_into(out_old, old, &format!("IAT hook {import_name:?}"));
                            write_unaligned(base_mut.add(slot_offset).cast::<*mut ()>(), new_fn);
                        })
                        .is_some();
                    }
                }
                i += 1;
            }
            desc_offset += size_of::<IMAGE_IMPORT_DESCRIPTOR>();
        }
        false
    }
}
