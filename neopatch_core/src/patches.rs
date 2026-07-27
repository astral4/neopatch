//! In-process byte patching primitives.

use crate::log::{flush, log_at};
use crate::modules::in_host_image;
use crate::protect::with_writable;
use std::fmt::Write as _;
use std::process::abort;
use std::ptr::{copy_nonoverlapping, with_exposed_provenance, with_exposed_provenance_mut};
use tracing::{error, info, warn};

/// Stack-buffer ceiling for patch reads.
const MAX_PATCH_LEN: usize = 8;

/// Length of a rel32 branch (opcode + `disp32`).
const REL32_LEN: usize = 5;

/// A planned code edit.
pub struct PatchSite {
    addr: usize,
    expected: &'static [u8],
    action: PatchAction,
    name: &'static str,
}

/// What [`PatchSite::apply`] writes over the expected bytes.
enum PatchAction {
    /// A static byte replacement of the same length as the expected bytes.
    Replace(&'static [u8]),
    /// A rel32 branch (`0xe9` jmp or `0xe8` call) to `dest`. Bytes past offset 4 are NOP-padded.
    Branch { opcode: u8, dest: *mut () },
}

impl PatchSite {
    /// Static byte patch. `expected` is overwritten with `replacement`.
    #[must_use]
    pub const fn replace<const N: usize>(
        addr: usize,
        expected: &'static [u8; N],
        replacement: &'static [u8; N],
        name: &'static str,
    ) -> Self {
        const { assert!(N <= MAX_PATCH_LEN, "patch length exceeds MAX_PATCH_LEN") };
        Self {
            addr,
            expected,
            action: PatchAction::Replace(replacement),
            name,
        }
    }

    /// Overwrites `expected` with `N` NOPs. Equivalent to [`Self::replace`] with an all-`0x90` replacement.
    #[must_use]
    pub const fn nop<const N: usize>(
        addr: usize,
        expected: &'static [u8; N],
        name: &'static str,
    ) -> Self {
        Self::replace(addr, expected, &[0x90; N], name)
    }

    const fn branch<const N: usize>(
        addr: usize,
        expected: &'static [u8; N],
        opcode: u8,
        dest: *mut (),
        name: &'static str,
    ) -> Self {
        const { assert!(N >= REL32_LEN, "rel32 branch needs at least 5 bytes") };
        const { assert!(N <= MAX_PATCH_LEN, "patch length exceeds MAX_PATCH_LEN") };
        Self {
            addr,
            expected,
            action: PatchAction::Branch { opcode, dest },
            name,
        }
    }

    /// 5-byte relative `e9 disp32` jmp at `addr` to `target`.
    /// `expected` is the full displaced-instruction byte sequence. Bytes past offset 4 are NOP-padded.
    #[must_use]
    pub const fn jmp<const N: usize>(
        addr: usize,
        expected: &'static [u8; N],
        target: *mut (),
        name: &'static str,
    ) -> Self {
        Self::branch(addr, expected, 0xe9, target, name)
    }

    /// Rewrite of a direct-call or indirect-call site so it targets `hook` instead of the original callee. `expected` is the full
    /// displaced-instruction byte sequence: 5 bytes for an `E8 disp32` direct call, 6 bytes for a `FF 15 disp32` indirect call.
    /// Bytes past offset 4 are NOP-padded. The wrapper at `hook` is responsible for calling the original callee if forwarding is desired,
    /// and must match the original callee's ABI as observed by the caller.
    #[must_use]
    pub const fn call<const N: usize>(
        addr: usize,
        expected: &'static [u8; N],
        hook: *mut (),
        name: &'static str,
    ) -> Self {
        Self::branch(addr, expected, 0xe8, hook, name)
    }

    /// Shifts the site's address by `slide`, for a host image not loaded at its preferred base.
    #[must_use]
    pub const fn rebase(mut self, slide: usize) -> Self {
        self.addr = self.addr.wrapping_add(slide);
        self
    }

