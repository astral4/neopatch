//! Typed handles for fixed game-memory addresses.
//!
//! [`GameAddr<T>`] pairs a constant address with an asserted layout `T`. Each address is declared with `GameAddr::new(0x...)`.
//! The address-to-layout pairing should be verified against the disassembly.
//! Subsequent reads and writes through the typed handle are safe because of the asserted layout.
//!
//! Pointer-derived addresses (e.g. `(*mgr).field` after dereferencing a game pointer we just read)
//! aren't instances of `GameAddr<T>` since the address isn't fixed. Those sites keep using `read_volatile` directly.

use std::marker::PhantomData;
use std::ptr::{
    read_volatile, with_exposed_provenance, with_exposed_provenance_mut, write_volatile,
};

#[derive(Clone, Copy)]
pub struct GameAddr<T> {
    addr: usize,
    _t: PhantomData<*mut T>,
}

impl<T: Copy> GameAddr<T> {
    /// # Safety
    /// `addr` must point to a value of layout `T` for the lifetime of the process in the game-binary version the call site targets.
    /// `read` and `write` calls through the handle must not race with conflicting access from another thread;
    /// the volatile ops are not atomic. The caller is responsible for verifying against the disassembly at the declaration site.
    #[must_use]
    pub const unsafe fn new(addr: usize) -> Self {
        Self {
            addr,
            _t: PhantomData,
        }
    }

    #[inline]
    #[must_use]
    pub fn read(self) -> T {
        unsafe { read_volatile(with_exposed_provenance::<T>(self.addr)) }
    }

    #[inline]
    pub fn write(self, v: T) {
        unsafe { write_volatile(with_exposed_provenance_mut::<T>(self.addr), v) };
    }
}
