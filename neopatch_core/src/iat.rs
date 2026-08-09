//! Utilities for walking a loaded module's import directory and replacing IAT slots.
//!
//! [`IatHook<F>`] carries the import's function-pointer type `F` through the install/capture/call chain.
//! The trampoline calls the captured original directly without transmuting.

use crate::protect::with_writable;
use crate::vtable::{FnSlot, fn_ptr_to_raw, raw_to_fn_ptr};
use std::ffi::CStr;
use std::mem::offset_of;
use std::ptr::{NonNull, read_unaligned, write_unaligned};
use tracing::{info, warn};
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::Diagnostics::Debug::{
    IMAGE_DATA_DIRECTORY, IMAGE_DIRECTORY_ENTRY_IMPORT, IMAGE_NT_HEADERS32, IMAGE_OPTIONAL_HEADER32,
};
use windows_sys::Win32::System::SystemServices::{
    IMAGE_DOS_HEADER, IMAGE_DOS_SIGNATURE, IMAGE_IMPORT_BY_NAME, IMAGE_IMPORT_DESCRIPTOR,
    IMAGE_NT_SIGNATURE, IMAGE_ORDINAL_FLAG32,
};
use windows_sys::Win32::System::WindowsProgramming::IMAGE_THUNK_DATA32;

/// Declares a typed IAT hook and a typed trampoline calling through it.
///
/// ```ignore
/// iat_hook! {
///     REAL_GET_DEVICE_CAPS / real_get_device_caps : "GetDeviceCaps"
///         as fn(hdc: HDC, index: i32) -> i32;
/// }
/// ```
///
/// The example above expands to `static REAL_GET_DEVICE_CAPS: IatHook<unsafe extern "system" fn(HDC, i32) -> i32>`
/// and a typed `real_get_device_caps` trampoline. Hook bodies installed against this slot are typechecked.
#[macro_export]
macro_rules! iat_hook {
    (
        $real:ident / $trampoline:ident : $name:literal
            as fn($($arg:ident : $argty:ty),* $(,)?) -> $ret:ty;
    ) => {
        static $real: $crate::iat::IatHook<
            unsafe extern "system" fn($($argty),*) -> $ret,
        > = $crate::iat::IatHook::new($name, stringify!($real));

        #[inline]
        #[allow(dead_code, clippy::too_many_arguments)]
        unsafe fn $trampoline($($arg : $argty),*) -> $ret {
            let f = $real.original();
            unsafe { f($($arg),*) }
        }
    };
}

/// Set-once-with-non-null storage for an IAT hook's import name and displaced original pointer. Use through [`iat_hook!`].
pub struct IatHook<F> {
    slot: FnSlot<F>,
    name: &'static str,
}

// TODO: Tighten to `F: FnPtr` if the `fn_ptr_trait` feature stabilizes.
impl<F: Copy + Send + Sync + Unpin + 'static> IatHook<F> {
    #[must_use]
    pub const fn new(name: &'static str, slot_name: &'static str) -> Self {
        Self {
            slot: FnSlot::new(slot_name),
            name,
        }
    }

    /// Reads the captured original.
    ///
    /// # Panics
    /// Panics if `install` was never called or returned without capturing the slot.
    pub fn original(&self) -> F {
        self.slot
            .try_get()
            .unwrap_or_else(|| panic!("IAT hook {:?} not installed", self.name))
    }

    /// Walks `host`'s IAT, displaces the slot, and captures the original. Returns `true` on hit and `false` on failure.
    /// Failures are logged. The trampoline panics on first call if the slot was never captured.
    ///
    /// # Safety
    /// `host` must be a loaded module handle.
    pub unsafe fn install(&self, host: HMODULE, hook: F) -> bool {
        let hook_raw = unsafe { fn_ptr_to_raw(hook) };
        let slot_ptr = match unsafe { find_iat_slot(host, self.name) } {
            SlotOutcome::Hit { ptr } => ptr,
            SlotOutcome::Miss { descriptors } => {
                info!(
                    kind = "iat_hook",
                    name = self.name,
                    status = "NOT_IMPORTED",
                    descriptors,
                );
                return false;
            }
        };
        let slot_raw = slot_ptr.as_ptr();
        // `install` runs during `DllMain(PROCESS_ATTACH)` while the process is effectively single-threaded.
        // The first call through the slot comes from game code after `DllMain` returns, so plain reads and writes don't need fences.
        let current_raw = unsafe { read_unaligned(slot_raw) };
        if current_raw == hook_raw && self.slot.try_get().is_some() {
            info!(kind = "iat_hook", name = self.name, status = "IDEMPOTENT");
            return true;
        }

        let Some(original) = (unsafe { raw_to_fn_ptr(current_raw) }) else {
            warn!(kind = "iat_hook", name = self.name, status = "NULL_SLOT");
            return false;
        };
        // The slot can be already populated by a different pointer if third-party code rebinds the IAT between two installations.
        // In this situation, we refuse installation to avoid silently bypassing the layered shim on the next trampoline call.
        if let Some(existing_raw) = self.slot.captured_raw() {
            if existing_raw != current_raw {
                warn!(
                    kind = "iat_hook",
                    name = self.name,
                    status = "RECAPTURE_DIVERGENT",
                    kept = format_args!("{existing_raw:p}"),
                    seen = format_args!("{current_raw:p}"),
                );
                return false;
            }
        } else {
            self.slot.store(original);
        }

        let written = unsafe {
            with_writable(slot_raw.cast::<u8>(), size_of::<*mut ()>(), |_| {
                write_unaligned(slot_raw, hook_raw);
            })
        };
        if written.is_some() {
            info!(kind = "iat_hook", name = self.name, status = "OK");
            true
        } else {
            warn!(
                kind = "iat_hook",
                name = self.name,
                status = "PROTECT_FAILED"
            );
            false
        }
    }
}

