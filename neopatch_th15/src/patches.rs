//! In-process byte patching primitives.
//!
//! `patch_bytes_verified` is the write+read-back-verify entry for static byte patches.
//! `patch_relative_branch` does the same for `e8/e9 disp32` site rewrites,
//! additionally surfacing the resolved branch target.

use crate::protect::with_writable;
use std::fmt::Write as _;
use std::ptr::{copy_nonoverlapping, with_exposed_provenance, with_exposed_provenance_mut};
use tracing::{info, warn};

pub(crate) struct Patch {
    pub(crate) addr: usize,
    pub(crate) bytes: &'static [u8],
    pub(crate) name: &'static str,
}

#[derive(Clone, Copy)]
pub(crate) enum BranchKind {
    /// A 5-byte `e9 disp32`.
    Jmp,
    /// A 6-byte `e8 disp32 90` used to overwrite a 6-byte
    /// `FF 15 disp32` indirect-call site in place.
    /// The trailing NOP keeps the byte count matched.
    CallOverIndirect,
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
        with_writable(dst, src.len(), |p| {
            copy_nonoverlapping(src.as_ptr(), p, src.len());
        });
    }
}

unsafe fn verify_patch(addr: usize, expected: &[u8], name: &str) {
    let mut actual = vec![0u8; expected.len()];
    unsafe {
        copy_nonoverlapping(
            with_exposed_provenance::<u8>(addr),
            actual.as_mut_ptr(),
            actual.len(),
        );
    }
    if actual == expected {
        info!(
            kind = "patch_verify",
            addr = format_args!("{addr:#010x}"),
            name,
            expected = %bytes_hex(expected),
            actual = %bytes_hex(&actual),
            status = "OK",
        );
    } else {
        warn!(
            kind = "patch_verify",
            addr = format_args!("{addr:#010x}"),
            name,
            expected = %bytes_hex(expected),
            actual = %bytes_hex(&actual),
            status = "MISMATCH",
        );
    }
}

/// Writes `src` at `addr` and immediately emits a `patch_verify` event
/// comparing the read-back to `src`. The write+verify pair is one call
/// so callers can't accidentally write without verifying.
pub(crate) unsafe fn patch_bytes_verified(addr: usize, src: &[u8], name: &str) {
    unsafe {
        patch_bytes(addr, src);
        verify_patch(addr, src, name);
    }
}

/// Writes a relative branch at `target` pointing to `hook`, reads it back,
/// decodes the displacement, and emits a `patch_verify` event
/// with both the raw bytes and the resolved branch destination.
#[allow(clippy::cast_possible_truncation)]
pub(crate) unsafe fn patch_relative_branch(
    target: usize,
    hook: usize,
    kind: BranchKind,
    name: &str,
) {
    unsafe {
        let target_u32 = target as u32;
        let hook_u32 = hook as u32;
        let disp = hook_u32.wrapping_sub(target_u32.wrapping_add(5));
        let (opcode, len) = match kind {
            BranchKind::Jmp => (0xe9_u8, 5),
            BranchKind::CallOverIndirect => (0xe8_u8, 6),
        };
        let mut bytes = [0u8; 6];
        bytes[0] = opcode;
        bytes[1..5].copy_from_slice(&disp.to_le_bytes());
        if matches!(kind, BranchKind::CallOverIndirect) {
            bytes[5] = 0x90;
        }
        patch_bytes(target, &bytes[..len]);

        let mut actual = [0u8; 6];
        copy_nonoverlapping(
            with_exposed_provenance::<u8>(target),
            actual.as_mut_ptr(),
            len,
        );
        let read_disp = i32::from_le_bytes([actual[1], actual[2], actual[3], actual[4]]);
        let resolved = target_u32.wrapping_add(5).wrapping_add_signed(read_disp);
        if actual[0] == opcode && resolved as usize == hook {
            info!(
                kind = "patch_verify",
                addr = format_args!("{target:#010x}"),
                name,
                expected = %bytes_hex(&bytes[..len]),
                actual = %bytes_hex(&actual[..len]),
                resolved_target = format_args!("{resolved:#010x}"),
                expected_target = format_args!("{hook:#010x}"),
                status = "OK",
            );
        } else {
            warn!(
                kind = "patch_verify",
                addr = format_args!("{target:#010x}"),
                name,
                expected = %bytes_hex(&bytes[..len]),
                actual = %bytes_hex(&actual[..len]),
                resolved_target = format_args!("{resolved:#010x}"),
                expected_target = format_args!("{hook:#010x}"),
                status = "MISMATCH",
            );
        }
    }
}
