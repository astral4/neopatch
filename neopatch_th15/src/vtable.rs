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

use crate::modules::{Module, ModuleRange, annotate_resolved, module_info, walk_modules};
use crate::protect::with_writable;
use std::mem::transmute_copy;
use std::ptr::{read_unaligned, write_unaligned};
use std::sync::OnceLock;
use tracing::{info, warn};
use windows_sys::Win32::Foundation::HMODULE;

/// Declares a typed `static FnSlot<F>` for a vtable slot,
/// optionally with a typed trampoline calling through it.
///
/// - `$slot / $trampoline : as fn(...) -> ret;` emits the slot plus a trampoline.
///   Use for intercepts and capture-only slots.
/// - `$slot : as fn(...) -> ret;` emits the slot alone.
///   Use to anchor `F` at a redirector's call site.
#[macro_export]
macro_rules! vtable_slot {
    (
        $slot:ident :
            as fn($($arg:ident : $argty:ty),* $(,)?) -> $ret:ty;
    ) => {
        static $slot: $crate::vtable::FnSlot<
            unsafe extern "system" fn($($argty),*) -> $ret,
        > = $crate::vtable::FnSlot::new(stringify!($slot));
    };
    (
        $slot:ident / $trampoline:ident :
            as fn($($arg:ident : $argty:ty),* $(,)?) -> $ret:ty;
    ) => {
        $crate::vtable_slot! {
            $slot : as fn($($arg : $argty),*) -> $ret;
        }

        #[inline]
        #[allow(dead_code, clippy::too_many_arguments)]
        unsafe fn $trampoline($($arg : $argty),*) -> $ret {
            unsafe { $slot.get()($($arg),*) }
        }
    };
}

// Set exactly once from `DllMain` and read lock-free thereafter.
// We want the OS's authoritative `hinst` rather than guessing
// via `GetModuleHandleW("dinput8.dll")`, which would collide
// with the real `System32\dinput8.dll`.
static OUR_DLL_RANGE: OnceLock<ModuleRange> = OnceLock::new();

/// A typed function-pointer slot. `F` is the function pointer type.
pub(crate) struct FnSlot<F: Copy + Send + Sync + 'static> {
    slot: OnceLock<F>,
    /// The slot's identifier used for panic messages.
    name: &'static str,
}

impl<F: Copy + Send + Sync + 'static> FnSlot<F> {
    pub(crate) const fn new(name: &'static str) -> Self {
        Self {
            slot: OnceLock::new(),
            name,
        }
    }

    /// Reads the pointer. Panics if the pointer is uncaptured.
    pub(crate) fn get(&self) -> F {
        *self
            .slot
            .get()
            .unwrap_or_else(|| panic!("slot `{}` not captured", self.name))
    }

    pub(crate) fn try_get(&self) -> Option<F> {
        self.slot.get().copied()
    }

    /// Stores a raw function pointer into the slot, reinterpreted as `F`.
    /// Panics on null or double-capture.
    pub(crate) fn store_raw(&self, raw: *mut ()) {
        const { assert!(size_of::<F>() == size_of::<*mut ()>()) };
        assert!(!raw.is_null(), "slot `{}`: null", self.name);
        // SAFETY: `F` is asserted pointer-sized. This is the boundary
        // where the raw IAT/vtable pointer becomes the typed `F`.
        // Soundness depends on `F` being a function-pointer type,
        // which is the only intended use of `FnSlot`.
        let f: F = unsafe { transmute_copy(&raw) };
        assert!(
            self.slot.set(f).is_ok(),
            "slot `{}`: already captured",
            self.name,
        );
    }
}

/// Converts a typed hook into the raw `*mut ()` the patcher writes.
/// Callers must provide `F` as a function pointer, not a function item (ZST).
pub(crate) fn hook_to_raw<F: Copy + 'static>(hook: F) -> *mut () {
    const { assert!(size_of::<F>() == size_of::<*mut ()>()) };
    // SAFETY: `F` is asserted pointer-sized; only function-pointer types are intended here.
    unsafe { transmute_copy(&hook) }
}

pub(crate) fn set_our_dll_handle(hinst: HMODULE) {
    if let Some(range) = module_info(hinst) {
        let _ = OUR_DLL_RANGE.set(range);
    }
}

fn our_dll_range() -> Option<ModuleRange> {
    OUR_DLL_RANGE.get().copied()
}

/// Reads a vtable slot we trampoline through but don't patch
/// (e.g. `CreateDeviceEx`, `ResetEx`) and publishes the function pointer into `dst`.
/// Panics if the slot is null or `dst` was already set.
///
/// # Safety
/// `vtbl` must point to a valid `V`, and the slot reached via `project(vtbl)`
/// must be readable.
pub(crate) unsafe fn capture_slot<F, P, V>(vtbl: *mut V, project: P, dst: &FnSlot<F>)
where
    F: Copy + Send + Sync + 'static,
    P: FnOnce(*mut V) -> *const F,
{
    let slot_ptr: *const F = project(vtbl);
    let raw: *mut () = unsafe { read_unaligned(slot_ptr.cast::<*mut ()>()) };
    dst.store_raw(raw);
}

pub(crate) struct VtblScope<'a, V> {
    vtbl: *mut V,
    modules: &'a [Module],
    our_range: Option<ModuleRange>,
    expected_range: Option<ModuleRange>,
}

