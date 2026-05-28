//! In-process byte patching primitives.
//!
//! Before writing, every patch primitive checks that the bytes currently at
//! the target address match the expected pattern. In the case of a mismatch,
//! the patch is not applied and the mismatch is logged.

use crate::protect::with_writable;
use std::fmt::Write as _;
use std::ptr::{copy_nonoverlapping, with_exposed_provenance, with_exposed_provenance_mut};
use tracing::{info, warn};

/// Stack-buffer ceiling for patch reads.
const MAX_PATCH_LEN: usize = 16;

/// Static byte patch.
pub struct Patch {
    addr: usize,
    expected: &'static [u8],
    replacement: &'static [u8],
    name: &'static str,
}

impl Patch {
    #[must_use]
    pub const fn new<const N: usize>(
        addr: usize,
        expected: &'static [u8; N],
        replacement: &'static [u8; N],
        name: &'static str,
    ) -> Self {
        Self {
            addr,
            expected,
            replacement,
            name,
        }
    }

    /// # Safety
    /// `self.addr` must be a writable code address.
    pub unsafe fn apply(&self) {
        unsafe {
            if !pre_check(self.addr, self.expected, self.name) {
                return;
            }
            patch_bytes(self.addr, self.replacement);
            verify_patch(self.addr, self.replacement, self.name);
        }
    }

    /// # Safety
    /// Each patch's `addr` must be a writable code address.
    pub unsafe fn apply_all(patches: &[Self]) {
        for p in patches {
            unsafe { p.apply() };
        }
    }
}

/// Writes a 5-byte relative `e9 disp32` jmp at `target` to `hook`. `expected` is the full
/// displaced-instruction byte sequence (at least 5 bytes). Verifies every byte of `expected`
/// matches the current site before the write proceeds. Bytes past offset 4 are NOP-padded.
///
/// # Safety
/// `target` must be a writable code address holding `expected`.
pub unsafe fn patch_jmp<const N: usize>(
    target: usize,
    expected: &[u8; N],
    hook: *mut (),
    name: &str,
) {
    unsafe { write_relative_branch::<N>(target, expected, hook, 0xe9, name) };
}

/// Rewrites a direct-call or indirect-call site so it targets `hook` instead of the original
/// callee. `expected` is the full displaced-instruction byte sequence: 5 bytes for an
/// `E8 disp32` direct call, 6 bytes for a `FF 15 disp32` indirect call. Bytes past offset 4
/// are NOP-padded so the call's return address (`target + 5`) lands on a NOP that
/// falls through to the original next instruction at `target + N`. The wrapper at `hook`
/// is responsible for calling the original callee if forwarding is desired.
///
/// # Safety
/// `target` must be a writable code address holding `expected`.
pub(crate) unsafe fn patch_call<const N: usize>(
    target: usize,
    expected: &[u8; N],
    hook: *mut (),
    name: &str,
) {
    unsafe { write_relative_branch::<N>(target, expected, hook, 0xe8, name) };
}

unsafe fn write_relative_branch<const N: usize>(
    target: usize,
    expected: &[u8; N],
    hook: *mut (),
    opcode: u8,
    name: &str,
) {
    const { assert!(N >= 5, "rel32 branch needs at least 5 bytes") };
    const { assert!(N <= MAX_PATCH_LEN, "patch length exceeds MAX_PATCH_LEN") };
    #[allow(clippy::cast_possible_truncation)]
    let (target_u32, hook_u32) = (target as u32, hook as u32);

    // The displacement is relative to `target + 5`, the byte after the 5-byte `e8/e9 disp32`.
    // Bytes at offsets 5..N are NOP-padded so a returning CALL lands on a NOP
    // that falls through to the original next instruction.
    let disp = hook_u32.wrapping_sub(target_u32.wrapping_add(5));
    let mut bytes = [0x90u8; MAX_PATCH_LEN];
    bytes[0] = opcode;
    bytes[1..5].copy_from_slice(&disp.to_le_bytes());

    unsafe {
        if !pre_check(target, expected, name) {
            return;
        }
        patch_bytes(target, &bytes[..N]);

        let buf = read_at(target, N);
        let actual = &buf[..N];
        let read_disp = i32::from_le_bytes([actual[1], actual[2], actual[3], actual[4]]);
        let resolved = target_u32.wrapping_add(5).wrapping_add_signed(read_disp);
        // We resolve the branch target as well as checking byte equality,
        // so a downstream displacement rewrite that keeps the opcode is still caught.
        let ok = actual[0] == opcode && resolved == hook_u32;
        let status = if ok { "OK" } else { "MISMATCH" };
        if ok {
            info!(
                kind = "patch_verify",
                addr = format_args!("{target:#010x}"),
                name,
                expected = %bytes_hex(&bytes[..N]),
                actual = %bytes_hex(actual),
                resolved_target = format_args!("{resolved:#010x}"),
                expected_target = format_args!("{hook_u32:#010x}"),
                status,
            );
        } else {
            warn!(
                kind = "patch_verify",
                addr = format_args!("{target:#010x}"),
                name,
                expected = %bytes_hex(&bytes[..N]),
                actual = %bytes_hex(actual),
                resolved_target = format_args!("{resolved:#010x}"),
                expected_target = format_args!("{hook_u32:#010x}"),
                status,
            );
        }
    }
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

unsafe fn patch_bytes(addr: usize, src: &[u8]) {
    unsafe {
        let dst: *mut u8 = with_exposed_provenance_mut(addr);
        let _ = with_writable(dst, src.len(), |p| {
            copy_nonoverlapping(src.as_ptr(), p, src.len());
        });
    }
}

unsafe fn read_at(addr: usize, len: usize) -> [u8; MAX_PATCH_LEN] {
    assert!(
        len <= MAX_PATCH_LEN,
        "patch length {len} exceeds MAX_PATCH_LEN ({MAX_PATCH_LEN})",
    );
    let mut buf = [0u8; MAX_PATCH_LEN];
    unsafe {
        copy_nonoverlapping(with_exposed_provenance::<u8>(addr), buf.as_mut_ptr(), len);
    }
    buf
}

unsafe fn verify_patch(addr: usize, expected: &[u8], name: &str) {
    let buf = unsafe { read_at(addr, expected.len()) };
    let actual = &buf[..expected.len()];
    if actual == expected {
        info!(
            kind = "patch_verify",
            addr = format_args!("{addr:#010x}"),
            name,
            expected = %bytes_hex(expected),
            actual = %bytes_hex(actual),
            status = "OK",
        );
    } else {
        warn!(
            kind = "patch_verify",
            addr = format_args!("{addr:#010x}"),
            name,
            expected = %bytes_hex(expected),
            actual = %bytes_hex(actual),
            status = "MISMATCH",
        );
    }
}

/// Reads `expected.len()` bytes at `addr` and returns `true` if they match `expected`.
/// In the case of a mismatch, this returns `false` and the mismatch is logged.
unsafe fn pre_check(addr: usize, expected: &[u8], name: &str) -> bool {
    let buf = unsafe { read_at(addr, expected.len()) };
    let actual = &buf[..expected.len()];
    if actual == expected {
        return true;
    }
    warn!(
        kind = "patch_skipped",
        addr = format_args!("{addr:#010x}"),
        name,
        expected = %bytes_hex(expected),
        actual = %bytes_hex(actual),
        status = "PRE_MISMATCH",
    );
    false
}
