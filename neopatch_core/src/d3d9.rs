//! Hooks for `IDirect3D9Ex` and `IDirect3DDevice9Ex`.
//!
//! We IAT-hook `Direct3DCreate9` and forward to `Direct3DCreate9Ex`. The Ex object is binary-compatible with `IDirect3D9`
//! for its first 17 vtable slots, so the game can keep using it as plain `IDirect3D9` while we get the Ex methods.
//!
//! We force `D3DPRESENT_INTERVAL_IMMEDIATE` in both windowed and fullscreen exclusive,
//! so `Present` never blocks on vblank and our pacer is the sole timing source, assuming no driver override.
//!
//! `D3DPOOL_MANAGED` is forced to `D3DPOOL_DEFAULT` + `D3DUSAGE_DYNAMIC` on every `CreateTexture` and `CreateVertexBuffer` call
//! because D3D9Ex removes managed pools and otherwise rejects calls with `D3DERR_INVALIDCALL`.
//!
//! `D3DCREATE_MULTITHREADED` is OR'd into the device behavior flags since the games use D3D9 from worker threads
//! without asking for a thread-safe device.
//!
//! Instead of per-instance vtable cloning, we do in-place slot patching against `d3d9.dll`'s `.rdata`.
//! Per-instance clones break because `d3d9.dll` dispatches through private vtable slots beyond the public COM footprint.

use crate::config::{CONFIG, RefreshRateMode};
use crate::log::log_at;
use crate::pacer::{PACER, PacingPolicy};
use crate::screenshot::{on_post_create_device, on_pre_present, on_pre_reset};
use crate::patches::PatchSite;
use crate::thread::{MainCell, MainToken};
use crate::vtable::{capture_slot, install_vtable, vtable_field, vtable_sig, vtable_slot};
use crate::{fmt_hr, iat_hook, match_named};
use std::cmp::min;
use std::ffi::c_void;
use std::ptr::{NonNull, null, null_mut};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use tracing::{error, info, warn};
use windows::Win32::Foundation::{HANDLE, HWND, RECT};
use windows::Win32::Graphics::Direct3D9::{
    D3DCREATE_MULTITHREADED, D3DDEVICE_CREATION_PARAMETERS, D3DDEVTYPE, D3DDISPLAYMODEEX,
    D3DDISPLAYMODEFILTER, D3DDISPLAYROTATION, D3DFMT_A1R5G5B5, D3DFMT_A2B10G10R10,
    D3DFMT_A2R10G10B10, D3DFMT_A4R4G4B4, D3DFMT_A8, D3DFMT_A8B8G8R8, D3DFMT_A8R3G3B2,
    D3DFMT_A8R8G8B8, D3DFMT_A16B16G16R16, D3DFMT_D15S1, D3DFMT_D16, D3DFMT_D16_LOCKABLE,
    D3DFMT_D24FS8, D3DFMT_D24S8, D3DFMT_D24X4S4, D3DFMT_D24X8, D3DFMT_D32, D3DFMT_D32F_LOCKABLE,
    D3DFMT_G16R16, D3DFMT_R3G3B2, D3DFMT_R5G6B5, D3DFMT_R8G8B8, D3DFMT_UNKNOWN, D3DFMT_X1R5G5B5,
    D3DFMT_X4R4G4B4, D3DFMT_X8B8G8R8, D3DFMT_X8R8G8B8, D3DFORMAT, D3DPOOL, D3DPOOL_DEFAULT,
    D3DPOOL_MANAGED, D3DPRESENT_INTERVAL_IMMEDIATE, D3DPRESENT_PARAMETERS,
    D3DPRESENTFLAG_LOCKABLE_BACKBUFFER, D3DRESOURCETYPE, D3DSCANLINEORDERING_PROGRESSIVE,
    D3DUSAGE_DYNAMIC, Direct3DCreate9Ex, IDirect3D9, IDirect3D9Ex_Vtbl, IDirect3DDevice9Ex,
    IDirect3DDevice9Ex_Vtbl,
};
use windows::Win32::Graphics::Gdi::{HMONITOR, RGNDATA};
use windows::core::{HRESULT, Interface};
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsExW, GetMonitorInfoW, MONITORINFO,
    MONITORINFOEXW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowPos,
};

#[allow(clippy::cast_possible_truncation)]
const D3DDISPLAYMODEEX_SIZE: u32 = size_of::<D3DDISPLAYMODEEX>() as u32;

#[allow(clippy::cast_possible_truncation)]
const D3DDISPLAYMODEFILTER_SIZE: u32 = size_of::<D3DDISPLAYMODEFILTER>() as u32;

const MAX_ENUM_RATES: usize = 64;
const MAX_ENUM_SCAN: u32 = 4096;

const D3DERR_DEVICELOST: HRESULT = HRESULT(0x8876_0868_u32.cast_signed());
const D3DERR_DEVICEREMOVED: HRESULT = HRESULT(0x8876_0870_u32.cast_signed());
const D3DERR_DEVICEHUNG: HRESULT = HRESULT(0x8876_0874_u32.cast_signed());
const D3DERR_OUTOFVIDEOMEMORY: HRESULT = HRESULT(0x8876_017c_u32.cast_signed());

/// Replay-speed state observed by game-specific crates, queried each `Present` to decide whether to switch the pacer policy.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayMode {
    Normal = 0,
    Skip = 1,
    Slow = 2,
}

// `Pacer::apply_policy` resets the deadline, so call it only on mode change.
static MODE: MainCell<ReplayMode> = MainCell::new(ReplayMode::Normal);

/// Callback registered by game-specific crates via [`set_replay_mode_fn`].
/// Defaults to `Normal` before `install` or for games without replay-speed control.
static REPLAY_MODE_FN: OnceLock<fn(&MainToken) -> ReplayMode> = OnceLock::new();

/// Registers the game-specific replay-mode probe; first caller wins. Call before [`install`].
pub fn set_replay_mode_fn(f: fn(&MainToken) -> ReplayMode) {
    let _ = REPLAY_MODE_FN.set(f);
}

fn replay_mode(tok: &MainToken) -> ReplayMode {
    REPLAY_MODE_FN
        .get()
        .copied()
        .map_or(ReplayMode::Normal, |f| f(tok))
}

static PRESENT_COUNT: AtomicU32 = AtomicU32::new(0);

pub(crate) fn present_count() -> u32 {
    PRESENT_COUNT.load(Ordering::Relaxed)
}

/// The most recent `Present` result and the frame at which it began.
#[derive(Clone, Copy)]
struct PresentState {
    hr: HRESULT,
    since_frame: u32,
}

static LAST_PRESENT: MainCell<PresentState> = MainCell::new(PresentState {
    hr: HRESULT(0),
    since_frame: 0,
});

