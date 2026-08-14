//! Utilities for walking a loaded module's import directory and replacing IAT slots.
//!
//! [`IatHook<F>`] carries the import's function-pointer type `F` through the install/capture/call chain.
//! The trampoline calls the captured original directly without transmuting.

use crate::log::log_at;
use crate::modules::annotate_addr;
use crate::protect::with_writable;
use crate::vtable::{FnSlot, SlotStatus, fn_ptr_to_raw, raw_to_fn_ptr};
use std::ffi::CStr;
use std::mem::offset_of;
use std::ptr::{NonNull, null_mut, read_unaligned, write_unaligned};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
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

/// The highest number of installs that [`audit_installed`] can cover.
const MAX_TRACKED: usize = 32;

/// Every successful install's record in installation order.
static INSTALLED: Registry = Registry {
    entries: [const { OnceLock::new() }; MAX_TRACKED],
    next: AtomicUsize::new(0),
};

/// Limits [`audit_installed`] to one report per session.
static AUDITED: AtomicBool = AtomicBool::new(false);

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
///
/// A `fallback <path>` clause after the import name makes the trampoline call `<path>` if the import walk never captured anything,
/// instead of panicking:
///
/// ```ignore
/// iat_hook! {
///     REAL_DIALOG_BOX_PARAM_A / real_dialog_box_param_a : "DialogBoxParamA" fallback DialogBoxParamA
///         as fn(hinst: HMODULE, template: PCSTR, parent: HWND, proc: DLGPROC, param: LPARAM) -> isize;
/// }
/// ```
///
/// This should be used when the hook can be reached without [`IatHook::install`] having succeeded.
/// For example, a byte-patched call site rewritten to call the hook directly stays reachable even if the import walk missed.
#[macro_export]
macro_rules! iat_hook {
    (
        $real:ident / $trampoline:ident : $name:literal
            as fn($($arg:ident : $argty:ty),* $(,)?) -> $ret:ty;
    ) => {
        $crate::iat_hook!(@decl $real / $trampoline : $name as fn($($arg : $argty),*) -> $ret);

        #[inline]
        #[allow(dead_code, clippy::too_many_arguments)]
        unsafe fn $trampoline($($arg : $argty),*) -> $ret {
            let f = $real.original();
            unsafe { f($($arg),*) }
        }
    };
    (
        $real:ident / $trampoline:ident : $name:literal fallback $fallback:path
            as fn($($arg:ident : $argty:ty),* $(,)?) -> $ret:ty;
    ) => {
        $crate::iat_hook!(@decl $real / $trampoline : $name as fn($($arg : $argty),*) -> $ret);

        #[inline]
        #[allow(dead_code, clippy::too_many_arguments)]
        unsafe fn $trampoline($($arg : $argty),*) -> $ret {
            match $real.try_original() {
                Some(f) => unsafe { f($($arg),*) },
                None => unsafe { $fallback($($arg),*) },
            }
        }
    };
    (
        @decl $real:ident / $trampoline:ident : $name:literal
            as fn($($arg:ident : $argty:ty),* $(,)?) -> $ret:ty
    ) => {
        static $real: $crate::iat::IatHook<
            unsafe extern "system" fn($($argty),*) -> $ret,
        > = $crate::iat::IatHook::new($name, stringify!($real));
    };
}

/// The result of one [`IatHook::install`].
struct InstallRecord {
    /// The import name.
    name: &'static str,
    /// The `FirstThunk` slot we wrote, or null until an install writes one.
    slot: AtomicPtr<*mut ()>,
    /// The hook we wrote there.
    hook: AtomicPtr<()>,
}

impl InstallRecord {
    const fn new(name: &'static str) -> Self {
        Self {
            name,
            slot: AtomicPtr::new(null_mut()),
            hook: AtomicPtr::new(null_mut()),
        }
    }

    /// Records the slot that `hook` was written into if the record is currently empty, else does nothing.
    /// Returns whether this record was still empty.
    fn set(&self, slot: *mut *mut (), hook: *mut ()) -> bool {
        // `hook` is stored first so that a reader which acquires a non-null `slot` also sees it.
        self.hook.store(hook, Ordering::Relaxed);
        self.slot
            .compare_exchange(null_mut(), slot, Ordering::Release, Ordering::Relaxed)
            .is_ok()
    }