    /// Applies the patch unconditionally, reporting whether the write landed.
    ///
    /// # Safety
    /// `self.addr` must be a valid, committed, readable code address with protection that `VirtualProtect` can modify.
    #[must_use]
    unsafe fn apply(&self) -> bool {
        unsafe {
            let mut buf = [0u8; MAX_PATCH_LEN];
            let written = self.written_bytes(&mut buf);
            if !patch_bytes(self.addr, written) {
                return false;
            }
            self.verify(written);
            true
        }
    }

    /// Reports whether the expected bytes currently hold at the site.
    ///
    /// # Safety
    /// `self.addr` must be a valid, committed, readable code address.
    unsafe fn holds_expected(&self) -> bool {
        let (addr, len) = (self.addr, self.expected.len());
        if !in_host_image(addr, len) {
            warn!(
                kind = "patch_skipped",
                addr = format_args!("{addr:#010x}"),
                name = self.name,
                status = "OUT_OF_IMAGE",
            );
            return false;
        }
        let mut buf = [0u8; MAX_PATCH_LEN];
        let actual = unsafe { read_at(addr, len, &mut buf) };
        if actual == self.expected {
            true
        } else {
            warn!(
                kind = "patch_skipped",
                addr = format_args!("{addr:#010x}"),
                name = self.name,
                expected = %bytes_hex(self.expected),
                actual = %bytes_hex(actual),
                status = "PRE_MISMATCH",
            );
            false
        }
    }

    /// Writes into `buf` the exact bytes that [`Self::apply`] would write at the site, returning them truncated to the site's length.
    fn written_bytes<'a>(&self, buf: &'a mut [u8; MAX_PATCH_LEN]) -> &'a [u8] {
        let len = self.expected.len();
        *buf = [0x90u8; MAX_PATCH_LEN];

        #[allow(clippy::cast_possible_truncation)]
        match self.action {
            PatchAction::Replace(replacement) => {
                buf[..len].copy_from_slice(replacement);
            }
            PatchAction::Branch { opcode, dest } => {
                let (dest_u32, addr_u32) = (dest.expose_provenance() as u32, self.addr as u32);
                let disp = dest_u32.wrapping_sub(addr_u32.wrapping_add(REL32_LEN as u32));
                buf[0] = opcode;
                buf[1..REL32_LEN].copy_from_slice(&disp.to_le_bytes());
            }
        }
        &buf[..len]
    }

    /// Logs whether the site contains exactly `written`.
    ///
    /// # Safety
    /// `self.addr` must be readable for `written.len()` bytes.
    unsafe fn verify(&self, written: &[u8]) {
        let addr = self.addr;
        let mut buf = [0u8; MAX_PATCH_LEN];

        let actual = unsafe { read_at(addr, written.len(), &mut buf) };
        let ok = actual == written;
        let status = if ok { "OK" } else { "MISMATCH" };

        #[allow(clippy::cast_possible_truncation)]
        match self.action {
            PatchAction::Replace(_) => log_at!(ok => info / warn,
                kind = "patch_verify",
                addr = format_args!("{addr:#010x}"),
                name = self.name,
                expected = %bytes_hex(written),
                actual = %bytes_hex(actual),
                status,
            ),
            PatchAction::Branch { dest, .. } => {
                let expected_target = dest.addr() as u32;
                let resolved_target = {
                    let disp = i32::from_le_bytes([actual[1], actual[2], actual[3], actual[4]]);
                    (addr as u32)
                        .wrapping_add(REL32_LEN as u32)
                        .wrapping_add_signed(disp)
                };
                log_at!(ok => info / warn,
                    kind = "patch_verify",
                    addr = format_args!("{addr:#010x}"),
                    name = self.name,
                    expected = %bytes_hex(written),
                    actual = %bytes_hex(actual),
                    resolved_target = format_args!("{resolved_target:#010x}"),
                    expected_target = format_args!("{expected_target:#010x}"),
                    status,
                );
            }
        }
    }
}

