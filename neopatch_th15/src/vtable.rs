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
use std::marker::PhantomData;
use std::mem::transmute_copy;
use std::ptr::{NonNull, read_unaligned, write_unaligned};
use std::sync::OnceLock;
use std::sync::atomic::{Ordering, fence};
use tracing::{info, warn};
use windows_sys::Win32::Foundation::HMODULE;

/// Declares a typed `static FnSlot<F>` for a vtable slot, plus a typed trampoline
/// calling through it. Use for intercepts and capture-only slots.
#[macro_export]
macro_rules! vtable_slot {
    (
        $slot:ident / $trampoline:ident :
            as fn($($arg:ident : $argty:ty),* $(,)?) -> $ret:ty;
    ) => {
        static $slot: $crate::vtable::FnSlot<
            unsafe extern "system" fn($($argty),*) -> $ret,
        > = $crate::vtable::FnSlot::new(stringify!($slot));

        #[inline]
        #[allow(dead_code, clippy::too_many_arguments)]
        unsafe fn $trampoline($($arg : $argty),*) -> $ret {
            unsafe { $slot.get()($($arg),*) }
        }
    };
}

/// Declares a typed `static Sig<F>` ZST for type inference of `F`
/// at a redirector's call site.
#[macro_export]
macro_rules! vtable_sig {
    (
        $slot:ident :
            as fn($($arg:ident : $argty:ty),* $(,)?) -> $ret:ty;
    ) => {
        static $slot: $crate::vtable::Sig<
            unsafe extern "system" fn($($argty),*) -> $ret,
        > = $crate::vtable::Sig::new();
    };
}

/// Constructs a `SlotProjection<V, F>` for a field path in vtable type `V`.
/// `F` is inferred from the context.
#[macro_export]
macro_rules! vtbl_field {
    ($vtbl_ty:ty, $($field:tt).+) => {
        $crate::vtable::SlotProjection::<$vtbl_ty, _>::at(
            ::core::mem::offset_of!($vtbl_ty, $($field).+),
        )
    };
}

// Set exactly once from `DllMain` and read lock-free thereafter.
// We want the OS's authoritative `hinst` rather than guessing
// via `GetModuleHandleW("dinput8.dll")`, which would collide
// with the real `System32\dinput8.dll`.
static OUR_DLL_RANGE: OnceLock<ModuleRange> = OnceLock::new();

/// Marker for a function-pointer type `F`. Use through `vtable_sig!`.
pub(crate) struct Sig<F>(PhantomData<F>);

impl<F> Sig<F> {
    pub(crate) const fn new() -> Self {
        Self(PhantomData)
    }
}

/// A typed function-pointer slot. `F` is the function pointer type.
pub(crate) struct FnSlot<F: Copy + Send + Sync + 'static> {
    slot: OnceLock<F>,
    /// The slot's identifier used for panic and diagnostic messages.
    name: &'static str,
}

impl<F: Copy + Send + Sync + 'static> FnSlot<F> {
    pub(crate) const fn new(name: &'static str) -> Self {
        Self {
            slot: OnceLock::new(),
            name,
        }
    }

    pub(crate) const fn name(&self) -> &'static str {
        self.name
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

    /// Stores `f` in the slot. Panics on double-capture.
    pub(crate) fn store(&self, f: F) {
        assert!(
            self.slot.set(f).is_ok(),
            "slot `{}`: already captured",
            self.name,
        );
    }
}

/// Reinterprets a raw pointer as a function pointer of type `F`.
/// Returns `None` for null. Soundness when the result is later actually invoked
/// depends on `raw` pointing to a function of signature `F`.
pub(crate) fn parse_fn_ptr<F: Copy>(raw: *mut ()) -> Option<F> {
    const { assert!(size_of::<F>() == size_of::<*mut ()>()) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: `F` is asserted pointer-sized; `raw` is non-null.
    // This is the boundary where the raw IAT/vtable pointer becomes the typed `F`.
    Some(unsafe { transmute_copy(&raw) })
}

/// Converts a typed hook into the raw `*mut ()` written by the patcher.
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

/// Compile-time-checked projection into a vtable `V` for a function-pointer slot of type `F`.
///
/// Construct via `vtbl_field!`. Writes through this projection are guaranteed to land
/// inside the protect window opened by `install_vtable` over `size_of::<V>()` bytes.
pub(crate) struct SlotProjection<V, F> {
    offset: usize,
    _phantom: PhantomData<(*mut V, F)>,
}

impl<V, F> Clone for SlotProjection<V, F> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<V, F> Copy for SlotProjection<V, F> {}

impl<V, F> SlotProjection<V, F> {
    pub(crate) const fn at(offset: usize) -> Self {
        assert!(
            offset + size_of::<F>() <= size_of::<V>(),
            "SlotProjection: slot extends past size_of::<V>()",
        );
        Self {
            offset,
            _phantom: PhantomData,
        }
    }

    fn slot_ptr(self, vtbl: *mut V) -> *mut F {
        // SAFETY: the assertion in `SlotProjection::at` bounds
        // `offset + size_of::<F>()` by `size_of::<V>()`,
        // so the resulting pointer stays inside `V`'s allocation when `vtbl` does.
        unsafe { vtbl.cast::<u8>().add(self.offset).cast::<F>() }
    }

    const fn offset(self) -> usize {
        self.offset
    }
}

/// Reads a vtable slot we trampoline through but don't patch
/// (e.g. `CreateDeviceEx`, `ResetEx`) and publishes the function pointer into `dst`.
/// Logs `capture_slot_null` and skips the publish if the slot is null (malformed vtable).
/// Panics if `dst` was already set.
///
/// # Safety
/// `vtbl` must point to a valid `V`. The slot at `proj` is read as a function pointer.
pub(crate) unsafe fn capture_slot<F, V>(
    vtbl: NonNull<V>,
    proj: SlotProjection<V, F>,
    dst: &FnSlot<F>,
) where
    F: Copy + Send + Sync + 'static,
{
    let slot_ptr: *const F = proj.slot_ptr(vtbl.as_ptr()).cast_const();
    let raw: *mut () = unsafe { read_unaligned(slot_ptr.cast::<*mut ()>()) };
    if let Some(f) = parse_fn_ptr::<F>(raw) {
        dst.store(f);
    } else {
        warn!(kind = "capture_slot_null", slot = dst.name());
    }
}

pub(crate) struct VtblScope<'a, V> {
    vtbl: *mut V,
    modules: &'a [Module],
    our_range: Option<ModuleRange>,
    expected_range: Option<ModuleRange>,
}

