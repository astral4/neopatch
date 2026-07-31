//! Code page logic for games with non-ASCII strings.
//!
//! Some games hardcode Shift-JIS strings. ANSI entry points convert those bytes to Unicode using the system ANSI code page,
//! so unless the machine is set to a Japanese locale, filenames are mangled and text degrades to mojibake.
//! Instead of requiring that configuration, we do the conversions ourselves with an explicit code page via `MultiByteToWideChar`.
//!
//! ASCII strings convert identically under any code page, so every hook is a no-op for them,
//! and on a machine that already runs the matching locale, the results should be byte-identical to vanilla.

use crate::iat_hook;
use crate::log::log_at;
use crate::untrusted::Untrusted;
use std::num::NonZero;
use std::sync::atomic::{AtomicU32, Ordering};
use tracing::{debug, info, warn};
use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, GetLastError, HANDLE, HMODULE,
    INVALID_HANDLE_VALUE, MAX_PATH, SetLastError,
};
use windows_sys::Win32::Globalization::{MB_ERR_INVALID_CHARS, MultiByteToWideChar};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontIndirectW, CreateFontW, HFONT, LOGFONTA, LOGFONTW,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, DeleteFileW, OPEN_EXISTING};
use windows_sys::core::{BOOL, PCSTR};

const MAX_ANSI_LEN: usize = 2 * MAX_PATH as usize;

pub const CP_SHIFT_JIS: NonZero<u32> = NonZero::new(932).unwrap();

// The configured code page. 0 means "not installed" and every hook passes straight through.
static CODEPAGE: AtomicU32 = AtomicU32::new(0);

/// The code page registered by [`install`], if any.
pub(crate) fn codepage() -> Option<NonZero<u32>> {
    NonZero::new(CODEPAGE.load(Ordering::Relaxed))
}

iat_hook! {
    REAL_CREATE_FILE_A / real_create_file_a : "CreateFileA"
        as fn(
            file_name: PCSTR,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *const SECURITY_ATTRIBUTES,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: HANDLE,
        ) -> HANDLE;
}
iat_hook! {
    REAL_DELETE_FILE_A / real_delete_file_a : "DeleteFileA"
        as fn(file_name: PCSTR) -> BOOL;
}
iat_hook! {
    REAL_CREATE_FONT_A / real_create_font_a : "CreateFontA"
        as fn(
            height: i32,
            width: i32,
            escapement: i32,
            orientation: i32,
            weight: i32,
            italic: u32,
            underline: u32,
            strike_out: u32,
            char_set: u32,
            out_precision: u32,
            clip_precision: u32,
            quality: u32,
            pitch_and_family: u32,
            face_name: PCSTR,
        ) -> HFONT;
}
iat_hook! {
    REAL_CREATE_FONT_INDIRECT_A / real_create_font_indirect_a : "CreateFontIndirectA"
        as fn(logfont: *const LOGFONTA) -> HFONT;
}

/// IAT-hooks the ANSI font entry points against `host`'s import table and registers `codepage` (e.g. 932 for Shift-JIS)
/// as the game code page, translating font face names through it instead of the system ANSI code page.
///
/// Registering the code page also arms `MessageBoxA` transcoding in [`crate::exit_hooks`] (installed separately).
///
/// # Safety
/// `host` must be a loaded module handle.
pub unsafe fn install(host: HMODULE, codepage: NonZero<u32>) {
    CODEPAGE.store(codepage.get(), Ordering::Relaxed);
    unsafe {
        REAL_CREATE_FONT_A.install(host, hook_create_font_a);
        REAL_CREATE_FONT_INDIRECT_A.install(host, hook_create_font_indirect_a);
    }
    info!(kind = "ansi_hooks_installed", codepage = codepage.get());
}

/// IAT-hooks the ANSI file entry points (`CreateFileA`/`DeleteFileA`), translating filenames through the code page registered by [`install`].
///
/// This is only for games whose every ANSI filename is game-authored in the game code page (e.g. th06's Shift-JIS `.cfg`/`.dat` names).
/// Games that pass OS-derived paths (e.g. `%APPDATA%`, result of `GetModuleFileNameA`) must not install these, since those bytes
/// are in the system ANSI code page. Reinterpreting them through the game code page can corrupt the path on non-matching locales.
///
/// # Safety
/// `host` must be a loaded module handle.
pub unsafe fn install_file_hooks(host: HMODULE) {
    unsafe {
        REAL_CREATE_FILE_A.install(host, hook_create_file_a);
        REAL_DELETE_FILE_A.install(host, hook_delete_file_a);
    }
    info!(kind = "ansi_file_hooks_installed");
}