    /// Rereads the recorded slot. Returns the hook we wrote alongside what the slot holds instead,
    /// or `None` while the game still reaches us through it. `None` is also returned for records that no install ever wrote.
    fn displaced(&self) -> Option<(*mut (), *mut ())> {
        let slot = self.slot.load(Ordering::Acquire);
        if slot.is_null() {
            return None;
        }
        let ours = self.hook.load(Ordering::Relaxed);
        // SAFETY: A non-null `slot` is only published by `install` after it writes a `FirstThunk` slot inside the host image.
        // The host image outlives the process, so the slot stays mapped and readable.
        let seen = unsafe { read_unaligned(slot) };
        (seen != ours).then_some((ours, seen))
    }
}

struct Registry {
    entries: [OnceLock<&'static InstallRecord>; MAX_TRACKED],
    next: AtomicUsize,
}

impl Registry {
    fn record(&self, entry: &'static InstallRecord) {
        let Ok(idx) = self
            .next
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_TRACKED).then_some(n + 1)
            })
        else {
            warn!(
                kind = "hook_slot",
                via = "iat",
                name = entry.name,
                status = %SlotStatus::PoolFull,
            );
            return;
        };
        // A fresh index is claimed exactly once, so its entry is necessarily empty.
        _ = self.entries[idx].set(entry);
    }

    /// Returns the records published so far.
    fn recorded(&self) -> impl Iterator<Item = &'static InstallRecord> {
        let tracked = self.next.load(Ordering::Acquire).min(MAX_TRACKED);
        self.entries[..tracked]
            .iter()
            .filter_map(|entry| entry.get().copied())
    }
}

/// Rereads every slot written by [`IatHook::install`] and reports the ones that no longer hold our hook.
///
/// We install during `DllMain`, which is the earliest we can run and therefore also the easiest position to be overwritten from.
/// A tool that initializes later can rebind the same slots and stop our hook from being called.
/// This should be called from game code so the read happens after any such injection.
///
/// `Direct3DCreate8` / `Direct3DCreate9` is expected to report as displaced whenever another tool detours it, but this is benign.
/// Games reach our hook through game-specific call-site patches which can't be displaced, unlike imports.
/// Also, every supported game reaches its call site before creating its window.
/// In this situation, the report means the other tool's own Direct3D hook is the one being bypassed.
pub(crate) fn audit_installed() {
    if AUDITED.swap(true, Ordering::Relaxed) {
        return;
    }

    let mut tracked = 0u32;
    let mut displaced = 0u32;

    for entry in INSTALLED.recorded() {
        tracked += 1;
        let Some((ours, seen)) = entry.displaced() else {
            continue;
        };

        displaced += 1;
        #[allow(clippy::cast_possible_truncation)]
        let owner = annotate_addr(seen.addr() as u32);
        let owner = owner.as_deref().unwrap_or("");
        warn!(
            kind = "hook_slot",
            via = "iat",
            name = entry.name,
            status = %SlotStatus::Displaced,
            ours = format_args!("{:#010x}", ours.addr()),
            seen = format_args!("{:#010x}", seen.addr()),
            owner,
        );
    }

    log_at!(displaced == 0 => info / warn,
        kind = "iat_audit",
        tracked,
        displaced,
    );
}

/// Set-once-with-non-null storage for an IAT hook's import name and displaced original pointer. Use through [`iat_hook!`].
pub struct IatHook<F> {
    slot: FnSlot<F>,
    record: InstallRecord,
}

