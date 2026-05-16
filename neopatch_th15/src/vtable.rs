//! In-place patches to `d3d9.dll`'s `.rdata` vtables.
//!
//! Cloning vtables into heap memory doesn't work because d3d9 dispatches
//! through private virtual slots beyond the typed-struct footprint in the `windows` crate.
//! Reads past the clone will hit uninitialized memory.
//!
//! Slots whose current value points into our own DLL are left alone (idempotent re-entry).
//! Any other value gets chained through, since things like apphelp
//! routinely hijack these slots before we get here.
//!
//! We don't use `FlushInstructionCache` because vtable slots are read as data.

use crate::modules::{ModuleRange, annotate_resolved, module_info, walk_modules};
use crate::protect::with_writable;
use std::mem::transmute;
use std::ptr::{null_mut, read_unaligned, write_unaligned};
use std::sync::OnceLock;
use tracing::{info, warn};
use windows_sys::Win32::Foundation::HMODULE;

/// Declares a function-pointer slot for a captured vtable original plus a typed trampoline
/// that calls through it. Use with `SlotPatch { original: SaveOriginal::Into(&$slot) }`
/// for the patcher to populate it, or with `capture_slot` for slots that are read but never patched.
///
/// Set-once-with-non-null storage: the slot is a `OnceLock<FnSlot>`,
/// so calling the trampoline before capture panics with the slot
/// name, and a second capture panics rather than silently overwriting.
///
/// This is similar to `iat_hook!` except the installer here is `patch_vtable_slots`,
/// not the IAT walker.
#[macro_export]
macro_rules! vtable_slot {
    (
        $slot:ident / $trampoline:ident :
            as fn($($arg:ident : $argty:ty),* $(,)?) -> $ret:ty;
    ) => {
        static $slot: ::std::sync::OnceLock<$crate::vtable::FnSlot> =
            ::std::sync::OnceLock::new();

        #[inline]
        #[allow(dead_code, clippy::too_many_arguments)]
        unsafe fn $trampoline($($arg : $argty),*) -> $ret {
            type __Sig = unsafe extern "system" fn($($argty),*) -> $ret;
            unsafe {
                let ptr = $slot
                    .get()
                    .expect(concat!(
                        "vtable slot `",
                        stringify!($slot),
                        "` not captured",
                    ))
                    .as_ptr();
                let f: __Sig = ::std::mem::transmute(ptr);
                f($($arg),*)
            }
        }
    };
}

// Set exactly once from `DllMain` and read lock-free thereafter.
// We want the OS's authoritative `hinst` rather than guessing
// via `GetModuleHandleW("dinput8.dll")`, which would collide
// with the real `System32\dinput8.dll`.
static OUR_DLL_RANGE: OnceLock<ModuleRange> = OnceLock::new();

/// A function-pointer cell suitable for storage as `static OnceLock<FnSlot>`.
#[derive(Clone, Copy)]
pub(crate) struct FnSlot(unsafe extern "system" fn());

impl FnSlot {
    pub(crate) fn new(p: *mut ()) -> Option<Self> {
        if p.is_null() {
            return None;
        }
        Some(Self(unsafe {
            transmute::<*mut (), unsafe extern "system" fn()>(p)
        }))
    }
    pub(crate) fn as_ptr(self) -> *mut () {
        self.0 as *mut ()
    }

    /// Captures `raw` into `dst`, enforcing the set-once-with-non-null invariant.
    /// Panics if `raw` is null or `dst` was already set, attributing the failure to `label`.
    pub(crate) fn store_into(dst: &OnceLock<FnSlot>, raw: *mut (), label: &str) {
        let nn = FnSlot::new(raw).unwrap_or_else(|| panic!("{label} is null"));
        assert!(dst.set(nn).is_ok(), "{label} captured twice");
    }
}

// Important: `offset` should come from `offset_of!(VtblStruct, field)`
// so a `windows` crate interface change becomes a compile error
// instead of silent slot-mismatch corruption.
pub(crate) struct SlotPatch {
    pub offset: usize,
    pub name: &'static str,
    pub hook: *mut (),
    pub original: SaveOriginal,
}

