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
use crate::pacer::{PACER, Pacer, PacingPolicy};
use crate::patches::PatchSite;
use crate::replay::{ReplayMode, policy_change};
use crate::screenshot::{on_device_creating, on_pre_present, on_pre_reset};
use crate::session::{
    device_creating, gate_d3d9, gate_device, is_game_d3d9, is_game_device, record_d3d9,
    record_device,
};
use crate::thread::{MainCell, MainToken};
use crate::vtable::{capture_slot, install_vtable, vtable_field, vtable_slot};
use crate::{fmt_hr, iat_hook, match_named};
use std::cmp::min;
use std::ffi::c_void;
use std::ptr::{NonNull, null, null_mut};
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

#[allow(clippy::cast_possible_truncation)]
const D3DDISPLAYMODEEX_SIZE: u32 = size_of::<D3DDISPLAYMODEEX>() as u32;

#[allow(clippy::cast_possible_truncation)]
const D3DDISPLAYMODEFILTER_SIZE: u32 = size_of::<D3DDISPLAYMODEFILTER>() as u32;

const MAX_ENUM_RATES: usize = 64;
const MAX_ENUM_SCAN: u32 = 4096;

pub(crate) const D3D_OK: HRESULT = HRESULT(0);
pub(crate) const D3DERR_INVALIDCALL: HRESULT = HRESULT(0x8876_086c_u32.cast_signed());
pub(crate) const D3DERR_NOTAVAILABLE: HRESULT = HRESULT(0x8876_086a_u32.cast_signed());
const D3DERR_DEVICELOST: HRESULT = HRESULT(0x8876_0868_u32.cast_signed());
const D3DERR_DEVICEREMOVED: HRESULT = HRESULT(0x8876_0870_u32.cast_signed());
const D3DERR_DEVICEHUNG: HRESULT = HRESULT(0x8876_0874_u32.cast_signed());
const D3DERR_OUTOFVIDEOMEMORY: HRESULT = HRESULT(0x8876_017c_u32.cast_signed());

static PRESENT_COUNT: AtomicU32 = AtomicU32::new(0);

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

// At most one `IDirect3D9` and one device are alive at a time in the game, so each slot is a single global.
// Read-only slots are populated via `capture_slot` and never patched. The trampolines exist so call sites don't have to manually transmute.
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
vtable_slot! {
    REAL_CREATE_DEVICE / call_real_create_device :
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
/// [`crate::config::CONFIG`] must be populated before calling this.
///
/// # Safety
/// `host` must be a loaded module handle.
pub unsafe fn install(host: HMODULE) {
    unsafe { REAL_DIRECT3D_CREATE9.install(host, create_hooked_d3d9) };
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

/// Present-parameter policy for the devices created from one `IDirect3D9Ex` object.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct PresentPolicy {
    /// Keep `D3DPRESENTFLAG_LOCKABLE_BACKBUFFER` instead of stripping it.
    pub(crate) keep_lockable_back_buffer: bool,
    /// Substitute a 32-bit back buffer for a 16-bit request up front, rather than only after the device refuses one.
    pub(crate) upgrade_16bit_back_buffer: bool,
}

/// The present-parameter policy that every device created through direct D3D9 gets.
const DIRECT_POLICY: PresentPolicy = PresentPolicy {
    keep_lockable_back_buffer: false,
    upgrade_16bit_back_buffer: false,
};

/// The `Direct3DCreate9` IAT-hook/call-site-rewrite target.
unsafe extern "system" fn create_hooked_d3d9(sdk_version: u32) -> *mut c_void {
    unsafe { create_hooked_d3d9_with(sdk_version, DIRECT_POLICY) }
        .map_or_else(null_mut, NonNull::as_ptr)
}

/// Creates an `IDirect3D9Ex` with our vtable hooks installed, exactly as if the game had called `Direct3DCreate9`.
/// `policy` determines the present-parameter rewrites applied to every device created from it; see [`materialize`].
pub(crate) unsafe fn create_hooked_d3d9_with(
    sdk_version: u32,
    policy: PresentPolicy,
) -> Option<NonNull<c_void>> {
    // The Ex object's first 17 vtable slots are the `IDirect3D9` vtable,
    // so the game can keep using the returned pointer as plain `IDirect3D9` while we get the Ex methods.
    let ex = match unsafe { Direct3DCreate9Ex(sdk_version) } {
        Ok(ex) => ex,
        Err(e) => {
            warn!(
                kind = "d3d9_init_failed",
                sdk_version = format_args!("{sdk_version:#x}"),
                hr = %fmt_hr!(e.code()),
            );
            return None;
        }
    };
    // `into_raw` transfers the ref to the game without `Release`.
    let p_ex = NonNull::new(ex.into_raw())?;

    let Some(tok) = MainToken::claim() else {
        // A claim can only already be held if an earlier create succeeded on another thread, so the shared vtable is already patched
        // and this object doesn't need further setup to work. Its `CreateDevice` then passes through the gate to the plain (non-Ex)
        // creation path, which produces a legacy device. This way, we degrade to vanilla-unpatched instead of a broken D3D9Ex device.
        warn!(
            kind = "d3d9_create_off_thread",
            p_ex = format_args!("{p_ex:p}"),
        );
        return Some(p_ex);
    };

    // A changed policy means two creation paths with different policies coexist in a single session, which no supported game does.
    let replaced = SESSION_POLICY.get(&tok).is_some_and(|prev| prev != policy);
    SESSION_POLICY.set(&tok, Some(policy));
    log_at!(!replaced => info / warn,
        kind = "present_params_policy",
        keep_lockable_back_buffer = policy.keep_lockable_back_buffer,
        upgrade_16bit_back_buffer = policy.upgrade_16bit_back_buffer,
        status = if replaced { "REPLACED" } else { "OK" },
    );

    // SAFETY: `p_ex` is the live object created above.
    unsafe { record_d3d9(&tok, p_ex) };
    unsafe { install_d3d9_hooks(p_ex) };
    info!(kind = "d3d9_init", p_ex = format_args!("{p_ex:p}"));
    Some(p_ex)
}

unsafe fn install_d3d9_hooks(d3d9_ex: NonNull<c_void>) {
    let vtbl = unsafe { *d3d9_ex.as_ptr().cast() };
    let Some(vtbl) = NonNull::new(vtbl) else {
        warn!(kind = "d3d9_vtbl_null", p_ex = format_args!("{d3d9_ex:p}"));
        return;
    };

    unsafe {
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
    }

    let result = unsafe {
        install_vtable(vtbl, |scope| {
            // The game's calls route to `CreateDeviceEx` via `REAL_CREATE_DEVICE_EX` rather than chaining through
            // to the displaced `CreateDevice`, which is captured only to hand foreign objects' calls through unchanged.
            scope.intercept(
                &REAL_CREATE_DEVICE,
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
        })
    };
    info!(kind = "d3d9_hooks_installed", protect_ok = result.is_some());
}

/// The optional overrides active for one device creation/reset attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FixSet {
    /// Pick the fullscreen refresh rate instead of resolving the game's own request. See [`pick_refresh_rate`].
    rate_override: bool,
    /// Substitute a 32-bit back buffer for a 16-bit request. See [`upgraded_back_buffer_format`].
    format_upgrade: bool,
}

/// One round of modification attempts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Round {
    /// The primary attempt.
    primary: FixSet,
    /// The primary attempt without the refresh-rate override. This is tried when the primary attempt fails due to a non-transient error.
    rollback: FixSet,
}

fn plan_attempts(policy_upgrade: bool, escalatable: bool) -> [Option<Round>; 2] {
    let round = |format_upgrade| Round {
        primary: FixSet {
            rate_override: true,
            format_upgrade,
        },
        rollback: FixSet {
            rate_override: false,
            format_upgrade,
        },
    };

    if !policy_upgrade && escalatable {
        [Some(round(false)), Some(round(true))]
    } else {
        [Some(round(policy_upgrade)), None]
    }
}

/// One fully-materialized device creation/reset attempt.
struct Attempt {
    pp: D3DPRESENT_PARAMETERS,
    /// Populated for exclusive fullscreen; `None` for windowed.
    mode: Option<D3DDISPLAYMODEEX>,
}

impl Attempt {
    /// Returns a raw pointer to the fullscreen display mode for the Ex calls; null for windowed.
    fn mode_ptr(&mut self) -> *mut D3DDISPLAYMODEEX {
        self.mode.as_mut().map_or_else(null_mut, |m| &raw mut *m)
    }
}

/// The `IDirect3D9Ex` an adapter is reached through, paired with its ordinal.
#[derive(Clone, Copy)]
struct Adapter<'a> {
    d3d9: &'a IDirect3D9,
    ordinal: u32,
}

impl<'a> Adapter<'a> {
    /// Borrows the interface pointer a hook was invoked on. Returns `None` if `this` is null.
    ///
    /// # Safety
    /// `this` must be null or a live `IDirect3D9Ex`, and must outlive the returned context.
    unsafe fn from_hook(this: &'a *mut c_void, ordinal: u32) -> Option<Self> {
        let d3d9 = unsafe { IDirect3D9::from_raw_borrowed(this)? };
        Some(Self { d3d9, ordinal })
    }

