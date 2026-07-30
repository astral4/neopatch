//! State tracking for the game's `IDirect3D9Ex` and device.
//!
//! Every object in the process reaches the shared-`.rdata` vtable hooks in [`crate::d3d9`], so each hook must tell the game's objects
//! from foreign ones (an overlay's, or a recycled allocation). The game's objects are recorded here at the game-only creation paths.
//!
//! Two main ideas:
//! - Identity: each object has an `AtomicPtr` mirror that is used for comparison from the hooks, which are thread-agnostic.
//! - Ownership: each recorded object is pinned by one COM reference we own, taken on record and released on overwrite,
//!   so a recorded address is always valid memory and its allocation can never be recycled into a foreign object while recorded.
//!   At process exit, the pins deliberately leak so our references outlive the game's own.

use crate::thread::{MainCell, MainToken};
use std::ffi::c_void;
use std::ptr::{NonNull, null_mut};
use std::sync::atomic::{AtomicPtr, Ordering};
use tracing::warn;
use windows::Win32::Graphics::Direct3D9::{IDirect3D9, IDirect3DDevice9Ex};
use windows::core::{Interface, InterfaceRef};

// The identity mirrors compared by the gates. Stores happen only on the render thread, but reads can also come from the games' worker threads.
// A worker can only possess the device pointer via the game's own publication of it.
// Regardless of synchronization, x86 TSO makes our earlier store visible, so `Relaxed` suffices.
static D3D9_MIRROR: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static DEVICE_MIRROR: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

// The pinned objects backing the mirrors. Each `Some` holds one COM reference we own. The device pin is `None` during the `device_creating` window.
static D3D9_PIN: MainCell<Option<NonNull<c_void>>> = MainCell::new(None);
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

/// Pins `new`, publishes the pin and then the mirror, and releases the previous pin.
/// Returns the object that was replaced, or `None` when the slot was empty or already held `new`.
///
/// # Safety
/// `new` must be a live COM object implementing `I`, and the slot's previous pins must have been taken with the same `I`.
unsafe fn record<I: Interface>(
    pin_cell: &MainCell<Option<NonNull<c_void>>>,
    mirror: &AtomicPtr<c_void>,
    tok: &MainToken,
    new: NonNull<c_void>,
) -> Option<NonNull<c_void>> {
    let prev = pin_cell.get(tok);
    if prev == Some(new) {
        return None;
    }
    unsafe { pin::<I>(new) };
    pin_cell.set(tok, Some(new));
    mirror.store(new.as_ptr(), Ordering::Relaxed);
    if let Some(old) = prev {
        // SAFETY: `old` came out of the pin, so it holds our reference and is live.
        unsafe { unpin::<I>(old) };
    }
    prev
}

/// Records `d3d9` as the game's `IDirect3D9Ex`, pinning it and releasing any previously recorded one.
/// Replacement is legal since a game may create, probe, release, and create again.
///
/// # Safety
/// `d3d9` must be a live `IDirect3D9Ex`.
pub(crate) unsafe fn record_d3d9(tok: &MainToken, d3d9: NonNull<c_void>) {
    // The pin only ever calls the `IUnknown` slots, so we use the non-Ex interface.
    if let Some(old) = unsafe { record::<IDirect3D9>(&D3D9_PIN, &D3D9_MIRROR, tok, d3d9) } {
        warn!(
            kind = "session_d3d9_replaced",
            old = format_args!("{:p}", old.as_ptr()),
            new = format_args!("{:p}", d3d9.as_ptr()),
        );
    }
}

/// Releases the device pin at the start of a replacement attempt, leaving the identity mirror recorded. In exclusive fullscreen,
/// the outgoing device holds the display mode and its VRAM, which can be enough for the replacement to be refused,
/// so keeping the mirror leaves the game's still-live device recognized after such a failure.
///
/// The cost is a window in which the mirror can dangle if the game had pre-released its device. Should the allocator recycle that address
/// into a foreign device, we'd be patching/hooking the wrong thing, but we're assuming this is very unlikely to happen.
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
    let _ = unsafe { record::<IDirect3DDevice9Ex>(&DEVICE_PIN, &DEVICE_MIRROR, tok, dev) };
}

