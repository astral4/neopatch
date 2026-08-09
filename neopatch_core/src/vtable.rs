//! In-place patches to `.rdata` vtables.
//!
//! Cloning vtables into heap memory doesn't work because D3D9 dispatches through private virtual slots
//! beyond the typed-struct footprint in the `windows` crate. Reads past the clone will hit uninitialized memory.
//!
//! Slots whose current value points into our own DLL are left alone (idempotent re-entry).
//! Any other value gets chained through, since things like apphelp routinely hijack these slots before we get here.
//!
//! We don't use `FlushInstructionCache` because vtable slots are read as data.

use crate::log::log_at;
use crate::modules::{ModuleRange, annotate_addr, module_containing, module_info};
use crate::protect::with_writable;
use std::marker::PhantomData;
use std::mem::transmute_copy;
use std::ptr::{NonNull, read_unaligned, write_unaligned};
use std::sync::OnceLock;
use std::sync::atomic::{Ordering, fence};
use tracing::{debug, warn};
use windows_sys::Win32::Foundation::HMODULE;

/// Declares a typed `static FnSlot<F>` for a vtable slot we patch, plus a typed trampoline calling through it.
/// The trampoline panics on an uncaptured slot.
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
pub(crate) use vtable_slot;

/// Constructs a [`SlotProjection<V, F>`] for a field path in vtable type `V`. `F` is inferred from the context.
macro_rules! vtable_field {
    ($vtbl_ty:ty, $($field:tt).+) => {
        $crate::vtable::SlotProjection::<$vtbl_ty, _>::at(
            ::core::mem::offset_of!($vtbl_ty, $($field).+),
        )
    };
}
pub(crate) use vtable_field;

// Set exactly once from `DllMain` and read lock-free thereafter. We want the OS's authoritative `hinst`
// rather than guessing via `GetModuleHandleW("dinput8.dll")`, which would collide with the real `System32\dinput8.dll`.
static OUR_DLL_RANGE: OnceLock<ModuleRange> = OnceLock::new();

/// Slot for a function-pointer type `F`.
pub(crate) struct FnSlot<F> {
    slot: OnceLock<F>,
    // The slot's identifier used for panic and diagnostic messages.
    name: &'static str,
}

// TODO: Tighten to `F: FnPtr` if the `fn_ptr_trait` feature stabilizes.
impl<F: Copy + Send + Sync + Unpin + 'static> FnSlot<F> {
    #[must_use]
    pub(crate) const fn new(name: &'static str) -> Self {
        Self {
            slot: OnceLock::new(),
            name,
        }
    }

    pub(crate) const fn name(&self) -> &'static str {
        self.name
    }

    /// Reads the pointer.
    ///
    /// # Panics
    /// Panics if the slot has not been captured. Always call `store` (directly or via the vtable/IAT installers)
    /// before calling `get` from a hook body.
    pub(crate) fn get(&self) -> F {
        *self
            .slot
            .get()
            .unwrap_or_else(|| panic!("slot `{}` not captured", self.name))
    }

    pub(crate) fn try_get(&self) -> Option<F> {
        self.slot.get().copied()
    }

    /// Returns the captured pointer as the raw `*mut ()` written by the patcher, or `None` if uncaptured.
    pub(crate) fn captured_raw(&self) -> Option<*mut ()> {
        // SAFETY: `FnSlot<F>` is only ever instantiated with a function pointer `F` (by `fn_ptr_to_raw`'s contract)
        // and only holds values passed to `store`, so the captured value is a valid function pointer.
        self.try_get().map(|f| unsafe { fn_ptr_to_raw(f) })
    }

    /// Stores `f` in the slot.
    ///
    /// # Panics
    /// Panics on double-capture. The vtable and IAT installers call this exactly once per slot; if you call it directly, do so only once.
    pub(crate) fn store(&self, f: F) {
        assert!(
            self.slot.set(f).is_ok(),
            "slot `{}`: already captured",
            self.name,
        );
    }
}

