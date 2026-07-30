//! Screenshot capture primitives.
//!
//! The implementations we replace lock the back buffer (`IDirect3DSurface9::LockRect`), which requires `D3DPRESENTFLAG_LOCKABLE_BACKBUFFER`.
//! Whether that flag survives our present-params rewrite is out of scope for this module. Therefore, our screenshot capture functions
//! never lock the back buffer themselves. Instead, we round-trip through `GetRenderTargetData` into a `D3DPOOL_SYSTEMMEM` offscreen surface,
//! which is lockable regardless of the presentation flags.
//!
//! A game's screenshot save function runs either before or after `Present`:
//! - th11–th18 ([`save_screenshot_live_bmp`]), th20 ([`save_screenshot_live_png`]): before `Present`,
//!   so the back buffer is still fresh and we capture synchronously, matching vanilla frame timing.
//! - th10 ([`save_screenshot_deferred_bmp`]): after `Present`, where the back buffer is undefined under D3D9Ex.
//!   We stash the filename, and the next `on_pre_present` captures the live back buffer one frame later.

use crate::fmt_hr;
use crate::session::pinned_device;
use crate::thread::{MainCell, MainToken};
use crate::untrusted::Untrusted;
use png::{BitDepth, ColorType, Encoder};
use std::ffi::c_void;
use std::mem::zeroed;
use std::ptr::{NonNull, null, null_mut};
use tracing::{info, warn};
use windows::Win32::Graphics::Direct3D9::{
    D3DBACKBUFFER_TYPE_MONO, D3DFMT_A8R8G8B8, D3DFMT_X8R8G8B8, D3DFORMAT, D3DLOCK_READONLY,
    D3DLOCKED_RECT, D3DPOOL_SYSTEMMEM, IDirect3DDevice9Ex, IDirect3DSurface9,
};
use windows::core::InterfaceRef;
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_WRITE, GetLastError, INVALID_HANDLE_VALUE, MAX_PATH,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_ALWAYS, CreateDirectoryA, CreateFileA, DeleteFileA, FILE_ATTRIBUTE_NORMAL,
    MOVEFILE_REPLACE_EXISTING, MoveFileExA, WriteFile,
};

/// Encodes a captured frame into an in-memory image (BMP or PNG). The parameters are `(width, height, pitch, src)`,
/// where `src` points to `height` rows of `width` 32-bit BGRX/BGRA pixels with `pitch` bytes between rows.
type ImageEncoder = unsafe fn(u32, u32, i32, *const u8) -> Result<Vec<u8>, String>;

/// Discards any pending deferred capture at the start of a device replacement, since it was stashed against the outgoing rendering context.
pub(crate) fn on_device_creating(tok: &MainToken) {
    drop_pending(tok, "screenshot_dropped_device_replaced");
}

/// Synchronously saves a screenshot as BMP. Use this for games whose save function trampoline runs before `Present`.
/// Returns 0 on success, 1 on failure. Callers must be on the render thread.
///
/// # Safety
/// `filename_ptr` must be valid.
#[must_use]
pub unsafe extern "C" fn save_screenshot_live_bmp(filename_ptr: *const u8) -> u32 {
    live_save(filename_ptr, build_bmp_24bpp, "live")
}

/// Synchronously saves a screenshot as PNG. Use this for games whose save function trampoline runs before `Present`.
/// Returns 0 on success, 1 on failure. Callers must be on the render thread.
///
/// # Safety
/// `filename_ptr` must be valid.
#[must_use]
pub unsafe extern "C" fn save_screenshot_live_png(filename_ptr: *const u8) -> u32 {
    live_save(filename_ptr, build_png_24bpp, "live_png")
}

fn live_save(filename_ptr: *const u8, encoder: ImageEncoder, source: &'static str) -> u32 {
    let Some(path) = sanitize_filename(filename_ptr) else {
        return 1;
    };
    // The save trampolines are patched into game code and should run on the render thread.
    // A miss means something unexpected invoked them, so the save is refused rather than touching render-thread state.
    let Some(tok) = MainToken::current() else {
        warn!(kind = "screenshot_off_thread");
        return 1;
    };

    let bytes = path.as_slice();
    match save_live(&tok, bytes, encoder) {
        Ok((w, h)) => {
            log_saved(bytes, w, h, source);
            0
        }
        Err(e) => {
            log_failed(bytes, &e);
            1
        }
    }
}