    /// The ABI pointer for the captured `REAL_*` slots, which take `this` raw.
    fn raw(self) -> *mut c_void {
        self.d3d9.as_raw()
    }
}

/// A distinct-refresh-rate table for one back buffer format, as returned by [`enumerate_supported_rates`].
type RateTable = ([u32; MAX_ENUM_RATES], usize);

/// Adapter information used by [`materialize`].
#[derive(Clone)]
struct AdapterSnapshot {
    /// The adapter's mode when sampling succeeded. Doubles as the [`warn_if_exclusive_degraded`] baseline.
    desktop_mode: Option<D3DDISPLAYMODEEX>,
    /// The desktop refresh rate, resolved through the Win32 fallback chain down to 60 Hz.
    desktop_rate: u32,
    /// Supported rates per candidate back buffer format. The 16-bit-escalation attempts materialize
    /// with a different format than the primary ones, and the two formats can advertise different mode sets.
    rates: [(D3DFORMAT, RateTable); 2],
    rates_len: usize,
}

impl AdapterSnapshot {
    fn empty() -> Self {
        Self {
            desktop_mode: None,
            desktop_rate: 60,
            rates: [(D3DFMT_UNKNOWN, ([0; MAX_ENUM_RATES], 0)); 2],
            rates_len: 0,
        }
    }

    /// Returns the supported rates recorded for `format`, or an empty slice for a format that wasn't sampled.
    fn rates_for(&self, format: D3DFORMAT) -> &[u32] {
        self.rates[..self.rates_len]
            .iter()
            .find(|(f, _)| *f == format)
            .map_or(&[], |(_, (rates, len))| &rates[..*len])
    }

