//! Utilities for walking a loaded module's import directory and replacing IAT slots.
//!
//! `IatHook<F>` carries the import's function-pointer type through the install /
//! capture / call chain. The trampoline calls the captured original directly
//! without transmuting. The install method takes the hook as typed `F`,
//! so a hook with a mismatched signature is a compile error.

use crate::protect::with_writable;
use crate::vtable::{FnSlot, hook_to_raw};
use std::ffi::{CStr, c_char};
use std::mem::offset_of;
use std::ptr::{read_unaligned, write_unaligned};
use tracing::{info, warn};
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::Diagnostics::Debug::{
    IMAGE_DATA_DIRECTORY, IMAGE_DIRECTORY_ENTRY_IMPORT, IMAGE_NT_HEADERS32, IMAGE_OPTIONAL_HEADER32,
};
use windows_sys::Win32::System::SystemServices::{
    IMAGE_DOS_HEADER, IMAGE_DOS_SIGNATURE, IMAGE_IMPORT_DESCRIPTOR, IMAGE_NT_SIGNATURE,
};
use windows_sys::Win32::System::WindowsProgramming::IMAGE_THUNK_DATA32;

/// Declares a typed IAT hook plus a typed trampoline calling through it.
///
/// ```text
/// iat_hook! {
///     REAL_GET_DEVICE_CAPS / real_get_device_caps : c"GetDeviceCaps"
///         as fn(hdc: HDC, index: i32) -> i32;
/// }
/// ```
///
/// The example above expands to a
/// `static REAL_GET_DEVICE_CAPS: IatHook<unsafe extern "system" fn(HDC, i32) -> i32>`
/// plus a typed `real_get_device_caps` trampoline. The signature lives once,
/// in the macro invocation. Hook bodies installed against this slot are typechecked against `F`.
#[macro_export]
macro_rules! iat_hook {
    (
        $real:ident / $trampoline:ident : $cstr:literal
            as fn($($arg:ident : $argty:ty),* $(,)?) -> $ret:ty;
    ) => {
        static $real: $crate::iat::IatHook<
            unsafe extern "system" fn($($argty),*) -> $ret,
        > = $crate::iat::IatHook::new($cstr, stringify!($real));

        #[inline]
        #[allow(dead_code, clippy::too_many_arguments)]
        unsafe fn $trampoline($($arg : $argty),*) -> $ret {
            unsafe { $real.original()($($arg),*) }
        }
    };
}

#[repr(C)]
struct ImageImportByName {
    hint: u16,
    name: [u8; 1],
}

/// Set-once-with-non-null storage for an IAT hook's import name and displaced original pointer.
/// Use through `iat_hook!`. `IatHook::original` panics if `install` was never called or missed,
/// so an uncaptured trampoline fires a named panic instead of dispatching through null.
pub(crate) struct IatHook<F: Copy + Send + Sync + 'static> {
    slot: FnSlot<F>,
    name: &'static CStr,
}

impl<F: Copy + Send + Sync + 'static> IatHook<F> {
    pub(crate) const fn new(name: &'static CStr, slot_name: &'static str) -> Self {
        Self {
            slot: FnSlot::new(slot_name),
            name,
        }
    }

    /// Reads the captured original. Panics if `install` was never called or missed.
    pub(crate) fn original(&self) -> F {
        self.slot
            .try_get()
            .unwrap_or_else(|| panic!("IAT hook {:?} not installed", self.name))
    }

    /// Walks `host`'s IAT, displaces the slot, captures the original.
    /// Returns `true` on hit. Logs OK/MISS so callers don't have to.
    ///
    /// # Safety
    /// `host` must be a loaded module handle.
    pub(crate) unsafe fn install(&self, host: HMODULE, hook: F) -> bool {
        let hook_raw = hook_to_raw(hook);
        let name_str = self.name.to_str().unwrap();
        let Some(slot_ptr) = (unsafe { find_iat_slot(host, self.name) }) else {
            warn!(kind = "iat_hook", name = name_str, status = "MISS");
            return false;
        };
        // Capture-then-write inside the same writable window so a concurrent caller
        // can't observe the new pointer before the original is stored
        // and the trampoline never sees an uncaptured slot.
        let written = unsafe {
            with_writable(slot_ptr.cast::<u8>(), size_of::<*mut ()>(), |_| {
                let old: *mut () = read_unaligned(slot_ptr);
                self.slot.store_raw(old);
                write_unaligned(slot_ptr, hook_raw);
            })
        };
        if written.is_some() {
            info!(kind = "iat_hook", name = name_str, status = "OK");
            true
        } else {
            warn!(kind = "iat_hook", name = name_str, status = "MISS");
            false
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

/// Walks `module`'s import directory (case-insensitive match on `import_name`)
/// and returns a pointer to the `FirstThunk` slot for the hit, or `None`.
/// `module` should always be the game (e.g. th15.exe via `GetModuleHandleW(NULL)`),
/// never our own DLL.
unsafe fn find_iat_slot(module: HMODULE, import_name: &CStr) -> Option<*mut *mut ()> {
    unsafe {
        let (imp_dir, _) = data_directory(module, IMAGE_DIRECTORY_ENTRY_IMPORT as usize)?;
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
                return None;
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
                        // All accesses through the returned pointer occur via `read_unaligned`
                        // and `write_unaligned`, so the alignment bump from `*mut u8` is fine.
                        #[allow(clippy::cast_ptr_alignment)]
                        let slot = base_mut.add(slot_offset).cast::<*mut ()>();
                        return Some(slot);
                    }
                }
                i += 1;
            }
            desc_offset += size_of::<IMAGE_IMPORT_DESCRIPTOR>();
        }
    }
}