// TODO: Tighten to `F: FnPtr` if the `fn_ptr_trait` feature stabilizes.
impl<F: Copy + Send + Sync + Unpin + 'static> IatHook<F> {
    #[must_use]
    pub const fn new(name: &'static str, slot_name: &'static str) -> Self {
        Self {
            slot: FnSlot::new(slot_name),
            record: InstallRecord::new(name),
        }
    }

    /// The import name targeted by this hook.
    const fn name(&self) -> &'static str {
        self.record.name
    }

    /// Reads the captured original.
    ///
    /// # Panics
    /// Panics if `install` was never called or returned without capturing the slot.
    pub fn original(&self) -> F {
        self.slot
            .try_get()
            .unwrap_or_else(|| panic!("IAT hook {:?} not installed", self.name()))
    }

    /// Reads the captured original, returning `None` if `install` never captured the slot.
    /// For fallback paths that must not panic; hook trampolines use [`Self::original`].
    pub fn try_original(&self) -> Option<F> {
        self.slot.try_get()
    }

    /// Walks `host`'s IAT, displaces the slot, and captures the original. Returns `true` on hit and `false` on failure.
    /// Failures are logged. The trampoline panics on first call if the slot was never captured.
    ///
    /// # Safety
    /// `host` must be a loaded module handle.
    pub unsafe fn install(&'static self, host: HMODULE, hook: F) -> bool {
        let hook_raw = unsafe { fn_ptr_to_raw(hook) };
        let slot_ptr = match unsafe { find_iat_slot(host, self.name()) } {
            SlotOutcome::Hit { ptr } => ptr,
            SlotOutcome::Miss { descriptors } => {
                info!(
                    kind = "hook_slot",
                    via = "iat",
                    name = self.name(),
                    status = %SlotStatus::NotImported,
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
            info!(
                kind = "hook_slot",
                via = "iat",
                name = self.name(),
                status = %SlotStatus::AlreadyOurs,
            );
            return true;
        }

        let Some(original) = (unsafe { raw_to_fn_ptr(current_raw) }) else {
            warn!(
                kind = "hook_slot",
                via = "iat",
                name = self.name(),
                status = %SlotStatus::NullSlot,
            );
            return false;
        };
        // The slot can be already populated by a different pointer if third-party code rebinds the IAT between two installations.
        // In this situation, we refuse installation to avoid silently bypassing the layered shim on the next trampoline call.
        if let Some(existing_raw) = self.slot.captured_raw() {
            if existing_raw != current_raw {
                warn!(
                    kind = "hook_slot",
                    via = "iat",
                    name = self.name(),
                    status = %SlotStatus::Refused,
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
            if self.record.set(slot_raw, hook_raw) {
                INSTALLED.record(&self.record);
            } else {
                warn!(
                    kind = "iat_record_conflict",
                    name = self.name(),
                    kept = format_args!("{:p}", self.record.slot.load(Ordering::Acquire)),
                    seen = format_args!("{slot_raw:p}"),
                );
            }
            info!(
                kind = "hook_slot",
                via = "iat",
                name = self.name(),
                status = %SlotStatus::Installed,
            );
            true
        } else {
            warn!(
                kind = "hook_slot",
                via = "iat",
                name = self.name(),
                status = %SlotStatus::ProtectFailed,
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

#[cfg(test)]
mod tests {
    use super::{InstallRecord, MAX_TRACKED, Registry};
    use std::ptr::without_provenance_mut;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const OUR_HOOK: usize = 0x3000_1000; // stand-in for a hook in neopatch's image
    const FOREIGN_HOOK: usize = 0x2000_1000; // stand-in for another tool's detour

    /// Returns a record describing `slot` as `install` would leave it after writing `hook` there.
    fn installed_record(slot: *mut *mut (), hook: usize) -> &'static InstallRecord {
        // We leak because the registry indexes records for the process' lifetime. `slot` must outlive every `displaced` call.
        let record = Box::leak(Box::new(InstallRecord::new("TestImport")));
        record.set(slot, without_provenance_mut(hook));
        record
    }

    fn fresh_registry() -> Registry {
        Registry {
            entries: [const { OnceLock::new() }; MAX_TRACKED],
            next: AtomicUsize::new(0),
        }
    }

    #[test]
    fn displacement_follows_live_slot() {
        assert_eq!(InstallRecord::new("NeverInstalled").displaced(), None);

        let mut slot = without_provenance_mut(OUR_HOOK);
        let slot = &raw mut slot;
        let record = installed_record(slot, OUR_HOOK);

        assert_eq!(record.displaced(), None);

        unsafe { slot.write(without_provenance_mut(FOREIGN_HOOK)) };
        let (ours, seen) = record
            .displaced()
            .expect("a rebound slot reads as displaced");
        assert_eq!(ours.addr(), OUR_HOOK);
        assert_eq!(seen.addr(), FOREIGN_HOOK);

        unsafe { slot.write(without_provenance_mut(OUR_HOOK)) };
        assert_eq!(record.displaced(), None);
    }

    #[test]
    fn registry_installation_order() {
        let reg = fresh_registry();
        let mut slot = without_provenance_mut(OUR_HOOK);
        let slot = &raw mut slot;

        for i in 0..MAX_TRACKED + 3 {
            reg.record(installed_record(slot, OUR_HOOK + i));
        }

        // The overflow is dropped rather than wrapping over earlier entries.
        assert_eq!(reg.next.load(Ordering::Acquire), MAX_TRACKED);
        for (i, entry) in reg.entries.iter().enumerate() {
            let hook = entry.get().map(|r| r.hook.load(Ordering::Relaxed).addr());
            assert_eq!(hook, Some(OUR_HOOK + i), "entry {i}");
        }
    }
}