impl<V> VtblScope<'_, V> {
    /// Capture the displaced original into `original`
    /// and write `hook` at the slot reached by `project(vtbl)`.
    pub(crate) fn intercept<F>(
        &self,
        original: &FnSlot<F>,
        project: impl FnOnce(*mut V) -> *mut F,
        name: &str,
        hook: F,
    ) where
        F: Copy + Send + Sync + 'static,
    {
        let slot_ptr = project(self.vtbl);
        self.write_slot(slot_ptr, name, hook, Some(original));
    }

    /// Like `intercept`, except the displaced original isn't captured.
    /// `_slot` is only a type anchor, declared via the bare `vtable_slot!` form.
    //
    // TODO: with more redirectors, consider replacing `&FnSlot<F>` here
    // with a dedicated `Sig<F>(PhantomData<F>)` ZST whose only API is type-tagging.
    pub(crate) fn redirect<F>(
        &self,
        _slot: &FnSlot<F>,
        project: impl FnOnce(*mut V) -> *mut F,
        name: &str,
        hook: F,
    ) where
        F: Copy + Send + Sync + 'static,
    {
        let slot_ptr = project(self.vtbl);
        self.write_slot::<F>(slot_ptr, name, hook, None);
    }

    fn write_slot<F>(&self, slot_ptr: *mut F, name: &str, hook: F, original: Option<&FnSlot<F>>)
    where
        F: Copy + Send + Sync + 'static,
    {
        let slot_raw: *mut *mut () = slot_ptr.cast();
        // SAFETY: writable window open for the scope; slot derived from `project(vtbl)`.
        let current: *mut () = unsafe { read_unaligned(slot_raw) };
        #[allow(clippy::cast_possible_truncation)]
        let current_addr = current as u32;
        // SAFETY: `slot_ptr` and `self.vtbl` come from the same allocation by construction.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let offset = unsafe { slot_ptr.cast::<u8>().offset_from(self.vtbl.cast::<u8>()) } as usize;

        let is_redirector = original.is_none();

        if let Some(ours) = self.our_range
            && ours.contains(current_addr)
        {
            self.log_outcome(
                name,
                offset,
                current,
                current,
                Outcome::AlreadyOurs,
                is_redirector,
            );
            return;
        }

        if let Some(slot) = original {
            slot.store_raw(current);
        }
        let hook_raw = hook_to_raw(hook);
        // SAFETY: see above.
        unsafe { write_unaligned(slot_raw, hook_raw) };

        // SAFETY: see above.
        let verify: *mut () = unsafe { read_unaligned(slot_raw) };
        let outcome = if verify == hook_raw {
            Outcome::Applied
        } else {
            Outcome::Mismatch
        };
        self.log_outcome(name, offset, current, hook_raw, outcome, is_redirector);
    }

    fn log_outcome(
        &self,
        name: &str,
        offset: usize,
        original: *mut (),
        new: *mut (),
        outcome: Outcome,
        is_redirector: bool,
    ) {
        let (tag, failed) = match outcome {
            Outcome::AlreadyOurs => ("[idempotent, already our hook]", false),
            Outcome::Applied => ("[verified]", false),
            Outcome::Mismatch => ("[MISMATCH]", true),
        };
        // Chain-through annotation when the original didn't come from
        // the vtable's home module, surfacing the shim layer we're stacked on.
        #[allow(clippy::cast_possible_truncation)]
        let original_u32 = original as u32;
        let chain_through = if matches!(outcome, Outcome::Applied)
            && self
                .expected_range
                .is_none_or(|r| !r.contains(original_u32))
        {
            annotate_resolved(original_u32, self.modules).map(|s| format!(" chained-through={s}"))
        } else {
            None
        };
        let chain_tag = chain_through.as_deref().unwrap_or("");
        let redirect_tag = if is_redirector {
            " [redirector, original discarded]"
        } else {
            ""
        };
        if failed {
            warn!(
                "vtable patch: {name} (off {offset:#x}) old=0x{:08x} new=0x{:08x} {tag}{chain_tag}{redirect_tag}",
                original as usize, new as usize,
            );
        } else {
            info!(
                "vtable patch: {name} (off {offset:#x}) old=0x{:08x} new=0x{:08x} {tag}{chain_tag}{redirect_tag}",
                original as usize, new as usize,
            );
        }
    }
}

#[derive(Clone, Copy)]
enum Outcome {
    Applied,
    AlreadyOurs,
    Mismatch,
}

/// Opens a writable window over `size_of::<V>()` bytes starting at `vtbl`,
/// builds a `VtblScope<V>`, and runs `scope`.
/// Returns `None` on `VirtualProtect` failure or null `vtbl`.
///
/// The chained-through annotation uses the loaded-module range that contains `vtbl`
/// as the "canonical implementation" module; slots whose displaced original
/// points outside that range are annotated.
///
/// # Safety
/// `vtbl` must point to a valid `V` whose backing memory can be made writable
/// through `VirtualProtect`.
pub(crate) unsafe fn install_vtable<V, R>(
    vtbl: *mut V,
    scope: impl FnOnce(&VtblScope<'_, V>) -> R,
) -> Option<R> {
    if vtbl.is_null() {
        return None;
    }
    let modules = walk_modules();
    let our_range = our_dll_range();
    #[allow(clippy::cast_possible_truncation)]
    let vtbl_addr = vtbl as u32;
    let expected_range = modules
        .iter()
        .find(|m| m.range.contains(vtbl_addr))
        .map(|m| m.range);

    let size = size_of::<V>();
    let region_start: *mut u8 = vtbl.cast();
    let result = unsafe {
        with_writable(region_start, size, |_| {
            let s = VtblScope {
                vtbl,
                modules: &modules,
                our_range,
                expected_range,
            };
            scope(&s)
        })
    };
    if result.is_none() {
        warn!(
            kind = "vtable_protect_failed",
            addr = format_args!("{region_start:p}"),
            span = size,
        );
    }
    result
}