// At most one `IDirect3D9` and one device are alive at a time in the game, so each slot is a single global. Read-only slots
// are populated via `capture_slot` and never patched. The trampolines exist so call sites don't have to manually transmute.
vtable_slot! {
    REAL_CREATE_DEVICE_EX / call_real_create_device_ex :
        as fn(
            this: *mut c_void,
            adapter: u32,
            device_type: D3DDEVTYPE,
            focus_window: HWND,
            behavior_flags: u32,
            pp: *mut D3DPRESENT_PARAMETERS,
            mode_ex: *mut D3DDISPLAYMODEEX,
            returned_device: *mut *mut c_void,
        ) -> HRESULT;
}
vtable_slot! {
    REAL_GET_ADAPTER_DISPLAY_MODE_EX / call_real_get_adapter_display_mode_ex :
        as fn(
            this: *mut c_void,
            adapter: u32,
            mode: *mut D3DDISPLAYMODEEX,
            rotation: *mut D3DDISPLAYROTATION,
        ) -> HRESULT;
}
vtable_slot! {
    REAL_GET_ADAPTER_MODE_COUNT_EX / call_real_get_adapter_mode_count_ex :
        as fn(
            this: *mut c_void,
            adapter: u32,
            filter: *const D3DDISPLAYMODEFILTER,
        ) -> u32;
}
vtable_slot! {
    REAL_ENUM_ADAPTER_MODES_EX / call_real_enum_adapter_modes_ex :
        as fn(
            this: *mut c_void,
            adapter: u32,
            filter: *const D3DDISPLAYMODEFILTER,
            mode_index: u32,
            mode: *mut D3DDISPLAYMODEEX,
        ) -> HRESULT;
}
vtable_slot! {
    REAL_GET_ADAPTER_MONITOR / call_real_get_adapter_monitor :
        as fn(this: *mut c_void, adapter: u32) -> HMONITOR;
}
vtable_sig! {
    REDIRECT_CREATE_DEVICE :
        as fn(
            this: *mut c_void,
            adapter: u32,
            device_type: D3DDEVTYPE,
            focus_window: HWND,
            behavior_flags: u32,
            pp: *mut D3DPRESENT_PARAMETERS,
            returned_device: *mut *mut c_void,
        ) -> HRESULT;
}
vtable_slot! {
    REAL_CHECK_DEVICE_FORMAT / call_real_check_device_format :
        as fn(
            this: *mut c_void,
            adapter: u32,
            device_type: D3DDEVTYPE,
            adapter_format: D3DFORMAT,
            usage: u32,
            rtype: D3DRESOURCETYPE,
            check_format: D3DFORMAT,
        ) -> HRESULT;
}
vtable_slot! {
    REAL_RESET_EX / call_real_reset_ex :
        as fn(
            this: *mut c_void,
            pp: *mut D3DPRESENT_PARAMETERS,
            mode_ex: *mut D3DDISPLAYMODEEX,
        ) -> HRESULT;
}
vtable_slot! {
    REAL_SET_MAX_FRAME_LATENCY / call_real_set_max_frame_latency :
        as fn(this: *mut c_void, max_latency: u32) -> HRESULT;
}
vtable_slot! {
    REAL_SET_GPU_THREAD_PRIORITY / call_real_set_gpu_thread_priority :
        as fn(this: *mut c_void, priority: i32) -> HRESULT;
}
vtable_slot! {
    REAL_RESET / call_real_reset :
        as fn(this: *mut c_void, pp: *mut D3DPRESENT_PARAMETERS) -> HRESULT;
}
vtable_slot! {
    REAL_PRESENT / call_real_present :
        as fn(
            this: *mut c_void,
            src_rect: *const RECT,
            dst_rect: *const RECT,
            dest_window_override: HWND,
            dirty_region: *const RGNDATA,
        ) -> HRESULT;
}
vtable_slot! {
    REAL_CREATE_TEXTURE / call_real_create_texture :
        as fn(
            this: *mut c_void,
            width: u32,
            height: u32,
            levels: u32,
            usage: u32,
            format: D3DFORMAT,
            pool: D3DPOOL,
            pp_texture: *mut *mut c_void,
            p_shared_handle: *mut HANDLE,
        ) -> HRESULT;
}
vtable_slot! {
    REAL_CREATE_VERTEX_BUFFER / call_real_create_vertex_buffer :
        as fn(
            this: *mut c_void,
            length: u32,
            usage: u32,
            fvf: u32,
            pool: D3DPOOL,
            pp_vertex_buffer: *mut *mut c_void,
            p_shared_handle: *mut HANDLE,
        ) -> HRESULT;
}
iat_hook! {
    REAL_DIRECT3D_CREATE9 / real_direct3d_create9 : "Direct3DCreate9"
        as fn(sdk_version: u32) -> *mut c_void;
}

/// IAT-hooks `Direct3DCreate9` against `host`'s import table, forwarding to `Direct3DCreate9Ex`.
/// For defense against tools that IAT-hook the same import after us, game-specific crates should additionally apply
/// [`call_site_rewrite`] for each known live call site. Rewritten sites bypass the IAT entirely.
///
/// # Safety
/// `host` must be a loaded module handle.
pub unsafe fn install(host: HMODULE) {
    unsafe {
        REAL_DIRECT3D_CREATE9.install(host, create_hooked_d3d9);
    }
}

/// Returns a patch that rewrites a `Direct3DCreate9` call site to a 5-byte direct call to our hook.
/// Accepts both 5-byte `E8 disp32` direct-call sites (th10–th12 + th20, where the original call goes through a thunk)
/// and 6-byte `FF 15 disp32` indirect-call sites (th13-th18, where the original call dispatches through the IAT).
/// The replacement is a 5-byte `E8 disp32`; the 6-byte variant gets a trailing NOP.
/// This bypasses any downstream IAT hook (e.g. thcrap) that would otherwise intercept `Direct3DCreate9` from us.
#[must_use]
pub const fn call_site_rewrite<const N: usize>(
    addr: usize,
    expected: &'static [u8; N],
) -> PatchSite {
    PatchSite::call(
        addr,
        expected,
        create_hooked_d3d9 as *mut (),
        "Direct3DCreate9 call-site rewrite",
    )
}

unsafe extern "system" fn create_hooked_d3d9(sdk_version: u32) -> *mut c_void {
    unsafe {
        // The Ex object's first 17 vtable slots are the `IDirect3D9` vtable,
        // so the game can keep using the returned pointer as plain `IDirect3D9` while we get the Ex methods.
        let ex = match Direct3DCreate9Ex(sdk_version) {
            Ok(ex) => ex,
            Err(e) => {
                warn!(
                    kind = "d3d9_init_failed",
                    sdk_version = format_args!("{sdk_version:#x}"),
                    hr = %fmt_hr!(e.code()),
                );
                return null_mut();
            }
        };
        // `into_raw` transfers the ref to the game without `Release`.
        let p_ex = ex.into_raw();
        let Some(p_ex_nn) = NonNull::new(p_ex) else {
            return null_mut();
        };
        install_d3d9_hooks(p_ex_nn);
        info!(kind = "d3d9_init", p_ex = format_args!("{p_ex:p}"));
        p_ex
    }
}

unsafe fn install_d3d9_hooks(d3d9_ex: NonNull<c_void>) {
    unsafe {
        let vtbl: *mut IDirect3D9Ex_Vtbl = *d3d9_ex.as_ptr().cast();
        let Some(vtbl) = NonNull::new(vtbl) else {
            warn!(kind = "d3d9_vtbl_null", p_ex = format_args!("{d3d9_ex:p}"));
            return;
        };

        capture_slot(
            vtbl,
            vtable_field!(IDirect3D9Ex_Vtbl, CreateDeviceEx),
            &REAL_CREATE_DEVICE_EX,
        );
        capture_slot(
            vtbl,
            vtable_field!(IDirect3D9Ex_Vtbl, GetAdapterDisplayModeEx),
            &REAL_GET_ADAPTER_DISPLAY_MODE_EX,
        );
        capture_slot(
            vtbl,
            vtable_field!(IDirect3D9Ex_Vtbl, GetAdapterModeCountEx),
            &REAL_GET_ADAPTER_MODE_COUNT_EX,
        );
        capture_slot(
            vtbl,
            vtable_field!(IDirect3D9Ex_Vtbl, EnumAdapterModesEx),
            &REAL_ENUM_ADAPTER_MODES_EX,
        );
        capture_slot(
            vtbl,
            vtable_field!(IDirect3D9Ex_Vtbl, base__.GetAdapterMonitor),
            &REAL_GET_ADAPTER_MONITOR,
        );

        let result = install_vtable(vtbl, |scope| {
            // `hook_create_device` routes to `CreateDeviceEx` via `REAL_CREATE_DEVICE_EX`
            // rather than chaining through to the displaced `CreateDevice`.
            scope.redirect(
                &REDIRECT_CREATE_DEVICE,
                vtable_field!(IDirect3D9Ex_Vtbl, base__.CreateDevice),
                "IDirect3D9::CreateDevice",
                hook_create_device,
            );
            scope.intercept(
                &REAL_CHECK_DEVICE_FORMAT,
                vtable_field!(IDirect3D9Ex_Vtbl, base__.CheckDeviceFormat),
                "IDirect3D9::CheckDeviceFormat",
                hook_check_device_format,
            );
        });
        info!(kind = "d3d9_hooks_installed", protect_ok = result.is_some());
    }
}

struct PresentParams {
    before: Option<D3DPRESENT_PARAMETERS>,
    after: Option<D3DPRESENT_PARAMETERS>,
    display_mode: Option<D3DDISPLAYMODEEX>,
}

impl PresentParams {
    /// Returns a tuple of the game's requested fullscreen rate, the rate after rewriting,
    /// and whether an active display-mode override changed it.
    fn refresh_override(&self) -> (u32, u32, bool) {
        let original = self.before.map_or(0, |b| b.FullScreen_RefreshRateInHz);
        let chosen = self.after.map_or(0, |a| a.FullScreen_RefreshRateInHz);
        (
            original,
            chosen,
            self.display_mode.is_some() && chosen != original,
        )
    }