/// Saves a screenshot, deferring capture to when `on_pre_present` is called next. Use this for games
/// whose save function trampoline runs after `Present`. Returns 0 if the filename was stashed for deferred capture, 1 if rejected.
/// The capture itself runs on the next `on_pre_present`. Callers must be on the render thread.
///
/// # Safety
/// `filename_ptr` must be valid.
#[must_use]
pub unsafe extern "C" fn save_screenshot_deferred_bmp(filename_ptr: *const u8) -> u32 {
    let Some(path) = sanitize_filename(filename_ptr) else {
        return 1;
    };
    let Some(tok) = MainToken::current() else {
        warn!(kind = "screenshot_off_thread");
        return 1;
    };
    if !set_pending_cached_save(&tok, &path) {
        return 1;
    }
    info!(kind = "screenshot_deferred", path = %String::from_utf8_lossy(path.as_slice()));
    0
}

/// Captured screenshot filename.
#[derive(Clone, Copy)]
struct PendingPath {
    buf: [u8; MAX_PATH as usize],
    // The number of valid bytes, excluding the NUL terminator.
    len: usize,
}

impl PendingPath {
    fn as_slice(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

// th10's screenshot save runs after `Present`, where the live back buffer is undefined under D3D9Ex.
// We stash here and capture one frame later from `on_pre_present`.
static PENDING_CAPTURE: MainCell<Option<PendingPath>> = MainCell::new(None);

/// Drops any pending deferred capture, logging it under `kind` if one was there.
fn drop_pending(tok: &MainToken, kind: &'static str) {
    if let Some(stale) = PENDING_CAPTURE.take(tok) {
        warn!(kind, path = %String::from_utf8_lossy(stale.as_slice()));
    }
}

/// Stashes a filename for capture on the next `hook_present`, unless no device exists at all.
/// Still-pending stashes are overwritten. Returns whether the stash was accepted.
fn set_pending_cached_save(tok: &MainToken, path: &PendingPath) -> bool {
    if pinned_device(tok).is_none() {
        warn!(
            kind = "screenshot_dropped_no_device",
            path = %String::from_utf8_lossy(path.as_slice()),
        );
        return false;
    }
    drop_pending(tok, "screenshot_dropped_overwrite");
    PENDING_CAPTURE.set(tok, Some(*path));
    true
}

/// Called from `d3d9::hook_present` before the real `Present`, with the device that call is going through.
pub(crate) fn on_pre_present(tok: &MainToken, device: NonNull<c_void>) {
    if pinned_device(tok) != Some(device) {
        return;
    }
    let Some(path) = PENDING_CAPTURE.take(tok) else {
        return;
    };
    // SAFETY: `device` is the device this `Present` is being made on, so it is live for the duration of this call.
    unsafe { save_pending_cached(device, path.as_slice()) };
}

// Called from `d3d9::hook_reset` at entry.
pub(crate) fn on_pre_reset(tok: &MainToken) {
    drop_pending(tok, "screenshot_dropped_on_reset");
}

/// Capture the live back buffer to `path` as a BMP. Called from [`on_pre_present`] when a th10-style cached save is pending.
///
/// # Safety
/// `device` must be a live `IDirect3DDevice9Ex` for the current render context. The caller must be on the render thread.
unsafe fn save_pending_cached(device: NonNull<c_void>, path: &[u8]) {
    ensure_parent(path);
    match unsafe { capture_live_and_write(device, path, build_bmp_24bpp) } {
        Ok((w, h)) => log_saved(path, w, h, "cached"),
        Err(e) => log_failed(path, &e),
    }
}

/// Captures the live back buffer to `path`, encoding it with `encode`. Returns `(width, height)` on success,
/// or an error string if no `CreateDeviceEx` call has succeeded yet or a Windows API call fails.
fn save_live(tok: &MainToken, path: &[u8], encode: ImageEncoder) -> Result<(u32, u32), String> {
    let Some(device) = pinned_device(tok) else {
        return Err("no active device".to_string());
    };
    ensure_parent(path);
    unsafe { capture_live_and_write(device, path, encode) }
}

/// Gets the live back buffer, allocates a sysmem surface, calls `GetRenderTargetData`, and delegates to [`lock_and_write`].
unsafe fn capture_live_and_write(
    device: NonNull<c_void>,
    path: &[u8],
    encode: ImageEncoder,
) -> Result<(u32, u32), String> {
    // The surface handles below invoke `Release` on function exit via `IDirect3DSurface9`'s `Drop` implementation.
    // `dev` doesn't because `InterfaceRef` is a plain view, and the `AddRef` behind `device` is not ours to match here.
    let dev = unsafe { InterfaceRef::<IDirect3DDevice9Ex>::from_raw(device) };
    let back_buffer = unsafe { dev.GetBackBuffer(0, 0, D3DBACKBUFFER_TYPE_MONO) }
        .map_err(|e| format!("GetBackBuffer hr={}", fmt_hr!(e.code())))?;
    let mut desc = unsafe { zeroed() };
    unsafe { back_buffer.GetDesc(&raw mut desc) }
        .map_err(|e| format!("GetDesc hr={}", fmt_hr!(e.code())))?;
    require_supported_format(desc.Format)?;

    let mut sysmem = None;
    unsafe {
        dev.CreateOffscreenPlainSurface(
            desc.Width,
            desc.Height,
            desc.Format,
            D3DPOOL_SYSTEMMEM,
            &raw mut sysmem,
            null_mut(),
        )
    }
    .map_err(|e| format!("CreateOffscreenPlainSurface hr={}", fmt_hr!(e.code())))?;
    let sysmem = sysmem.ok_or_else(|| "CreateOffscreenPlainSurface returned null".to_string())?;

    unsafe { dev.GetRenderTargetData(&back_buffer, &sysmem) }
        .map_err(|e| format!("GetRenderTargetData hr={}", fmt_hr!(e.code())))?;

    lock_and_write(&sysmem, desc.Width, desc.Height, path, encode)
}

fn lock_and_write(
    surface: &IDirect3DSurface9,
    width: u32,
    height: u32,
    path: &[u8],
    encode: ImageEncoder,
) -> Result<(u32, u32), String> {
    let mut locked = D3DLOCKED_RECT::default();
    unsafe { surface.LockRect(&raw mut locked, null(), D3DLOCK_READONLY.cast_unsigned()) }
        .map_err(|e| format!("LockRect hr={}", fmt_hr!(e.code())))?;
    // We encode while the surface is locked since the encoder dereferences `src`.
    let encoded = unsafe {
        encode(
            width,
            height,
            locked.Pitch,
            locked.pBits.cast::<u8>().cast_const(),
        )
    };
    if let Err(e) = unsafe { surface.UnlockRect() } {
        warn!(
            kind = "screenshot_unlock_failed",
            hr = %fmt_hr!(e.code()),
        );
    }
    let bytes = encoded?;
    let tmp = tmp_path(path);
    write_atomic(&tmp, path, &bytes)?;
    Ok((width, height))
}

fn require_supported_format(format: D3DFORMAT) -> Result<(), String> {
    if format == D3DFMT_X8R8G8B8 || format == D3DFMT_A8R8G8B8 {
        Ok(())
    } else {
        Err(format!("unsupported back buffer format {:#x}", format.0))
    }
}

/// Creates the parent directory of `path` if `path` contains a separator.
fn ensure_parent(path: &[u8]) {
    let Some(sep_idx) = path.iter().rposition(|b| matches!(b, b'/' | b'\\')) else {
        return;
    };
    if sep_idx == 0 {
        return;
    }
    let parent = nul_terminate(&path[..sep_idx]);
    unsafe { CreateDirectoryA(parent.as_ptr(), null()) };
}

/// Constructs a 24bpp BGR Windows BMP byte stream.
///
/// # Safety
/// `src` must point to `height` rows of `width` 32-bit BGRX/BGRA pixels, with `pitch` bytes between row starts.
unsafe fn build_bmp_24bpp(
    width: u32,
    height: u32,
    pitch: i32,
    src: *const u8,
) -> Result<Vec<u8>, String> {
    let row_bytes_unpadded = width.checked_mul(3).ok_or("width too large")?;
    let pad = (4 - (row_bytes_unpadded % 4)) % 4;
    let row_bytes_padded = row_bytes_unpadded + pad;
    let pixel_data_size = row_bytes_padded
        .checked_mul(height)
        .ok_or("image too large")?;
    let file_size = 54u32
        .checked_add(pixel_data_size)
        .ok_or("file size overflow")?;

    let mut buf = Vec::with_capacity(file_size.try_into().unwrap_or(0));
    // BITMAPFILEHEADER
    buf.extend_from_slice(b"BM");
    buf.extend_from_slice(&file_size.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&54u32.to_le_bytes());
    // BITMAPINFOHEADER
    buf.extend_from_slice(&40u32.to_le_bytes());
    buf.extend_from_slice(&width.to_le_bytes());
    buf.extend_from_slice(&height.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&24u16.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // compression
    buf.extend_from_slice(&pixel_data_size.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // x ppm
    buf.extend_from_slice(&0u32.to_le_bytes()); // y ppm
    buf.extend_from_slice(&0u32.to_le_bytes()); // colors used
    buf.extend_from_slice(&0u32.to_le_bytes()); // important colors

    let pad_zeros = [0u8; 3];
    // We write rows bottom-up. Rows start `pitch` bytes apart (signed; always positive for our `CreateOffscreenPlainSurface`
    // sysmem surface). Each pixel is 4 bytes BGRX/BGRA. We copy BGR and discard X/A components.
    for y in (0..height).rev() {
        let row_off = isize::try_from(y).map_err(|e| e.to_string())?
            * isize::try_from(pitch).map_err(|e| e.to_string())?;
        let row_ptr = unsafe { src.offset(row_off) };
        for x in 0..width {
            let p = unsafe { row_ptr.add((x * 4) as usize) };
            buf.push(unsafe { *p });
            buf.push(unsafe { *p.add(1) });
            buf.push(unsafe { *p.add(2) });
        }
        if pad > 0 {
            buf.extend_from_slice(&pad_zeros[..pad as usize]);
        }
    }
    Ok(buf)
}

/// Constructs an 8-bit truecolor (24bpp RGB) PNG.
///
/// # Safety
/// `src` must point to `height` rows of `width` 32-bit BGRX/BGRA pixels, with `pitch` bytes between row starts.
unsafe fn build_png_24bpp(
    width: u32,
    height: u32,
    pitch: i32,
    src: *const u8,
) -> Result<Vec<u8>, String> {
    let pixels = width.checked_mul(height).ok_or("image too large")?;
    let rgb_len = pixels.checked_mul(3).ok_or("image too large")?;
    let mut rgb = Vec::with_capacity(rgb_len.try_into().unwrap_or(0));
    for y in 0..height {
        let row_off = isize::try_from(y).map_err(|e| e.to_string())?
            * isize::try_from(pitch).map_err(|e| e.to_string())?;
        let row_ptr = unsafe { src.offset(row_off) };
        for x in 0..width {
            let p = unsafe { row_ptr.add((x * 4) as usize) };
            rgb.push(unsafe { *p.add(2) });
            rgb.push(unsafe { *p.add(1) });
            rgb.push(unsafe { *p });
        }
    }

    let mut out = Vec::new();
    let mut encoder = Encoder::new(&mut out, width, height);
    encoder.set_color(ColorType::Rgb);
    encoder.set_depth(BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("png write_header: {e}"))?;
    writer
        .write_image_data(&rgb)
        .map_err(|e| format!("png write_image_data: {e}"))?;
    writer.finish().map_err(|e| format!("png finish: {e}"))?;
    Ok(out)
}

/// Writes `data` to `tmp` via `CreateFileA + WriteFile`, then renames `tmp` to `dst` using `MoveFileExA(MOVEFILE_REPLACE_EXISTING)`.
/// On any failure, the partial `tmp` is removed. Both path arguments should be raw (not NUL-terminated) ANSI bytes.
fn write_atomic(tmp: &[u8], dst: &[u8], data: &[u8]) -> Result<(), String> {
    let tmp_c = nul_terminate(tmp);
    let dst_c = nul_terminate(dst);
    let len = u32::try_from(data.len()).map_err(|_| "image too large for WriteFile".to_string())?;

    // We clear any leftover tempfile from a previous crashed write
    // so `CREATE_ALWAYS` doesn't inherit attributes/ACLs from a half-written file.
    unsafe { DeleteFileA(tmp_c.as_ptr()) };

    let h = unsafe {
        CreateFileA(
            tmp_c.as_ptr(),
            GENERIC_WRITE,
            0,
            null(),
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        let err = unsafe { GetLastError() };
        return Err(format!("CreateFileA gle={err}"));
    }

    let mut written = 0;
    let write_ok = unsafe { WriteFile(h, data.as_ptr(), len, &raw mut written, null_mut()) };
    let write_err = if write_ok == 0 {
        Some(format!("WriteFile gle={}", unsafe { GetLastError() }))
    } else if written != len {
        Some(format!("WriteFile short {written}/{len}"))
    } else {
        None
    };
    unsafe { CloseHandle(h) };

    if let Some(e) = write_err {
        unsafe { DeleteFileA(tmp_c.as_ptr()) };
        return Err(e);
    }

    let move_ok = unsafe { MoveFileExA(tmp_c.as_ptr(), dst_c.as_ptr(), MOVEFILE_REPLACE_EXISTING) };
    if move_ok == 0 {
        let err = unsafe { GetLastError() };
        unsafe { DeleteFileA(tmp_c.as_ptr()) };
        return Err(format!("MoveFileExA gle={err}"));
    }
    Ok(())
}

fn tmp_path(path: &[u8]) -> Vec<u8> {
    let mut tmp = Vec::with_capacity(path.len() + 4);
    tmp.extend_from_slice(path);
    tmp.extend_from_slice(b".tmp");
    tmp
}

/// Returns a copy of `bytes` with a NUL byte appended.
fn nul_terminate(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    out.extend_from_slice(bytes);
    out.push(0);
    out
}

/// Reads a NUL-terminated ASCII/ANSI filename from a caller-controlled pointer.
/// Null pointers, empty paths, and strings with no NUL terminator within `MAX_PATH` bytes are rejected.
fn sanitize_filename(filename_ptr: *const u8) -> Option<PendingPath> {
    let untrusted = Untrusted::from_raw(filename_ptr);
    let mut buf = [0u8; MAX_PATH as usize];
    let n = untrusted.safe_read(&mut buf);
    if n == 0 {
        warn!(kind = "screenshot_filename_unreadable");
        return None;
    }
    let Some(nul_pos) = buf[..n].iter().position(|b| *b == 0) else {
        warn!(
            kind = "screenshot_filename_too_long_or_unterminated",
            budget = MAX_PATH,
            read = n,
        );
        return None;
    };
    if nul_pos == 0 {
        warn!(kind = "screenshot_filename_empty");
        return None;
    }
    Some(PendingPath { buf, len: nul_pos })
}

fn log_saved(path: &[u8], w: u32, h: u32, source: &'static str) {
    info!(
        kind = "screenshot_saved",
        path = %String::from_utf8_lossy(path),
        width = w,
        height = h,
        source,
    );
}

fn log_failed(path: &[u8], error: &str) {
    warn!(
        kind = "screenshot_failed",
        path = %String::from_utf8_lossy(path),
        error,
    );
}

#[cfg(test)]
mod tests {
    use super::build_png_24bpp;
    use png::{BitDepth, ColorType, Decoder};

    /// Encodes a top-down 32bpp BGRX source and decodes the PNG back into `(width, height, RGB bytes)`.
    fn round_trip(width: u32, height: u32, pitch: i32, src: &[u8]) -> (u32, u32, Vec<u8>) {
        let encoded =
            unsafe { build_png_24bpp(width, height, pitch, src.as_ptr()) }.expect("encode");

        let mut reader = Decoder::new(encoded.as_slice())
            .read_info()
            .expect("read_info");
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).expect("next_frame");

        assert_eq!(info.color_type, ColorType::Rgb);
        assert_eq!(info.bit_depth, BitDepth::Eight);

        buf.truncate(info.buffer_size());
        (info.width, info.height, buf)
    }

    #[test]
    fn build_png_24bpp_round_trip() {
        const BLUE: [u8; 3] = [0x00, 0x00, 0xff];
        const GREEN: [u8; 3] = [0x00, 0xff, 0x00];
        const RED: [u8; 3] = [0xff, 0x00, 0x00];
        const WHITE: [u8; 3] = [0xff, 0xff, 0xff];
        const YELLOW: [u8; 3] = [0xff, 0xff, 0x00];
        const MAGENTA: [u8; 3] = [0xff, 0x00, 0xff];

        // 2x2 source, pitch = 8 bytes/row. There is no row tail to skip.
        #[rustfmt::skip]
        let tight: [u8; 16] = [
            0xff, 0x00, 0x00, 0x00,    0x00, 0xff, 0x00, 0x00, // blue, green
            0x00, 0x00, 0xff, 0x00,    0xff, 0xff, 0xff, 0x00, // red, white
        ];
        assert_eq!(
            round_trip(2, 2, 8, &tight),
            (2, 2, [BLUE, GREEN, RED, WHITE].concat()),
        );

        // 2x3 source, pitch = 12 bytes/row. The 0xAA row tail padding must be skipped by the stride math.
        // If it leaked into the output, then the decoded pixels would be wrong.
        #[rustfmt::skip]
        let padded: [u8; 36] = [
            0xff, 0x00, 0x00, 0x00,    0x00, 0xff, 0x00, 0x00,    0xaa, 0xaa, 0xaa, 0xaa, // blue, green
            0x00, 0x00, 0xff, 0x00,    0xff, 0xff, 0xff, 0x00,    0xaa, 0xaa, 0xaa, 0xaa, // red, white
            0x00, 0xff, 0xff, 0x00,    0xff, 0x00, 0xff, 0x00,    0xaa, 0xaa, 0xaa, 0xaa, // yellow, magenta
        ];
        assert_eq!(
            round_trip(2, 3, 12, &padded),
            (2, 3, [BLUE, GREEN, RED, WHITE, YELLOW, MAGENTA].concat()),
        );
    }
}