/// Converts `bytes` through `codepage` into a NUL-terminated UTF-16 string. With `strict`, bytes that are invalid in the code page
/// fail the conversion instead of best-effort mapping. Returns `None` on empty input or conversion failure.
pub(crate) fn to_wide(codepage: NonZero<u32>, bytes: &[u8], strict: bool) -> Option<Vec<u16>> {
    if bytes.is_empty() {
        return None;
    }

    let flags = if strict { MB_ERR_INVALID_CHARS } else { 0 };
    let mut wide = vec![0u16; bytes.len() + 1];
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let written = unsafe {
        MultiByteToWideChar(
            codepage.get(),
            flags,
            bytes.as_ptr(),
            bytes.len() as i32,
            wide.as_mut_ptr(),
            bytes.len() as i32,
        )
    };
    if written <= 0 {
        log_at!(strict => debug / warn,
            kind = "ansi_convert_failed",
            codepage = codepage.get(),
            strict,
        );
        return None;
    }
    wide.truncate(written.cast_unsigned() as usize);
    wide.push(0);

    Some(wide)
}

/// Reads a NUL-terminated ANSI string and converts it through the configured code page.
/// Returns `None` if the string doesn't terminate within the read buffer.
fn convert_z(raw: PCSTR, strict: bool) -> Option<Vec<u16>> {
    let cp = codepage()?;
    let mut buf = [0u8; MAX_ANSI_LEN + 1];
    let Some(bytes) = Untrusted::from_raw(raw).safe_read_terminated(&mut buf, 0) else {
        warn!(
            kind = "ansi_unterminated",
            ptr = format_args!("{raw:p}"),
            strict
        );
        return None;
    };
    to_wide(cp, bytes, strict)
}

unsafe extern "system" fn hook_create_file_a(
    file_name: PCSTR,
    desired_access: u32,
    share_mode: u32,
    security_attributes: *const SECURITY_ATTRIBUTES,
    creation_disposition: u32,
    flags_and_attributes: u32,
    template_file: HANDLE,
) -> HANDLE {
    if let Some(wide) = convert_z(file_name, false) {
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                desired_access,
                share_mode,
                security_attributes,
                creation_disposition,
                flags_and_attributes,
                template_file,
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return handle;
        }

        let error = unsafe { GetLastError() };
        if !should_fall_back(creation_disposition, error) {
            debug!(
                kind = "ansi_create_file_failed",
                creation_disposition, error
            );
            unsafe { SetLastError(error) };
            return INVALID_HANDLE_VALUE;
        }
        debug!(kind = "ansi_create_file_fallback", error);
    }
    unsafe {
        real_create_file_a(
            file_name,
            desired_access,
            share_mode,
            security_attributes,
            creation_disposition,
            flags_and_attributes,
            template_file,
        )
    }
}

fn is_not_found(error: u32) -> bool {
    matches!(error, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND)
}

fn should_fall_back(creation_disposition: u32, error: u32) -> bool {
    creation_disposition == OPEN_EXISTING && is_not_found(error)
}