    /// Raw pointer to the fullscreen display mode for the Ex calls. This is populated for exclusive fullscreen and null for windowed.
    fn display_mode_ptr(&mut self) -> *mut D3DDISPLAYMODEEX {
        self.display_mode
            .as_mut()
            .map_or(null_mut(), |m| &raw mut *m)
    }
}

/// Snapshots, rewrites, and (if exclusive fullscreen) populates a `D3DDISPLAYMODEEX`
/// for the present-params block needed by both `CreateDeviceEx` and `ResetEx`.
unsafe fn prep_present_params(
    pp: *mut D3DPRESENT_PARAMETERS,
    d3d9: *mut c_void,
    adapter: u32,
    desktop_mode: Option<D3DDISPLAYMODEEX>,
) -> PresentParams {
    unsafe {
        let Some(p) = pp.as_mut() else {
            return PresentParams {
                before: None,
                after: None,
                display_mode: None,
            };
        };
        let before = *p;
        rewrite_present_params(p);
        let display_mode = if p.Windowed.0 == 0 {
            let cfg = CONFIG.get().unwrap();
            apply_refresh_override(p, d3d9, adapter, cfg.display.refresh_rate, desktop_mode);
            Some(build_display_mode_ex(p, p.FullScreen_RefreshRateInHz))
        } else {
            None
        };
        PresentParams {
            before: Some(before),
            after: Some(*p),
            display_mode,
        }
    }
}

fn rewrite_present_params(pp: &mut D3DPRESENT_PARAMETERS) {
    pp.PresentationInterval = D3DPRESENT_INTERVAL_IMMEDIATE.cast_unsigned();
    pp.Flags &= !D3DPRESENTFLAG_LOCKABLE_BACKBUFFER;
    // `CreateDeviceEx` rejects `A8R8G8B8` in fullscreen because it isn't scanout-compatible, so we substitute with `X8R8G8B8`.
    if pp.Windowed.0 == 0 && pp.BackBufferFormat == D3DFMT_A8R8G8B8 {
        let original_format = pp.BackBufferFormat;
        pp.BackBufferFormat = D3DFMT_X8R8G8B8;
        info!(
            kind = "back_buffer_format_substituted",
            original = format_name(original_format),
            original_n = original_format.0,
            forced = format_name(D3DFMT_X8R8G8B8),
            forced_n = D3DFMT_X8R8G8B8.0,
        );
    }
}

/// Overrides the game's hard-coded 60 Hz in `pp.FullScreen_RefreshRateInHz` with the result of [`pick_refresh_rate`],
/// validated against the back buffer format/dimensions.
unsafe fn apply_refresh_override(
    pp: &mut D3DPRESENT_PARAMETERS,
    d3d9: *mut c_void,
    adapter: u32,
    mode: RefreshRateMode,
    desktop_mode: Option<D3DDISPLAYMODEEX>,
) {
    pp.FullScreen_RefreshRateInHz = unsafe {
        pick_refresh_rate(
            d3d9,
            adapter,
            mode,
            desktop_mode,
            pp.BackBufferFormat,
            pp.BackBufferWidth,
            pp.BackBufferHeight,
        )
    };
}

/// Returns the distinct refresh rates advertised by the adapter at exactly `width` × `height` in `format`,
/// plus the count of valid entries. The count is 0 on any failure.
/// At most [`MAX_ENUM_SCAN`] modes are scanned and at most [`MAX_ENUM_RATES`] distinct rates are kept.
unsafe fn enumerate_supported_rates(
    this: *mut c_void,
    adapter: u32,
    format: D3DFORMAT,
    width: u32,
    height: u32,
) -> ([u32; MAX_ENUM_RATES], usize) {
    let mut rates = [0u32; MAX_ENUM_RATES];
    let mut len = 0usize;

    let (Some(count_fn), Some(enum_fn)) = (
        REAL_GET_ADAPTER_MODE_COUNT_EX.try_get(),
        REAL_ENUM_ADAPTER_MODES_EX.try_get(),
    ) else {
        return (rates, 0);
    };

    let filter = D3DDISPLAYMODEFILTER {
        Size: D3DDISPLAYMODEFILTER_SIZE,
        Format: format,
        ScanLineOrdering: D3DSCANLINEORDERING_PROGRESSIVE,
    };

    let count = unsafe { count_fn(this, adapter, &raw const filter) };
    if count > MAX_ENUM_SCAN {
        warn!(kind = "mode_enum_truncated", count, max = MAX_ENUM_SCAN);
    }
    for i in 0..min(count, MAX_ENUM_SCAN) {
        if len == rates.len() {
            warn!(
                kind = "mode_rates_truncated",
                kept = len,
                max = MAX_ENUM_RATES
            );
            break;
        }
        let mut mode = D3DDISPLAYMODEEX {
            Size: D3DDISPLAYMODEEX_SIZE,
            ..D3DDISPLAYMODEEX::default()
        };
        let hr = unsafe { enum_fn(this, adapter, &raw const filter, i, &raw mut mode) };
        if hr.is_ok()
            && mode.Width == width
            && mode.Height == height
            && is_real_refresh_rate(mode.RefreshRate)
            && !rates[..len].contains(&mode.RefreshRate)
        {
            rates[len] = mode.RefreshRate;
            len += 1;
        }
    }
    (rates, len)
}

/// Chooses the fullscreen refresh rate for `mode`, validated against the modes the adapter advertises at `width` x `height` in `format`.
/// `desktop_mode` is the desktop mode already sampled by the caller, or `None` if the read failed.
unsafe fn pick_refresh_rate(
    this: *mut c_void,
    adapter: u32,
    mode: RefreshRateMode,
    desktop_mode: Option<D3DDISPLAYMODEEX>,
    format: D3DFORMAT,
    width: u32,
    height: u32,
) -> u32 {
    let (rates, n) = unsafe { enumerate_supported_rates(this, adapter, format, width, height) };
    let reported_hz = desktop_mode.map(|m| m.RefreshRate);
    let desktop_rate = match reported_hz {
        Some(hz) if is_real_refresh_rate(hz) => hz,
        _ => {
            let device = unsafe { adapter_display_device(this, adapter) };
            let win32_rate = win32_current_refresh_rate(device.as_ref());
            let fallback = win32_rate.unwrap_or(60);
            warn!(
                kind = "pick_refresh_rate_fallback",
                reported_hz = ?reported_hz,
                win32_rate = ?win32_rate,
                fallback,
            );
            fallback
        }
    };

    if let RefreshRateMode::Fixed(target) = mode {
        let target = target.get();
        if n == 0 {
            info!(kind = "refresh_rate_fixed_unvalidated", target_hz = target);
        } else if !rates[..n]
            .iter()
            .any(|&r| r == target || normalize_reported_rate(r) == target)
        {
            error!(
                kind = "refresh_rate_fixed_unsupported",
                target_hz = target,
                supported = ?&rates[..n],
            );
        }
    }

    let chosen = select_refresh_rate(mode, &rates[..n], desktop_rate);
    info!(
        kind = "refresh_rate_decision",
        desktop_rate_hz = desktop_rate,
        supported_n = n,
        chosen_hz = chosen,
    );
    chosen
}

/// Applies the refresh-rate policy against the adapter's `supported` rates at the target resolution.
/// `desktop_rate` is the raw reported rate.
/// - `Native`: see `native_rate`.
/// - `NativeMultiple`: the highest supported multiple of 60 not above the desktop rate,
///   or the `Native` value if no multiple of 60 is available.
/// - `Fixed`: the supported rate equal to the target, else one that normalizes to it
///   (e.g. 119 for a `Fixed(120)`), else the target unchanged.
fn select_refresh_rate(mode: RefreshRateMode, supported: &[u32], desktop_rate: u32) -> u32 {
    match mode {
        RefreshRateMode::Native => native_rate(supported, desktop_rate),
        RefreshRateMode::NativeMultiple => supported
            .iter()
            .copied()
            .filter(|&r| {
                let hz = normalize_reported_rate(r);
                hz.is_multiple_of(60) && hz <= normalize_reported_rate(desktop_rate)
            })
            .max()
            .unwrap_or_else(|| native_rate(supported, desktop_rate)),

        RefreshRateMode::Fixed(target) => {
            let target = target.get();
            supported
                .iter()
                .copied()
                .find(|&r| r == target)
                .or_else(|| {
                    supported
                        .iter()
                        .copied()
                        .find(|&r| normalize_reported_rate(r) == target)
                })
                .unwrap_or(target)
        }
    }
}