/// Returns whether every site in `groups` currently holds its expected bytes.
///
/// # Safety
/// Each patch site's address must be a valid, committed, readable code address.
#[must_use]
unsafe fn sites_hold(groups: &[&[PatchSite]]) -> bool {
    let mut held = 0usize;
    let mut total = 0usize;
    let mut first_mismatch = None;

    for site in groups.iter().copied().flatten() {
        total += 1;
        if unsafe { site.holds_expected() } {
            held += 1;
        } else if first_mismatch.is_none() {
            first_mismatch = Some(site);
        }
    }

    if let Some(site) = first_mismatch {
        error!(
            kind = "preflight",
            status = "HOST_MISMATCH",
            sites = total,
            held,
            first_addr = format_args!("{:#010x}", site.addr),
            first_name = site.name,
        );
        false
    } else {
        info!(kind = "preflight", status = "OK", sites = total);
        true
    }
}

fn abort_on_partial_patch(addr: usize, name: &'static str) -> ! {
    error!(
        kind = "patch_write_refused",
        addr = format_args!("{addr:#010x}"),
        name,
        detail = "protection changed between preflight and write; process is partially patched",
    );
    flush();
    abort();
}

/// Verifies every patch site in `groups`, then applies all of them. Returns `false` and writes nothing if any site is a mismatch.
///
/// This should be called before installing anything else, since a `false` result means the host binary or environment is unexpected.
///
/// # Safety
/// Each patch site's address must be a valid, committed, readable code address with protection that `VirtualProtect` can modify.
#[must_use]
pub unsafe fn install_all(groups: &[&[PatchSite]]) -> bool {
    if !unsafe { sites_hold(groups) } {
        return false;
    }
    for (written, site) in groups.iter().copied().flatten().enumerate() {
        if !unsafe { site.apply() } {
            if written > 0 {
                abort_on_partial_patch(site.addr, site.name);
            }
            return false;
        }
    }
    true
}