unsafe extern "system" fn hook_delete_file_a(file_name: PCSTR) -> BOOL {
    if let Some(wide) = convert_z(file_name, false) {
        let ok = unsafe { DeleteFileW(wide.as_ptr()) };
        if ok != 0 {
            return ok;
        }

        let error = unsafe { GetLastError() };
        if !is_not_found(error) {
            debug!(kind = "ansi_delete_file_failed", error);
            unsafe { SetLastError(error) };
            return 0;
        }
        debug!(kind = "ansi_delete_file_fallback", error);
    }
    unsafe { real_delete_file_a(file_name) }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn hook_create_font_a(
    height: i32,
    width: i32,
    escapement: i32,
    orientation: i32,
    weight: i32,
    italic: u32,
    underline: u32,
    strike_out: u32,
    char_set: u32,
    out_precision: u32,
    clip_precision: u32,
    quality: u32,
    pitch_and_family: u32,
    face_name: PCSTR,
) -> HFONT {
    if let Some(wide) = convert_z(face_name, true) {
        let font = unsafe {
            CreateFontW(
                height,
                width,
                escapement,
                orientation,
                weight,
                italic,
                underline,
                strike_out,
                char_set,
                out_precision,
                clip_precision,
                quality,
                pitch_and_family,
                wide.as_ptr(),
            )
        };
        if !font.is_null() {
            return font;
        }
    }
    unsafe {
        real_create_font_a(
            height,
            width,
            escapement,
            orientation,
            weight,
            italic,
            underline,
            strike_out,
            char_set,
            out_precision,
            clip_precision,
            quality,
            pitch_and_family,
            face_name,
        )
    }
}

/// Copies a `LOGFONTA` into a `LOGFONTW`, converting the font face name through `codepage`.
/// Returns `None` if the font face name doesn't convert cleanly.
fn convert_logfont(codepage: NonZero<u32>, a: &LOGFONTA) -> Option<LOGFONTW> {
    let face_len = a
        .lfFaceName
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(a.lfFaceName.len());
    let mut face_bytes = [0u8; 32];
    for (dst, src) in face_bytes.iter_mut().zip(&a.lfFaceName[..face_len]) {
        *dst = src.cast_unsigned();
    }
    let wide = to_wide(codepage, &face_bytes[..face_len], true)?;
    let mut w = LOGFONTW {
        lfHeight: a.lfHeight,
        lfWidth: a.lfWidth,
        lfEscapement: a.lfEscapement,
        lfOrientation: a.lfOrientation,
        lfWeight: a.lfWeight,
        lfItalic: a.lfItalic,
        lfUnderline: a.lfUnderline,
        lfStrikeOut: a.lfStrikeOut,
        lfCharSet: a.lfCharSet,
        lfOutPrecision: a.lfOutPrecision,
        lfClipPrecision: a.lfClipPrecision,
        lfQuality: a.lfQuality,
        lfPitchAndFamily: a.lfPitchAndFamily,
        lfFaceName: [0u16; 32],
    };
    // `wide` is NUL-terminated and can't exceed the face buffer.
    let n = wide.len().min(w.lfFaceName.len());
    w.lfFaceName[..n].copy_from_slice(&wide[..n]);
    w.lfFaceName[w.lfFaceName.len() - 1] = 0;
    Some(w)
}

unsafe extern "system" fn hook_create_font_indirect_a(logfont: *const LOGFONTA) -> HFONT {
    if let (Some(cp), Some(a)) = (codepage(), unsafe { logfont.as_ref() })
        && let Some(w) = convert_logfont(cp, a)
    {
        let font = unsafe { CreateFontIndirectW(&raw const w) };
        if !font.is_null() {
            return font;
        }
    }
    unsafe { real_create_font_indirect_a(logfont) }
}

#[cfg(test)]
mod tests {
    use super::{CP_SHIFT_JIS, convert_logfont, is_not_found, should_fall_back, to_wide};
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SHARING_VIOLATION,
    };
    use windows_sys::Win32::Graphics::Gdi::{LOGFONTA, SHIFTJIS_CHARSET};
    use windows_sys::Win32::Storage::FileSystem::{CREATE_ALWAYS, OPEN_ALWAYS, OPEN_EXISTING};

    // Shift-JIS bytes of th06 strings and their expected UTF-16 forms.
    const IN_DAT_SJIS: &[u8] = &[
        0x8d, 0x67, 0x96, 0x82, 0x8b, 0xbd, 0x49, 0x4e, 0x2e, 0x64, 0x61, 0x74,
    ];
    const IN_DAT_WIDE: &[u16] = &[
        0x7d05, 0x9b54, 0x90f7, 0x49, 0x4e, 0x2e, 0x64, 0x61, 0x74, 0,
    ];
    const CFG_SJIS: &[u8] = &[
        0x93, 0x8c, 0x95, 0xfb, 0x8d, 0x67, 0x96, 0x82, 0x8b, 0xbd, 0x2e, 0x63, 0x66, 0x67,
    ];
    const CFG_WIDE: &[u16] = &[
        0x6771, 0x65b9, 0x7d05, 0x9b54, 0x90f7, 0x2e, 0x63, 0x66, 0x67, 0,
    ];
    const FONT_SJIS: &[u8] = &[
        0x82, 0x6c, 0x82, 0x72, 0x20, 0x83, 0x53, 0x83, 0x56, 0x83, 0x62, 0x83, 0x4e,
    ];
    const FONT_WIDE: &[u16] = &[0xff2d, 0xff33, 0x20, 0x30b4, 0x30b7, 0x30c3, 0x30af, 0];

    #[test]
    fn shift_jis_th06_names() {
        assert_eq!(
            to_wide(CP_SHIFT_JIS, IN_DAT_SJIS, false).unwrap(),
            IN_DAT_WIDE
        );
        assert_eq!(to_wide(CP_SHIFT_JIS, CFG_SJIS, false).unwrap(), CFG_WIDE);
        assert_eq!(to_wide(CP_SHIFT_JIS, FONT_SJIS, true).unwrap(), FONT_WIDE);
    }

    #[test]
    fn ascii_paths_convert_identically() {
        for name in [
            "score.dat",
            "replay/th6_01.rpy",
            "bgm/th06_01.wav",
            "./log.txt",
        ] {
            let wide: Vec<_> = name.bytes().map(u16::from).chain([0]).collect();
            assert_eq!(
                to_wide(CP_SHIFT_JIS, name.as_bytes(), true).unwrap(),
                wide,
                "{name}"
            );
        }
    }

    #[test]
    fn strict_conversion_rejects_invalid_bytes() {
        assert_eq!(to_wide(CP_SHIFT_JIS, &[0x8d], true), None);
        assert_eq!(to_wide(CP_SHIFT_JIS, &[0x41, 0x8d], true), None);
        assert!(to_wide(CP_SHIFT_JIS, &[0x41, 0x8d], false).is_some());
    }

    #[test]
    fn create_file_fallback() {
        // Fall back when reading an existing file whose converted name isn't found.
        assert!(should_fall_back(OPEN_EXISTING, ERROR_FILE_NOT_FOUND));
        assert!(should_fall_back(OPEN_EXISTING, ERROR_PATH_NOT_FOUND));
        // Don't fall back when it would create a mojibake-named file.
        assert!(!should_fall_back(CREATE_ALWAYS, ERROR_FILE_NOT_FOUND));
        assert!(!should_fall_back(OPEN_ALWAYS, ERROR_FILE_NOT_FOUND));
        assert!(!should_fall_back(OPEN_EXISTING, ERROR_SHARING_VIOLATION));
    }

    #[test]
    fn delete_fallback() {
        assert!(is_not_found(ERROR_FILE_NOT_FOUND));
        assert!(is_not_found(ERROR_PATH_NOT_FOUND));
        assert!(!is_not_found(ERROR_ACCESS_DENIED));
        assert!(!is_not_found(ERROR_SHARING_VIOLATION));
    }

    #[test]
    fn logfont_conversion_works() {
        let mut a = LOGFONTA {
            lfHeight: -16,
            lfWidth: 0,
            lfEscapement: 0,
            lfOrientation: 0,
            lfWeight: 700,
            lfItalic: 0,
            lfUnderline: 0,
            lfStrikeOut: 0,
            lfCharSet: SHIFTJIS_CHARSET,
            lfOutPrecision: 0,
            lfClipPrecision: 0,
            lfQuality: 4,
            lfPitchAndFamily: 0x31,
            lfFaceName: [0; 32],
        };
        for (dst, src) in a.lfFaceName.iter_mut().zip(FONT_SJIS) {
            *dst = src.cast_signed();
        }
        let w = convert_logfont(CP_SHIFT_JIS, &a).unwrap();
        assert_eq!(w.lfHeight, -16);
        assert_eq!(w.lfWeight, 700);
        assert_eq!(w.lfCharSet, 128);
        assert_eq!(w.lfQuality, 4);
        assert_eq!(w.lfPitchAndFamily, 0x31);
        assert_eq!(&w.lfFaceName[..FONT_WIDE.len()], FONT_WIDE);
    }
}