/// The highest supported rate at or below the desktop rate (after NTSC-derived normalization),
/// else the lowest supported rate, else the raw desktop rate when the adapter advertises nothing.
fn native_rate(supported: &[u32], desktop_rate: u32) -> u32 {
    let ceiling = normalize_reported_rate(desktop_rate);
    supported
        .iter()
        .copied()
        .filter(|&r| normalize_reported_rate(r) <= ceiling)
        .max()
        .or_else(|| supported.iter().copied().min())
        .unwrap_or(desktop_rate)
}

// 0 and 1 are magic values meaning "hardware default rate," not real refresh rates.
fn is_real_refresh_rate(rate: u32) -> bool {
    rate > 1
}

/// Rounds up NTSC-derived (1000/1001) refresh rates like 59.94, 119.88, 143.86, 239.76, etc.
fn normalize_reported_rate(rate: u32) -> u32 {
    if rate % 12 == 11 { rate + 1 } else { rate }
}

/// Resolves the GDI device name of `adapter`'s monitor for Win32 display queries.
unsafe fn adapter_display_device(d3d9: *mut c_void, adapter: u32) -> Option<[u16; 32]> {
    let monitor_fn = REAL_GET_ADAPTER_MONITOR.try_get()?;
    let monitor = unsafe { monitor_fn(d3d9, adapter) };
    if monitor.is_invalid() {
        return None;
    }
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: u32::try_from(size_of::<MONITORINFOEXW>()).unwrap_or(0),
            ..MONITORINFO::default()
        },
        ..MONITORINFOEXW::default()
    };
    let ok = unsafe { GetMonitorInfoW(monitor.0, (&raw mut info).cast()) };
    (ok != 0).then_some(info.szDevice)
}

/// Win32 fallback for refresh-rate query via `EnumDisplaySettingsExW`, scoped to the GDI display name `device` when given,
/// else the primary display. Returns `None` if the call fails or if a real refresh rate is not returned.
fn win32_current_refresh_rate(device: Option<&[u16; 32]>) -> Option<u32> {
    let mut dm = DEVMODEW {
        dmSize: u16::try_from(size_of::<DEVMODEW>()).unwrap_or(0),
        ..DEVMODEW::default()
    };
    let name = device.map_or(null(), |d| d.as_ptr());
    let ok = unsafe { EnumDisplaySettingsExW(name, ENUM_CURRENT_SETTINGS, &raw mut dm, 0) };
    if ok == 0 {
        return None;
    }
    let hz = dm.dmDisplayFrequency;
    is_real_refresh_rate(hz).then_some(hz)
}

/// Populates a `D3DDISPLAYMODEEX` from the present-params back buffer and an explicit refresh rate.
/// The Ex `CreateDevice` and `Reset` signatures require a fully-filled struct for exclusive fullscreen and a null pointer for windowed
/// (a non-null struct there is `D3DERR_INVALIDCALL`), so callers should only use this when `Windowed == FALSE` holds.
fn build_display_mode_ex(pp: &D3DPRESENT_PARAMETERS, refresh: u32) -> D3DDISPLAYMODEEX {
    D3DDISPLAYMODEEX {
        Size: D3DDISPLAYMODEEX_SIZE,
        Width: pp.BackBufferWidth,
        Height: pp.BackBufferHeight,
        RefreshRate: refresh,
        Format: pp.BackBufferFormat,
        ScanLineOrdering: D3DSCANLINEORDERING_PROGRESSIVE,
    }
}

/// D3D9Ex rejects `D3DPOOL_MANAGED` with `INVALIDCALL`, so we substitute the closest valid pair on every `Create*Texture`
/// and `CreateVertexBuffer` path where the game or D3DX9 hands us `MANAGED`. Returns whether a translation happened.
pub(crate) fn translate_managed_pool(pool: &mut D3DPOOL, usage: &mut u32) -> bool {
    if *pool == D3DPOOL_MANAGED {
        *pool = D3DPOOL_DEFAULT;
        *usage |= D3DUSAGE_DYNAMIC.cast_unsigned();
        true
    } else {
        false
    }
}

/// Reads the object pointer from a `Create*`-style `*mut *mut c_void` out param, returning null when the out param itself is null.
pub(crate) unsafe fn out_ptr(pp: *mut *mut c_void) -> *const c_void {
    if pp.is_null() {
        null()
    } else {
        unsafe { (*pp).cast_const() }
    }
}

pub(crate) fn format_name(f: D3DFORMAT) -> &'static str {
    match_named!(
        f,
        D3DFMT_UNKNOWN,
        D3DFMT_R8G8B8,
        D3DFMT_A8R8G8B8,
        D3DFMT_X8R8G8B8,
        D3DFMT_R5G6B5,
        D3DFMT_X1R5G5B5,
        D3DFMT_A1R5G5B5,
        D3DFMT_A4R4G4B4,
        D3DFMT_R3G3B2,
        D3DFMT_A8,
        D3DFMT_A8R3G3B2,
        D3DFMT_X4R4G4B4,
        D3DFMT_A2B10G10R10,
        D3DFMT_A8B8G8R8,
        D3DFMT_X8B8G8R8,
        D3DFMT_G16R16,
        D3DFMT_A2R10G10B10,
        D3DFMT_A16B16G16R16,
        D3DFMT_D16_LOCKABLE,
        D3DFMT_D32,
        D3DFMT_D15S1,
        D3DFMT_D24S8,
        D3DFMT_D24X8,
        D3DFMT_D24X4S4,
        D3DFMT_D16,
        D3DFMT_D32F_LOCKABLE,
        D3DFMT_D24FS8,
    )
}

#[allow(clippy::too_many_arguments)]
unsafe fn create_device_once(
    this: *mut c_void,
    adapter: u32,
    device_type: D3DDEVTYPE,
    focus_window: HWND,
    behavior_flags_in: u32,
    behavior_flags: u32,
    pp: *mut D3DPRESENT_PARAMETERS,
    display_mode_ptr: *mut D3DDISPLAYMODEEX,
    returned_device: *mut *mut c_void,
    attempt: u32,
) -> (HRESULT, *mut c_void) {
    unsafe {
        info!(
            kind = "create_device_call",
            attempt,
            this = format_args!("{this:p}"),
            adapter,
            device_type = ?device_type,
            behavior_flags_in = format_args!("{behavior_flags_in:#x}"),
            behavior_flags = format_args!("{behavior_flags:#x}"),
            focus_window = format_args!("{:p}", focus_window.0),
            pp = ?pp.as_ref(),
            display_mode = if display_mode_ptr.is_null() { "null" } else { "set" },
        );
        let hr = call_real_create_device_ex(
            this,
            adapter,
            device_type,
            focus_window,
            behavior_flags,
            pp,
            display_mode_ptr,
            returned_device,
        );
        let dev: *mut c_void = if returned_device.is_null() {
            null_mut()
        } else {
            *returned_device
        };
        info!(
            kind = "create_device_result",
            attempt,
            hr = fmt_hr!(hr),
            device = format_args!("{dev:p}"),
        );
        (hr, dev)
    }
}

unsafe extern "system" fn hook_create_device(
    this: *mut c_void,
    adapter: u32,
    device_type: D3DDEVTYPE,
    focus_window: HWND,
    behavior_flags: u32,
    pp: *mut D3DPRESENT_PARAMETERS,
    returned_device: *mut *mut c_void,
) -> HRESULT {
    let tok = MainToken::new();
    unsafe {
        let behavior_flags_in = behavior_flags;
        let behavior_flags = rewrite_behavior_flags(behavior_flags);

        let desktop_before = sample_for_degradation_check(this, adapter, pp);

        let mut prep = prep_present_params(pp, this, adapter, desktop_before);

        let mut dev: *mut c_void = null_mut();
        let hr = run_with_refresh_failsafe(
            pp,
            &mut prep,
            desktop_before,
            "create_device_refresh_failsafe",
            |display_mode_ptr, attempt| {
                let (hr, d) = create_device_once(
                    this,
                    adapter,
                    device_type,
                    focus_window,
                    behavior_flags_in,
                    behavior_flags,
                    pp,
                    display_mode_ptr,
                    returned_device,
                    attempt,
                );
                dev = d;
                hr
            },
        );

        if hr.is_ok()
            && let Some(dev) = NonNull::new(dev)
        {
            // Apparently D3D9Ex breaks the window style on `CreateDeviceEx`.
            // OILP's `CreateDevice_hook` applies the same `SWP_SHOWWINDOW` fix.
            SetWindowPos(
                focus_window.0,
                null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOSIZE | SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );

            install_device_hooks(dev);
            post_device_alive(&tok, dev);

            if let Some(before) = desktop_before {
                warn_if_exclusive_degraded(this, adapter, before, &prep);
            }
        }
        hr
    }
}