impl<V> VtblScope<'_, V> {
    /// Capture the displaced original into `original`
    /// and write `hook` at the slot reached by `proj`.
    pub(crate) fn intercept<F>(
        &self,
        original: &FnSlot<F>,
        proj: SlotProjection<V, F>,
        name: &str,
        hook: F,
    ) where
        F: Copy + Send + Sync + 'static,
    {
        self.write_slot(proj, name, hook, Some(original));
    }

    /// Like `intercept`, except the displaced original isn't captured.
    /// `_sig` is  declared via `vtable_sig!` and used as type inference for `F`.
    pub(crate) fn redirect<F>(&self, _sig: &Sig<F>, proj: SlotProjection<V, F>, name: &str, hook: F)
    where
        F: Copy + Send + Sync + 'static,
    {
        self.write_slot::<F>(proj, name, hook, None);
    }

    fn write_slot<F>(
        &self,
        proj: SlotProjection<V, F>,
        name: &str,
        hook: F,
        original: Option<&FnSlot<F>>,
    ) where
        F: Copy + Send + Sync + 'static,
    {
        let slot_ptr = proj.slot_ptr(self.vtbl);
        let slot_raw: *mut *mut () = slot_ptr.cast();
        // SAFETY: writable window open for the scope; the projection's const assert
        // guarantees the slot lies within the `size_of::<V>()` protected range.
        let current: *mut () = unsafe { read_unaligned(slot_raw) };
        #[allow(clippy::cast_possible_truncation)]
        let current_addr = current as u32;
        let offset = proj.offset();

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
            // Intercept: we must be able to chain through the displaced original.
            // A null current slot has no original to capture, so we refuse the install
            // rather than write our hook over a null slot we can't trampoline through.
            let Some(f) = parse_fn_ptr::<F>(current) else {
                warn!(
                    kind = "vtable_patch",
                    name,
                    offset = format_args!("{offset:#x}"),
                    status = "NULL_SLOT_REFUSED",
                );
                return;
            };
            slot.store(f);
        }
        let hook_raw = hook_to_raw(hook);
        // Release fence: order `slot.store` before the vtable write so trampolines reading
        // the new slot value also see the captured `original` via `FnSlot::try_get`.
        fence(Ordering::Release);
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
        let (status, failed) = match outcome {
            Outcome::AlreadyOurs => ("IDEMPOTENT", false),
            Outcome::Applied => ("OK", false),
            Outcome::Mismatch => ("MISMATCH", true),
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
            annotate_resolved(original_u32, self.modules)
        } else {
            None
        };
        let chain = chain_through.as_deref().unwrap_or("");
        #[allow(clippy::cast_possible_truncation)]
        let new_u32 = new as u32;

        macro_rules! emit {
            ($level:ident) => {
                $level!(
                    kind = "vtable_patch",
                    name,
                    offset = format_args!("{offset:#x}"),
                    old = format_args!("{original_u32:#010x}"),
                    new = format_args!("{new_u32:#010x}"),
                    status,
                    chain_through = chain,
                    redirector = is_redirector,
                );
            };
        }
        if failed {
            emit!(warn);
        } else {
            emit!(info);
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
/// Returns `None` on `VirtualProtect` failure.
///
/// The chained-through annotation uses the loaded-module range that contains `vtbl`
/// as the "canonical implementation" module; slots whose displaced original
/// points outside that range are annotated.
///
/// # Safety
/// `vtbl` must point to a valid `V` whose backing memory can be made writable
/// through `VirtualProtect`.
pub(crate) unsafe fn install_vtable<V, R>(
    vtbl: NonNull<V>,
    scope: impl FnOnce(&VtblScope<'_, V>) -> R,
) -> Option<R> {
    let modules = walk_modules();
    let our_range = our_dll_range();
    #[allow(clippy::cast_possible_truncation)]
    let vtbl_addr = vtbl.as_ptr() as u32;
    let expected_range = modules
        .iter()
        .find(|m| m.range.contains(vtbl_addr))
        .map(|m| m.range);

    let size = size_of::<V>();
    let region_start: *mut u8 = vtbl.as_ptr().cast();
    let result = unsafe {
        with_writable(region_start, size, |_| {
            let s = VtblScope {
                vtbl: vtbl.as_ptr(),
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
