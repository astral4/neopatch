//! Typed wrappers for raw pointers whose validity isn't established by code we control.
//!
//! `Untrusted<T>` wraps `*const T` and exposes only `safe_read*` methods,
//! which route through `ReadProcessMemory` and return short reads on a guard-page fault
//! rather than AV'ing the host. Hook bodies should wrap caller-controlled FFI pointers
//! in `Untrusted` at the entry so the rest of the code can't accidentally deref one.
//! `safe_read_stack` is the analogous entry for register-value-like pointers
//! (e.g. ESP/EBP recovered from another thread's `CONTEXT`).

use std::ffi::c_void;
use std::ptr::with_exposed_provenance;
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

// Sealed marker: the all-zero bit pattern is a valid value of `T`.
// Required by `safe_read`'s partial-`T` zero-fill at page boundaries.
mod sealed {
    pub trait Zeroable: Copy {}
    impl Zeroable for u8 {}
    impl Zeroable for u16 {}
    impl Zeroable for u32 {}
    impl<T: Zeroable, const N: usize> Zeroable for [T; N] {}
}

/// A pointer whose validity isn't established by code we control.
#[derive(Clone, Copy)]
pub struct Untrusted<T>(*const T);

impl<T> Untrusted<T> {
    // This is sound to construct from any raw pointer
    // because `Untrusted` has no `Deref` impl or raw accessor.
    pub const fn from_raw(raw: *const T) -> Self {
        Self(raw)
    }

    #[allow(clippy::wrong_self_convention)]
    #[must_use]
    pub fn is_null(self) -> bool {
        self.0.is_null()
    }
}

impl<T: sealed::Zeroable> Untrusted<T> {
    /// Best-effort copy of up to `buf.len()` elements. See `safe_read` for more details.
    /// Partial-`T` trailing reads are zeroed.
    pub fn safe_read(self, buf: &mut [T]) -> usize {
        safe_read(self.0, buf)
    }

    /// `safe_read`s into `buf`, then returns the populated prefix up to (but excluding)
    /// the first `terminator` element (or the full read length if no terminator is found).
    pub fn safe_read_until(self, buf: &mut [T], terminator: T) -> &[T]
    where
        T: PartialEq,
    {
        let n = self.safe_read(buf);
        let len = buf[..n].iter().position(|t| *t == terminator).unwrap_or(n);
        &buf[..len]
    }
}

/// Best-effort copy of up to `buf.len()` elements from `src` into `buf`.
/// Returns the number of complete `T`s read. A partial-`T` trailing read
/// (RPM stopping mid-`T` at a page boundary) zeroes `buf[n]`, so the all-zero
/// bit pattern must be valid for `T`. The returned `n` excludes the partial slot.
fn safe_read<T: sealed::Zeroable>(src: *const T, buf: &mut [T]) -> usize {
    let bytes = rpm(
        src.cast::<c_void>(),
        buf.as_mut_ptr().cast::<c_void>(),
        size_of_val(buf),
    );
    let n = bytes / size_of::<T>();
    if !bytes.is_multiple_of(size_of::<T>()) && n < buf.len() {
        // SAFETY: `n < buf.len()` so `buf[n]` is in-bounds; we zero exactly one `T`'s
        // worth of bytes, overwriting any partial bytes RPM wrote.
        unsafe {
            buf.as_mut_ptr()
                .add(n)
                .cast::<u8>()
                .write_bytes(0, size_of::<T>());
        }
    }
    n
}

/// Best-effort copy of up to `N` `u32`s starting at `esp`.
pub fn safe_read_stack<const N: usize>(esp: u32, out: &mut [u32; N]) -> usize {
    let src: *const u32 = with_exposed_provenance(esp as usize);
    safe_read(src, out)
}

/// Returns bytes read; 0 on null source or `ReadProcessMemory` failure.
fn rpm(src: *const c_void, dst: *mut c_void, len: usize) -> usize {
    if src.is_null() {
        return 0;
    }
    let mut bytes_read: usize = 0;
    let _ = unsafe { ReadProcessMemory(GetCurrentProcess(), src, dst, len, &raw mut bytes_read) };
    bytes_read
}
