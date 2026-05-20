//! Hooks for `IDirect3D9Ex` and `IDirect3DDevice9Ex`.
//!
//! We IAT-hook `Direct3DCreate9` and forward to `Direct3DCreate9Ex`.
//! The Ex object is binary-compatible with `IDirect3D9` for its first 17 vtable slots,
//! so the game can keep using it as plain `IDirect3D9` while we get the Ex methods.
//!
//! The Ex device's flip-model present path honors `D3DPRESENT_INTERVAL_IMMEDIATE` even in
//! fullscreen exclusive, which makes our pacer the sole timing source on every display mode.
//! The legacy D3D9 path silently re-enables driver vsync in fullscreen exclusive.
//! (Aside: this is why vpatch only works in windowed mode!)
//!
//! `D3DPOOL_MANAGED` is forced to `D3DPOOL_DEFAULT` + `D3DUSAGE_DYNAMIC` on every
//! `CreateTexture` and `CreateVertexBuffer` call because D3D9Ex deprecates
//! the managed pool and silently translates it on a slow path.
//!
//! Instead of per-instance vtable cloning, we do in-place slot patching against
//! `d3d9.dll`'s `.rdata`. (Some drivers depend on the instance's vtable pointer
//! being equal to the canonical one.) See `vtable.rs` for the protect/write/restore
//! mechanics, as well as how we handle idempotency and chain-through.

use crate::config::{CONFIG, RefreshRateMode};
use crate::log::LogCap;
use crate::pacer::{PACER, PacingPolicy};
use crate::patches::patch_call_over_indirect;
use crate::thread::{MainCell, MainToken};
use crate::vtable::{capture_slot, install_vtable};
use crate::{iat_hook, match_named, vtable_sig, vtable_slot, vtbl_field};
use std::ffi::c_void;
use std::num::NonZero;
use std::ptr::{NonNull, null, null_mut};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use tracing::{info, warn};
use windows::Win32::Foundation::{HANDLE, HWND, RECT};
use windows::Win32::Graphics::Direct3D9::{
    D3DDEVTYPE, D3DDISPLAYMODEEX, D3DDISPLAYROTATION, D3DFMT_A1R5G5B5, D3DFMT_A2B10G10R10,
    D3DFMT_A2R10G10B10, D3DFMT_A4R4G4B4, D3DFMT_A8, D3DFMT_A8B8G8R8, D3DFMT_A8R3G3B2,
    D3DFMT_A8R8G8B8, D3DFMT_A16B16G16R16, D3DFMT_D15S1, D3DFMT_D16, D3DFMT_D16_LOCKABLE,
    D3DFMT_D24FS8, D3DFMT_D24S8, D3DFMT_D24X4S4, D3DFMT_D24X8, D3DFMT_D32, D3DFMT_D32F_LOCKABLE,
    D3DFMT_G16R16, D3DFMT_R3G3B2, D3DFMT_R5G6B5, D3DFMT_R8G8B8, D3DFMT_UNKNOWN, D3DFMT_X1R5G5B5,
    D3DFMT_X4R4G4B4, D3DFMT_X8B8G8R8, D3DFMT_X8R8G8B8, D3DFORMAT, D3DPOOL, D3DPOOL_DEFAULT,
    D3DPOOL_MANAGED, D3DPRESENT_INTERVAL_IMMEDIATE, D3DPRESENT_PARAMETERS,
    D3DPRESENTFLAG_LOCKABLE_BACKBUFFER, D3DRESOURCETYPE, D3DSCANLINEORDERING_PROGRESSIVE,
    D3DUSAGE_DYNAMIC, Direct3DCreate9Ex, IDirect3D9Ex_Vtbl, IDirect3DDevice9Ex_Vtbl,
};
use windows::Win32::Graphics::Gdi::RGNDATA;
use windows::core::{HRESULT, Interface};
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsExW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowPos,
};

/// Renders an HRESULT as `0xNNNNNNNN`.
#[macro_export]
macro_rules! fmt_hr {
    ($hr:expr) => {
        ::core::format_args!("{:#010x}", $hr.0.cast_unsigned())
    };
}

#[allow(clippy::cast_possible_truncation)]
const D3DDISPLAYMODEEX_SIZE: u32 = size_of::<D3DDISPLAYMODEEX>() as u32;