/// Adds `D3DCREATE_MULTITHREADED`.
fn rewrite_behavior_flags(flags: u32) -> u32 {
    flags | D3DCREATE_MULTITHREADED.cast_unsigned()
}

/// Reads the display mode of `adapter`. Returns `None` on failure.
unsafe fn sample_adapter_display_mode(d3d9: *mut c_void, adapter: u32) -> Option<D3DDISPLAYMODEEX> {
    unsafe {
        let mut current = D3DDISPLAYMODEEX {
            Size: D3DDISPLAYMODEEX_SIZE,
            ..D3DDISPLAYMODEEX::default()
        };
        let hr = call_real_get_adapter_display_mode_ex(d3d9, adapter, &raw mut current, null_mut());
        if hr.is_ok() { Some(current) } else { None }
    }
}

/// Captures the adapter mode before a `CreateDevice` or `Reset` call, but only when exclusive fullscreen is requested
/// (i.e. `pp.Windowed == FALSE`). Returns `None` if no sample is needed or the read failed.
unsafe fn sample_for_degradation_check(
    d3d9: *mut c_void,
    adapter: u32,
    pp: *mut D3DPRESENT_PARAMETERS,
) -> Option<D3DDISPLAYMODEEX> {
    let pp = unsafe { pp.as_ref()? };
    if pp.Windowed.0 != 0 {
        return None;
    }
    unsafe { sample_adapter_display_mode(d3d9, adapter) }
}

/// Heuristic warning for situations where exclusive fullscreen is silently degraded to windowed presentation.
/// It's possible for an adapter to not actually move to the requested mode even if `CreateDeviceEx` returns `S_OK`,
/// so we compare the desktop mode before and after device creation.
///
/// The check is skipped when the requested presentation parameters matches the desktop mode:
/// no mode switch was needed, and exclusive fullscreen vs. windowed are indistinguishable in this case.
unsafe fn warn_if_exclusive_degraded(
    d3d9: *mut c_void,
    adapter: u32,
    before: D3DDISPLAYMODEEX,
    prep: &PresentParams,
) {
    let Some(after_pp) = prep.after else { return };
    let req_w = after_pp.BackBufferWidth;
    let req_h = after_pp.BackBufferHeight;
    let req_hz = after_pp.FullScreen_RefreshRateInHz;
    let requested_matches_desktop = req_w == before.Width
        && req_h == before.Height
        && (req_hz == 0
            || normalize_reported_rate(req_hz) == normalize_reported_rate(before.RefreshRate));
    if requested_matches_desktop {
        return;
    }
    let Some(after) = (unsafe { sample_adapter_display_mode(d3d9, adapter) }) else {
        return;
    };
    let adapter_unchanged = after.Width == before.Width
        && after.Height == before.Height
        && after.RefreshRate == before.RefreshRate;
    if adapter_unchanged {
        warn!(
            kind = "exclusive_fullscreen_suspect_degraded",
            requested_width = req_w,
            requested_height = req_h,
            requested_hz = req_hz,
            desktop_width = before.Width,
            desktop_height = before.Height,
            desktop_hz = before.RefreshRate,
            hint = "CreateDeviceEx returned OK but the adapter mode didn't change. \
                    If the session looked broken, please file an issue including this log!",
        );
    }
}

/// Substitutes `X8R8G8B8` for `A8R8G8B8` when a game passes the latter as `AdapterFormat`.
///
/// `A8R8G8B8` isn't a displayable format. Vanilla D3D9 silently accepts it and returns `D3D_OK`,
/// but D3D9Ex is strict and returns `D3DERR_NOTAVAILABLE`. Games written against the lenient behavior can fall down
/// a reduced-color-mode path that fails subsequent resource creation. The substitution gives the call its intended meaning.
unsafe extern "system" fn hook_check_device_format(
    this: *mut c_void,
    adapter: u32,
    device_type: D3DDEVTYPE,
    adapter_format: D3DFORMAT,
    usage: u32,
    rtype: D3DRESOURCETYPE,
    check_format: D3DFORMAT,
) -> HRESULT {
    unsafe {
        let forwarded_adapter_fmt = if adapter_format == D3DFMT_A8R8G8B8 {
            D3DFMT_X8R8G8B8
        } else {
            adapter_format
        };

        let substituted = forwarded_adapter_fmt != adapter_format;

        let hr = call_real_check_device_format(
            this,
            adapter,
            device_type,
            forwarded_adapter_fmt,
            usage,
            rtype,
            check_format,
        );

        let forwarded_format = if substituted {
            format_name(forwarded_adapter_fmt)
        } else {
            ""
        };

        log_at!(substituted => info / debug,
            kind = "check_device_format",
            adapter,
            device_type = device_type.0,
            adapter_format = format_name(adapter_format),
            adapter_format_n = adapter_format.0,
            substituted,
            forwarded_format,
            forwarded_format_n = forwarded_adapter_fmt.0,
            usage = format_args!("{usage:#x}"),
            rtype = rtype.0,
            check_format = format_name(check_format),
            check_format_n = check_format.0,
            hr = fmt_hr!(hr),
        );

        hr
    }
}

unsafe fn install_device_hooks(dev: NonNull<c_void>) {
    unsafe {
        let vtbl = *dev.as_ptr().cast();
        let Some(vtbl) = NonNull::new(vtbl) else {
            warn!(kind = "device_vtbl_null", dev = format_args!("{dev:p}"));
            return;
        };

        capture_slot(
            vtbl,
            vtable_field!(IDirect3DDevice9Ex_Vtbl, ResetEx),
            &REAL_RESET_EX,
        );
        capture_slot(
            vtbl,
            vtable_field!(IDirect3DDevice9Ex_Vtbl, SetMaximumFrameLatency),
            &REAL_SET_MAX_FRAME_LATENCY,
        );
        capture_slot(
            vtbl,
            vtable_field!(IDirect3DDevice9Ex_Vtbl, SetGPUThreadPriority),
            &REAL_SET_GPU_THREAD_PRIORITY,
        );

        let result = install_vtable(vtbl, |scope| {
            scope.intercept(
                &REAL_RESET,
                vtable_field!(IDirect3DDevice9Ex_Vtbl, base__.Reset),
                "Reset",
                hook_reset,
            );
            scope.intercept(
                &REAL_PRESENT,
                vtable_field!(IDirect3DDevice9Ex_Vtbl, base__.Present),
                "Present",
                hook_present,
            );
            scope.intercept(
                &REAL_CREATE_TEXTURE,
                vtable_field!(IDirect3DDevice9Ex_Vtbl, base__.CreateTexture),
                "CreateTexture",
                hook_create_texture,
            );
            scope.intercept(
                &REAL_CREATE_VERTEX_BUFFER,
                vtable_field!(IDirect3DDevice9Ex_Vtbl, base__.CreateVertexBuffer),
                "CreateVertexBuffer",
                hook_create_vertex_buffer,
            );
        });
        info!(
            kind = "d3d9_device_hooks_installed",
            protect_ok = result.is_some()
        );
    }
}

/// `SetMaximumFrameLatency(1)` caps the GPU input queue at 1 (default 3) so frames spend less time enqueued before display,
/// shaving up to two frames of end-to-end latency. `SetGPUThreadPriority(7)` raises the device's WDDM GPU-scheduling priority
/// so its command submissions are preferred over other processes' GPU work.
unsafe fn apply_device_ex_tunables(dev: NonNull<c_void>) {
    unsafe {
        let latency_hr = call_real_set_max_frame_latency(dev.as_ptr(), 1);
        info!(
            kind = "set_max_frame_latency",
            value = 1,
            hr = %fmt_hr!(latency_hr),
        );
        let gpu_pri_hr = call_real_set_gpu_thread_priority(dev.as_ptr(), 7);
        info!(
            kind = "set_gpu_thread_priority",
            value = 7,
            hr = %fmt_hr!(gpu_pri_hr),
        );
    }
}