unsafe fn data_directory(module: HMODULE, idx: usize) -> Option<(*const u8, u32)> {
    let base = module.cast::<u8>().cast_const();
    let e_magic: u16 =
        unsafe { read_unaligned(base.add(offset_of!(IMAGE_DOS_HEADER, e_magic)).cast()) };
    if e_magic != IMAGE_DOS_SIGNATURE {
        return None;
    }
    // We treat negative `e_lfanew` as malformed rather than wrapping.
    let e_lfanew: i32 =
        unsafe { read_unaligned(base.add(offset_of!(IMAGE_DOS_HEADER, e_lfanew)).cast()) };
    let e_lfanew = usize::try_from(e_lfanew).ok()?;
    let nt_base = unsafe { base.add(e_lfanew) };
    let signature: u32 = unsafe {
        read_unaligned(
            nt_base
                .add(offset_of!(IMAGE_NT_HEADERS32, Signature))
                .cast(),
        )
    };
    if signature != IMAGE_NT_SIGNATURE {
        return None;
    }
    // `offset_of!` only takes literal paths, so the array index is manual.
    let dd_offset = offset_of!(IMAGE_NT_HEADERS32, OptionalHeader)
        + offset_of!(IMAGE_OPTIONAL_HEADER32, DataDirectory)
        + idx * size_of::<IMAGE_DATA_DIRECTORY>();
    let dir_rva: u32 = unsafe {
        read_unaligned(
            nt_base
                .add(dd_offset + offset_of!(IMAGE_DATA_DIRECTORY, VirtualAddress))
                .cast(),
        )
    };
    let size: u32 = unsafe {
        read_unaligned(
            nt_base
                .add(dd_offset + offset_of!(IMAGE_DATA_DIRECTORY, Size))
                .cast(),
        )
    };
    if dir_rva == 0 || size == 0 {
        return None;
    }
    Some((unsafe { base.add(dir_rva as usize) }, size))
}

enum SlotOutcome {
    /// Pointer to the `FirstThunk` slot.
    Hit { ptr: NonNull<*mut ()> },
    /// The number of descriptors that couldn't be searched. Nonzero means the miss may be a false negative.
    Miss { descriptors: u32 },
}

/// Walks `module`'s import directory for a case-insensitive match on `import_name`.
/// Returns a pointer to the `FirstThunk` slot on hit, or the count of descriptors that couldn't be searched on miss.
/// `module` must always be the game (e.g. th15.exe via `GetModuleHandleW(NULL)`), never our own DLL.
unsafe fn find_iat_slot(module: HMODULE, import_name: &str) -> SlotOutcome {
    let mut descriptors = 0u32;
    let Some((imp_dir, _)) =
        (unsafe { data_directory(module, IMAGE_DIRECTORY_ENTRY_IMPORT as usize) })
    else {
        return SlotOutcome::Miss { descriptors };
    };
    let base_mut: *mut u8 = module.cast();
    let base = base_mut.cast_const();

    let mut desc_offset = 0;
    loop {
        let dll_name_rva: u32 = unsafe {
            read_unaligned(
                imp_dir
                    .add(desc_offset + offset_of!(IMAGE_IMPORT_DESCRIPTOR, Name))
                    .cast(),
            )
        };
        if dll_name_rva == 0 {
            return SlotOutcome::Miss { descriptors };
        }

        // OFT holds name RVAs (`Anonymous` union aliases it). FT holds bound function VAs after the loader runs.
        // We can't fall back from OFT to FT after binding.
        let oft: u32 = unsafe {
            read_unaligned(
                imp_dir
                    .add(desc_offset + offset_of!(IMAGE_IMPORT_DESCRIPTOR, Anonymous))
                    .cast(),
            )
        };
        let ft: u32 = unsafe {
            read_unaligned(
                imp_dir
                    .add(desc_offset + offset_of!(IMAGE_IMPORT_DESCRIPTOR, FirstThunk))
                    .cast(),
            )
        };
        if oft == 0 || ft == 0 {
            descriptors += 1;
            desc_offset += size_of::<IMAGE_IMPORT_DESCRIPTOR>();
            continue;
        }
        let lookup_rva = oft;

        let mut i = 0;
        loop {
            let entry: u32 = unsafe {
                read_unaligned(
                    base.add(lookup_rva as usize + i * size_of::<IMAGE_THUNK_DATA32>())
                        .cast(),
                )
            };
            if entry == 0 {
                break;
            }
            if entry & IMAGE_ORDINAL_FLAG32 == 0 {
                // By-name import; `entry` is the RVA of `IMAGE_IMPORT_BY_NAME`.
                let name_rva = entry as usize + offset_of!(IMAGE_IMPORT_BY_NAME, Name);
                let name_ptr = unsafe { base.add(name_rva).cast() };
                let imp_name = unsafe { CStr::from_ptr(name_ptr) }.to_bytes();
                if imp_name.eq_ignore_ascii_case(import_name.as_bytes()) {
                    let slot_rva = ft as usize + i * size_of::<IMAGE_THUNK_DATA32>();
                    // All accesses through the returned pointer occur via `read_unaligned` and `write_unaligned`,
                    // so the alignment bump from `*mut u8` is fine.
                    #[allow(clippy::cast_ptr_alignment)]
                    let slot = unsafe { base_mut.add(slot_rva).cast() };
                    return match NonNull::new(slot) {
                        Some(ptr) => SlotOutcome::Hit { ptr },
                        None => SlotOutcome::Miss { descriptors },
                    };
                }
            }
            i += 1;
        }
        desc_offset += size_of::<IMAGE_IMPORT_DESCRIPTOR>();
    }
}