/// Returns the pinned device.
pub(crate) fn pinned_device(tok: &MainToken) -> Option<NonNull<c_void>> {
    DEVICE_PIN.get(tok)
}

/// Returns `this` when it is the object recorded in `mirror`, or `None` when it is null or foreign.
fn matched(mirror: &AtomicPtr<c_void>, this: *mut c_void) -> Option<NonNull<c_void>> {
    let this = NonNull::new(this)?;
    (this.as_ptr() == mirror.load(Ordering::Relaxed)).then_some(this)
}

/// Checks if `this` is the game's current `IDirect3D9Ex`.
pub(crate) fn is_game_d3d9(this: *mut c_void) -> bool {
    matched(&D3D9_MIRROR, this).is_some()
}

/// Checks if `this` is the game's current device.
pub(crate) fn is_game_device(this: *mut c_void) -> bool {
    matched(&DEVICE_MIRROR, this).is_some()
}

/// Returns `Some` if `this` is the game's current `IDirect3D9Ex` and the caller is on the render thread. Otherwise, returns `None`.
pub(crate) fn gate_d3d9(this: *mut c_void) -> Option<MainToken> {
    if !is_game_d3d9(this) {
        return None;
    }
    MainToken::current()
}

/// Returns `Some` with `this` as a `NonNull` if it is the game's current device and the caller is on the render thread.
/// Otherwise, returns `None`.
pub(crate) fn gate_device(this: *mut c_void) -> Option<(MainToken, NonNull<c_void>)> {
    let dev = matched(&DEVICE_MIRROR, this)?;
    Some((MainToken::current()?, dev))
}

#[cfg(test)]
mod tests {
    use super::{D3D9_MIRROR, DEVICE_MIRROR, gate_d3d9, gate_device, is_game_d3d9, is_game_device};
    use crate::thread::MainToken;
    use crate::thread::test_support::MainClaim;
    use std::ptr::{NonNull, null_mut};
    use std::sync::atomic::Ordering;

    #[test]
    fn mirrors_and_gates() {
        let _claim = MainClaim::acquire();

        D3D9_MIRROR.store(null_mut(), Ordering::Relaxed);
        DEVICE_MIRROR.store(null_mut(), Ordering::Relaxed);

        // Dangling pointers with distinct addresses; identity checks only compare, never dereference.
        let a = NonNull::<u32>::dangling().as_ptr().cast();
        let b = NonNull::<u64>::dangling().as_ptr().cast();

        // Nothing recorded: everything is foreign, including null.
        assert!(!is_game_d3d9(a));
        assert!(!is_game_device(a));
        assert!(!is_game_d3d9(null_mut()));
        assert!(!is_game_device(null_mut()));

        D3D9_MIRROR.store(a, Ordering::Relaxed);
        assert!(is_game_d3d9(a));
        assert!(!is_game_d3d9(b));
        assert!(!is_game_d3d9(null_mut()));
        assert!(!is_game_device(a));

        DEVICE_MIRROR.store(b, Ordering::Relaxed);
        assert!(is_game_device(b));
        assert!(!is_game_device(a));

        assert!(gate_d3d9(a).is_none());
        assert!(gate_device(b).is_none());
        let _tok = MainToken::claim().unwrap();
        assert!(gate_d3d9(a).is_some());
        assert!(gate_d3d9(b).is_none());
        assert_eq!(gate_device(b).map(|(_, dev)| dev.as_ptr()), Some(b));

        DEVICE_MIRROR.store(null_mut(), Ordering::Relaxed);
        assert!(!is_game_device(b));
        assert!(gate_device(b).is_none());

        D3D9_MIRROR.store(null_mut(), Ordering::Relaxed);
    }
}