/// Re-applies the device tunables, since D3D9Ex preserves them across `Reset` but a translation layer might not.
/// Also refreshes `ACTIVE_DEVICE`. Fires after successful `CreateDeviceEx` and successful `Reset` / `ResetEx`.
unsafe fn post_device_alive(tok: &MainToken, dev: NonNull<c_void>) {
    unsafe { apply_device_ex_tunables(dev) };
    on_post_create_device(tok, dev.as_ptr());
}

unsafe extern "system" fn hook_present(
    this: *mut c_void,
    src_rect: *const RECT,
    dst_rect: *const RECT,
    dest_window_override: HWND,
    dirty_region: *const RGNDATA,
) -> HRESULT {
    let tok = MainToken::new();
    unsafe {
        let pacer = PACER.get().unwrap();
        let observed_mode = replay_mode(&tok);

        if MODE.get(&tok) != observed_mode {
            MODE.set(&tok, observed_mode);
            let cfg = CONFIG.get().unwrap();
            let policy = match observed_mode {
                ReplayMode::Normal => PacingPolicy::LiveInput {
                    target_fps: cfg.framerate.game_fps,
                },
                ReplayMode::Skip => PacingPolicy::InternalCadence {
                    target_fps: cfg.framerate.replay_skip_fps,
                },
                ReplayMode::Slow => PacingPolicy::InternalCadence {
                    target_fps: cfg.framerate.replay_slow_fps,
                },
            };
            info!(
                kind = "replay_mode_change",
                mode = ?observed_mode,
                target_fps = policy.target_fps(),
                frame = PRESENT_COUNT.load(Ordering::Relaxed),
            );
            pacer.apply_policy(&tok, policy);
        }
        pacer.wait(&tok);

        on_pre_present(&tok);

        // We increment before `Present` so `PRESENT_COUNT` uses the in-flight frame.
        // This way, a crash inside `Present` leaves the count at the attempted frame, not the last completed.
        PRESENT_COUNT.fetch_add(1, Ordering::Relaxed);

        let hr = call_real_present(this, src_rect, dst_rect, dest_window_override, dirty_region);
        log_present_outcome(&tok, hr);
        hr
    }
}

/// Emits a log event whenever the result of `Present` differs from the previous call's result.
fn log_present_outcome(tok: &MainToken, hr: HRESULT) {
    let prev = LAST_PRESENT.get(tok);
    if hr == prev.hr {
        return;
    }

    let frame = PRESENT_COUNT.load(Ordering::Relaxed);

    log_at!(hr.is_ok() => info / warn,
        kind = "present_result_changed",
        hr = fmt_hr!(hr),
        prev_hr = fmt_hr!(prev.hr),
        frames_in_prev = frame.wrapping_sub(prev.since_frame),
        frame,
    );

    LAST_PRESENT.set(
        tok,
        PresentState {
            hr,
            since_frame: frame,
        },
    );
}

unsafe fn reset_once(
    this: *mut c_void,
    pp: *mut D3DPRESENT_PARAMETERS,
    display_mode_ptr: *mut D3DDISPLAYMODEEX,
    use_reset_ex: bool,
    attempt: u32,
) -> HRESULT {
    unsafe {
        info!(
            kind = "reset_call",
            attempt,
            this = format_args!("{this:p}"),
            pp = ?pp.as_ref(),
            display_mode = if display_mode_ptr.is_null() { "null" } else { "set" },
            path = if use_reset_ex { "ResetEx" } else { "Reset" },
        );
        // Plain `Reset` on Alt+Enter crashed for a tester, but `ResetEx` didn't.
        let hr = if use_reset_ex {
            call_real_reset_ex(this, pp, display_mode_ptr)
        } else {
            call_real_reset(this, pp)
        };
        info!(kind = "reset_result", attempt, hr = fmt_hr!(hr));
        hr
    }
}

/// The parent `IDirect3D9Ex` and adapter ordinal of a live device.
struct DeviceParent {
    /// The reference returned by `GetDirect3D`.
    d3d9: IDirect3D9,
    adapter: u32,
}

impl DeviceParent {
    /// # Safety
    /// `dev` must be a live `IDirect3DDevice9Ex`.
    unsafe fn new(dev: *mut c_void) -> Self {
        unsafe {
            let dev = IDirect3DDevice9Ex::from_raw_borrowed(&dev)
                .expect("hook_reset called on a null device");
            let d3d9 = dev
                .GetDirect3D()
                .expect("GetDirect3D failed on a live device");

            let mut cp = D3DDEVICE_CREATION_PARAMETERS::default();
            dev.GetCreationParameters(&raw mut cp)
                .expect("GetCreationParameters failed on a live device");

            Self {
                d3d9,
                adapter: cp.AdapterOrdinal,
            }
        }
    }
}

unsafe extern "system" fn hook_reset(this: *mut c_void, pp: *mut D3DPRESENT_PARAMETERS) -> HRESULT {
    let tok = MainToken::new();
    on_pre_reset(&tok);
    unsafe {
        let parent = DeviceParent::new(this);
        let desktop_before = sample_for_degradation_check(parent.d3d9.as_raw(), parent.adapter, pp);
        // We reapply refresh rate selection so runtime rate toggles take effect at the next `Reset`.
        let mut prep =
            prep_present_params(pp, parent.d3d9.as_raw(), parent.adapter, desktop_before);
        let use_reset_ex = REAL_RESET_EX.try_get().is_some();

        let hr = run_with_refresh_failsafe(
            pp,
            &mut prep,
            desktop_before,
            "reset_refresh_failsafe",
            |display_mode_ptr, attempt| {
                reset_once(this, pp, display_mode_ptr, use_reset_ex, attempt)
            },
        );

        if hr.is_ok() {
            // SAFETY: `this` was already dereferenced by `call_real_reset` / `call_real_reset_ex` above.
            let dev = NonNull::new_unchecked(this);
            post_device_alive(&tok, dev);
            if let Some(before) = desktop_before {
                warn_if_exclusive_degraded(parent.d3d9.as_raw(), parent.adapter, before, &prep);
            }
        }
        hr
    }
}

/// Runs a `CreateDeviceEx` or `ResetEx` call with the refresh-override failsafe.
///
/// `attempt` receives the fullscreen display-mode pointer and the attempt index.
/// If the first attempt fails while a refresh override is active and the error isn't a transient device error,
/// the override is rolled back to the game's original rate and the call is retried once.
unsafe fn run_with_refresh_failsafe(
    pp: *mut D3DPRESENT_PARAMETERS,
    prep: &mut PresentParams,
    desktop_before: Option<D3DDISPLAYMODEEX>,
    failsafe_kind: &'static str,
    mut attempt: impl FnMut(*mut D3DDISPLAYMODEEX, u32) -> HRESULT,
) -> HRESULT {
    let (original_refresh, chosen_refresh, overrode_fs) = prep.refresh_override();
    info!(kind = "present_rewrite", pp_before = ?prep.before, pp_after = ?prep.after);
    let hr = attempt(prep.display_mode_ptr(), 0);
    if hr.is_ok() || !overrode_fs {
        return hr;
    }
    if is_transient_device_error(hr) {
        info!(
            kind = "refresh_failsafe_declined",
            context = failsafe_kind,
            hr = fmt_hr!(hr),
            chosen_hz = chosen_refresh,
        );
        return hr;
    }
    // The game's original rate rolls back to `pp` as-is (0 is legal there),
    // but an Ex display mode needs a real rate, so 0 falls back to the sampled desktop rate.
    let mode_refresh = if is_real_refresh_rate(original_refresh) {
        original_refresh
    } else {
        desktop_before
            .map(|m| m.RefreshRate)
            .filter(|&r| is_real_refresh_rate(r))
            .unwrap_or(60)
    };
    let display_mode_ptr = unsafe {
        rollback_refresh_override(
            pp,
            prep,
            original_refresh,
            mode_refresh,
            chosen_refresh,
            failsafe_kind,
            hr,
        )
    };
    attempt(display_mode_ptr, 1)
}

fn is_transient_device_error(hr: HRESULT) -> bool {
    matches!(
        hr,
        D3DERR_DEVICELOST | D3DERR_DEVICEREMOVED | D3DERR_DEVICEHUNG | D3DERR_OUTOFVIDEOMEMORY
    )
}

