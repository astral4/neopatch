//! Typed wrappers for raw pointers whose validity isn't established by code we control.
//!
//! [`Untrusted<T>`] wraps `*const T` and exposes only `safe_read*` methods, which route through `ReadProcessMemory`
//! and return short reads on a guard-page fault rather than AV'ing the host. Hook bodies should wrap caller-controlled FFI pointers
//! in `Untrusted<T>` at the entry so the rest of the code can't accidentally dereference one.
//! [`safe_read_stack`] is the analogous entry for register-value-like pointers (e.g. ESP/EBP recovered from another thread's `CONTEXT`).

use std::ffi::c_void;
use std::ptr::with_exposed_provenance;
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

// Sealed; the all-zero bit pattern is a valid value of `T` named `ZERO`.
mod sealed {
    pub(crate) trait Zeroable: Copy {
        const ZERO: Self;
    }
    impl Zeroable for u8 {
        const ZERO: Self = 0;
    }
    impl Zeroable for u16 {
        const ZERO: Self = 0;
    }
    impl Zeroable for u32 {
        const ZERO: Self = 0;
    }
    impl<T: Zeroable, const N: usize> Zeroable for [T; N] {
        const ZERO: Self = [T::ZERO; N];
    }
}

/// A pointer whose validity isn't established by code we control.
#[derive(Clone, Copy)]
pub(crate) struct Untrusted<T>(*const T);

impl<T> Untrusted<T> {
    // This is sound to construct from any raw pointer because `Untrusted` has no `Deref` impl or raw accessor.
    pub(crate) const fn from_raw(raw: *const T) -> Self {
        Self(raw)
    }

    #[must_use]
    pub(crate) fn is_null(&self) -> bool {
        self.0.is_null()
    }
}

impl<T: sealed::Zeroable> Untrusted<T> {
    /// Best-effort copy of up to `buf.len()` elements. See [`safe_read`] for more details. Partial-`T` trailing reads are zeroed.
    pub(crate) fn safe_read(self, buf: &mut [T]) -> usize {
        safe_read(self.0, buf)
    }

    /// [`safe_read`]s into `buf`, then returns the populated prefix up to (but excluding) the first `terminator` element
    /// (or the full read length if no terminator is found).
    pub(crate) fn safe_read_until(self, buf: &mut [T], terminator: T) -> &[T]
    where
        T: PartialEq,
    {
        let n = self.safe_read(buf);
        let len = buf[..n].iter().position(|t| *t == terminator).unwrap_or(n);
        &buf[..len]
    }

    /// True if the pointed-to buffer reads as exactly `expected` followed by a NUL (`T::ZERO`).
    /// A short read (an `ATOM` in the null-guard region, or a guard-page fault) will fail to match.
    pub(crate) fn matches_nul_terminated<const N: usize>(self, expected: &[T; N]) -> bool
    where
        T: PartialEq,
    {
        const CAP: usize = 5;
        const {
            assert!(N < CAP, "expected length + NUL must fit the scratch buffer");
        };
        let mut buf = [T::ZERO; CAP];
        let want_len = N + 1;
        let n = self.safe_read(&mut buf[..want_len]);
        n == want_len && &buf[..N] == expected.as_slice() && buf[N] == T::ZERO
    }
}

/// Best-effort copy of up to `buf.len()` elements from `src` into `buf`. Returns the number of complete `T`s read.
/// A partial-`T` trailing read (`ReadProcessMemory` stopping mid-`T` at a page boundary) overwrites `buf[n]` with `T::ZERO`.
/// The returned `n` excludes the partial slot.
fn safe_read<T: sealed::Zeroable>(src: *const T, buf: &mut [T]) -> usize {
    let bytes = rpm(
        src.cast::<c_void>(),
        buf.as_mut_ptr().cast::<c_void>(),
        size_of_val(buf),
    );
    let n = bytes / size_of::<T>();
    if !bytes.is_multiple_of(size_of::<T>()) && n < buf.len() {
        // A partial-`T` tail read left `ReadProcessMemory`'s leftover bytes in `buf[n]`,
        // so we overwrite the whole slot to the all-zero value that `Zeroable` guarantees is valid.
        buf[n] = T::ZERO;
    }
    n
}

/// Best-effort copy of up to `N` `u32`s starting at `esp`.
pub(crate) fn safe_read_stack<const N: usize>(esp: u32, out: &mut [u32; N]) -> usize {
    let src: *const u32 = with_exposed_provenance(esp as usize);
    safe_read(src, out)
}

/// Returns the number of bytes read; 0 on null source or `ReadProcessMemory` failure.
fn rpm(src: *const c_void, dst: *mut c_void, len: usize) -> usize {
    if src.is_null() {
        return 0;
    }
    let mut bytes_read = 0;
    let _ = unsafe { ReadProcessMemory(GetCurrentProcess(), src, dst, len, &raw mut bytes_read) };
    bytes_read
}
