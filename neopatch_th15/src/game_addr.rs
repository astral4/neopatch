//! Typed handles for fixed game-memory addresses.
//!
//! `GameAddr<T>` pairs a constant address with an asserted layout.
//! Each address is declared with `GameAddr::new(0x...)` where the address-to-layout pairing
//! is verified against the disasm. Subsequent reads and writes through the typed handle
//! are safe because of the asserted layout.
//!
//! Pointer-derived addresses (e.g. `(*mgr).field` after dereferencing
//! a game pointer we just read) aren't instances of `GameAddr`s
//! since the address isn't fixed. Those sites keep using `read_volatile` directly.

use std::marker::PhantomData;
use std::ptr::{
    read_volatile, with_exposed_provenance, with_exposed_provenance_mut, write_volatile,
};

#[derive(Clone, Copy)]
pub(crate) struct GameAddr<T: Copy> {
    addr: usize,
    _t: PhantomData<*mut T>,
}

impl<T: Copy> GameAddr<T> {
    /// # Safety
    /// `addr` must point to a value of layout `T` in `th15.exe v1.00b`
    /// for the lifetime of the process. The caller is responsible for
    /// confirming this against the disasm at the declaration site.
    pub(crate) const unsafe fn new(addr: usize) -> Self {
        Self {
            addr,
            _t: PhantomData,
        }
    }

    #[inline]
    pub(crate) fn read(self) -> T {
        unsafe { read_volatile(with_exposed_provenance::<T>(self.addr)) }
    }

    #[inline]
    pub(crate) fn write(self, v: T) {
        unsafe { write_volatile(with_exposed_provenance_mut::<T>(self.addr), v) };
    }
}