/// Replay-speed state observed by game-specific crates, queried each `Present`
/// to decide whether to switch the pacer policy.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayMode {
    Normal = 0,
    Skip = 1,
    Slow = 2,
}

/// Callback registered by game-specific crates via [`set_replay_mode_fn`].
/// Defaults to `Normal` before `install` or for games without replay-speed control.
static REPLAY_MODE_FN: OnceLock<fn() -> ReplayMode> = OnceLock::new();

/// Registers the game-specific replay-mode probe. Idempotent: the first caller wins.
/// Call before [`install`]; later callers are silently ignored.
pub fn set_replay_mode_fn(f: fn() -> ReplayMode) {
    let _ = REPLAY_MODE_FN.set(f);
}

fn replay_mode() -> ReplayMode {
    REPLAY_MODE_FN
        .get()
        .copied()
        .map_or(ReplayMode::Normal, |f| f())
}

// At most one `IDirect3D9` and one device is live at a time in the game.
// `REAL_CREATE_DEVICE_EX` and `REAL_RESET_EX` are read at install time,
// not via `SlotPatch`, and never patch themselves. We still keep the trampolines
// so call sites don't have to open-code the transmute.
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
    REAL_RESET / call_real_reset :
        as fn(this: *mut c_void, pp: *mut D3DPRESENT_PARAMETERS) -> HRESULT;
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
// These three are read at install time from the `IDirect3D9Ex` and `Device9Ex` vtables
// and never patched themselves. Like `REAL_CREATE_DEVICE_EX` and `REAL_RESET_EX` above,
// we still keep the trampolines so call sites don't have to open-code the transmute.
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
    REAL_SET_MAX_FRAME_LATENCY / call_real_set_max_frame_latency :
        as fn(this: *mut c_void, max_latency: u32) -> HRESULT;
}
vtable_slot! {
    REAL_SET_GPU_THREAD_PRIORITY / call_real_set_gpu_thread_priority :
        as fn(this: *mut c_void, priority: i32) -> HRESULT;
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

iat_hook! {
    REAL_DIRECT3D_CREATE9 / real_direct3d_create9 : "Direct3DCreate9"
        as fn(sdk_version: u32) -> *mut c_void;
}

static TEX_LOG: LogCap = LogCap::new(NonZero::new(64).unwrap());
static VBUF_LOG: LogCap = LogCap::new(NonZero::new(64).unwrap());
static CHECK_DEVICE_FORMAT_LOG: LogCap = LogCap::new(NonZero::new(64).unwrap());

static PRESENT_COUNT: AtomicU32 = AtomicU32::new(0);

// `Pacer::apply_policy` resets the deadline, so call it only on mode change.
static MODE: MainCell<ReplayMode> = MainCell::new(ReplayMode::Normal);

/// `IDirect3D9Ex*` and `adapter` from the most recent successful `CreateDeviceEx`.
/// The pointer is a raw borrow of the COM object the game holds. It is valid
/// for the device's lifetime by D3D9's contract; the device implicitly refs its parent.
#[derive(Clone, Copy)]
struct ResetCtx {
    d3d9: *mut c_void,
    adapter: u32,
}
static RESET_CTX: MainCell<Option<ResetCtx>> = MainCell::new(None);

pub(crate) fn present_count() -> u32 {
    PRESENT_COUNT.load(Ordering::Relaxed)
}

/// IAT-hook `Direct3DCreate9` against `host`'s import table, forwarding to `Direct3DCreate9Ex`.
/// For defense against tools that IAT-hook the same import after us, game-specific crates
/// should additionally call [`install_call_site_rewrite`] for each known live call site.
/// Rewritten sites bypass the IAT entirely.
///
/// # Safety
/// `host` must be a loaded module handle.
pub unsafe fn install(host: HMODULE) {
    unsafe {
        REAL_DIRECT3D_CREATE9.install(host, hook_direct3dcreate9);
    }
}

/// Rewrites a 6-byte `FF 15 disp32` indirect-call site to a 5-byte direct call
/// to our `Direct3DCreate9` hook plus a trailing NOP.
///
/// # Safety
/// `addr` must be a writable code address holding a 6-byte indirect call whose
/// bytes equal `expected`.
pub unsafe fn install_call_site_rewrite(addr: usize, expected: &[u8; 6]) {
    unsafe {
        patch_call_over_indirect(
            addr,
            expected,
            hook_direct3dcreate9 as *mut (),
            "Direct3DCreate9 call-site rewrite",
        );
    }
}

unsafe extern "system" fn hook_direct3dcreate9(sdk_version: u32) -> *mut c_void {
    unsafe {
        // The Ex object's first 17 vtable slots are the `IDirect3D9` vtable,
        // so the game can keep using the returned pointer as plain `IDirect3D9`
        // while we get the Ex methods.
        let Ok(ex) = Direct3DCreate9Ex(sdk_version) else {
            return null_mut();
        };
        // `into_raw` transfers the ref to the game without `Release`.
        let p_ex: *mut c_void = ex.into_raw();
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

        // `CreateDeviceEx` and `GetAdapterDisplayModeEx` are read, not patched.
        capture_slot(
            vtbl,
            vtbl_field!(IDirect3D9Ex_Vtbl, CreateDeviceEx),
            &REAL_CREATE_DEVICE_EX,
        );
        capture_slot(
            vtbl,
            vtbl_field!(IDirect3D9Ex_Vtbl, GetAdapterDisplayModeEx),
            &REAL_GET_ADAPTER_DISPLAY_MODE_EX,
        );

        let result = install_vtable(vtbl, |scope| {
            // `hook_create_device` routes to `CreateDeviceEx` via `REAL_CREATE_DEVICE_EX`
            // rather than chaining through to the displaced `CreateDevice`.
            scope.redirect(
                &REDIRECT_CREATE_DEVICE,
                vtbl_field!(IDirect3D9Ex_Vtbl, base__.CreateDevice),
                "IDirect3D9::CreateDevice",
                hook_create_device,
            );
            scope.intercept(
                &REAL_CHECK_DEVICE_FORMAT,
                vtbl_field!(IDirect3D9Ex_Vtbl, base__.CheckDeviceFormat),
                "IDirect3D9::CheckDeviceFormat",
                hook_check_device_format,
            );
        });
        info!(kind = "d3d9_hooks_installed", protect_ok = result.is_some());
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
        // Exclusive fullscreen needs a populated `D3DDISPLAYMODEEX`; windowed needs `NULL`.
        let mut display_mode: Option<D3DDISPLAYMODEEX> = None;
        let (pp_before, pp_after) = match pp.as_mut() {
            Some(p) => {
                let before = *p;
                rewrite_present_params(p);
                if p.Windowed.0 == 0 {
                    let cfg = CONFIG.get().unwrap();
                    apply_refresh_override(p, this, adapter, cfg.display.refresh_rate);
                    display_mode = Some(build_display_mode_ex(p, p.FullScreen_RefreshRateInHz));
                }
                (Some(before), Some(*p))
            }
            None => (None, None),
        };
        let display_mode_ptr: *mut D3DDISPLAYMODEEX =
            display_mode.as_mut().map_or(null_mut(), |m| &raw mut *m);

        // Log before the `CreateDeviceEx` call so we have visibility if it crashes inside.
        info!(
            kind = "create_device_call",
            this = format_args!("{this:p}"),
            adapter,
            device_type = ?device_type,
            behavior_flags = format_args!("{behavior_flags:#x}"),
            focus_window = format_args!("{:p}", focus_window.0),
            pp_before = ?pp_before,
            pp_after = ?pp_after,
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
            hr = fmt_hr!(hr),
            device = format_args!("{dev:p}"),
        );
        if hr.is_ok()
            && let Some(dev) = NonNull::new(dev)
        {
            // Apparently D3D9Ex breaks the window style on `CreateDeviceEx`.
            // OILP's `CreateDevice_hook` applies the same `SWP_SHOWWINDOW` fix.
            // Without it, the game's main pump doesn't run properly.
            SetWindowPos(
                focus_window.0,
                null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOSIZE | SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );

            RESET_CTX.set(
                &tok,
                Some(ResetCtx {
                    d3d9: this,
                    adapter,
                }),
            );

            install_device_hooks(dev);
            apply_device_ex_tunables(dev);
        }
        hr
    }
}

/// Reads the current desktop mode via `GetAdapterDisplayModeEx` and applies the user's policy.
///
/// We deliberately do not enumerate display modes because both `EnumAdapterModes`
/// and `EnumAdapterModesEx` hard-faulted on an NVIDIA driver, and we can't recover from a fault.
/// Unfortunately, this means we can't discover refresh rates above the current desktop's
/// under the `NativeMultiple` setting. Users who need that can use `Fixed(N)`.
///
/// On `GetAdapterDisplayModeEx` failure we try `EnumDisplaySettingsExW`
/// (Win32 GDI path; doesn't touch d3d9), then fall back to 60 Hz if both fail.
unsafe fn pick_refresh_rate(this: *mut c_void, adapter: u32, mode: RefreshRateMode) -> u32 {
    unsafe {
        let mut current = D3DDISPLAYMODEEX {
            Size: D3DDISPLAYMODEEX_SIZE,
            ..D3DDISPLAYMODEEX::default()
        };
        let hr = call_real_get_adapter_display_mode_ex(this, adapter, &raw mut current, null_mut());
        let current_rate = if hr.is_ok() {
            current.RefreshRate
        } else {
            let win32_rate = win32_current_refresh_rate();
            let fallback = win32_rate.unwrap_or(60);
            warn!(
                kind = "pick_refresh_rate_fallback",
                d3d9_hr = fmt_hr!(hr),
                win32_rate = ?win32_rate,
                fallback,
            );
            fallback
        };
        let chosen = compute_refresh_rate(mode, current_rate);
        info!(
            kind = "refresh_rate_decision",
            desktop_rate_hz = current_rate,
            chosen_hz = chosen,
        );
        if let RefreshRateMode::Fixed(target) = mode {
            info!(
                kind = "refresh_rate_fixed_unvalidated",
                target_hz = target.get(),
            );
        }
        chosen
    }
}

/// Override the game's hard-coded 60 Hz in `pp.FullScreen_RefreshRateInHz`
/// with the result of `pick_refresh_rate`.
unsafe fn apply_refresh_override(
    pp: &mut D3DPRESENT_PARAMETERS,
    d3d9: *mut c_void,
    adapter: u32,
    mode: RefreshRateMode,
) {
    pp.FullScreen_RefreshRateInHz = unsafe { pick_refresh_rate(d3d9, adapter, mode) };
}

/// Win32 fallback for refresh-rate query. Returns the current desktop's refresh rate
/// via `EnumDisplaySettingsExW`. Returns `None` if the call fails, or if `dmDisplayFrequency`
/// is 0 or 1 (magic values that mean "hardware default rate," not a real refresh rate).
fn win32_current_refresh_rate() -> Option<u32> {
    // Caller-set `dmSize` tells Win32 which `DEVMODE` fields are valid.
    // `dmDisplayFrequency` lives well within the size we report.
    // `DEVMODEW` is smaller than `u16::MAX` bytes.
    let mut dm = DEVMODEW {
        dmSize: u16::try_from(size_of::<DEVMODEW>()).unwrap_or(0),
        ..DEVMODEW::default()
    };
    let ok = unsafe { EnumDisplaySettingsExW(null(), ENUM_CURRENT_SETTINGS, &raw mut dm, 0) };
    if ok == 0 {
        return None;
    }
    match dm.dmDisplayFrequency {
        0 | 1 => None,
        n => Some(n),
    }
}

/// `NativeMultiple` floors to a multiple of 60 capped at `current_rate`.
/// On sub-60-Hz desktops, it falls back to 60 Hz rather than picking 0.
/// `Fixed` passes through.
fn compute_refresh_rate(mode: RefreshRateMode, current_rate: u32) -> u32 {
    match mode {
        RefreshRateMode::Native => current_rate,
        RefreshRateMode::NativeMultiple => {
            if current_rate >= 60 {
                (current_rate / 60) * 60
            } else {
                60
            }
        }
        RefreshRateMode::Fixed(target) => target.get(),
    }
}

/// `SetMaximumFrameLatency(1)` caps the GPU input queue at 1 (default 3)
/// so frames spend less time enqueued before display, shaving up to two frames
/// of end-to-display latency. `SetGPUThreadPriority(7)` reduces CPU-scheduler jitter
/// on a contended D3D9Ex worker thread marshalling `Present`.
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

unsafe fn install_device_hooks(dev: NonNull<c_void>) {
    unsafe {
        let vtbl: *mut IDirect3DDevice9Ex_Vtbl = *dev.as_ptr().cast();
        let Some(vtbl) = NonNull::new(vtbl) else {
            warn!(kind = "device_vtbl_null", dev = format_args!("{dev:p}"));
            return;
        };

        capture_slot(
            vtbl,
            vtbl_field!(IDirect3DDevice9Ex_Vtbl, ResetEx),
            &REAL_RESET_EX,
        );
        capture_slot(
            vtbl,
            vtbl_field!(IDirect3DDevice9Ex_Vtbl, SetMaximumFrameLatency),
            &REAL_SET_MAX_FRAME_LATENCY,
        );
        capture_slot(
            vtbl,
            vtbl_field!(IDirect3DDevice9Ex_Vtbl, SetGPUThreadPriority),
            &REAL_SET_GPU_THREAD_PRIORITY,
        );

        let result = install_vtable(vtbl, |scope| {
            scope.intercept(
                &REAL_RESET,
                vtbl_field!(IDirect3DDevice9Ex_Vtbl, base__.Reset),
                "Reset",
                hook_reset,
            );
            scope.intercept(
                &REAL_PRESENT,
                vtbl_field!(IDirect3DDevice9Ex_Vtbl, base__.Present),
                "Present",
                hook_present,
            );
            scope.intercept(
                &REAL_CREATE_TEXTURE,
                vtbl_field!(IDirect3DDevice9Ex_Vtbl, base__.CreateTexture),
                "CreateTexture",
                hook_create_texture,
            );
            scope.intercept(
                &REAL_CREATE_VERTEX_BUFFER,
                vtbl_field!(IDirect3DDevice9Ex_Vtbl, base__.CreateVertexBuffer),
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

/// Substitutes `X8R8G8B8` for `A8R8G8B8` when a game passes the latter as `AdapterFormat`.
///
/// `A8R8G8B8` isn't a displayable format. Vanilla D3D9 silently accepts it and returns
/// `D3D_OK`; D3D9Ex is strict and returns `D3DERR_NOTAVAILABLE`. Games written against
/// the lenient behavior can fall down a reduced-color-mode path that fails subsequent
/// resource creation. The substitution gives the call its intended meaning.
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
        if let Some(n) = CHECK_DEVICE_FORMAT_LOG.tick() {
            let forwarded_format = if substituted {
                format_name(forwarded_adapter_fmt)
            } else {
                ""
            };
            info!(
                kind = "check_device_format",
                n = n + 1,
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
        }
        hr
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

/// Populates a `D3DDISPLAYMODEEX` from the present-params back buffer
/// plus an explicit refresh rate. The Ex `CreateDevice` and `Reset` signatures
/// require a fully-filled struct for exclusive fullscreen and ignore it for windowed.
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

/// D3D9Ex rejects `D3DPOOL_MANAGED` with `INVALIDCALL`, so we substitute
/// the closest valid pair on every `Create*Texture` and `CreateVertexBuffer` path
/// where the game or d3dx9 hands us `MANAGED`. Returns whether a translation happened.
/// OILP also does this substitution.
pub(crate) fn translate_managed_pool(pool: &mut D3DPOOL, usage: &mut u32) -> bool {
    if *pool == D3DPOOL_MANAGED {
        *pool = D3DPOOL_DEFAULT;
        *usage |= D3DUSAGE_DYNAMIC.cast_unsigned();
        true
    } else {
        false
    }
}

/// Reads the object pointer from a `Create*`-style `*mut *mut c_void` out param,
/// returning null when the out param itself is null.
pub(crate) unsafe fn out_ptr(pp: *mut *mut c_void) -> *const c_void {
    if pp.is_null() {
        null()
    } else {
        unsafe { (*pp).cast_const() }
    }
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
        let observed_mode = replay_mode();

        // Load-then-conditional-store gates the heavier `apply_policy` call
        // behind an actual mode change.
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

        // We increment before `Present` so `PRESENT_COUNT` names the in-flight frame.
        // This way, a crash inside `Present` leaves the count at the attempted frame,
        // not the last completed.
        PRESENT_COUNT.fetch_add(1, Ordering::Relaxed);

        call_real_present(this, src_rect, dst_rect, dest_window_override, dirty_region)
    }
}

// `Reset`/`ResetEx` deliberately does not re-apply:
// - Device tunables (`SetMaximumFrameLatency`, `SetGPUThreadPriority`): these are
//   device-wide runtime settings, not render state. `Reset` re-inits the swap chain
//   but doesn't tear down the device, so they persist.
//   NOTE: If a latency regression ever shows up across a `Reset`, considering re-applying here.
// - `SetWindowPos SWP_SHOWWINDOW` fix: the style breakage is specific to
//   `CreateDeviceEx` re-associating the focus window. `Reset` reuses
//   the existing association, so the bug shouldn't reappear.
//
// `pick_refresh_rate` is re-applied so runtime refresh-rate toggles
// take effect at the next `Reset`.
unsafe extern "system" fn hook_reset(this: *mut c_void, pp: *mut D3DPRESENT_PARAMETERS) -> HRESULT {
    let tok = MainToken::new();
    unsafe {
        let mut display_mode: Option<D3DDISPLAYMODEEX> = None;
        let (pp_before, pp_after) = match pp.as_mut() {
            Some(p) => {
                let before = *p;
                rewrite_present_params(p);
                if p.Windowed.0 == 0 {
                    let cfg = CONFIG.get().unwrap();
                    let ctx = RESET_CTX
                        .get(&tok)
                        .expect("hook_reset fired before hook_create_device populated RESET_CTX");
                    apply_refresh_override(p, ctx.d3d9, ctx.adapter, cfg.display.refresh_rate);
                    display_mode = Some(build_display_mode_ex(p, p.FullScreen_RefreshRateInHz));
                }
                (Some(before), Some(*p))
            }
            None => (None, None),
        };
        let display_mode_ptr: *mut D3DDISPLAYMODEEX =
            display_mode.as_mut().map_or(null_mut(), |m| &raw mut *m);

        // Log before the call in case there's a crash inside `ResetEx`.
        let use_reset_ex = REAL_RESET_EX.try_get().is_some();
        info!(
            kind = "reset_call",
            this = format_args!("{this:p}"),
            pp_before = ?pp_before,
            pp_after = ?pp_after,
            display_mode = if display_mode_ptr.is_null() { "null" } else { "set" },
            path = if use_reset_ex { "ResetEx" } else { "Reset" },
        );

        // Plain `Reset` on Alt+Enter crashed for a tester, but `ResetEx` didn't.
        let hr = if use_reset_ex {
            call_real_reset_ex(this, pp, display_mode_ptr)
        } else {
            call_real_reset(this, pp)
        };

        info!(kind = "reset_result", hr = fmt_hr!(hr));
        hr
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
        if let Some(n) = TEX_LOG.tick() {
            let returned = out_ptr(pp_texture);
            info!(
                kind = "create_texture",
                n = n + 1,
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
        }
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
        if let Some(n) = VBUF_LOG.tick() {
            let returned = out_ptr(pp_vertex_buffer);
            info!(
                kind = "create_vbuffer",
                n = n + 1,
                length,
                fvf = format_args!("{fvf:#x}"),
                pool_in = ?pool_orig,
                pool_out = ?pool,
                usage_in = format_args!("{usage_orig:#x}"),
                usage_out = format_args!("{usage:#x}"),
                hr = fmt_hr!(hr),
                ptr = format_args!("{returned:p}"),
            );
        }
        hr
    }
}

/// On D3D9Ex with `SWAPEFFECT_DISCARD`, `D3DPRESENTFLAG_LOCKABLE_BACKBUFFER`
/// breaks flip-model presentation on native NVIDIA: window opens; black screen; exit.
/// DXVK doesn't trip on it because Vulkan has no equivalent concept.
fn rewrite_present_params(pp: &mut D3DPRESENT_PARAMETERS) {
    // `cast_unsigned` preserves the bit pattern.
    pp.PresentationInterval = D3DPRESENT_INTERVAL_IMMEDIATE.cast_unsigned();
    pp.Flags &= !D3DPRESENTFLAG_LOCKABLE_BACKBUFFER;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZero;
    use windows::Win32::Graphics::Direct3D9::{
        D3DFMT_R5G6B5, D3DPOOL_SCRATCH, D3DPOOL_SYSTEMMEM, D3DPRESENT_INTERVAL_ONE,
        D3DPRESENTFLAG_DISCARD_DEPTHSTENCIL, D3DSWAPEFFECT_DISCARD, D3DUSAGE_WRITEONLY,
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
    fn rewrite_present_params_strips_lockable_backbuffer() {
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
        // Locks in the current "we only touch interval + lockable flag" contract.
        //
        // TODO: The FLIPEX-direct backlog item will modify `SwapEffect`
        // and `BackBufferCount here, so this test should be updated.
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
    fn translate_managed_pool_swaps_managed_for_default_dynamic() {
        let mut pool = D3DPOOL_MANAGED;
        let mut usage: u32 = 0;
        assert!(translate_managed_pool(&mut pool, &mut usage));
        assert_eq!(pool, D3DPOOL_DEFAULT);
        assert_eq!(usage, D3DUSAGE_DYNAMIC.cast_unsigned());
    }

    #[test]
    fn translate_managed_pool_preserves_existing_usage_bits() {
        let mut pool = D3DPOOL_MANAGED;
        let mut usage: u32 = D3DUSAGE_WRITEONLY.cast_unsigned();
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
            let mut usage: u32 = 0;
            assert!(!translate_managed_pool(&mut pool, &mut usage));
            assert_eq!(pool, pool_in);
            assert_eq!(usage, 0);
        }
    }

    #[test]
    fn build_display_mode_ex_copies_pp_fields_and_uses_param_refresh() {
        let pp = D3DPRESENT_PARAMETERS {
            BackBufferWidth: 1280,
            BackBufferHeight: 960,
            BackBufferFormat: D3DFMT_X8R8G8B8,
            // This is deliberately wrong on `pp`.
            // `build_display_mode_ex` must use the explicit `refresh` arg, not this field.
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
    fn compute_refresh_rate_native_passthrough() {
        for rate in [0u32, 30, 60, 100, 144, 240] {
            assert_eq!(compute_refresh_rate(RefreshRateMode::Native, rate), rate);
        }
    }

    #[test]
    fn compute_refresh_rate_native_multiple_floors_to_60() {
        assert_eq!(
            compute_refresh_rate(RefreshRateMode::NativeMultiple, 144),
            120
        );
        assert_eq!(
            compute_refresh_rate(RefreshRateMode::NativeMultiple, 100),
            60
        );
        assert_eq!(
            compute_refresh_rate(RefreshRateMode::NativeMultiple, 60),
            60
        );
        assert_eq!(
            compute_refresh_rate(RefreshRateMode::NativeMultiple, 240),
            240
        );
        assert_eq!(
            compute_refresh_rate(RefreshRateMode::NativeMultiple, 75),
            60
        );
        assert_eq!(
            compute_refresh_rate(RefreshRateMode::NativeMultiple, 119),
            60
        );
        assert_eq!(
            compute_refresh_rate(RefreshRateMode::NativeMultiple, 120),
            120
        );
    }

    #[test]
    fn compute_refresh_rate_native_multiple_below_60_clamps_up() {
        // Sub-60-Hz desktop: floor would give 0; the implementation clamps to 60
        // so `CreateDeviceEx` receives a value D3D9 accepts.
        for rate in [0u32, 1, 30, 59] {
            assert_eq!(
                compute_refresh_rate(RefreshRateMode::NativeMultiple, rate),
                60
            );
        }
    }

    #[test]
    fn compute_refresh_rate_fixed_passes_target_through() {
        assert_eq!(
            compute_refresh_rate(RefreshRateMode::Fixed(nz(144)), 60),
            144,
        );
        assert_eq!(compute_refresh_rate(RefreshRateMode::Fixed(nz(1)), 240), 1);
        // `current_rate` is ignored.
        assert_eq!(
            compute_refresh_rate(RefreshRateMode::Fixed(nz(60)), 999_999),
            60,
        );
    }
}