// Intercepting hooks chain through to the original after pre/post work,
// so the original pointer needs to be stashed. Redirecting hooks translate to
// a different method entirely (e.g. `IDirect3D9::CreateDevice` routes through
// `CreateDeviceEx` via a separately-read slot) and have nothing to chain to.
pub(crate) enum SaveOriginal {
    Into(&'static OnceLock<FnSlot>),
    Discard,
}

#[derive(Clone, Copy)]
enum SlotOutcome {
    Applied,
    /// Already our hook; leaving it alone avoids a self-loop trampoline.
    AlreadyOurs,
    ProtectFailed,
}

pub(crate) fn set_our_dll_handle(hinst: HMODULE) {
    if let Some(range) = module_info(hinst) {
        let _ = OUR_DLL_RANGE.set(range);
    }
}

fn our_dll_range() -> Option<ModuleRange> {
    OUR_DLL_RANGE.get().copied()
}

/// Reads a vtable slot we trampoline through but don't patch (e.g. `CreateDeviceEx`,
/// `ResetEx`, etc.) and publishes the function pointer into `dst`.
/// Returns the captured pointer so the caller can log it if needed.
/// Panics if the slot is null or `dst` was already set.
pub(crate) unsafe fn capture_slot(vtbl: *mut u8, offset: usize, dst: &OnceLock<FnSlot>) -> *mut () {
    let ptr: *mut () = unsafe { read_unaligned(vtbl.add(offset).cast()) };
    FnSlot::store_into(dst, ptr, "vtable slot");
    ptr
}

/// Returns `(applied, total)`. Slots already pointing into our DLL
/// are skipped (idempotency); everything else chains through.
///
/// The "canonical implementation" module (against which chain-through
/// is decided) is derived from the vtable pointer itself: whichever
/// loaded module contains `vtbl` is by definition the module that
/// implements this interface, so we don't need to know its filename.
/// This handles d3d9.dll, a renamed wrapper, or a heap-allocated
/// vtable (the last case lands on `None` and every slot is annotated
/// as chained-through).
pub(crate) unsafe fn patch_vtable_slots(vtbl: *mut u8, patches: &[SlotPatch]) -> (usize, usize) {
    if vtbl.is_null() || patches.is_empty() {
        return (0, patches.len());
    }
    let min_off = patches.iter().map(|p| p.offset).min().unwrap_or(0);
    let max_off = patches.iter().map(|p| p.offset).max().unwrap_or(0);
    let span = max_off - min_off + size_of::<*mut ()>();
    let region_start: *mut u8 = unsafe { vtbl.add(min_off) };

    let modules = walk_modules();
    let our_range = our_dll_range();
    // i686-only (see lib.rs compile_error): pointer == u32, bit-identity.
    #[allow(clippy::cast_possible_truncation)]
    let vtbl_addr = vtbl as u32;
    let expected_range = modules
        .iter()
        .find(|m| m.range.contains(vtbl_addr))
        .map(|m| m.range);

    let mut outcomes: Vec<SlotOutcome> = vec![SlotOutcome::ProtectFailed; patches.len()];
    let mut captured_originals: Vec<*mut ()> = vec![null_mut(); patches.len()];

    let applied_opt = unsafe {
        with_writable(region_start, span, |_| {
            let mut applied = 0usize;
            for (i, p) in patches.iter().enumerate() {
                let slot: *mut *mut () = vtbl.add(p.offset).cast();
                let current: *mut () = read_unaligned(slot);
                captured_originals[i] = current;
                // i686-only (see lib.rs compile_error): pointer == u32, bit-identity.
                #[allow(clippy::cast_possible_truncation)]
                let current_addr = current as u32;

                // Already our hook: don't save our own trampoline as `original`.
                if let Some(ours) = our_range
                    && ours.contains(current_addr)
                {
                    outcomes[i] = SlotOutcome::AlreadyOurs;
                    continue;
                }

                if let SaveOriginal::Into(slot_lock) = &p.original {
                    FnSlot::store_into(slot_lock, current, &format!("slot `{}`", p.name));
                }
                write_unaligned(slot, p.hook);
                outcomes[i] = SlotOutcome::Applied;
                applied += 1;
            }
            applied
        })
    };
    let Some(applied) = applied_opt else {
        warn!(
            kind = "vtable_protect_failed",
            addr = format_args!("{region_start:p}"),
            span,
        );
        return (0, patches.len());
    };

    for (i, p) in patches.iter().enumerate() {
        let slot: *const *mut () = unsafe { vtbl.add(p.offset).cast() };
        let now: *mut () = unsafe { read_unaligned(slot) };
        let original = captured_originals[i];
        // Routine outcomes ([verified], [idempotent, ...]) stay INFO;
        // failure outcomes ([MISMATCH], [protect-failed]) surface at WARN.
        // Tag and severity derive from the same match so adding a variant
        // forces both to be considered.
        let (tag, failed) = match outcomes[i] {
            SlotOutcome::AlreadyOurs => ("[idempotent, already our hook]", false),
            SlotOutcome::ProtectFailed => ("[protect-failed]", true),
            SlotOutcome::Applied if now == p.hook => ("[verified]", false),
            SlotOutcome::Applied => ("[MISMATCH]", true),
        };
        // Chain-through annotation when the original didn't come from d3d9,
        // surfacing the shim layer we're stacked on.
        #[allow(clippy::cast_possible_truncation)]
        let original_u32 = original as u32;
        let chain_through = match outcomes[i] {
            SlotOutcome::Applied if expected_range.is_none_or(|r| !r.contains(original_u32)) => {
                annotate_resolved(original_u32, &modules).map(|s| format!(" chained-through={s}"))
            }
            _ => None,
        };
        let chain_tag = chain_through.as_deref().unwrap_or("");
        let redirect_tag = if matches!(p.original, SaveOriginal::Discard) {
            " [redirector, original discarded]"
        } else {
            ""
        };
        if failed {
            warn!(
                "vtable patch: {} (off {:#x}) old=0x{:08x} new=0x{:08x} {tag}{chain_tag}{redirect_tag}",
                p.name, p.offset, original as usize, p.hook as usize,
            );
        } else {
            info!(
                "vtable patch: {} (off {:#x}) old=0x{:08x} new=0x{:08x} {tag}{chain_tag}{redirect_tag}",
                p.name, p.offset, original as usize, p.hook as usize,
            );
        }
    }

    (applied, patches.len())
}