/// Reinterprets a raw pointer as a function pointer of type `F`. Returns `None` for null.
/// Sound when invoked iff `raw` points to a function with `F`'s signature.
///
/// # Safety
/// `F` must be a function pointer. Note that `F` cannot be a function item (ZST) or pointer-sized non-fn-ptr type.
// TODO: Tighten to `F: FnPtr` if the `fn_ptr_trait` feature stabilizes.
pub(crate) unsafe fn raw_to_fn_ptr<F>(raw: *mut ()) -> Option<F>
where
    F: Copy + Send + Sync + Unpin + 'static,
{
    const { assert!(size_of::<F>() == size_of::<*mut ()>()) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: `F` is asserted pointer-sized and is a function-pointer type per the contract above; `raw` is non-null.
    // This is the boundary where the raw IAT/vtable pointer becomes the typed `F`.
    Some(unsafe { transmute_copy(&raw) })
}

/// Converts a function pointer into a raw pointer.
///
/// # Safety
/// `F` must be a function pointer. Note that `F` cannot be a function item (ZST) or pointer-sized non-fn-ptr type.
// TODO: Tighten to `F: FnPtr` if the `fn_ptr_trait` feature stabilizes.
pub(crate) unsafe fn fn_ptr_to_raw<F>(f: F) -> *mut ()
where
    F: Copy + Send + Sync + Unpin + 'static,
{
    const { assert!(size_of::<F>() == size_of::<*mut ()>()) };
    // SAFETY: `F` is asserted pointer-sized and is a function-pointer type per the contract above.
    unsafe { transmute_copy(&f) }
}

pub fn set_our_dll_handle(hinst: HMODULE) {
    if let Some(range) = module_info(hinst) {
        let _ = OUR_DLL_RANGE.set(range);
    }
}

fn our_dll_range() -> Option<ModuleRange> {
    OUR_DLL_RANGE.get().copied()
}

/// Projection into a vtable `V` for a function-pointer slot of type `F`.
///
/// Construct via [`vtable_field!`]. Writes through this projection are guaranteed to land inside
/// the protect window opened by [`install_vtable`] over `size_of::<V>()` bytes.
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
    /// Constructs a projection at `offset` bytes within vtable type `V`.
    ///
    /// # Panics
    /// Panics if `offset + size_of::<F>() > size_of::<V>()` holds.
    #[must_use]
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
        // SAFETY: the assertion in `SlotProjection::at` bounds `offset + size_of::<F>()` by `size_of::<V>()`,
        // so the resulting pointer stays inside `V`'s allocation when `vtbl` does.
        unsafe { vtbl.cast::<u8>().add(self.offset).cast() }
    }

    const fn offset(self) -> usize {
        self.offset
    }
}

pub(crate) struct VtblScope<V> {
    vtbl: NonNull<V>,
    our_range: Option<ModuleRange>,
    expected_range: Option<ModuleRange>,
}

impl<V> VtblScope<V> {
    /// Capture the displaced original into `original` and write `hook` at the slot reached by `proj`.
    // TODO: Tighten to `F: FnPtr` if the `fn_ptr_trait` feature stabilizes.
    pub(crate) fn intercept<F>(
        &self,
        original: &FnSlot<F>,
        proj: SlotProjection<V, F>,
        name: &str,
        hook: F,
    ) where
        F: Copy + Send + Sync + Unpin + 'static,
    {
        let slot_ptr = proj.slot_ptr(self.vtbl.as_ptr());
        let slot_raw: *mut *mut () = slot_ptr.cast();
        // SAFETY: The writable window is open for the scope, and the projection's const assert
        // guarantees that the slot lies within the `size_of::<V>()` protected range.
        let current_raw = unsafe { read_unaligned(slot_raw) };
        let offset = proj.offset();

        #[allow(clippy::cast_possible_truncation)]
        if let Some(ours) = self.our_range
            && ours.contains(current_raw.addr() as u32)
        {
            self.log_outcome(
                name,
                offset,
                current_raw,
                current_raw,
                PatchOutcome::AlreadyOurs,
            );
            return;
        }

        // We must be able to chain through the displaced original. A null current slot has no original to capture,
        // so we refuse the installation rather than write our hook over a null slot we can't trampoline through.
        let Some(f) = (unsafe { raw_to_fn_ptr(current_raw) }) else {
            warn!(
                kind = "vtable_patch",
                name,
                offset = format_args!("{offset:#x}"),
                status = "NULL_SLOT_REFUSED",
            );
            return;
        };

        // When `PatchOutcome::AlreadyOurs` misses because the second visit arrives through a different vtable allocation
        // for the same logical slot, the visit reads a slot that still holds a real original and we'd panic in `FnSlot::store`.
        // We skip the store but still patch the slot so calls through this distinct vtable route through us.
        // In the case of a divergent value (e.g. a shim layered between our two patches), we keep the original capture.
        // This means the trampoline skips the shim, but at least we don't silently lose intercept coverage of this slot.
        if let Some(existing_raw) = original.captured_raw() {
            if existing_raw == current_raw {
                debug!(
                    kind = "intercept_recapture_skipped",
                    name,
                    offset = format_args!("{offset:#x}"),
                    value = format_args!("{existing_raw:p}"),
                );
            } else {
                warn!(
                    kind = "intercept_recapture_divergent",
                    name,
                    offset = format_args!("{offset:#x}"),
                    kept = format_args!("{existing_raw:p}"),
                    seen = format_args!("{current_raw:p}"),
                );
            }
        } else {
            original.store(f);
        }
        let hook_raw = unsafe { fn_ptr_to_raw(hook) };
        // Trampolines reading the new slot value must also see the captured `original` via `FnSlot::try_get`.
        // The vtable write below is a plain store, so the release fence formally only acts as a compiler barrier.
        // Cross-thread ordering rests on x86 TSO and the hardware atomicity of an aligned 4-byte store.
        fence(Ordering::Release);
        // SAFETY: see above.
        unsafe { write_unaligned(slot_raw, hook_raw) };

        // SAFETY: see above.
        let verify = unsafe { read_unaligned(slot_raw) };
        let outcome = if verify == hook_raw {
            PatchOutcome::Applied
        } else {
            PatchOutcome::Mismatch
        };
        self.log_outcome(name, offset, current_raw, hook_raw, outcome);
    }