    /// Samples the adapter for a fullscreen `requested`. Returns [`AdapterSnapshot::empty`] for windowed or null requests.
    unsafe fn capture(
        adapter: Adapter<'_>,
        requested: Option<&D3DPRESENT_PARAMETERS>,
        policy: PresentPolicy,
        controls_timing: bool,
    ) -> Self {
        let Some(req) = requested else {
            return Self::empty();
        };
        if req.Windowed.0 != 0 {
            return Self::empty();
        }

        let desktop_mode = unsafe { sample_adapter_display_mode(adapter) };
        // A rate pick happens exactly when the config override applies or the game's own rate is a magic value
        // needing the `Native` default, and only picks consult the resolved desktop rate (rollbacks read the raw desktop mode).
        // When no pick can occur, the Win32 fallback probe and its warn are skipped.
        let will_pick = controls_timing || !is_real_refresh_rate(req.FullScreen_RefreshRateInHz);
        let reported_hz = desktop_mode.map(|m| m.RefreshRate);
        let desktop_rate = match reported_hz {
            _ if !will_pick => 60,
            Some(hz) if is_real_refresh_rate(hz) => hz,
            _ => {
                let device = unsafe { adapter_display_device(adapter) };
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

        let mut snap = Self {
            desktop_mode,
            desktop_rate,
            ..Self::empty()
        };
        let windowed = req.Windowed.0 != 0;
        let first = format_plan(
            req.BackBufferFormat,
            windowed,
            policy.upgrade_16bit_back_buffer,
        );
        snap.rates[0] = (first.format, unsafe {
            enumerate_supported_rates(
                adapter,
                first.format,
                req.BackBufferWidth,
                req.BackBufferHeight,
            )
        });
        snap.rates_len = 1;

        if !policy.upgrade_16bit_back_buffer {
            let escalated = format_plan(req.BackBufferFormat, windowed, true);
            if escalated.format != first.format {
                snap.rates[1] = (escalated.format, unsafe {
                    enumerate_supported_rates(
                        adapter,
                        escalated.format,
                        req.BackBufferWidth,
                        req.BackBufferHeight,
                    )
                });
                snap.rates_len = 2;
            }
        }
        snap
    }
}

/// The back-buffer-format decision for one attempt.
#[derive(Clone, Copy)]
struct FormatPlan {
    format: D3DFORMAT,
    /// The format after the 16-bit upgrade, when that rule fired.
    upgraded: Option<D3DFORMAT>,
    /// Whether the fullscreen scanout substitution fired. `CreateDeviceEx` rejects `A8R8G8B8` in fullscreen
    /// because it isn't scanout-compatible, so it is substituted with `X8R8G8B8`.
    scanout_substituted: bool,
}

fn format_plan(requested: D3DFORMAT, windowed: bool, upgrade: bool) -> FormatPlan {
    let mut format = requested;
    let mut upgraded = None;
    if upgrade && let Some(up) = upgraded_back_buffer_format(format) {
        format = up;
        upgraded = Some(up);
    }
    let scanout_substituted = !windowed && format == D3DFMT_A8R8G8B8;
    if scanout_substituted {
        format = D3DFMT_X8R8G8B8;
    }
    FormatPlan {
        format,
        upgraded,
        scanout_substituted,
    }
}

/// Per-call configuration for one create/reset attempt sequence.
struct LadderCtx {
    snapshot: AdapterSnapshot,
    policy: PresentPolicy,
    /// Whether the pacer controls frame timing; see [`rewrite_present_params_impl`].
    /// This also gates the config-driven rate override, which is meaningless when the game is left synced to vblank.
    controls_timing: bool,
    refresh_mode: RefreshRateMode,
    /// Whether the attempt runner will actually pass the Ex display mode.
    consumes_display_mode: bool,
}

impl LadderCtx {
    /// Resolves the entire attempt sequence context for one hook call.
    ///
    /// # Safety
    /// `pp` must be null or point to a live `D3DPRESENT_PARAMETERS`. `adapter` must be live for the call.
    unsafe fn resolve(
        tok: &MainToken,
        adapter: Adapter<'_>,
        pp: *mut D3DPRESENT_PARAMETERS,
        consumes_display_mode: bool,
    ) -> Self {
        let policy = session_policy(tok);
        let controls_timing = PACER.get().is_some();
        Self {
            snapshot: unsafe {
                AdapterSnapshot::capture(adapter, pp.as_ref(), policy, controls_timing)
            },
            policy,
            controls_timing,
            refresh_mode: CONFIG.get().unwrap().display.refresh_rate,
            consumes_display_mode,
        }
    }
}

/// Builds the complete attempt for `fixes` from the game's `requested` params.
fn materialize(requested: &D3DPRESENT_PARAMETERS, fixes: FixSet, ctx: &LadderCtx) -> Attempt {
    let mut pp = *requested;
    rewrite_present_params_impl(
        &mut pp,
        PresentPolicy {
            keep_lockable_back_buffer: ctx.policy.keep_lockable_back_buffer,
            upgrade_16bit_back_buffer: fixes.format_upgrade,
        },
        ctx.controls_timing,
    );

    let mode = if pp.Windowed.0 == 0 {
        let rates = ctx.snapshot.rates_for(pp.BackBufferFormat);
        if fixes.rate_override {
            if ctx.controls_timing {
                pp.FullScreen_RefreshRateInHz =
                    pick_refresh_rate(ctx.refresh_mode, rates, ctx.snapshot.desktop_rate);
            }
            // The Ex display mode must have a real refresh rate, so a leftover magic value is resolved with the `Native` policy.
            if !is_real_refresh_rate(pp.FullScreen_RefreshRateInHz) {
                let chosen =
                    pick_refresh_rate(RefreshRateMode::Native, rates, ctx.snapshot.desktop_rate);
                info!(
                    kind = "display_mode_refresh_defaulted",
                    requested_hz = pp.FullScreen_RefreshRateInHz,
                    chosen_hz = chosen,
                );
                pp.FullScreen_RefreshRateInHz = chosen;
            }
        } else {
            // We roll back the game's own rate, resolving a magic value to the sampled desktop rate for the display mode.
            // We don't pick a rate again here since the pick has just been rejected and rederiving it would fail the same way.
            let original = requested.FullScreen_RefreshRateInHz;
            let mode_rate = if is_real_refresh_rate(original) {
                original
            } else {
                ctx.snapshot
                    .desktop_mode
                    .map(|m| m.RefreshRate)
                    .filter(|&r| is_real_refresh_rate(r))
                    .unwrap_or(60)
            };
            pp.FullScreen_RefreshRateInHz = if ctx.consumes_display_mode {
                mode_rate
            } else {
                original
            };
        }
        Some(build_display_mode_ex(&pp))
    } else {
        None
    };

    Attempt { pp, mode }
}

/// The present-parameter policy recorded by [`create_hooked_d3d9_with`] for the current session.
static SESSION_POLICY: MainCell<Option<PresentPolicy>> = MainCell::new(None);

/// Returns the recorded policy, or [`DIRECT_POLICY`] before one exists.
fn session_policy(tok: &MainToken) -> PresentPolicy {
    SESSION_POLICY.get(tok).unwrap_or(DIRECT_POLICY)
}

/// The back buffer format of the live device, or `D3DFMT_UNKNOWN` before one exists.
static ACTIVE_BACK_BUFFER_FORMAT: AtomicU32 = AtomicU32::new(D3DFMT_UNKNOWN.0);

/// The back buffer format the live device was actually created or last reset with, or `None` before the first device exists.
/// This isn't necessarily what the game requested, since [`rewrite_present_params_impl`] and [`run_fix_ladder`]'s escalation
/// both change the format without the in-game caller knowing.
pub fn active_back_buffer_format() -> Option<D3DFORMAT> {
    let format = D3DFORMAT(ACTIVE_BACK_BUFFER_FORMAT.load(Ordering::Relaxed));
    (format != D3DFMT_UNKNOWN).then_some(format)
}

/// Records the format the device ended up with once one is known to be alive.
fn record_back_buffer_format(attempt: Option<&Attempt>) {
    // `Attempt::pp` is the request as we rewrote it, snapshotted before the device call, whereas `d3d8::sync_present_params_back`
    // reads what the runtime wrote back after the device call. `D3DFMT_UNKNOWN` means the game let the runtime choose,
    // so there is no concrete format to reconcile against and it is not recorded.
    if let Some(format) = attempt.map(|a| a.pp.BackBufferFormat)
        && format != D3DFMT_UNKNOWN
    {
        ACTIVE_BACK_BUFFER_FORMAT.store(format.0, Ordering::Relaxed);
    }
}

/// The 32-bit substitute for a 16-bit back buffer format, or `None` for a format that doesn't need substitution.
fn upgraded_back_buffer_format(f: D3DFORMAT) -> Option<D3DFORMAT> {
    match f {
        D3DFMT_R5G6B5 | D3DFMT_X1R5G5B5 => Some(D3DFMT_X8R8G8B8),
        D3DFMT_A1R5G5B5 => Some(D3DFMT_A8R8G8B8),
        _ => None,
    }
}

fn rewrite_present_params_impl(
    pp: &mut D3DPRESENT_PARAMETERS,
    policy: PresentPolicy,
    controls_timing: bool,
) {
    if !policy.keep_lockable_back_buffer {
        pp.Flags &= !D3DPRESENTFLAG_LOCKABLE_BACKBUFFER;
    }
    if controls_timing {
        pp.PresentationInterval = D3DPRESENT_INTERVAL_IMMEDIATE.cast_unsigned();
    }
    let plan = format_plan(
        pp.BackBufferFormat,
        pp.Windowed.0 != 0,
        policy.upgrade_16bit_back_buffer,
    );
    if let Some(upgraded) = plan.upgraded {
        substitute_back_buffer_format(pp, upgraded, "16bit_back_buffer_upgraded");
    }
    if plan.scanout_substituted {
        substitute_back_buffer_format(pp, plan.format, "back_buffer_format_substituted");
    }
}

fn substitute_back_buffer_format(
    pp: &mut D3DPRESENT_PARAMETERS,
    new: D3DFORMAT,
    kind: &'static str,
) {
    let old = pp.BackBufferFormat;
    pp.BackBufferFormat = new;
    info!(kind, old = format_name(old), new = format_name(new));
}

/// Returns the distinct refresh rates advertised by the adapter at exactly `width` × `height` in `format`,
/// plus the count of valid entries. The count is 0 on any failure.
/// At most [`MAX_ENUM_SCAN`] modes are scanned and at most [`MAX_ENUM_RATES`] distinct rates are kept.
unsafe fn enumerate_supported_rates(
    adapter: Adapter<'_>,
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

    let count = unsafe { count_fn(adapter.raw(), adapter.ordinal, &raw const filter) };
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
        let hr = unsafe {
            enum_fn(
                adapter.raw(),
                adapter.ordinal,
                &raw const filter,
                i,
                &raw mut mode,
            )
        };
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

/// Chooses the fullscreen refresh rate for `mode`. Rates are validated against `supported`, the rates the adapter advertises
/// at the target resolution and format; see [`AdapterSnapshot`]. `desktop_rate` is the resolved desktop rate.
fn pick_refresh_rate(mode: RefreshRateMode, supported: &[u32], desktop_rate: u32) -> u32 {
    if let RefreshRateMode::Fixed(target) = mode {
        let target = target.get();
        if supported.is_empty() {
            info!(kind = "refresh_rate_fixed_unvalidated", target_hz = target);
        } else if !supported
            .iter()
            .any(|&r| r == target || normalize_reported_rate(r) == target)
        {
            error!(
                kind = "refresh_rate_fixed_unsupported",
                target_hz = target,
                supported = ?supported,
            );
        }
    }

    let chosen = select_refresh_rate(mode, supported, desktop_rate);
    info!(
        kind = "refresh_rate_decision",
        desktop_rate_hz = desktop_rate,
        supported_n = supported.len(),
        chosen_hz = chosen,
    );
    chosen
}

/// Applies the refresh-rate policy against the adapter's `supported` rates at the target resolution.
/// `desktop_rate` is the raw reported rate.
/// - `Native`: see [`native_rate`].
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
unsafe fn adapter_display_device(adapter: Adapter<'_>) -> Option<[u16; 32]> {
    let monitor_fn = REAL_GET_ADAPTER_MONITOR.try_get()?;
    let monitor = unsafe { monitor_fn(adapter.raw(), adapter.ordinal) };
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
    let name = device.map_or_else(null, |d| d.as_ptr());
    let ok = unsafe { EnumDisplaySettingsExW(name, ENUM_CURRENT_SETTINGS, &raw mut dm, 0) };
    if ok == 0 {
        return None;
    }
    let hz = dm.dmDisplayFrequency;
    is_real_refresh_rate(hz).then_some(hz)
}

/// Populates a `D3DDISPLAYMODEEX` from the present params.
/// Callers should resolve `pp.FullScreen_RefreshRateInHz` to a real rate beforehand, as [`materialize`] does.
///
/// The Ex `CreateDevice` and `Reset` signatures require a fully-filled struct for exclusive fullscreen and a null pointer for windowed
/// (a non-null struct there is `D3DERR_INVALIDCALL`), so callers should only use this when `Windowed == FALSE` holds.
fn build_display_mode_ex(pp: &D3DPRESENT_PARAMETERS) -> D3DDISPLAYMODEEX {
    D3DDISPLAYMODEEX {
        Size: D3DDISPLAYMODEEX_SIZE,
        Width: pp.BackBufferWidth,
        Height: pp.BackBufferHeight,
        RefreshRate: pp.FullScreen_RefreshRateInHz,
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
    adapter: Adapter<'_>,
    device_type: D3DDEVTYPE,
    focus_window: HWND,
    behavior_flags_in: u32,
    behavior_flags: u32,
    pp: *mut D3DPRESENT_PARAMETERS,
    display_mode_ptr: *mut D3DDISPLAYMODEEX,
    returned_device: *mut *mut c_void,
    attempt: u32,
) -> (HRESULT, *mut c_void) {
    let pp_dbg = unsafe { pp.as_ref() };
    info!(
        kind = "create_device_call",
        attempt,
        this = format_args!("{:p}", adapter.raw()),
        adapter = adapter.ordinal,
        device_type = ?device_type,
        behavior_flags_in = format_args!("{behavior_flags_in:#x}"),
        behavior_flags = format_args!("{behavior_flags:#x}"),
        focus_window = format_args!("{:p}", focus_window.0),
        pp = ?pp_dbg,
        display_mode = if display_mode_ptr.is_null() { "null" } else { "set" },
    );
    let hr = unsafe {
        call_real_create_device_ex(
            adapter.raw(),
            adapter.ordinal,
            device_type,
            focus_window,
            behavior_flags,
            pp,
            display_mode_ptr,
            returned_device,
        )
    };
    let dev = if returned_device.is_null() {
        null_mut()
    } else {
        unsafe { *returned_device }
    };
    info!(
        kind = "create_device_result",
        attempt,
        hr = fmt_hr!(hr),
        device = format_args!("{dev:p}"),
    );
    (hr, dev)
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
    if this.is_null() {
        warn!(kind = "d3d9_null_this", call = "IDirect3D9::CreateDevice");
        return D3DERR_INVALIDCALL;
    }
    let Some(tok) = gate_d3d9(this) else {
        // A foreign `IDirect3D9Ex` (e.g. an overlay's) dispatches through the same patched `.rdata` vtable, and the game's own object
        // could in principle be driven from another thread. In this situation, we pass the call through untouched.
        if is_game_d3d9(this) {
            warn!(
                kind = "session_thread_miss",
                call = "IDirect3D9::CreateDevice",
                this = format_args!("{this:p}"),
            );
        }
        return unsafe {
            call_real_create_device(
                this,
                adapter,
                device_type,
                focus_window,
                behavior_flags,
                pp,
                returned_device,
            )
        };
    };

    // The outgoing device's pin is released up front because, in exclusive fullscreen, it holds the display mode and its VRAM,
    // which can be enough for the replacement to be refused. However, its identity stays recorded in case the replacement fails.
    device_creating(&tok);
    on_device_creating(&tok);

    let behavior_flags_in = behavior_flags;
    let behavior_flags = rewrite_behavior_flags(behavior_flags);

    let adapter = unsafe { Adapter::from_hook(&this, adapter) }
        .expect("the gate matched `this` against a non-null recording");

    let requested = unsafe { pp.as_ref().copied() };
    // `CreateDeviceEx` always takes the display mode, so `pp` must keep agreeing with it.
    let ctx = unsafe { LadderCtx::resolve(&tok, adapter, pp, true) };

    let mut dev = null_mut();
    let (hr, attempt) = unsafe {
        run_fix_ladder(
            pp,
            &ctx,
            "create_device_refresh_failsafe",
            |display_mode_ptr, slot| {
                let (hr, d) = create_device_once(
                    adapter,
                    device_type,
                    focus_window,
                    behavior_flags_in,
                    behavior_flags,
                    pp,
                    display_mode_ptr,
                    returned_device,
                    slot,
                );
                dev = d;
                hr
            },
        )
    };

    if hr.is_ok() {
        let Some(dev) = NonNull::new(dev) else {
            warn!(
                kind = "d3d9_null_on_success",
                call = "IDirect3D9Ex::CreateDeviceEx",
                out_param = if returned_device.is_null() {
                    "caller_null"
                } else {
                    "written_null"
                },
            );
            // The ladder left its successful attempt's rewrite (plus driver write-back) in `pp`.
            // This exit is a failure from the game's point of view, so it gets its own request back like every other one.
            if let (Some(p), Some(req)) = (unsafe { pp.as_mut() }, requested) {
                *p = req;
            }
            return D3DERR_INVALIDCALL;
        };

        unsafe {
            install_device_hooks(dev);
            post_device_alive(&tok, dev, attempt.as_ref());
        }

        if let Some(before) = ctx.snapshot.desktop_mode {
            unsafe { warn_if_exclusive_degraded(adapter, before, attempt.as_ref()) };
        }
    }
    hr
}

/// Adds `D3DCREATE_MULTITHREADED`.
fn rewrite_behavior_flags(flags: u32) -> u32 {
    flags | D3DCREATE_MULTITHREADED.cast_unsigned()
}

/// Reads the display mode of `adapter`. Returns `None` on failure.
unsafe fn sample_adapter_display_mode(adapter: Adapter<'_>) -> Option<D3DDISPLAYMODEEX> {
    let mut current = D3DDISPLAYMODEEX {
        Size: D3DDISPLAYMODEEX_SIZE,
        ..D3DDISPLAYMODEEX::default()
    };
    let hr = unsafe {
        call_real_get_adapter_display_mode_ex(
            adapter.raw(),
            adapter.ordinal,
            &raw mut current,
            null_mut(),
        )
    };
    if hr.is_ok() { Some(current) } else { None }
}

/// Heuristic warning for situations where exclusive fullscreen is silently degraded to windowed presentation.
/// It's possible for an adapter to not actually move to the requested mode even if `CreateDeviceEx` returns `S_OK`,
/// so we compare the desktop mode before and after device creation.
///
/// The check is skipped when the requested presentation parameters matches the desktop mode:
/// no mode switch was needed, and exclusive fullscreen vs. windowed are indistinguishable in this case.
unsafe fn warn_if_exclusive_degraded(
    adapter: Adapter<'_>,
    before: D3DDISPLAYMODEEX,
    attempt: Option<&Attempt>,
) {
    let Some(after_pp) = attempt.map(|a| a.pp) else {
        return;
    };
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
    let Some(after) = (unsafe { sample_adapter_display_mode(adapter) }) else {
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
    if !is_game_d3d9(this) {
        // A foreign object gets the strict D3D9Ex answer it asked for, unsubstituted and unlogged.
        return unsafe {
            call_real_check_device_format(
                this,
                adapter,
                device_type,
                adapter_format,
                usage,
                rtype,
                check_format,
            )
        };
    }

    let forwarded_adapter_fmt = if adapter_format == D3DFMT_A8R8G8B8 {
        D3DFMT_X8R8G8B8
    } else {
        adapter_format
    };

    let substituted = forwarded_adapter_fmt != adapter_format;

    let hr = unsafe {
        call_real_check_device_format(
            this,
            adapter,
            device_type,
            forwarded_adapter_fmt,
            usage,
            rtype,
            check_format,
        )
    };

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

unsafe fn install_device_hooks(dev: NonNull<c_void>) {
    let vtbl = unsafe { *dev.as_ptr().cast() };
    let Some(vtbl) = NonNull::new(vtbl) else {
        warn!(kind = "device_vtbl_null", dev = format_args!("{dev:p}"));
        return;
    };

    unsafe {
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
    }

    let result = unsafe {
        install_vtable(vtbl, |scope| {
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
        })
    };
    info!(
        kind = "d3d9_device_hooks_installed",
        protect_ok = result.is_some()
    );
}

/// `SetMaximumFrameLatency(1)` caps the GPU input queue at 1 (default 3) so frames spend less time enqueued before display,
/// shaving up to two frames of end-to-end latency. `SetGPUThreadPriority(7)` raises the device's WDDM GPU-scheduling priority
/// so its command submissions are preferred over other processes' GPU work.
unsafe fn apply_device_ex_tunables(dev: NonNull<c_void>) {
    let latency_hr = unsafe { call_real_set_max_frame_latency(dev.as_ptr(), 1) };
    info!(
        kind = "set_max_frame_latency",
        value = 1,
        hr = %fmt_hr!(latency_hr),
    );
    let gpu_pri_hr = unsafe { call_real_set_gpu_thread_priority(dev.as_ptr(), 7) };
    info!(
        kind = "set_gpu_thread_priority",
        value = 7,
        hr = %fmt_hr!(gpu_pri_hr),
    );
}

/// Re-applies the device tunables, since D3D9Ex preserves them across `Reset` but a translation layer might not.
/// Also records (and pins) the device as the session's. Fires after successful `CreateDeviceEx` and successful `Reset` / `ResetEx`.
unsafe fn post_device_alive(tok: &MainToken, dev: NonNull<c_void>, attempt: Option<&Attempt>) {
    unsafe { apply_device_ex_tunables(dev) };
    // SAFETY: `dev` is the live device the successful call just created or reset.
    unsafe { record_device(tok, dev) };
    record_back_buffer_format(attempt);
}

unsafe extern "system" fn hook_present(
    this: *mut c_void,
    src_rect: *const RECT,
    dst_rect: *const RECT,
    dest_window_override: HWND,
    dirty_region: *const RGNDATA,
) -> HRESULT {
    // `install_device_hooks` patches the `IDirect3DDevice9Ex` vtable in place, and every device in the process shares it,
    // so devices other than the game's on its render thread could reach this hook.
    let Some((tok, dev)) = gate_device(this) else {
        return unsafe {
            call_real_present(this, src_rect, dst_rect, dest_window_override, dirty_region)
        };
    };

    if let Some(pacer) = PACER.get() {
        if let Some((mode, policy)) = policy_change(&tok) {
            apply_policy_change(&tok, pacer, mode, policy);
        }
        pacer.wait(&tok);
    }

    on_pre_present(&tok, dev);

    // We increment before `Present` so `PRESENT_COUNT` uses the in-flight frame.
    // This way, a crash inside `Present` leaves the count at the attempted frame, not the last completed.
    PRESENT_COUNT.fetch_add(1, Ordering::Relaxed);

    let hr =
        unsafe { call_real_present(this, src_rect, dst_rect, dest_window_override, dirty_region) };
    log_present_outcome(&tok, hr);
    hr
}

/// Logs and applies a replay-mode transition.
#[cold]
#[inline(never)]
fn apply_policy_change(tok: &MainToken, pacer: &Pacer, mode: ReplayMode, policy: PacingPolicy) {
    info!(
        kind = "replay_mode_change",
        mode = ?mode,
        target_fps = policy.target_fps(),
        frame = PRESENT_COUNT.load(Ordering::Relaxed),
    );
    pacer.apply_policy(tok, policy);
}

/// Emits a log event whenever the result of `Present` differs from the previous call's result.
fn log_present_outcome(tok: &MainToken, hr: HRESULT) {
    let prev = LAST_PRESENT.get(tok);
    if hr == prev.hr {
        return;
    }
    log_present_changed(tok, hr, prev);
}

/// Records and logs a changed `Present` result.
#[cold]
#[inline(never)]
fn log_present_changed(tok: &MainToken, hr: HRESULT, prev: PresentState) {
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
    let pp_dbg = unsafe { pp.as_ref() };
    info!(
        kind = "reset_call",
        attempt,
        this = format_args!("{this:p}"),
        pp = ?pp_dbg,
        display_mode = if display_mode_ptr.is_null() { "null" } else { "set" },
        path = if use_reset_ex { "ResetEx" } else { "Reset" },
    );
    // Plain `Reset` on Alt+Enter crashed for a tester, but `ResetEx` didn't.
    let hr = unsafe {
        if use_reset_ex {
            call_real_reset_ex(this, pp, display_mode_ptr)
        } else {
            call_real_reset(this, pp)
        }
    };
    info!(kind = "reset_result", attempt, hr = fmt_hr!(hr));
    hr
}

/// The parent `IDirect3D9Ex` and adapter ordinal of a live device.
struct DeviceParent {
    /// The reference returned by `GetDirect3D`.
    d3d9: IDirect3D9,
    adapter: u32,
}

impl DeviceParent {
    /// Queries the parent interface and adapter ordinal of a live device. Returns `None` if either query fails.
    ///
    /// # Safety
    /// `dev` must be null or a live `IDirect3DDevice9Ex`.
    unsafe fn new(dev: *mut c_void) -> Option<Self> {
        let dev = unsafe { IDirect3DDevice9Ex::from_raw_borrowed(&dev) }?;
        let d3d9 = match unsafe { dev.GetDirect3D() } {
            Ok(d3d9) => d3d9,
            Err(e) => {
                warn!(
                    kind = "reset_parent_query_failed",
                    call = "GetDirect3D",
                    hr = %fmt_hr!(e.code()),
                );
                return None;
            }
        };

        let mut cp = D3DDEVICE_CREATION_PARAMETERS::default();
        if let Err(e) = unsafe { dev.GetCreationParameters(&raw mut cp) } {
            warn!(
                kind = "reset_parent_query_failed",
                call = "GetCreationParameters",
                hr = %fmt_hr!(e.code()),
            );
            return None;
        }

        Some(Self {
            d3d9,
            adapter: cp.AdapterOrdinal,
        })
    }

    /// Borrows the owned interface for the adapter queries. The context must not outlive `self`,
    /// which holds the reference returned by `GetDirect3D`.
    fn adapter(&self) -> Adapter<'_> {
        Adapter {
            d3d9: &self.d3d9,
            ordinal: self.adapter,
        }
    }
}

unsafe extern "system" fn hook_reset(this: *mut c_void, pp: *mut D3DPRESENT_PARAMETERS) -> HRESULT {
    if this.is_null() {
        warn!(kind = "d3d9_null_this", call = "IDirect3DDevice9::Reset");
        return D3DERR_INVALIDCALL;
    }

    let Some((tok, dev)) = gate_device(this) else {
        if is_game_device(this) {
            warn!(
                kind = "session_thread_miss",
                call = "IDirect3DDevice9::Reset",
                this = format_args!("{this:p}"),
            );
        }
        return unsafe { call_real_reset(this, pp) };
    };

    on_pre_reset(&tok);

    let Some(parent) = (unsafe { DeviceParent::new(this) }) else {
        return unsafe { call_real_reset(this, pp) };
    };

    let adapter = parent.adapter();
    let use_reset_ex = REAL_RESET_EX.try_get().is_some();
    // Plain `Reset` ignores the display mode entirely, so on that path `pp` is the whole request.
    let ctx = unsafe { LadderCtx::resolve(&tok, adapter, pp, use_reset_ex) };

    // We reapply refresh rate selection so runtime rate toggles take effect at the next `Reset`.
    let (hr, attempt) = unsafe {
        run_fix_ladder(
            pp,
            &ctx,
            "reset_refresh_failsafe",
            |display_mode_ptr, slot| reset_once(this, pp, display_mode_ptr, use_reset_ex, slot),
        )
    };

    if hr.is_ok() {
        // `Reset` keeps the same object, so the gate's device is still the one that just came back alive.
        unsafe { post_device_alive(&tok, dev, attempt.as_ref()) };
        if let Some(before) = ctx.snapshot.desktop_mode {
            unsafe { warn_if_exclusive_degraded(adapter, before, attempt.as_ref()) };
        }
    }
    hr
}

/// Runs a `CreateDeviceEx`, `Reset`, or `ResetEx` call through the attempt sequence/ladder from [`plan_attempts`].
///
/// On success, the runtime's write-back is left in `pp` for the game to read; when every attempt fails,
/// `pp` is restored to the game's own request so its retry logic sees its own values, not our rewrites.
///
/// `attempt_fn` receives the fullscreen display-mode pointer (null for windowed) and the ladder slot index,
/// which is the attempt number in logs.
///
/// Returns the final result and the last attempt made, or `None` when `pp` is null and a single bare call was made.
unsafe fn run_fix_ladder(
    pp: *mut D3DPRESENT_PARAMETERS,
    ctx: &LadderCtx,
    failsafe_kind: &'static str,
    mut attempt_fn: impl FnMut(*mut D3DDISPLAYMODEEX, u32) -> HRESULT,
) -> (HRESULT, Option<Attempt>) {
    let Some(requested) = (unsafe { pp.as_ref() }).copied() else {
        return (attempt_fn(null_mut(), 0), None);
    };

    let escalatable = upgraded_back_buffer_format(requested.BackBufferFormat).is_some();
    let ladder = plan_attempts(ctx.policy.upgrade_16bit_back_buffer, escalatable);
    let materialize_slot = |fixes: FixSet| materialize(&requested, fixes, ctx);

    // Round 0 always runs, so every `last`-returning exit has been overwritten by then.
    let mut last = (D3DERR_INVALIDCALL, None);
    for (round_index, round) in ladder.iter().enumerate() {
        let Some(round) = round else { break };
        #[allow(clippy::cast_possible_truncation)]
        let primary_slot = (round_index * 2) as u32;
        if round_index > 0 {
            // A 16-bit fullscreen mode may simply not exist on this adapter, and a substituted format beats no device at all.
            warn!(
                kind = "back_buffer_upgrade_escalated",
                context = failsafe_kind,
                hr = %fmt_hr!(last.0),
            );
        }

        let mut attempt = materialize_slot(round.primary);
        info!(kind = "present_rewrite", pp_before = ?Some(requested), pp_after = ?Some(attempt.pp));
        unsafe { *pp = attempt.pp };
        let mut hr = attempt_fn(attempt.mode_ptr(), primary_slot);
        if hr.is_ok() {
            return (hr, Some(attempt));
        }

        let chosen = attempt.pp.FullScreen_RefreshRateInHz;
        let overrode_fs = attempt.mode.is_some() && chosen != requested.FullScreen_RefreshRateInHz;
        if overrode_fs {
            if is_transient_device_error(hr) {
                info!(
                    kind = "refresh_failsafe_declined",
                    context = failsafe_kind,
                    hr = fmt_hr!(hr),
                    chosen_hz = chosen,
                );
                unsafe { *pp = requested };
                return (hr, Some(attempt));
            }

            let mut rollback = materialize_slot(round.rollback);
            warn!(
                kind = failsafe_kind,
                from_hz = chosen,
                to_hz = rollback.pp.FullScreen_RefreshRateInHz,
                first_hr = fmt_hr!(hr),
            );
            unsafe { *pp = rollback.pp };
            hr = attempt_fn(rollback.mode_ptr(), primary_slot + 1);
            attempt = rollback;
            if hr.is_ok() {
                return (hr, Some(attempt));
            }
        }

        last = (hr, Some(attempt));
        if is_transient_device_error(hr) {
            break;
        }
    }

    // Every attempt failed: hand the game back its own request, not our last rewrite.
    unsafe { *pp = requested };
    last
}

pub(crate) fn is_transient_device_error(hr: HRESULT) -> bool {
    matches!(
        hr,
        D3DERR_DEVICELOST | D3DERR_DEVICEREMOVED | D3DERR_DEVICEHUNG | D3DERR_OUTOFVIDEOMEMORY
    )
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
    if !is_game_device(this) {
        // A foreign device may be plain D3D9 where `D3DPOOL_MANAGED` is legitimate, so its requests pass through untranslated.
        return unsafe {
            call_real_create_texture(
                this,
                width,
                height,
                levels,
                usage,
                format,
                pool,
                pp_texture,
                p_shared_handle,
            )
        };
    }

    let usage_orig = usage;
    let pool_orig = pool;
    translate_managed_pool(&mut pool, &mut usage);
    let hr = unsafe {
        call_real_create_texture(
            this,
            width,
            height,
            levels,
            usage,
            format,
            pool,
            pp_texture,
            p_shared_handle,
        )
    };
    let returned = unsafe { out_ptr(pp_texture) };

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

unsafe extern "system" fn hook_create_vertex_buffer(
    this: *mut c_void,
    length: u32,
    mut usage: u32,
    fvf: u32,
    mut pool: D3DPOOL,
    pp_vertex_buffer: *mut *mut c_void,
    p_shared_handle: *mut HANDLE,
) -> HRESULT {
    if !is_game_device(this) {
        return unsafe {
            call_real_create_vertex_buffer(
                this,
                length,
                usage,
                fvf,
                pool,
                pp_vertex_buffer,
                p_shared_handle,
            )
        };
    }

    let usage_orig = usage;
    let pool_orig = pool;
    translate_managed_pool(&mut pool, &mut usage);
    let hr = unsafe {
        call_real_create_vertex_buffer(
            this,
            length,
            usage,
            fvf,
            pool,
            pp_vertex_buffer,
            p_shared_handle,
        )
    };
    let returned = unsafe { out_ptr(pp_vertex_buffer) };

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

#[cfg(test)]
mod tests {
    use super::{
        AdapterSnapshot, Attempt, D3D_OK, D3DDISPLAYMODEEX_SIZE, D3DERR_DEVICEHUNG,
        D3DERR_DEVICELOST, D3DERR_DEVICEREMOVED, D3DERR_INVALIDCALL, D3DERR_OUTOFVIDEOMEMORY,
        FixSet, LadderCtx, MAX_ENUM_RATES, PresentPolicy, RefreshRateMode, Round,
        build_display_mode_ex, format_name, is_real_refresh_rate, is_transient_device_error,
        materialize, normalize_reported_rate, plan_attempts, rewrite_behavior_flags,
        rewrite_present_params_impl, run_fix_ladder, select_refresh_rate, translate_managed_pool,
        upgraded_back_buffer_format,
    };
    use crate::fmt_hr;
    use std::num::NonZero;
    use std::ptr::null_mut;
    use windows::Win32::Graphics::Direct3D9::D3DDISPLAYMODEEX;
    use windows::Win32::Graphics::Direct3D9::{
        D3DCREATE_HARDWARE_VERTEXPROCESSING, D3DCREATE_MULTITHREADED, D3DFMT_A1R5G5B5,
        D3DFMT_A8R8G8B8, D3DFMT_R5G6B5, D3DFMT_X1R5G5B5, D3DFMT_X8R8G8B8, D3DFORMAT,
        D3DPOOL_DEFAULT, D3DPOOL_MANAGED, D3DPOOL_SCRATCH, D3DPOOL_SYSTEMMEM,
        D3DPRESENT_INTERVAL_IMMEDIATE, D3DPRESENT_INTERVAL_ONE, D3DPRESENT_PARAMETERS,
        D3DPRESENTFLAG_DISCARD_DEPTHSTENCIL, D3DPRESENTFLAG_LOCKABLE_BACKBUFFER,
        D3DSCANLINEORDERING_PROGRESSIVE, D3DSWAPEFFECT_DISCARD, D3DUSAGE_DYNAMIC,
        D3DUSAGE_WRITEONLY,
    };
    use windows::core::HRESULT;

    fn policy(keep_lockable_back_buffer: bool) -> PresentPolicy {
        PresentPolicy {
            keep_lockable_back_buffer,
            upgrade_16bit_back_buffer: false,
        }
    }

    fn upgrading() -> PresentPolicy {
        PresentPolicy {
            keep_lockable_back_buffer: false,
            upgrade_16bit_back_buffer: true,
        }
    }

    #[test]
    fn upgraded_back_buffer_format_16bit_only() {
        assert_eq!(
            upgraded_back_buffer_format(D3DFMT_R5G6B5),
            Some(D3DFMT_X8R8G8B8)
        );
        assert_eq!(
            upgraded_back_buffer_format(D3DFMT_X1R5G5B5),
            Some(D3DFMT_X8R8G8B8)
        );
        assert_eq!(
            upgraded_back_buffer_format(D3DFMT_A1R5G5B5),
            Some(D3DFMT_A8R8G8B8)
        );
        assert_eq!(upgraded_back_buffer_format(D3DFMT_X8R8G8B8), None);
        assert_eq!(upgraded_back_buffer_format(D3DFMT_A8R8G8B8), None);
    }

    #[test]
    fn back_buffer_upgrade_depends_on_policy() {
        for (requested, upgraded) in [
            (D3DFMT_R5G6B5, D3DFMT_X8R8G8B8),
            (D3DFMT_X1R5G5B5, D3DFMT_X8R8G8B8),
            // This is a windowed format, so the `A8R8G8B8` fullscreen rule below doesn't also fire.
            (D3DFMT_A1R5G5B5, D3DFMT_A8R8G8B8),
        ] {
            let base = D3DPRESENT_PARAMETERS {
                BackBufferFormat: requested,
                Windowed: true.into(),
                ..Default::default()
            };

            let mut off = base;
            rewrite_present_params_impl(&mut off, policy(false), false);
            assert_eq!(off.BackBufferFormat, requested);

            let mut on = base;
            rewrite_present_params_impl(&mut on, upgrading(), false);
            assert_eq!(on.BackBufferFormat, upgraded);
        }

        let mut pp = D3DPRESENT_PARAMETERS {
            BackBufferFormat: D3DFMT_A1R5G5B5,
            Windowed: false.into(),
            ..Default::default()
        };
        rewrite_present_params_impl(&mut pp, upgrading(), false);
        assert_eq!(pp.BackBufferFormat, D3DFMT_X8R8G8B8);
    }

    fn nz(n: u32) -> NonZero<u32> {
        NonZero::new(n).unwrap()
    }

    #[test]
    fn rewrite_present_params_interval() {
        for original in [
            0u32,
            D3DPRESENT_INTERVAL_ONE.cast_unsigned(),
            D3DPRESENT_INTERVAL_IMMEDIATE.cast_unsigned(),
        ] {
            let base = D3DPRESENT_PARAMETERS {
                PresentationInterval: original,
                ..Default::default()
            };

            // When the pacer controls timing, any interval becomes IMMEDIATE so `Present` never blocks.
            let mut paced = base;
            rewrite_present_params_impl(&mut paced, policy(false), true);
            assert_eq!(
                paced.PresentationInterval,
                D3DPRESENT_INTERVAL_IMMEDIATE.cast_unsigned(),
                "input interval {original:#x}",
            );

            // Otherwise, the game's own choice stands untouched.
            let mut unpaced = base;
            rewrite_present_params_impl(&mut unpaced, policy(false), false);
            assert_eq!(
                unpaced.PresentationInterval, original,
                "input interval {original:#x}",
            );
        }
    }

    #[test]
    fn rewrite_present_params_lockable_back_buffer() {
        let mut pp = D3DPRESENT_PARAMETERS {
            Flags: D3DPRESENTFLAG_LOCKABLE_BACKBUFFER,
            ..Default::default()
        };
        rewrite_present_params_impl(&mut pp, policy(false), true);
        assert_eq!(pp.Flags, 0);

        let mut pp = D3DPRESENT_PARAMETERS {
            Flags: D3DPRESENTFLAG_LOCKABLE_BACKBUFFER | D3DPRESENTFLAG_DISCARD_DEPTHSTENCIL,
            ..Default::default()
        };
        rewrite_present_params_impl(&mut pp, policy(false), true);
        assert_eq!(pp.Flags, D3DPRESENTFLAG_DISCARD_DEPTHSTENCIL);

        let mut pp = D3DPRESENT_PARAMETERS {
            Flags: D3DPRESENTFLAG_LOCKABLE_BACKBUFFER,
            ..Default::default()
        };
        rewrite_present_params_impl(&mut pp, policy(true), true);
        assert_eq!(pp.Flags, D3DPRESENTFLAG_LOCKABLE_BACKBUFFER);
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
        rewrite_present_params_impl(&mut pp, policy(false), true);
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
        let cases = &[
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
            rewrite_present_params_impl(&mut pp, policy(false), true);
            assert_eq!(
                pp.BackBufferFormat, expected,
                "src={src:?} windowed={windowed}",
            );
        }
    }

    #[test]
    fn translate_managed_pool_translation() {
        let mut pool = D3DPOOL_MANAGED;
        let mut usage = 0;
        assert!(translate_managed_pool(&mut pool, &mut usage));
        assert_eq!(pool, D3DPOOL_DEFAULT);
        assert_eq!(usage, D3DUSAGE_DYNAMIC.cast_unsigned());

        let mut pool = D3DPOOL_MANAGED;
        let mut usage = D3DUSAGE_WRITEONLY.cast_unsigned();
        assert!(translate_managed_pool(&mut pool, &mut usage));
        assert_eq!(pool, D3DPOOL_DEFAULT);
        assert_eq!(
            usage,
            D3DUSAGE_DYNAMIC.cast_unsigned() | D3DUSAGE_WRITEONLY.cast_unsigned(),
        );

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
    fn build_display_mode_ex_derive_fields_from_pp() {
        let pp = D3DPRESENT_PARAMETERS {
            BackBufferWidth: 1280,
            BackBufferHeight: 960,
            BackBufferFormat: D3DFMT_X8R8G8B8,
            FullScreen_RefreshRateInHz: 120,
            ..Default::default()
        };
        let mode = build_display_mode_ex(&pp);
        assert_eq!(mode.Size, D3DDISPLAYMODEEX_SIZE);
        assert_eq!(mode.Width, pp.BackBufferWidth);
        assert_eq!(mode.Height, pp.BackBufferHeight);
        assert_eq!(mode.Format, pp.BackBufferFormat);
        assert_eq!(mode.RefreshRate, pp.FullScreen_RefreshRateInHz);
        assert_eq!(mode.ScanLineOrdering, D3DSCANLINEORDERING_PROGRESSIVE);
    }

    fn fixes(rate_override: bool, format_upgrade: bool) -> FixSet {
        FixSet {
            rate_override,
            format_upgrade,
        }
    }

    fn snapshot(
        desktop_hz: Option<u32>,
        desktop_rate: u32,
        rates: &[(D3DFORMAT, &[u32])],
    ) -> AdapterSnapshot {
        let mut snap = AdapterSnapshot::empty();
        snap.desktop_mode = desktop_hz.map(|hz| D3DDISPLAYMODEEX {
            Size: D3DDISPLAYMODEEX_SIZE,
            RefreshRate: hz,
            ..Default::default()
        });
        snap.desktop_rate = desktop_rate;
        for (i, (format, list)) in rates.iter().enumerate() {
            let mut table = [0u32; MAX_ENUM_RATES];
            table[..list.len()].copy_from_slice(list);
            snap.rates[i] = (*format, (table, list.len()));
            snap.rates_len = i + 1;
        }
        snap
    }

    fn fs_pp(format: D3DFORMAT, hz: u32) -> D3DPRESENT_PARAMETERS {
        D3DPRESENT_PARAMETERS {
            BackBufferWidth: 640,
            BackBufferHeight: 480,
            BackBufferFormat: format,
            Windowed: false.into(),
            FullScreen_RefreshRateInHz: hz,
            ..Default::default()
        }
    }

    fn ctx(
        snapshot: AdapterSnapshot,
        policy: PresentPolicy,
        controls_timing: bool,
        refresh_mode: RefreshRateMode,
        consumes_display_mode: bool,
    ) -> LadderCtx {
        LadderCtx {
            snapshot,
            policy,
            controls_timing,
            refresh_mode,
            consumes_display_mode,
        }
    }

    fn round(format_upgrade: bool) -> Round {
        Round {
            primary: fixes(true, format_upgrade),
            rollback: fixes(false, format_upgrade),
        }
    }

    #[test]
    fn plan_attempts_sequences() {
        // Shim policy
        for escalatable in [false, true] {
            assert_eq!(plan_attempts(true, escalatable), [Some(round(true)), None]);
        }

        // Direct policy + 16-bit request
        assert_eq!(
            plan_attempts(false, true),
            [Some(round(false)), Some(round(true))],
        );

        // Direct policy, nothing to escalate to
        assert_eq!(plan_attempts(false, false), [Some(round(false)), None]);
    }

    #[test]
    fn materialize_mode_agreement() {
        let snap = snapshot(Some(120), 120, &[(D3DFMT_X8R8G8B8, &[60, 120])]);
        let c = ctx(snap, policy(false), true, RefreshRateMode::Native, true);
        let att = materialize(&fs_pp(D3DFMT_X8R8G8B8, 0), fixes(true, false), &c);
        let mode = att.mode.expect("fullscreen materializes a display mode");
        assert_eq!(mode.Width, att.pp.BackBufferWidth);
        assert_eq!(mode.Height, att.pp.BackBufferHeight);
        assert_eq!(mode.Format, att.pp.BackBufferFormat);
        assert_eq!(mode.RefreshRate, att.pp.FullScreen_RefreshRateInHz);
        assert_eq!(att.pp.FullScreen_RefreshRateInHz, 120);

        let windowed = D3DPRESENT_PARAMETERS {
            Windowed: true.into(),
            ..fs_pp(D3DFMT_X8R8G8B8, 0)
        };
        let att = materialize(&windowed, fixes(true, false), &c);
        assert!(att.mode.is_none());
        assert_eq!(att.pp.FullScreen_RefreshRateInHz, 0);
    }

    #[test]
    fn materialize_rate_pipeline() {
        let snap = snapshot(Some(144), 144, &[(D3DFMT_X8R8G8B8, &[60, 120, 144])]);
        let mk = |hz, controls_timing, mode| {
            let c = ctx(snap.clone(), policy(false), controls_timing, mode, true);
            materialize(&fs_pp(D3DFMT_X8R8G8B8, hz), fixes(true, false), &c)
                .pp
                .FullScreen_RefreshRateInHz
        };

        // With the pacer, the configured mode overrides the game's rate.
        assert_eq!(mk(60, true, RefreshRateMode::Native), 144);
        assert_eq!(mk(60, true, RefreshRateMode::NativeMultiple), 120);
        assert_eq!(mk(60, true, RefreshRateMode::Fixed(nz(120))), 120);
        // A `Fixed` target below 2 is a magic value; the display mode resolves it with the `Native` pick.
        assert_eq!(mk(0, true, RefreshRateMode::Fixed(nz(1))), 144);
        // Without the pacer, a real game rate passes through untouched.
        assert_eq!(mk(75, false, RefreshRateMode::Fixed(nz(120))), 75);
        // However, a magic value is still resolved for the display mode.
        assert_eq!(mk(0, false, RefreshRateMode::Fixed(nz(120))), 144);
    }

    #[test]
    fn materialize_rates_follow_materialized_format() {
        // The 16-bit and upgraded formats advertise different mode sets; each attempt consults its own format's table.
        let snap = snapshot(
            Some(144),
            144,
            &[(D3DFMT_R5G6B5, &[60]), (D3DFMT_X8R8G8B8, &[60, 144])],
        );
        let requested = fs_pp(D3DFMT_R5G6B5, 0);
        let c = ctx(snap, policy(false), true, RefreshRateMode::Native, true);
        let plain = materialize(&requested, fixes(true, false), &c);
        assert_eq!(plain.pp.BackBufferFormat, D3DFMT_R5G6B5);
        assert_eq!(plain.pp.FullScreen_RefreshRateInHz, 60);
        let upgraded = materialize(&requested, fixes(true, true), &c);
        assert_eq!(upgraded.pp.BackBufferFormat, D3DFMT_X8R8G8B8);
        assert_eq!(upgraded.pp.FullScreen_RefreshRateInHz, 144);
    }

    #[test]
    fn materialize_rollback_rate_resolution() {
        let snap = snapshot(Some(59), 60, &[(D3DFMT_X8R8G8B8, &[60, 120])]);
        let mk = |hz, consumes| {
            let c = ctx(
                snap.clone(),
                policy(false),
                true,
                RefreshRateMode::Native,
                consumes,
            );
            materialize(&fs_pp(D3DFMT_X8R8G8B8, hz), fixes(false, false), &c)
        };

        // A real game rate rolls back to itself.
        assert_eq!(mk(120, true).pp.FullScreen_RefreshRateInHz, 120);
        // A magic value resolves to the sampled desktop rate where the Ex display mode must carry a real one.
        let att = mk(0, true);
        assert_eq!(att.pp.FullScreen_RefreshRateInHz, 59);
        assert_eq!(att.mode.unwrap().RefreshRate, 59);
        // However, plain `Reset` keeps the game's raw value, since there `pp` is the whole request.
        assert_eq!(mk(0, false).pp.FullScreen_RefreshRateInHz, 0);

        let no_desktop = snapshot(None, 60, &[(D3DFMT_X8R8G8B8, &[120])]);
        let c = ctx(
            no_desktop,
            policy(false),
            true,
            RefreshRateMode::Native,
            true,
        );
        let att = materialize(&fs_pp(D3DFMT_X8R8G8B8, 0), fixes(false, false), &c);
        assert_eq!(att.pp.FullScreen_RefreshRateInHz, 60);
    }

    #[test]
    fn materialize_shim_policy_changes() {
        let shim = PresentPolicy {
            keep_lockable_back_buffer: true,
            upgrade_16bit_back_buffer: true,
        };
        let snap = snapshot(Some(60), 60, &[(D3DFMT_X8R8G8B8, &[60])]);
        let requested = D3DPRESENT_PARAMETERS {
            Flags: D3DPRESENTFLAG_LOCKABLE_BACKBUFFER,
            ..fs_pp(D3DFMT_R5G6B5, 0)
        };
        let first = plan_attempts(true, true)[0].unwrap().primary;
        let c = ctx(snap, shim, true, RefreshRateMode::Native, true);
        let att = materialize(&requested, first, &c);
        assert_eq!(att.pp.BackBufferFormat, D3DFMT_X8R8G8B8);
        assert_eq!(att.pp.Flags, D3DPRESENTFLAG_LOCKABLE_BACKBUFFER);
        assert_eq!(
            att.pp.PresentationInterval,
            D3DPRESENT_INTERVAL_IMMEDIATE.cast_unsigned(),
        );
    }

    struct LadderRun {
        hr: HRESULT,
        /// Per call: the slot index, the caller-struct contents at call time, and the display mode handed over.
        calls: Vec<(u32, D3DPRESENT_PARAMETERS, Option<D3DDISPLAYMODEEX>)>,
        /// The caller struct after the ladder finished.
        final_pp: D3DPRESENT_PARAMETERS,
        attempt: Option<Attempt>,
    }

    fn run_ladder(
        requested: D3DPRESENT_PARAMETERS,
        c: &LadderCtx,
        script: &[HRESULT],
        write_back: Option<u32>,
    ) -> LadderRun {
        let mut pp = requested;
        let pp_ptr = &raw mut pp;
        let mut calls = Vec::new();
        let (hr, attempt) = unsafe {
            run_fix_ladder(pp_ptr, c, "test_failsafe", |mode_ptr, slot| {
                let hr = script[calls.len()];
                calls.push((slot, *pp_ptr, mode_ptr.as_ref().copied()));
                // Simulated runtime write-back into the caller's struct on success.
                if hr.is_ok()
                    && let Some(marker) = write_back
                {
                    (*pp_ptr).BackBufferCount = marker;
                }
                hr
            })
        };
        LadderRun {
            hr,
            calls,
            final_pp: pp,
            attempt,
        }
    }

    #[test]
    fn ladder_success_first_try_keeps_write_back() {
        let snap = snapshot(Some(120), 120, &[(D3DFMT_X8R8G8B8, &[60, 120])]);
        let run = run_ladder(
            fs_pp(D3DFMT_X8R8G8B8, 0),
            &ctx(snap, policy(false), true, RefreshRateMode::Native, true),
            &[D3D_OK],
            Some(7),
        );
        assert!(run.hr.is_ok());
        assert_eq!(run.calls.len(), 1);
        assert_eq!(run.calls[0].0, 0);
        assert_eq!(run.calls[0].1.FullScreen_RefreshRateInHz, 120);
        assert_eq!(run.final_pp.BackBufferCount, 7);
        assert_eq!(run.attempt.unwrap().pp.FullScreen_RefreshRateInHz, 120);
    }

    #[test]
    fn ladder_rollback_after_refused_override() {
        let snap = snapshot(Some(59), 59, &[(D3DFMT_X8R8G8B8, &[60, 120])]);
        let run = run_ladder(
            fs_pp(D3DFMT_X8R8G8B8, 0),
            &ctx(
                snap,
                policy(false),
                true,
                RefreshRateMode::Fixed(nz(120)),
                true,
            ),
            &[D3DERR_INVALIDCALL, D3D_OK],
            None,
        );
        assert!(run.hr.is_ok());
        assert_eq!(run.calls.len(), 2);
        assert_eq!(
            (run.calls[0].0, run.calls[0].1.FullScreen_RefreshRateInHz),
            (0, 120)
        );
        assert_eq!(
            (run.calls[1].0, run.calls[1].1.FullScreen_RefreshRateInHz),
            (1, 59)
        );
        assert_eq!(run.calls[1].2.unwrap().RefreshRate, 59);
    }

    #[test]
    fn ladder_rollback_skipped_when_rate_unchanged() {
        let snap = snapshot(Some(60), 60, &[(D3DFMT_X8R8G8B8, &[60])]);
        let run = run_ladder(
            fs_pp(D3DFMT_X8R8G8B8, 60),
            &ctx(snap, policy(false), true, RefreshRateMode::Native, true),
            &[D3DERR_INVALIDCALL],
            None,
        );
        assert_eq!(run.hr, D3DERR_INVALIDCALL);
        assert_eq!(run.calls.len(), 1);
        assert_eq!(run.final_pp.PresentationInterval, 0);
        assert_eq!(run.final_pp.FullScreen_RefreshRateInHz, 60);
    }

    #[test]
    fn ladder_redundant_rollback_still_retries() {
        let snap = snapshot(Some(60), 60, &[(D3DFMT_X8R8G8B8, &[60])]);
        let run = run_ladder(
            fs_pp(D3DFMT_X8R8G8B8, 0),
            &ctx(snap, policy(false), true, RefreshRateMode::Native, true),
            &[D3DERR_INVALIDCALL, D3DERR_INVALIDCALL],
            None,
        );
        assert_eq!(run.calls.len(), 2);
        assert_eq!(run.calls[0].1.FullScreen_RefreshRateInHz, 60);
        assert_eq!(run.calls[1].1.FullScreen_RefreshRateInHz, 60);
        assert_eq!(run.final_pp.FullScreen_RefreshRateInHz, 0);
    }

    #[test]
    fn ladder_transient_stops_everything() {
        let snap = snapshot(
            Some(120),
            120,
            &[(D3DFMT_R5G6B5, &[60]), (D3DFMT_X8R8G8B8, &[60, 120])],
        );
        let c = ctx(snap, policy(false), true, RefreshRateMode::Native, true);
        let run = run_ladder(fs_pp(D3DFMT_R5G6B5, 0), &c, &[D3DERR_DEVICELOST], None);
        assert_eq!(run.hr, D3DERR_DEVICELOST);
        assert_eq!(run.calls.len(), 1);

        let run = run_ladder(
            fs_pp(D3DFMT_R5G6B5, 0),
            &c,
            &[D3DERR_INVALIDCALL, D3DERR_DEVICELOST],
            None,
        );
        assert_eq!(run.hr, D3DERR_DEVICELOST);
        assert_eq!(run.calls.len(), 2);
    }

    #[test]
    fn ladder_full_escalation_rederives_from_the_request() {
        let snap = snapshot(
            Some(120),
            120,
            &[(D3DFMT_R5G6B5, &[60]), (D3DFMT_X8R8G8B8, &[60, 120])],
        );
        let run = run_ladder(
            fs_pp(D3DFMT_R5G6B5, 0),
            &ctx(snap, policy(false), true, RefreshRateMode::Native, true),
            &[D3DERR_INVALIDCALL; 4],
            None,
        );
        assert_eq!(run.hr, D3DERR_INVALIDCALL);
        let seen: Vec<_> = run
            .calls
            .iter()
            .map(|(slot, pp, _)| (*slot, pp.BackBufferFormat, pp.FullScreen_RefreshRateInHz))
            .collect();
        assert_eq!(
            seen,
            vec![
                (0, D3DFMT_R5G6B5, 60),
                (1, D3DFMT_R5G6B5, 120),
                (2, D3DFMT_X8R8G8B8, 120),
                (3, D3DFMT_X8R8G8B8, 120),
            ],
        );
        assert_eq!(run.final_pp.BackBufferFormat, D3DFMT_R5G6B5);
        assert_eq!(run.final_pp.FullScreen_RefreshRateInHz, 0);
    }

    #[test]
    fn ladder_windowed_escalation_skips_rollbacks() {
        // Windowed: no display mode, so no rate rollback; a refused 16-bit request still escalates to 32-bit.
        let windowed = D3DPRESENT_PARAMETERS {
            BackBufferFormat: D3DFMT_R5G6B5,
            Windowed: true.into(),
            ..Default::default()
        };
        let snap = AdapterSnapshot::empty();
        let run = run_ladder(
            windowed,
            &ctx(snap, policy(false), true, RefreshRateMode::Native, true),
            &[D3DERR_INVALIDCALL, D3DERR_INVALIDCALL],
            None,
        );
        assert_eq!(run.calls.len(), 2);
        assert_eq!(
            (run.calls[0].0, run.calls[0].1.BackBufferFormat),
            (0, D3DFMT_R5G6B5)
        );
        assert_eq!(
            (run.calls[1].0, run.calls[1].1.BackBufferFormat),
            (2, D3DFMT_X8R8G8B8)
        );
        assert!(run.calls[1].2.is_none());
        assert_eq!(run.final_pp.BackBufferFormat, D3DFMT_R5G6B5);
    }

    #[test]
    fn ladder_null_pp_single_bare_call() {
        let snap = AdapterSnapshot::empty();
        let c = ctx(snap, policy(false), true, RefreshRateMode::Native, true);
        let mut seen = Vec::new();
        let (hr, attempt) = unsafe {
            run_fix_ladder(null_mut(), &c, "test_failsafe", |mode_ptr, slot| {
                seen.push((slot, mode_ptr.is_null()));
                D3DERR_INVALIDCALL
            })
        };
        assert_eq!(hr, D3DERR_INVALIDCALL);
        assert!(attempt.is_none());
        assert_eq!(seen, vec![(0, true)]);
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
    fn select_refresh_rate_without_advertised_modes() {
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
    fn select_refresh_rate_picks_from_advertised_modes() {
        // (mode, advertised rates, desktop rate, expected pick)
        let cases: &[(RefreshRateMode, &[u32], u32, u32)] = &[
            (RefreshRateMode::Native, &[60, 120, 144], 144, 144),
            (RefreshRateMode::Native, &[60, 100], 144, 100),
            (RefreshRateMode::Native, &[144, 60, 120], 120, 120),
            (RefreshRateMode::Native, &[60, 80], 70, 60),
            (RefreshRateMode::Native, &[100, 120], 60, 100),
            (RefreshRateMode::Native, &[120, 144], 60, 120),
            (RefreshRateMode::NativeMultiple, &[60, 120, 144], 144, 120),
            (RefreshRateMode::NativeMultiple, &[60, 120], 60, 60),
            (RefreshRateMode::NativeMultiple, &[60, 120], 120, 120),
            (RefreshRateMode::NativeMultiple, &[60], 144, 60),
            (RefreshRateMode::NativeMultiple, &[50, 75], 75, 75),
            (RefreshRateMode::NativeMultiple, &[50, 75], 60, 50),
            (RefreshRateMode::NativeMultiple, &[120, 144], 60, 120),
            (RefreshRateMode::NativeMultiple, &[119, 143], 144, 119),
            (RefreshRateMode::Native, &[119, 143], 144, 143),
            (RefreshRateMode::Fixed(nz(120)), &[60, 120], 60, 120),
            (RefreshRateMode::Fixed(nz(120)), &[119, 120], 120, 120),
            (RefreshRateMode::Fixed(nz(120)), &[119], 119, 119),
            (RefreshRateMode::Fixed(nz(240)), &[60, 120], 120, 240),
        ];

        for &(mode, supported, desktop, expected) in cases {
            assert_eq!(
                select_refresh_rate(mode, supported, desktop),
                expected,
                "{mode:?} supported={supported:?} desktop={desktop}",
            );
        }
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
        assert!(!is_transient_device_error(D3DERR_INVALIDCALL));
        assert!(!is_transient_device_error(D3D_OK));
    }
}