fn bytes_hex(bs: &[u8]) -> String {
    let mut s = String::with_capacity(bs.len() * 3);
    for (i, b) in bs.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Writes `src` over the bytes at `addr`. Returns whether the protection window could be opened.
#[must_use]
unsafe fn patch_bytes(addr: usize, src: &[u8]) -> bool {
    unsafe {
        let dst = with_exposed_provenance_mut(addr);
        with_writable(dst, src.len(), |p| {
            copy_nonoverlapping(src.as_ptr(), p, src.len());
        })
        .is_some()
    }
}

/// Reads `len` bytes at `addr` into `buf` and returns the bytes.
unsafe fn read_at(addr: usize, len: usize, buf: &mut [u8; MAX_PATCH_LEN]) -> &[u8] {
    unsafe {
        copy_nonoverlapping(with_exposed_provenance(addr), buf.as_mut_ptr(), len);
    }
    &buf[..len]
}

#[cfg(test)]
mod tests {
    use super::{MAX_PATCH_LEN, PatchSite, install_all, read_at, sites_hold};
    use std::cell::UnsafeCell;
    use std::ptr::without_provenance_mut;

    struct Patchable(UnsafeCell<[u8; 2]>);

    // SAFETY: Each instance below is declared inside a single test function, so no two threads ever reach the same instance.
    unsafe impl Sync for Patchable {}

    impl Patchable {
        const fn new(bytes: [u8; 2]) -> Self {
            Self(UnsafeCell::new(bytes))
        }

        fn addr(&self) -> usize {
            self.0.get().expose_provenance()
        }

        fn read(&self) -> [u8; 2] {
            let mut buf = [0u8; MAX_PATCH_LEN];
            let bytes = unsafe { read_at(self.addr(), 2, &mut buf) };
            [bytes[0], bytes[1]]
        }
    }

    #[test]
    fn written_bytes_replace() {
        let site = PatchSite::replace(0x1000, &[0x74, 0x11], &[0xeb, 0x11], "test");
        let mut buf = [0u8; MAX_PATCH_LEN];
        assert_eq!(site.written_bytes(&mut buf), &[0xeb, 0x11]);
    }

    #[test]
    fn written_bytes_jmp() {
        let site = PatchSite::jmp(
            0x2000,
            &[0x0f, 0x8f, 0x00, 0x00, 0x00, 0x00],
            without_provenance_mut(0x1000),
            "test",
        );
        let mut buf = [0u8; MAX_PATCH_LEN];
        assert_eq!(
            site.written_bytes(&mut buf),
            &[0xe9, 0xfb, 0xef, 0xff, 0xff, 0x90]
        );
    }

    #[test]
    fn written_bytes_call() {
        let site = PatchSite::call(
            0x1000,
            &[0xff, 0x15, 0x00, 0x00, 0x00, 0x00],
            without_provenance_mut(0x2000),
            "test",
        );
        let mut buf = [0u8; MAX_PATCH_LEN];

        assert_eq!(
            site.written_bytes(&mut buf),
            &[0xe8, 0xfb, 0x0f, 0x00, 0x00, 0x90]
        );
    }

    #[test]
    fn sites_hold_accept_matching_sites() {
        static A: Patchable = Patchable::new([0x74, 0x11]);
        static B: Patchable = Patchable::new([0x75, 0x22]);

        let first = [PatchSite::replace(
            A.addr(),
            &[0x74, 0x11],
            &[0xeb, 0x11],
            "a",
        )];
        let second = [PatchSite::nop(B.addr(), &[0x75, 0x22], "b")];

        assert!(unsafe { sites_hold(&[&first, &second]) });
        assert_eq!(A.read(), [0x74, 0x11]);
        assert_eq!(B.read(), [0x75, 0x22]);
    }

    #[test]
    fn sites_hold_reject_site_drifted() {
        static A: Patchable = Patchable::new([0x74, 0x11]);

        let sites = [
            PatchSite::replace(A.addr(), &[0x74, 0x11], &[0xeb, 0x11], "works"),
            PatchSite::replace(0x1000, &[0x74], &[0xeb], "out of image"),
        ];

        assert!(!unsafe { sites_hold(&[&sites]) });
        assert_eq!(A.read(), [0x74, 0x11]);
    }

    #[test]
    fn sites_hold_reject_bytes_drifted() {
        static A: Patchable = Patchable::new([0x74, 0x11]);
        static B: Patchable = Patchable::new([0x90, 0x90]);

        let sites = [
            PatchSite::replace(A.addr(), &[0x74, 0x11], &[0xeb, 0x11], "works"),
            PatchSite::replace(B.addr(), &[0x74, 0x11], &[0xeb, 0x11], "drifted"),
        ];

        assert!(!unsafe { sites_hold(&[&sites]) });
        assert_eq!(A.read(), [0x74, 0x11]);
        assert_eq!(B.read(), [0x90, 0x90]);
    }

    #[test]
    fn install_all_accept() {
        static A: Patchable = Patchable::new([0x74, 0x11]);
        static B: Patchable = Patchable::new([0x75, 0x22]);

        let first = [PatchSite::replace(
            A.addr(),
            &[0x74, 0x11],
            &[0xeb, 0x11],
            "a",
        )];
        let second = [PatchSite::nop(B.addr(), &[0x75, 0x22], "b")];

        assert!(unsafe { install_all(&[&first, &second]) });
        assert_eq!(A.read(), [0xeb, 0x11]);
        assert_eq!(B.read(), [0x90, 0x90]);
    }

    #[test]
    fn install_all_reject() {
        static A: Patchable = Patchable::new([0x74, 0x11]);
        static B: Patchable = Patchable::new([0x90, 0x90]);

        let sites = [
            PatchSite::replace(A.addr(), &[0x74, 0x11], &[0xeb, 0x11], "works"),
            PatchSite::replace(B.addr(), &[0x74, 0x11], &[0xeb, 0x11], "drifted"),
        ];

        assert!(!unsafe { install_all(&[&sites]) });
        assert_eq!(A.read(), [0x74, 0x11]);
        assert_eq!(B.read(), [0x90, 0x90]);
    }
}