/// Rewrites `pp` and `prep` back to the game's original refresh rate after a failed attempt.
/// `mode_refresh` is the rate for the rebuilt Ex display mode.
unsafe fn rollback_refresh_override(
    pp: *mut D3DPRESENT_PARAMETERS,
    prep: &mut PresentParams,
    original_refresh: u32,
    mode_refresh: u32,
    chosen_refresh: u32,
    kind: &'static str,
    first_hr: HRESULT,
) -> *mut D3DDISPLAYMODEEX {
    unsafe {
        if let Some(clean) = prep.after {
            // We discard any driver write-back from the failed attempt.
            *pp = clean;
        }
        (*pp).FullScreen_RefreshRateInHz = original_refresh;
        prep.display_mode = Some(build_display_mode_ex(&*pp, mode_refresh));
        prep.after = Some(*pp);
        warn!(
            kind = kind,
            from_hz = chosen_refresh,
            to_hz = original_refresh,
            display_mode_hz = mode_refresh,
            first_hr = fmt_hr!(first_hr),
        );
        prep.display_mode_ptr()
    }
}

unsafe extern "system" fn hook_create_texture(
    this: *mut c_void,
    width: u32,
    height: u32,
    levels: u32,
    mut usage: u32,
    format: D3DFORMAT,
    mut pool: D3DPOOL,
    pp_texture: *mut *mut c_void,
    p_shared_handle: *mut HANDLE,
) -> HRESULT {
    unsafe {
        let usage_orig = usage;
        let pool_orig = pool;
        translate_managed_pool(&mut pool, &mut usage);
        let hr = call_real_create_texture(
            this,
            width,
            height,
            levels,
            usage,
            format,
            pool,
            pp_texture,
            p_shared_handle,
        );
        let returned = out_ptr(pp_texture);

        log_at!(hr.is_ok() => debug / warn,
            kind = "create_texture",
            width,
            height,
            levels,
            format = ?format,
            pool_in = ?pool_orig,
            pool_out = ?pool,
            usage_in = format_args!("{usage_orig:#x}"),
            usage_out = format_args!("{usage:#x}"),
            hr = fmt_hr!(hr),
            ptr = format_args!("{returned:p}"),
        );

        hr
    }
}