    /// Reads a slot we trampoline through but don't patch.
    /// If the slot is non-null, then the function pointer is published into `dst`. Otherwise, `dst` is left empty.
    ///
    /// This operation is idempotent: a revisit with the same slot value does nothing, since re-creation of the COM object
    /// (e.g. recovery from a lost device) reads the same function pointer and there's nothing new to capture.
    /// A divergent value means another shim stacked itself on top of us between visits; we keep the originally-captured pointer.
    // TODO: Tighten to `F: FnPtr` if the `fn_ptr_trait` feature stabilizes.
    pub(crate) fn capture<F>(&self, proj: SlotProjection<V, F>, dst: &FnSlot<F>)
    where
        F: Copy + Send + Sync + Unpin + 'static,
    {
        let slot_ptr = proj.slot_ptr(self.vtbl.as_ptr()).cast_const();
        // SAFETY: The scope's vtable points to a valid `V` due to `install_vtable`'s contract,
        // and the projection's const assert bounds the slot within `V`.
        let current_raw = unsafe { read_unaligned(slot_ptr.cast()) };

        if let Some(existing_raw) = dst.captured_raw() {
            if existing_raw == current_raw {
                debug!(
                    kind = "capture_slot_skipped",
                    slot = dst.name(),
                    value = format_args!("{existing_raw:p}"),
                );
            } else {
                warn!(
                    kind = "capture_slot_divergent",
                    slot = dst.name(),
                    kept = format_args!("{existing_raw:p}"),
                    seen = format_args!("{current_raw:p}"),
                );
            }
            return;
        }

        if let Some(f) = unsafe { raw_to_fn_ptr(current_raw) } {
            dst.store(f);
        } else {
            warn!(kind = "capture_slot_null", slot = dst.name());
        }
    }

    fn log_outcome(
        &self,
        name: &str,
        offset: usize,
        original: *mut (),
        new: *mut (),
        outcome: PatchOutcome,
    ) {
        #[allow(clippy::cast_possible_truncation)]
        let original_addr = original.addr() as u32;
        #[allow(clippy::cast_possible_truncation)]
        let new_addr = new.addr() as u32;

        let (status, failed) = match outcome {
            PatchOutcome::AlreadyOurs => ("IDEMPOTENT", false),
            PatchOutcome::Applied => ("OK", false),
            PatchOutcome::Mismatch => ("MISMATCH", true),
        };

        let chain_through = if matches!(outcome, PatchOutcome::Applied)
            && self
                .expected_range
                .is_none_or(|r| !r.contains(original_addr))
        {
            annotate_addr(original_addr)
        } else {
            None
        };
        let chain_through = chain_through.as_deref().unwrap_or("");

        log_at!(failed => warn / info,
            kind = "vtable_patch",
            name,
            offset = format_args!("{offset:#x}"),
            old = format_args!("{original_addr:#010x}"),
            new = format_args!("{new_addr:#010x}"),
            status,
            chain_through,
        );
    }
}

#[derive(Clone, Copy)]
enum PatchOutcome {
    Applied,
    AlreadyOurs,
    Mismatch,
}

/// Opens a writable window over `size_of::<V>()` bytes starting at `vtbl`, builds a [`VtblScope<V>`], and runs `scope`.
/// If `VirtualProtect` fails, then `scope` does not run.
///
/// The chained-through annotation uses the loaded-module range that contains `vtbl` as the "canonical implementation" module;
/// slots whose displaced original points outside that range are annotated.
///
/// # Safety
/// `vtbl` must point to a valid `V` whose backing memory can be made writable through `VirtualProtect`.
pub(crate) unsafe fn install_vtable<V>(vtbl: NonNull<V>, scope: impl FnOnce(&VtblScope<V>)) {
    let our_range = our_dll_range();
    let expected_range = module_containing(vtbl.addr().get());

    let size = size_of::<V>();
    let region_start = vtbl.as_ptr().cast();
    let ran = unsafe {
        with_writable(region_start, size, |_| {
            let s = VtblScope {
                vtbl,
                our_range,
                expected_range,
            };
            scope(&s);
        })
    };
    if ran.is_none() {
        warn!(
            kind = "vtable_protect_failed",
            addr = format_args!("{region_start:p}"),
            span = size,
        );
    }
}
