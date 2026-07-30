//! State tracking for the game's device.
//!
//! The device is recorded here at the creation paths and pinned by one COM reference we own, taken on record and released
//! on overwrite, so a recorded address is always valid memory. At process exit, the pin deliberately leaks
//! so our reference outlives the game's own.

use crate::thread::{MainCell, MainToken};
use std::ffi::c_void;
use std::ptr::NonNull;
use windows::Win32::Graphics::Direct3D9::IDirect3DDevice9Ex;
use windows::core::{Interface, InterfaceRef};

// The pinned object. `Some` holds one COM reference we own. The pin is `None` during the `device_creating` window.
static DEVICE_PIN: MainCell<Option<NonNull<c_void>>> = MainCell::new(None);

/// Takes an owning reference on `p` as interface `I` and files it with the pin. This operation is similar to `Rc::increment_strong_count`.
///
/// # Safety
/// `p` must be a live COM object implementing `I`.
unsafe fn pin<I: Interface>(p: NonNull<c_void>) {
    // `InterfaceRef::to_owned` performs the `AddRef`, and `Interface::into_raw` hands the reference over without giving it back.
    let owned = unsafe { InterfaceRef::<I>::from_raw(p) }.to_owned();
    let _ = owned.into_raw();
}

/// Releases the owning reference [`pin`] took on `p`. This operation is similar to `Rc::decrement_strong_count`.
///
/// # Safety
/// A reference on `p` must have been taken by [`pin`] with the same `I` and not yet released.
unsafe fn unpin<I: Interface>(p: NonNull<c_void>) {
    // The `Drop` implementation of `Interface` performs the `Release`.
    drop(unsafe { I::from_raw(p.as_ptr()) });
}

/// Pins `new`, publishes the pin, and releases the previous pin.
/// Returns the object that was replaced, or `None` when the slot was empty or already held `new`.
///
/// # Safety
/// `new` must be a live COM object implementing `I`, and the slot's previous pins must have been taken with the same `I`.
unsafe fn record<I: Interface>(
    pin_cell: &MainCell<Option<NonNull<c_void>>>,
    tok: &MainToken,
    new: NonNull<c_void>,
) -> Option<NonNull<c_void>> {
    let prev = pin_cell.get(tok);
    if prev == Some(new) {
        return None;
    }
    unsafe { pin::<I>(new) };
    pin_cell.set(tok, Some(new));
    if let Some(old) = prev {
        // SAFETY: `old` came out of the pin, so it holds our reference and is live.
        unsafe { unpin::<I>(old) };
    }
    prev
}

/// Releases the device pin at the start of a replacement attempt.
///
/// We release here rather than when the replacement succeeds because, in exclusive fullscreen, the outgoing device
/// still holds the display mode and its VRAM, which can be enough for the replacement to be refused.
pub(crate) fn device_creating(tok: &MainToken) {
    if let Some(old) = DEVICE_PIN.take(tok) {
        // SAFETY: `old` came out of the pin, so it holds our reference and is live.
        unsafe { unpin::<IDirect3DDevice9Ex>(old) };
    }
}

/// Records `dev` as the game's device after a successful create or reset, pinning it.
///
/// # Safety
/// `dev` must be a live `IDirect3DDevice9Ex`.
pub(crate) unsafe fn record_device(tok: &MainToken, dev: NonNull<c_void>) {
    let _ = unsafe { record::<IDirect3DDevice9Ex>(&DEVICE_PIN, tok, dev) };
}

/// Returns the pinned device.
pub(crate) fn pinned_device(tok: &MainToken) -> Option<NonNull<c_void>> {
    DEVICE_PIN.get(tok)
}