unsafe extern "system" fn hook_create_vertex_buffer(
    this: *mut c_void,
    length: u32,
    mut usage: u32,
    fvf: u32,
    mut pool: D3DPOOL,
    pp_vertex_buffer: *mut *mut c_void,
    p_shared_handle: *mut HANDLE,
) -> HRESULT {
    unsafe {
        let usage_orig = usage;
        let pool_orig = pool;
        translate_managed_pool(&mut pool, &mut usage);
        let hr = call_real_create_vertex_buffer(
            this,
            length,
            usage,
            fvf,
            pool,
            pp_vertex_buffer,
            p_shared_handle,
        );
        let returned = out_ptr(pp_vertex_buffer);

        log_at!(hr.is_ok() => debug / warn,
            kind = "create_vbuffer",
            length,
            fvf = format_args!("{fvf:#x}"),
            pool_in = ?pool_orig,
            pool_out = ?pool,
            usage_in = format_args!("{usage_orig:#x}"),
            usage_out = format_args!("{usage:#x}"),
            hr = fmt_hr!(hr),
            ptr = format_args!("{returned:p}"),
        );

        hr
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZero;
    use windows::Win32::Graphics::Direct3D9::{
        D3DCREATE_HARDWARE_VERTEXPROCESSING, D3DFMT_R5G6B5, D3DPOOL_SCRATCH, D3DPOOL_SYSTEMMEM,
        D3DPRESENT_INTERVAL_ONE, D3DPRESENTFLAG_DISCARD_DEPTHSTENCIL, D3DSWAPEFFECT_DISCARD,
        D3DUSAGE_WRITEONLY,
    };

    fn nz(n: u32) -> NonZero<u32> {
        NonZero::new(n).unwrap()
    }

    #[test]
    fn rewrite_present_params_forces_immediate_interval() {
        for original in [
            0u32,
            D3DPRESENT_INTERVAL_ONE.cast_unsigned(),
            D3DPRESENT_INTERVAL_IMMEDIATE.cast_unsigned(),
        ] {
            let mut pp = D3DPRESENT_PARAMETERS {
                PresentationInterval: original,
                ..Default::default()
            };
            rewrite_present_params(&mut pp);
            assert_eq!(
                pp.PresentationInterval,
                D3DPRESENT_INTERVAL_IMMEDIATE.cast_unsigned(),
                "input interval {original:#x}",
            );
        }
    }

    #[test]
    fn rewrite_present_params_strips_lockable_back_buffer() {
        let mut pp = D3DPRESENT_PARAMETERS {
            Flags: D3DPRESENTFLAG_LOCKABLE_BACKBUFFER,
            ..Default::default()
        };
        rewrite_present_params(&mut pp);
        assert_eq!(pp.Flags, 0);

        let mut pp = D3DPRESENT_PARAMETERS {
            Flags: D3DPRESENTFLAG_LOCKABLE_BACKBUFFER | D3DPRESENTFLAG_DISCARD_DEPTHSTENCIL,
            ..Default::default()
        };
        rewrite_present_params(&mut pp);
        assert_eq!(pp.Flags, D3DPRESENTFLAG_DISCARD_DEPTHSTENCIL);
    }

    #[test]
    fn rewrite_present_params_preserves_other_fields() {
        // Locks in the current contract of only touching interval, lockable flag, and back buffer format.
        // TODO: The FLIPEX-direct backlog item will modify `SwapEffect` and `BackBufferCount` here, so this test should be updated.
        let baseline = D3DPRESENT_PARAMETERS {
            BackBufferWidth: 1280,
            BackBufferHeight: 960,
            BackBufferFormat: D3DFMT_X8R8G8B8,
            BackBufferCount: 1,
            SwapEffect: D3DSWAPEFFECT_DISCARD,
            Windowed: true.into(),
            EnableAutoDepthStencil: true.into(),
            AutoDepthStencilFormat: D3DFMT_X8R8G8B8,
            FullScreen_RefreshRateInHz: 144,
            ..Default::default()
        };
        let mut pp = baseline;
        rewrite_present_params(&mut pp);
        assert_eq!(pp.BackBufferWidth, baseline.BackBufferWidth);
        assert_eq!(pp.BackBufferHeight, baseline.BackBufferHeight);
        assert_eq!(pp.BackBufferFormat, baseline.BackBufferFormat);
        assert_eq!(pp.BackBufferCount, baseline.BackBufferCount);
        assert_eq!(pp.SwapEffect, baseline.SwapEffect);
        assert_eq!(pp.Windowed, baseline.Windowed);
        assert_eq!(pp.EnableAutoDepthStencil, baseline.EnableAutoDepthStencil);
        assert_eq!(pp.AutoDepthStencilFormat, baseline.AutoDepthStencilFormat);
        assert_eq!(
            pp.FullScreen_RefreshRateInHz,
            baseline.FullScreen_RefreshRateInHz,
        );
    }

    #[test]
    fn rewrite_present_params_substitutes_only_fullscreen_a8r8g8b8() {
        let cases: &[(D3DFORMAT, bool, D3DFORMAT)] = &[
            // A8R8G8B8 is a valid windowed back buffer but an invalid fullscreen adapter format.
            (D3DFMT_A8R8G8B8, true, D3DFMT_A8R8G8B8),
            (D3DFMT_A8R8G8B8, false, D3DFMT_X8R8G8B8),
            // X8R8G8B8 is passed through.
            (D3DFMT_X8R8G8B8, true, D3DFMT_X8R8G8B8),
            (D3DFMT_X8R8G8B8, false, D3DFMT_X8R8G8B8),
            // 16-bit formats are passed through.
            (D3DFMT_R5G6B5, true, D3DFMT_R5G6B5),
            (D3DFMT_R5G6B5, false, D3DFMT_R5G6B5),
            (D3DFMT_X1R5G5B5, true, D3DFMT_X1R5G5B5),
            (D3DFMT_A1R5G5B5, false, D3DFMT_A1R5G5B5),
        ];
        for &(src, windowed, expected) in cases {
            let mut pp = D3DPRESENT_PARAMETERS {
                BackBufferFormat: src,
                Windowed: windowed.into(),
                ..Default::default()
            };
            rewrite_present_params(&mut pp);
            assert_eq!(
                pp.BackBufferFormat, expected,
                "src={src:?} windowed={windowed}",
            );
        }
    }

    #[test]
    fn translate_managed_pool_swaps_managed_for_default_dynamic() {
        let mut pool = D3DPOOL_MANAGED;
        let mut usage = 0;
        assert!(translate_managed_pool(&mut pool, &mut usage));
        assert_eq!(pool, D3DPOOL_DEFAULT);
        assert_eq!(usage, D3DUSAGE_DYNAMIC.cast_unsigned());
    }

    #[test]
    fn translate_managed_pool_preserves_existing_usage_bits() {
        let mut pool = D3DPOOL_MANAGED;
        let mut usage = D3DUSAGE_WRITEONLY.cast_unsigned();
        assert!(translate_managed_pool(&mut pool, &mut usage));
        assert_eq!(pool, D3DPOOL_DEFAULT);
        assert_eq!(
            usage,
            D3DUSAGE_DYNAMIC.cast_unsigned() | D3DUSAGE_WRITEONLY.cast_unsigned(),
        );
    }

    #[test]
    fn translate_managed_pool_leaves_non_managed_pools_alone() {
        for pool_in in [D3DPOOL_DEFAULT, D3DPOOL_SYSTEMMEM, D3DPOOL_SCRATCH] {
            let mut pool = pool_in;
            let mut usage = 0;
            assert!(!translate_managed_pool(&mut pool, &mut usage));
            assert_eq!(pool, pool_in);
            assert_eq!(usage, 0);
        }
    }

    #[test]
    fn rewrite_behavior_flags_adds_multithreaded() {
        let mt = D3DCREATE_MULTITHREADED.cast_unsigned();
        let game_flags = D3DCREATE_HARDWARE_VERTEXPROCESSING.cast_unsigned();
        let out = rewrite_behavior_flags(game_flags);
        assert_eq!(out & mt, mt);
        assert_eq!(out & !mt, game_flags);
        assert_eq!(rewrite_behavior_flags(out), out);
    }

    #[test]
    fn build_display_mode_ex_copies_pp_fields_and_uses_param_refresh() {
        let pp = D3DPRESENT_PARAMETERS {
            BackBufferWidth: 1280,
            BackBufferHeight: 960,
            BackBufferFormat: D3DFMT_X8R8G8B8,
            // This is deliberately wrong on `pp`. `build_display_mode_ex` must use the explicit `refresh` arg, not this field.
            FullScreen_RefreshRateInHz: 999,
            ..Default::default()
        };
        let mode = build_display_mode_ex(&pp, 120);
        assert_eq!(mode.Size, D3DDISPLAYMODEEX_SIZE);
        assert_eq!(mode.Width, 1280);
        assert_eq!(mode.Height, 960);
        assert_eq!(mode.Format, D3DFMT_X8R8G8B8);
        assert_eq!(mode.RefreshRate, 120);
        assert_eq!(mode.ScanLineOrdering, D3DSCANLINEORDERING_PROGRESSIVE);
    }

    #[test]
    fn format_name_known_and_unknown() {
        assert_eq!(format_name(D3DFMT_X8R8G8B8), "D3DFMT_X8R8G8B8");
        assert_eq!(format_name(D3DFMT_A8R8G8B8), "D3DFMT_A8R8G8B8");
        assert_eq!(format_name(D3DFMT_R5G6B5), "D3DFMT_R5G6B5");
        assert_eq!(format_name(D3DFORMAT(0)), "D3DFMT_UNKNOWN");
        assert_eq!(format_name(D3DFORMAT(9999)), "?");
    }

    #[test]
    fn is_real_refresh_rate_rejects_magic_values() {
        assert!(!is_real_refresh_rate(0));
        assert!(!is_real_refresh_rate(1));
        assert!(is_real_refresh_rate(2));
        assert!(is_real_refresh_rate(60));
        assert!(is_real_refresh_rate(144));
    }

    #[test]
    fn empty_supported_refresh_rates() {
        for rate in [0u32, 30, 59, 60, 100, 144, 240] {
            assert_eq!(
                select_refresh_rate(RefreshRateMode::Native, &[], rate),
                rate
            );
            assert_eq!(
                select_refresh_rate(RefreshRateMode::NativeMultiple, &[], rate),
                rate,
            );
        }
        assert_eq!(
            select_refresh_rate(RefreshRateMode::Fixed(nz(144)), &[], 60),
            144,
        );
        assert_eq!(
            select_refresh_rate(RefreshRateMode::Fixed(nz(60)), &[], 999_999),
            60,
        );
    }

    #[test]
    fn select_refresh_rate_native() {
        assert_eq!(
            select_refresh_rate(RefreshRateMode::Native, &[60, 120, 144], 144),
            144,
        );
        assert_eq!(
            select_refresh_rate(RefreshRateMode::Native, &[60, 100], 144),
            100,
        );
        assert_eq!(
            select_refresh_rate(RefreshRateMode::Native, &[100, 120], 60),
            100,
        );
        assert_eq!(
            select_refresh_rate(RefreshRateMode::Native, &[144, 60, 120], 120),
            120,
        );
        assert_eq!(
            select_refresh_rate(RefreshRateMode::Native, &[60, 80], 70),
            60
        );
        assert_eq!(
            select_refresh_rate(RefreshRateMode::Native, &[120, 144], 60),
            120,
        );
    }

    #[test]
    fn select_refresh_rate_native_multiple() {
        assert_eq!(
            select_refresh_rate(RefreshRateMode::NativeMultiple, &[60, 120, 144], 144),
            120,
        );
        assert_eq!(
            select_refresh_rate(RefreshRateMode::NativeMultiple, &[60, 120], 60),
            60,
        );
        assert_eq!(
            select_refresh_rate(RefreshRateMode::NativeMultiple, &[50, 75], 75),
            75,
        );
        assert_eq!(
            select_refresh_rate(RefreshRateMode::NativeMultiple, &[60], 144),
            60,
        );
    }

    #[test]
    fn select_refresh_rate_native_multiple_fallback() {
        assert_eq!(
            select_refresh_rate(RefreshRateMode::NativeMultiple, &[120, 144], 60),
            120,
        );
        assert_eq!(
            select_refresh_rate(RefreshRateMode::Native, &[120, 144], 60),
            120,
        );
        assert_eq!(
            select_refresh_rate(RefreshRateMode::NativeMultiple, &[50, 75], 60),
            50,
        );
        assert_eq!(
            select_refresh_rate(RefreshRateMode::NativeMultiple, &[60, 120], 120),
            120,
        );
    }

    #[test]
    fn select_refresh_rate_fixed() {
        assert_eq!(
            select_refresh_rate(RefreshRateMode::Fixed(nz(240)), &[60, 120], 120),
            240,
        );
        assert_eq!(
            select_refresh_rate(RefreshRateMode::Fixed(nz(120)), &[60, 120], 60),
            120,
        );
        assert_eq!(
            select_refresh_rate(RefreshRateMode::Fixed(nz(120)), &[119], 119),
            119,
        );
        assert_eq!(
            select_refresh_rate(RefreshRateMode::Fixed(nz(120)), &[119, 120], 120),
            120,
        );
    }

    #[test]
    fn select_refresh_rate_normalize_rates() {
        assert_eq!(
            select_refresh_rate(RefreshRateMode::NativeMultiple, &[119, 143], 144),
            119,
        );
        assert_eq!(
            select_refresh_rate(RefreshRateMode::Native, &[119, 143], 144),
            143,
        );
    }

    #[test]
    fn normalize_reported_rate_rounding() {
        assert_eq!(normalize_reported_rate(59), 60);
        assert_eq!(normalize_reported_rate(119), 120);
        assert_eq!(normalize_reported_rate(143), 144);
        assert_eq!(normalize_reported_rate(179), 180);
        assert_eq!(normalize_reported_rate(239), 240);
        assert_eq!(normalize_reported_rate(299), 300);
        assert_eq!(normalize_reported_rate(359), 360);
        for rate in [0u32, 1, 30, 50, 60, 75, 100, 120, 144, 240] {
            assert_eq!(normalize_reported_rate(rate), rate);
        }
    }

    #[test]
    fn transient_device_errors() {
        for hr in [
            D3DERR_DEVICELOST,
            D3DERR_DEVICEREMOVED,
            D3DERR_DEVICEHUNG,
            D3DERR_OUTOFVIDEOMEMORY,
        ] {
            assert!(is_transient_device_error(hr), "{}", fmt_hr!(hr));
        }
        assert!(!is_transient_device_error(HRESULT(
            0x8876_086c_u32.cast_signed()
        )));
        assert!(!is_transient_device_error(HRESULT(0)));
    }
}
