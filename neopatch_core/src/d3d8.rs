//! D3D8 to D3D9Ex translation.
//!
//! Rather than calling into `d3d8.dll`, we implement the subset of the D3D8 COM surface the games use.
//! `Direct3DCreate8` is intercepted and returns an `IDirect3D8` whose methods translate to an `IDirect3D9Ex`.
//!
//! Wrapper refcounts are plain `Cell`s, so every game that uses this code must call D3D from one thread only.

use crate::d3d9::{
    D3D_OK, D3DERR_INVALIDCALL, D3DERR_NOTAVAILABLE, PresentPolicy, create_hooked_d3d9_with,
    format_name, is_transient_device_error,
};
use crate::log::log_at;
use crate::patches::PatchSite;
use crate::{fmt_hr, iat_hook};
use std::cell::Cell;
use std::ffi::c_void;
use std::mem::offset_of;
use std::ptr::{copy_nonoverlapping, null_mut};
use std::sync::OnceLock;
use tracing::{debug, info, warn};
use windows::Win32::Foundation::{E_NOINTERFACE, HWND, POINT, RECT, S_FALSE};
use windows::Win32::Graphics::Direct3D9::{
    D3D_SDK_VERSION, D3DBACKBUFFER_TYPE, D3DBACKBUFFER_TYPE_MONO, D3DCAPS9, D3DCLIPSTATUS9,
    D3DDEVICE_CREATION_PARAMETERS, D3DDEVTYPE, D3DDISPLAYMODE, D3DFMT_A1R5G5B5, D3DFMT_A4L4,
    D3DFMT_A4R4G4B4, D3DFMT_A8, D3DFMT_A8L8, D3DFMT_A8P8, D3DFMT_A8R8G8B8, D3DFMT_D15S1,
    D3DFMT_D16, D3DFMT_D16_LOCKABLE, D3DFMT_D24S8, D3DFMT_D24X8, D3DFMT_D32, D3DFMT_L8, D3DFMT_P8,
    D3DFMT_R5G6B5, D3DFMT_R8G8B8, D3DFMT_X1R5G5B5, D3DFMT_X4R4G4B4, D3DFMT_X8R8G8B8, D3DFORMAT,
    D3DGAMMARAMP, D3DLIGHT9, D3DLOCK_READONLY, D3DLOCKED_RECT, D3DMATERIAL9, D3DMULTISAMPLE_NONE,
    D3DMULTISAMPLE_TYPE, D3DPOOL, D3DPOOL_DEFAULT, D3DPOOL_SYSTEMMEM, D3DPRESENT_PARAMETERS,
    D3DPRIMITIVETYPE, D3DRASTER_STATUS, D3DRECT, D3DRENDERSTATETYPE, D3DRESOURCETYPE,
    D3DSAMP_ADDRESSU, D3DSAMP_ADDRESSV, D3DSAMP_ADDRESSW, D3DSAMP_BORDERCOLOR, D3DSAMP_MAGFILTER,
    D3DSAMP_MAXANISOTROPY, D3DSAMP_MAXMIPLEVEL, D3DSAMP_MINFILTER, D3DSAMP_MIPFILTER,
    D3DSAMP_MIPMAPLODBIAS, D3DSAMPLERSTATETYPE, D3DSTATEBLOCKTYPE, D3DSURFACE_DESC, D3DSWAPEFFECT,
    D3DSWAPEFFECT_COPY, D3DSWAPEFFECT_DISCARD, D3DSWAPEFFECT_FLIP, D3DTEXF_NONE,
    D3DTEXTURESTAGESTATETYPE, D3DTRANSFORMSTATETYPE, D3DVIEWPORT9, IDirect3D9Ex_Vtbl,
    IDirect3DDevice9Ex_Vtbl, IDirect3DResource9_Vtbl, IDirect3DSurface9_Vtbl,
    IDirect3DTexture9_Vtbl, IDirect3DVertexBuffer9_Vtbl,
};
use windows::Win32::Graphics::Gdi::{HMONITOR, RGNDATA};
use windows::core::{BOOL, GUID, HRESULT, IUnknown_Vtbl};
use windows_sys::Win32::Foundation::HMODULE;

/// D3D8's `D3DPRESENT_PARAMETERS`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
#[allow(non_snake_case)]
struct D3DPresentParameters8 {
    BackBufferWidth: u32,
    BackBufferHeight: u32,
    BackBufferFormat: D3DFORMAT,
    BackBufferCount: u32,
    MultiSampleType: D3DMULTISAMPLE_TYPE,
    SwapEffect: u32,
    hDeviceWindow: HWND,
    Windowed: BOOL,
    EnableAutoDepthStencil: BOOL,
    AutoDepthStencilFormat: D3DFORMAT,
    Flags: u32,
    FullScreen_RefreshRateInHz: u32,
    FullScreen_PresentationInterval: u32,
}

const _: () = assert!(size_of::<D3DPresentParameters8>() == 0x34);

// D3D8's `D3DVIEWPORT8` is identical to D3D9's `D3DVIEWPORT9`.
pub type D3DViewport8 = D3DVIEWPORT9;

const _: () = assert!(size_of::<D3DViewport8>() == 0x18);

/// D3D8's `D3DSURFACE_DESC`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
#[allow(non_snake_case)]
struct D3DSurfaceDesc8 {
    Format: D3DFORMAT,
    Type: u32,
    Usage: u32,
    Pool: D3DPOOL,
    Size: u32,
    MultiSampleType: D3DMULTISAMPLE_TYPE,
    Width: u32,
    Height: u32,
}

const _: () = assert!(size_of::<D3DSurfaceDesc8>() == 0x20);

/// D3D8's `D3DCAPS8`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
#[allow(non_snake_case)]
struct D3DCaps8 {
    DeviceType: D3DDEVTYPE,
    AdapterOrdinal: u32,
    Caps: u32,
    Caps2: u32,
    Caps3: u32,
    PresentationIntervals: u32,
    CursorCaps: u32,
    DevCaps: u32,
    PrimitiveMiscCaps: u32,
    RasterCaps: u32,
    ZCmpCaps: u32,
    SrcBlendCaps: u32,
    DestBlendCaps: u32,
    AlphaCmpCaps: u32,
    ShadeCaps: u32,
    TextureCaps: u32,
    TextureFilterCaps: u32,
    CubeTextureFilterCaps: u32,
    VolumeTextureFilterCaps: u32,
    TextureAddressCaps: u32,
    VolumeTextureAddressCaps: u32,
    LineCaps: u32,
    MaxTextureWidth: u32,
    MaxTextureHeight: u32,
    MaxVolumeExtent: u32,
    MaxTextureRepeat: u32,
    MaxTextureAspectRatio: u32,
    MaxAnisotropy: u32,
    MaxVertexW: f32,
    GuardBandLeft: f32,
    GuardBandTop: f32,
    GuardBandRight: f32,
    GuardBandBottom: f32,
    ExtentsAdjust: f32,
    StencilCaps: u32,
    FVFCaps: u32,
    TextureOpCaps: u32,
    MaxTextureBlendStages: u32,
    MaxSimultaneousTextures: u32,
    VertexProcessingCaps: u32,
    MaxActiveLights: u32,
    MaxUserClipPlanes: u32,
    MaxVertexBlendMatrices: u32,
    MaxVertexBlendMatrixIndex: u32,
    MaxPointSize: f32,
    MaxPrimitiveCount: u32,
    MaxVertexIndex: u32,
    MaxStreams: u32,
    MaxStreamStride: u32,
    VertexShaderVersion: u32,
    MaxVertexShaderConst: u32,
    PixelShaderVersion: u32,
    MaxPixelShaderValue: f32,
}

const _: () = assert!(size_of::<D3DCaps8>() == 51 * 4 + 2 * 4);

fn caps_9_to_8(c: &D3DCAPS9) -> D3DCaps8 {
    macro_rules! caps_prefix {
        ($c:expr; $($f:ident),* $(,)?) => {
            D3DCaps8 {
                $($f: $c.$f,)*
                // D3D9 renamed D3D8's `MaxPixelShaderValue` to `PixelShader1xMaxValue`.
                MaxPixelShaderValue: $c.PixelShader1xMaxValue,
            }
        };
    }

    caps_prefix!(c;
        DeviceType, AdapterOrdinal, Caps, Caps2, Caps3, PresentationIntervals, CursorCaps, DevCaps, PrimitiveMiscCaps, RasterCaps,
        ZCmpCaps, SrcBlendCaps, DestBlendCaps, AlphaCmpCaps, ShadeCaps, TextureCaps, TextureFilterCaps, CubeTextureFilterCaps,
        VolumeTextureFilterCaps, TextureAddressCaps, VolumeTextureAddressCaps, LineCaps, MaxTextureWidth, MaxTextureHeight,
        MaxVolumeExtent, MaxTextureRepeat, MaxTextureAspectRatio, MaxAnisotropy, MaxVertexW, GuardBandLeft, GuardBandTop, GuardBandRight,
        GuardBandBottom, ExtentsAdjust, StencilCaps, FVFCaps, TextureOpCaps, MaxTextureBlendStages, MaxSimultaneousTextures,
        VertexProcessingCaps, MaxActiveLights, MaxUserClipPlanes, MaxVertexBlendMatrices, MaxVertexBlendMatrixIndex, MaxPointSize,
        MaxPrimitiveCount, MaxVertexIndex, MaxStreams, MaxStreamStride, VertexShaderVersion, MaxVertexShaderConst, PixelShaderVersion,
    )
}

/// Returns the number of bytes per pixel, or `None` when a rect's row isn't exactly `width * bytes_pp` contiguous bytes.
fn bytes_per_pixel(f: D3DFORMAT) -> Option<u32> {
    match f {
        D3DFMT_A8R8G8B8 | D3DFMT_X8R8G8B8 | D3DFMT_D24S8 | D3DFMT_D24X8 | D3DFMT_D32 => Some(4),
        D3DFMT_R8G8B8 => Some(3),
        D3DFMT_R5G6B5 | D3DFMT_X1R5G5B5 | D3DFMT_A1R5G5B5 | D3DFMT_A4R4G4B4 | D3DFMT_X4R4G4B4
        | D3DFMT_A8L8 | D3DFMT_A8P8 | D3DFMT_D16 | D3DFMT_D16_LOCKABLE | D3DFMT_D15S1 => Some(2),
        D3DFMT_A8 | D3DFMT_P8 | D3DFMT_L8 | D3DFMT_A4L4 => Some(1),
        _ => None,
    }
}

fn surface_desc_9_to_8(d: &D3DSURFACE_DESC) -> D3DSurfaceDesc8 {
    // No D3D8 format exceeds 4 bytes per pixel, so guessing high can't make a caller underallocate.
    let bytes_pp = bytes_per_pixel(d.Format).unwrap_or_else(|| {
        warn!(
            kind = "surface_size_guessed",
            format = format_name(d.Format),
            raw = d.Format.0,
        );
        4
    });
    // `Size` is a D3D8-only field reconstructed from the pixel size.
    let size = d.Width.saturating_mul(d.Height).saturating_mul(bytes_pp);

    D3DSurfaceDesc8 {
        Format: d.Format,
        Type: d.Type.0.cast_unsigned(),
        Usage: d.Usage,
        Pool: d.Pool,
        Size: size,
        MultiSampleType: d.MultiSampleType,
        Width: d.Width,
        Height: d.Height,
    }
}

/// Maps a D3D8 texture stage state type to its corresponding D3D9 sampler state if applicable, or `None` if it stayed a stage state.
fn tss_to_sampler_state(ty: u32) -> Option<D3DSAMPLERSTATETYPE> {
    // D3D8's `D3DTEXTURESTAGESTATETYPE` values.
    const D3DTSS8_ADDRESSU: u32 = 13;
    const D3DTSS8_ADDRESSV: u32 = 14;
    const D3DTSS8_BORDERCOLOR: u32 = 15;
    const D3DTSS8_MAGFILTER: u32 = 16;
    const D3DTSS8_MINFILTER: u32 = 17;
    const D3DTSS8_MIPFILTER: u32 = 18;
    const D3DTSS8_MIPMAPLODBIAS: u32 = 19;
    const D3DTSS8_MAXMIPLEVEL: u32 = 20;
    const D3DTSS8_MAXANISOTROPY: u32 = 21;
    const D3DTSS8_ADDRESSW: u32 = 25;

    match ty {
        D3DTSS8_ADDRESSU => Some(D3DSAMP_ADDRESSU),
        D3DTSS8_ADDRESSV => Some(D3DSAMP_ADDRESSV),
        D3DTSS8_ADDRESSW => Some(D3DSAMP_ADDRESSW),
        D3DTSS8_BORDERCOLOR => Some(D3DSAMP_BORDERCOLOR),
        D3DTSS8_MAGFILTER => Some(D3DSAMP_MAGFILTER),
        D3DTSS8_MINFILTER => Some(D3DSAMP_MINFILTER),
        D3DTSS8_MIPFILTER => Some(D3DSAMP_MIPFILTER),
        D3DTSS8_MIPMAPLODBIAS => Some(D3DSAMP_MIPMAPLODBIAS),
        D3DTSS8_MAXMIPLEVEL => Some(D3DSAMP_MAXMIPLEVEL),
        D3DTSS8_MAXANISOTROPY => Some(D3DSAMP_MAXANISOTROPY),
        _ => None,
    }
}

/// Translates D3D8 present parameters to D3D9.
fn convert_present_params(pp8: &D3DPresentParameters8) -> D3DPRESENT_PARAMETERS {
    const D3DSWAPEFFECT_COPY_VSYNC8: u32 = 4;

    let swap_effect = match pp8.SwapEffect {
        // This mapping drops the "sync to vblank" semantics, but it's fine because we also override `PresentationInterval` itself.
        D3DSWAPEFFECT_COPY_VSYNC8 => D3DSWAPEFFECT_COPY,
        // D3D9 rejects every swap effect but `DISCARD` once multisampling is on.
        // In th06, `GameWindow::Present` captures the back buffer after `Present` for pause/retry menu backgrounds, so we can't use `DISCARD`.
        // NOTE/TODO: If any games use `FLIP` and multisampling, we should probably keep `FLIP` and disable multisampling.
        e if e == D3DSWAPEFFECT_FLIP.0.cast_unsigned() => {
            if pp8.MultiSampleType == D3DMULTISAMPLE_NONE {
                D3DSWAPEFFECT_FLIP
            } else {
                D3DSWAPEFFECT_DISCARD
            }
        }
        other => D3DSWAPEFFECT(other.cast_signed()),
    };

    D3DPRESENT_PARAMETERS {
        BackBufferWidth: pp8.BackBufferWidth,
        BackBufferHeight: pp8.BackBufferHeight,
        // Back buffer substitution happens in `d3d9::rewrite_present_params_impl`.
        BackBufferFormat: pp8.BackBufferFormat,
        BackBufferCount: pp8.BackBufferCount,
        MultiSampleType: pp8.MultiSampleType,
        MultiSampleQuality: 0,
        SwapEffect: swap_effect,
        hDeviceWindow: pp8.hDeviceWindow,
        Windowed: pp8.Windowed,
        EnableAutoDepthStencil: pp8.EnableAutoDepthStencil,
        AutoDepthStencilFormat: pp8.AutoDepthStencilFormat,
        Flags: pp8.Flags,
        FullScreen_RefreshRateInHz: pp8.FullScreen_RefreshRateInHz,
        PresentationInterval: pp8.FullScreen_PresentationInterval,
    }
}

/// Copies the parameters the device was created or reset with back into the caller's D3D8 struct.
/// This should only be called after a successful call, since the games may mutate their own copy between retries.
/// (For example, th06's `InitD3dRendering` drops the refresh rate and lockable back buffer flag.)
///
/// The D3D9 call took `pp9` as in/out, so it no longer holds what [`convert_present_params`] built: `d3d9::prep_present_params`
/// rewrote it in place and the runtime filled in whatever the game left it to choose. What comes back is what the game must see
/// and cannot get elsewhere — the runtime's `BackBufferWidth` and `BackBufferHeight`, plus `BackBufferFormat`, which is ours but
/// has a real reader: th06 probes `CheckDeviceFormat` with it right after creation, where a stale value asks about the wrong
/// display mode. Only the struct the caller passed is updated, which need not be the copy the game keeps; see [`SHIM_POLICY`].
///
/// Everything else keeps the game's own value; for most fields nothing on either side changed it. `SwapEffect`,
/// `FullScreen_PresentationInterval` and `FullScreen_RefreshRateInHz` are withheld on purpose, since `pp9` carries our
/// translation, the pacer's forced `IMMEDIATE`, and `d3d9::apply_refresh_override`'s pick. The games resubmit this struct on
/// their next `Reset`, so handing those back would make our overrides look like the game's own request. For the refresh rate
/// that has teeth: `d3d9::run_with_refresh_failsafe` reads the resubmitted rate as the value to roll back to, so a laundered
/// pick would displace the game's original as the failsafe's recovery target.
fn sync_present_params_back(pp8: &mut D3DPresentParameters8, pp9: &D3DPRESENT_PARAMETERS) {
    let D3DPRESENT_PARAMETERS {
        BackBufferWidth,
        BackBufferHeight,
        BackBufferFormat,
        ..
    } = *pp9;

    pp8.BackBufferWidth = BackBufferWidth;
    pp8.BackBufferHeight = BackBufferHeight;
    pp8.BackBufferFormat = BackBufferFormat;
}

/// Common prefix of every wrapper object.
#[repr(C)]
struct ComHeader {
    vtbl: *const c_void,
    refs: Cell<u32>,
    /// The wrapped D3D9 interface pointer.
    inner: Cell<*mut c_void>,
}

/// Reads the wrapped D3D9 pointer out of a game-supplied wrapper pointer. Null passes through.
unsafe fn unwrap8(p: *mut c_void) -> *mut c_void {
    if p.is_null() {
        null_mut()
    } else {
        unsafe { (*p.cast::<ComHeader>()).inner.get() }
    }
}

unsafe fn com_add_ref(p: *mut c_void) -> u32 {
    unsafe {
        let vtbl: *const IUnknown_Vtbl = *p.cast();
        ((*vtbl).AddRef)(p)
    }
}

unsafe fn com_release(p: *mut c_void) -> u32 {
    unsafe {
        let vtbl: *const IUnknown_Vtbl = *p.cast();
        ((*vtbl).Release)(p)
    }
}

macro_rules! stub8 {
    // Explicit return type (e.g. `-> u32 = 0`).
    ($fn_name:ident, $method:literal ( $($arg:ident : $ty:ty),* ) -> $ret_ty:ty = $ret:expr) => {
        unsafe extern "system" fn $fn_name(_this: *mut c_void $(, $arg : $ty)*) -> $ret_ty {
            $(let _ = $arg;)*
            warn!(kind = "d3d8_stub", method = $method);
            $ret
        }
    };
    // `HRESULT`-returning shorthand for the case above.
    ($fn_name:ident, $method:literal ( $($arg:ident : $ty:ty),* ) -> $ret:expr) => {
        stub8!($fn_name, $method ( $($arg : $ty),* ) -> HRESULT = $ret);
    };
    // No return value.
    ($fn_name:ident, $method:literal ( $($arg:ident : $ty:ty),* )) => {
        unsafe extern "system" fn $fn_name(_this: *mut c_void $(, $arg : $ty)*) {
            $(let _ = $arg;)*
            warn!(kind = "d3d8_stub", method = $method);
        }
    };
    // `HRESULT`-returning stub with an interface out-slot.
    // `nulls` sets the `*mut *mut c_void` argument to NULL before refusing, matching what real D3D8 hands back on failure.
    ($fn_name:ident, $method:literal ( $($arg:ident : $ty:ty),* ) nulls $out:ident -> $ret:expr) => {
        unsafe extern "system" fn $fn_name(_this: *mut c_void $(, $arg : $ty)*) -> HRESULT {
            $(let _ = $arg;)*
            warn!(kind = "d3d8_stub", method = $method);
            if !$out.is_null() {
                unsafe { $out.write(null_mut()) };
            }
            $ret
        }
    };
}

/// A D3D8 method implemented by calling the wrapped D3D9 object.
/// `$acc` unwraps the wrapper (`dev9` for the device, `unwrap8` for resources) and `$vt` projects its vtable.
macro_rules! forward8 {
    ($(#[$attr:meta])* $fn_name:ident, $acc:ident / $vt:ident . $($slot:ident).+ ( $($arg:ident : $ty:ty),* $(,)? ) -> $ret:ty) => {
        $(#[$attr])*
        unsafe extern "system" fn $fn_name(this: *mut c_void $(, $arg : $ty)*) -> $ret {
            unsafe {
                let p = $acc(this);
                ($vt(p).$($slot).+)(p $(, $arg)*)
            }
        }
    };
    ($(#[$attr:meta])* $fn_name:ident, $acc:ident / $vt:ident . $($slot:ident).+ ( $($arg:ident : $ty:ty),* $(,)? ) -> $ret:ty => ( $($fwd:expr),* $(,)? )) => {
        $(#[$attr])*
        unsafe extern "system" fn $fn_name(this: *mut c_void $(, $arg : $ty)*) -> $ret {
            unsafe {
                let p = $acc(this);
                ($vt(p).$($slot).+)(p $(, $fwd)*)
            }
        }
    };
}

/// Shared `QueryInterface` for every wrapper kind.
unsafe extern "system" fn wrap_query_interface(
    _this: *mut c_void,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let riid_dbg = unsafe { riid.as_ref() };
    warn!(kind = "d3d8_stub", method = "QueryInterface", riid = ?riid_dbg);
    if out.is_null() {
        return D3DERR_INVALIDCALL;
    }
    unsafe { *out = null_mut() };
    E_NOINTERFACE
}

unsafe extern "system" fn wrap_add_ref(this: *mut c_void) -> u32 {
    let h = unsafe { &*this.cast::<ComHeader>() };
    // A dead wrapper's inner reference is gone. Resurrecting it would AddRef a possibly destroyed D3D9 object.
    if h.refs.get() == 0 {
        warn!(kind = "wrapper_add_ref_after_death");
        return 0;
    }
    unsafe { com_add_ref(h.inner.get()) };
    let n = h.refs.get() + 1;
    h.refs.set(n);
    n
}

unsafe fn com_header_release(this: *mut c_void, wrapper: &'static str) -> Option<u32> {
    let h = unsafe { &*this.cast::<ComHeader>() };
    if h.refs.get() == 0 {
        warn!(kind = "wrapper_over_release", wrapper);
        return None;
    }
    unsafe { com_release(h.inner.get()) };
    let n = h.refs.get() - 1;
    h.refs.set(n);
    Some(n)
}

macro_rules! vtable_accessors {
    ($($fn_name:ident => $vtbl_ty:ty),* $(,)?) => {
        $(
            /// # Safety
            /// `p` must be a live COM object whose vtable has the layout of the return type.
            unsafe fn $fn_name<'a>(p: *mut c_void) -> &'a $vtbl_ty {
                unsafe { &**p.cast::<*const $vtbl_ty>() }
            }
        )*
    };
}

vtable_accessors! {
    d3d9_vt => IDirect3D9Ex_Vtbl,
    dev9_vt => IDirect3DDevice9Ex_Vtbl,
    surf9_vt => IDirect3DSurface9_Vtbl,
    tex9_vt => IDirect3DTexture9_Vtbl,
    vb9_vt => IDirect3DVertexBuffer9_Vtbl,
    res9_vt => IDirect3DResource9_Vtbl,
    dev8_vt => Device8Vtbl,
}

/// Wrapper for every D3D8 resource interface.
#[repr(C)]
struct Resource8 {
    header: ComHeader,
    /// Backpointer for `GetDevice`.
    device: *mut Device8,
    /// Emulates the internal-to-`d3d8.dll` flags dword at surface offset `+0x10`, which th08's D3DX8 reads directly whenever `GetContainer`
    /// says a surface is not texture-owned (see [`surface8_get_container`]). Without the field, that read would be out of bounds.
    /// Only bit 26 ("directly lockable") is modeled, and the value is always 0 for texture and vertex-buffer wrappers.
    internal_flags: u32,
}

const _: () = assert!(offset_of!(Resource8, internal_flags) == 0x10);

/// Indicates whether the surface is directly lockable.
const D3D8_INTERNAL_LOCKABLE: u32 = 1 << 26;

impl Resource8 {
    unsafe fn new_raw(
        vtbl: *const c_void,
        inner: *mut c_void,
        device: *mut Device8,
        lockable: bool,
    ) -> *mut c_void {
        if !device.is_null() {
            unsafe { wrap_add_ref(device.cast()) };
        }

        Box::into_raw(Box::new(Self {
            header: ComHeader {
                vtbl,
                refs: Cell::new(1),
                inner: Cell::new(inner),
            },
            device,
            internal_flags: if lockable { D3D8_INTERNAL_LOCKABLE } else { 0 },
        }))
        .cast()
    }

    /// Takes over the caller's freshly acquired D3D9 reference (which carries ownership of the refcount increment that
    /// D3D9's `GetBackBuffer` performed on the surface) and mints the game's reference on the wrapper itself, reviving the wrapper if dead.
    ///
    /// This method and [`resource8_release`] form two halves of a ledger: this files one D3D9 reference in and mints
    /// one game-facing reference out; `resource8_release` burns one of each. The device lifetime reference at [`Resource8::device`]
    /// bounds the entire live period. The full cycle balances like double-entry bookkeeping:
    ///
    /// | Event                                        | Wrapper `refs` | Inner refs held      | Device ref    |
    /// |:---------------------------------------------|:---------------|:---------------------|:--------------|
    /// | `GetBackBuffer` -> `Resource8::adopt` (dead) | 0 -> 1         | +1 (absorbed)        | +1 (taken)    |
    /// | `GetBackBuffer` -> `Resource8::adopt` (live) | n -> n + 1     | +1 (absorbed)        | no change     |
    /// | game `Release`                               | n -> n - 1     | -1 ([`com_release`]) | no change     |
    /// | game `Release` reaching 0                    | 1 -> 0         | -1                   | -1 (returned) |
    ///
    /// The two entries are similar to different `Rc` operations: `Rc::from_raw` for the surface, whose reference already exists
    /// and only changes owner, and `Rc::increment_strong_count` for the wrapper, which returns nothing yet leaves a reference
    /// the game must release. However, unlike an `Rc`, `refs` is owed against whatever `header.inner` holds at release time,
    /// not against a fixed object.
    unsafe fn adopt(&self, inner: *mut c_void) {
        // NOTE/TODO: We assume the result of `GetBackBuffer(0, MONO)` is stable. Adopting a different surface onto a live wrapper
        // would silently ref all of them to the new one, over-releasing it while orphaning the old one.
        // If a game requests more than one back buffer or holds a surface across a `Present`, then we need to revisit this.
        if self.header.refs.get() == 0 && !self.device.is_null() {
            unsafe { wrap_add_ref(self.device.cast()) };
        }
        // A dead wrapper's stored surface may have been destroyed by a `Reset` and must not be reused, so we always overwrite.
        self.header.inner.set(inner);
        self.header.refs.update(|n| n + 1);
    }
}

/// Wraps a newly-created, already-referenced D3D9 resource, or forwards failure.
unsafe fn wrap_created(
    call: &'static str,
    hr: HRESULT,
    inner: *mut c_void,
    vtbl: *const c_void,
    device: *mut Device8,
    lockable: bool,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        // The D3D9 object already exists; without this a null out-pointer leaks one reference per call.
        if hr.is_ok() && !inner.is_null() {
            unsafe { com_release(inner) };
        }
        return D3DERR_INVALIDCALL;
    }
    if hr.is_err() {
        unsafe { *out = null_mut() };
        return hr;
    }
    if inner.is_null() {
        warn!(kind = "d3d9_null_on_success", call);
        unsafe { *out = null_mut() };
        return D3DERR_INVALIDCALL;
    }
    unsafe { *out = Resource8::new_raw(vtbl, inner, device, lockable) };
    hr
}

unsafe extern "system" fn resource8_release(this: *mut c_void) -> u32 {
    let Some(n) = (unsafe { com_header_release(this, "resource8") }) else {
        return 0;
    };
    if n == 0 {
        // The dead wrapper stays behind to catch over-release. Freeing here can't be made unconditional anyway,
        // since this is the `release` slot for the device's embedded `back_buffer`, which is a field of the `Device8` allocation.
        // Calling `Box::from_raw` would free the middle of a live device, so we retain and leak the boxes.
        let h = unsafe { &*this.cast::<Resource8>() };
        if !h.device.is_null() {
            unsafe { device8_release(h.device.cast()) };
        }
    }
    n
}

unsafe extern "system" fn resource8_get_device(
    this: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    let h = unsafe { &*this.cast::<Resource8>() };
    if out.is_null() || h.device.is_null() {
        return D3DERR_INVALIDCALL;
    }
    unsafe {
        wrap_add_ref(h.device.cast());
        *out = h.device.cast();
    }
    D3D_OK
}

stub8!(resource8_set_private_data, "IDirect3DResource8::SetPrivateData"(_refguid: *const GUID, _data: *const c_void, _size: u32, _flags: u32) -> D3D_OK);
stub8!(resource8_get_private_data, "IDirect3DResource8::GetPrivateData"(_refguid: *const GUID, _data: *mut c_void, _size: *mut u32) -> D3DERR_NOTAVAILABLE);
stub8!(resource8_free_private_data, "IDirect3DResource8::FreePrivateData"(_refguid: *const GUID) -> D3D_OK);
forward8!(resource8_set_priority, unwrap8 / res9_vt.SetPriority(priority: u32) -> u32);
stub8!(resource8_get_priority, "IDirect3DResource8::GetPriority"() -> u32 = 0);

unsafe extern "system" fn resource8_pre_load(this: *mut c_void) {
    unsafe {
        let inner = unwrap8(this);
        (res9_vt(inner).PreLoad)(inner);
    }
}

// 0 is deliberately not a valid `D3DRESOURCETYPE`.
stub8!(resource8_get_type, "IDirect3DResource8::GetType"() -> u32 = 0);

#[repr(C)]
#[rustfmt::skip]
struct Surface8Vtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_device: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    set_private_data: unsafe extern "system" fn(*mut c_void, *const GUID, *const c_void, u32, u32) -> HRESULT,
    get_private_data: unsafe extern "system" fn(*mut c_void, *const GUID, *mut c_void, *mut u32) -> HRESULT,
    free_private_data: unsafe extern "system" fn(*mut c_void, *const GUID) -> HRESULT,
    get_container: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    get_desc: unsafe extern "system" fn(*mut c_void, *mut D3DSurfaceDesc8) -> HRESULT,
    lock_rect: unsafe extern "system" fn(*mut c_void, *mut D3DLOCKED_RECT, *const RECT, u32) -> HRESULT,
    unlock_rect: unsafe extern "system" fn(*mut c_void) -> HRESULT,
}

const _: () = assert!(size_of::<Surface8Vtbl>() == 11 * 4);

static SURFACE8_VTBL: Surface8Vtbl = Surface8Vtbl {
    query_interface: wrap_query_interface,
    add_ref: wrap_add_ref,
    release: resource8_release,
    get_device: resource8_get_device,
    set_private_data: resource8_set_private_data,
    get_private_data: resource8_get_private_data,
    free_private_data: resource8_free_private_data,
    get_container: surface8_get_container,
    get_desc: surface8_get_desc,
    lock_rect: surface8_lock_rect,
    unlock_rect: surface8_unlock_rect,
};

// The container IIDs that th08's D3DX8 probes with.
// th08 has two `GetContainer` call sites referencing these in `.rdata` at `0x4bc024` and `0x4bc014`, respectively.
const IID_IDIRECT3D_BASE_TEXTURE8: GUID = GUID::from_values(
    0xb421_1cfa,
    0x51b9,
    0x4a9f,
    [0xab, 0x78, 0xdb, 0x99, 0xb2, 0xbb, 0x67, 0x8e],
);
const IID_IDIRECT3D_TEXTURE8: GUID = GUID::from_values(
    0xe4cd_d575,
    0x2866,
    0x4f01,
    [0xb1, 0x2e, 0x7e, 0xec, 0xe1, 0xec, 0x93, 0x58],
);

// th08's D3DX8 calls this in its surface-lock helper to pick a lock strategy for DEFAULT-pool non-DYNAMIC surfaces.
// Success means a texture level, which isn't directly lockable, and routes the lock through a temp surface and `CopyRects`;
// failure sends it to bit 26 of the flags dword at `+0x10` instead (see `Resource8::internal_flags`).
unsafe extern "system" fn surface8_get_container(
    _this: *mut c_void,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if !out.is_null() {
        unsafe { *out = null_mut() };
    }
    let riid = unsafe { riid.as_ref() };
    if riid == Some(&IID_IDIRECT3D_BASE_TEXTURE8) || riid == Some(&IID_IDIRECT3D_TEXTURE8) {
        debug!(kind = "d3d8_get_container_not_texture");
    } else {
        warn!(
            kind = "d3d8_stub",
            method = "IDirect3DSurface8::GetContainer",
            riid = ?riid,
        );
    }
    // We assume every surface the games lock through the helper is the back buffer or a render target, so refusing outright is correct.
    E_NOINTERFACE
}

unsafe extern "system" fn surface8_get_desc(
    this: *mut c_void,
    out: *mut D3DSurfaceDesc8,
) -> HRESULT {
    if out.is_null() {
        return D3DERR_INVALIDCALL;
    }
    match unsafe { surface_desc9(unwrap8(this)) } {
        Ok(d9) => {
            unsafe { *out = surface_desc_9_to_8(&d9) };
            D3D_OK
        }
        Err(hr) => hr,
    }
}

forward8!(surface8_lock_rect, unwrap8 / surf9_vt.LockRect(locked: *mut D3DLOCKED_RECT, rect: *const RECT, flags: u32) -> HRESULT);
forward8!(surface8_unlock_rect, unwrap8 / surf9_vt.UnlockRect() -> HRESULT);

#[repr(C)]
#[rustfmt::skip]
struct Texture8Vtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_device: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    set_private_data: unsafe extern "system" fn(*mut c_void, *const GUID, *const c_void, u32, u32) -> HRESULT,
    get_private_data: unsafe extern "system" fn(*mut c_void, *const GUID, *mut c_void, *mut u32) -> HRESULT,
    free_private_data: unsafe extern "system" fn(*mut c_void, *const GUID) -> HRESULT,
    set_priority: unsafe extern "system" fn(*mut c_void, u32) -> u32,
    get_priority: unsafe extern "system" fn(*mut c_void) -> u32,
    pre_load: unsafe extern "system" fn(*mut c_void),
    get_type: unsafe extern "system" fn(*mut c_void) -> u32,
    set_lod: unsafe extern "system" fn(*mut c_void, u32) -> u32,
    get_lod: unsafe extern "system" fn(*mut c_void) -> u32,
    get_level_count: unsafe extern "system" fn(*mut c_void) -> u32,
    get_level_desc: unsafe extern "system" fn(*mut c_void, u32, *mut D3DSurfaceDesc8) -> HRESULT,
    get_surface_level: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT,
    lock_rect: unsafe extern "system" fn(*mut c_void, u32, *mut D3DLOCKED_RECT, *const RECT, u32) -> HRESULT,
    unlock_rect: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    add_dirty_rect: unsafe extern "system" fn(*mut c_void, *const RECT) -> HRESULT,
}

const _: () = assert!(size_of::<Texture8Vtbl>() == 19 * 4);

static TEXTURE8_VTBL: Texture8Vtbl = Texture8Vtbl {
    query_interface: wrap_query_interface,
    add_ref: wrap_add_ref,
    release: resource8_release,
    get_device: resource8_get_device,
    set_private_data: resource8_set_private_data,
    get_private_data: resource8_get_private_data,
    free_private_data: resource8_free_private_data,
    set_priority: resource8_set_priority,
    get_priority: resource8_get_priority,
    pre_load: resource8_pre_load,
    get_type: resource8_get_type,
    set_lod: texture8_set_lod,
    get_lod: texture8_get_lod,
    get_level_count: texture8_get_level_count,
    get_level_desc: texture8_get_level_desc,
    get_surface_level: texture8_get_surface_level,
    lock_rect: texture8_lock_rect,
    unlock_rect: texture8_unlock_rect,
    add_dirty_rect: texture8_add_dirty_rect,
};

stub8!(texture8_set_lod, "IDirect3DTexture8::SetLOD"(_lod: u32) -> u32 = 0);
stub8!(texture8_get_lod, "IDirect3DTexture8::GetLOD"() -> u32 = 0);
forward8!(texture8_get_level_count, unwrap8 / tex9_vt.base__.GetLevelCount() -> u32);

unsafe extern "system" fn texture8_get_level_desc(
    this: *mut c_void,
    level: u32,
    out: *mut D3DSurfaceDesc8,
) -> HRESULT {
    if out.is_null() {
        return D3DERR_INVALIDCALL;
    }
    let inner = unsafe { unwrap8(this) };
    let mut d9 = D3DSURFACE_DESC::default();
    let hr = unsafe { (tex9_vt(inner).GetLevelDesc)(inner, level, &raw mut d9) };
    if hr.is_ok() {
        unsafe { *out = surface_desc_9_to_8(&d9) };
    }
    hr
}

unsafe extern "system" fn texture8_get_surface_level(
    this: *mut c_void,
    level: u32,
    out: *mut *mut c_void,
) -> HRESULT {
    let h = unsafe { &*this.cast::<Resource8>() };
    let mut s9 = null_mut();
    let inner = h.header.inner.get();
    let hr = unsafe { (tex9_vt(inner).GetSurfaceLevel)(inner, level, &raw mut s9) };
    // Texture levels are not directly lockable surfaces in the internal-flags sense.
    unsafe {
        wrap_created(
            "IDirect3DTexture9::GetSurfaceLevel",
            hr,
            s9,
            (&raw const SURFACE8_VTBL).cast(),
            h.device,
            false,
            out,
        )
    }
}

forward8!(texture8_lock_rect, unwrap8 / tex9_vt.LockRect(level: u32, locked: *mut D3DLOCKED_RECT, rect: *const RECT, flags: u32) -> HRESULT);
forward8!(texture8_unlock_rect, unwrap8 / tex9_vt.UnlockRect(level: u32) -> HRESULT);
stub8!(texture8_add_dirty_rect, "IDirect3DTexture8::AddDirtyRect"(_rect: *const RECT) -> D3D_OK);

#[repr(C)]
#[rustfmt::skip]
struct Buffer8Vtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_device: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    set_private_data: unsafe extern "system" fn(*mut c_void, *const GUID, *const c_void, u32, u32) -> HRESULT,
    get_private_data: unsafe extern "system" fn(*mut c_void, *const GUID, *mut c_void, *mut u32) -> HRESULT,
    free_private_data: unsafe extern "system" fn(*mut c_void, *const GUID) -> HRESULT,
    set_priority: unsafe extern "system" fn(*mut c_void, u32) -> u32,
    get_priority: unsafe extern "system" fn(*mut c_void) -> u32,
    pre_load: unsafe extern "system" fn(*mut c_void),
    get_type: unsafe extern "system" fn(*mut c_void) -> u32,
    lock: unsafe extern "system" fn(*mut c_void, u32, u32, *mut *mut u8, u32) -> HRESULT,
    unlock: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    get_desc: unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
}

const _: () = assert!(size_of::<Buffer8Vtbl>() == 14 * 4);

static VERTEX_BUFFER8_VTBL: Buffer8Vtbl = Buffer8Vtbl {
    query_interface: wrap_query_interface,
    add_ref: wrap_add_ref,
    release: resource8_release,
    get_device: resource8_get_device,
    set_private_data: resource8_set_private_data,
    get_private_data: resource8_get_private_data,
    free_private_data: resource8_free_private_data,
    set_priority: resource8_set_priority,
    get_priority: resource8_get_priority,
    pre_load: resource8_pre_load,
    get_type: resource8_get_type,
    lock: vertex_buffer8_lock,
    unlock: vertex_buffer8_unlock,
    get_desc: vertex_buffer8_get_desc,
};

forward8!(vertex_buffer8_lock, unwrap8 / vb9_vt.Lock(offset: u32, size: u32, data: *mut *mut u8, flags: u32) -> HRESULT => (offset, size, data.cast(), flags));
forward8!(vertex_buffer8_unlock, unwrap8 / vb9_vt.Unlock() -> HRESULT);
stub8!(vertex_buffer8_get_desc, "IDirect3DVertexBuffer8::GetDesc"(_out: *mut c_void) -> D3DERR_NOTAVAILABLE);

/// # Safety
/// `dev8` must be a live `IDirect3DDevice8` created by this module.
pub unsafe fn call_begin_scene(dev8: *mut c_void) -> i32 {
    unsafe { (dev8_vt(dev8).begin_scene)(dev8).0 }
}

/// # Safety
/// `dev8` must be a live `IDirect3DDevice8` created by this module.
pub unsafe fn call_end_scene(dev8: *mut c_void) -> i32 {
    unsafe { (dev8_vt(dev8).end_scene)(dev8).0 }
}

/// # Safety
/// `dev8` must be a live `IDirect3DDevice8` created by this module. `rects` must be null or point to `count` `D3DRECT`s.
pub unsafe fn call_clear(
    dev8: *mut c_void,
    count: u32,
    rects: *const c_void,
    flags: u32,
    color: u32,
    z: f32,
    stencil: u32,
) -> i32 {
    unsafe { (dev8_vt(dev8).clear)(dev8, count, rects.cast(), flags, color, z, stencil).0 }
}

/// # Safety
/// `dev8` must be a live `IDirect3DDevice8` created by this module. `viewport` must point to a live [`D3DViewport8`].
pub unsafe fn call_set_viewport(dev8: *mut c_void, viewport: *const D3DViewport8) -> i32 {
    unsafe { (dev8_vt(dev8).set_viewport)(dev8, viewport).0 }
}

/// # Safety
/// `dev8` must be a live `IDirect3DDevice8` created by this module.
/// `texture8` must be null or an `IDirect3DBaseTexture8` created by this module.
pub unsafe fn call_set_texture(dev8: *mut c_void, stage: u32, texture8: *mut c_void) -> i32 {
    unsafe { (dev8_vt(dev8).set_texture)(dev8, stage, texture8).0 }
}

#[repr(C)]
struct Device8 {
    // `header.inner` is the wrapped `IDirect3DDevice9Ex`.
    header: ComHeader,
    /// The parent [`D3d8`], kept alive by a real reference for this device's lifetime.
    parent: *mut D3d8,
    /// Wrapper for the swap chain's back buffer.
    back_buffer: Resource8,
}

impl Device8 {
    fn inner(&self) -> *mut c_void {
        self.header.inner.get()
    }
}

unsafe fn device8<'a>(this: *mut c_void) -> &'a Device8 {
    unsafe { &*this.cast() }
}

/// Returns the wrapped `IDirect3DDevice9Ex` of a game-facing device pointer.
unsafe fn dev9(this: *mut c_void) -> *mut c_void {
    unsafe { device8(this).inner() }
}

#[repr(C)]
#[rustfmt::skip]
struct Device8Vtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    test_cooperative_level: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    get_available_texture_mem: unsafe extern "system" fn(*mut c_void) -> u32,
    resource_manager_discard_bytes: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    get_direct3d: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    get_device_caps: unsafe extern "system" fn(*mut c_void, *mut D3DCaps8) -> HRESULT,
    get_display_mode: unsafe extern "system" fn(*mut c_void, *mut D3DDISPLAYMODE) -> HRESULT,
    get_creation_parameters: unsafe extern "system" fn(*mut c_void, *mut D3DDEVICE_CREATION_PARAMETERS) -> HRESULT,
    set_cursor_properties: unsafe extern "system" fn(*mut c_void, u32, u32, *mut c_void) -> HRESULT,
    set_cursor_position: unsafe extern "system" fn(*mut c_void, u32, u32, u32),
    show_cursor: unsafe extern "system" fn(*mut c_void, BOOL) -> BOOL,
    create_additional_swap_chain: unsafe extern "system" fn(*mut c_void, *mut D3DPresentParameters8, *mut *mut c_void) -> HRESULT,
    reset: unsafe extern "system" fn(*mut c_void, *mut D3DPresentParameters8) -> HRESULT,
    present: unsafe extern "system" fn(*mut c_void, *const RECT, *const RECT, HWND, *const RGNDATA) -> HRESULT,
    get_back_buffer: unsafe extern "system" fn(*mut c_void, u32, D3DBACKBUFFER_TYPE, *mut *mut c_void) -> HRESULT,
    get_raster_status: unsafe extern "system" fn(*mut c_void, *mut D3DRASTER_STATUS) -> HRESULT,
    set_gamma_ramp: unsafe extern "system" fn(*mut c_void, u32, *const D3DGAMMARAMP),
    get_gamma_ramp: unsafe extern "system" fn(*mut c_void, *mut D3DGAMMARAMP),
    create_texture: unsafe extern "system" fn(*mut c_void, u32, u32, u32, u32, D3DFORMAT, D3DPOOL, *mut *mut c_void) -> HRESULT,
    create_volume_texture: unsafe extern "system" fn(*mut c_void, u32, u32, u32, u32, u32, D3DFORMAT, D3DPOOL, *mut *mut c_void) -> HRESULT,
    create_cube_texture: unsafe extern "system" fn(*mut c_void, u32, u32, u32, D3DFORMAT, D3DPOOL, *mut *mut c_void) -> HRESULT,
    create_vertex_buffer: unsafe extern "system" fn(*mut c_void, u32, u32, u32, D3DPOOL, *mut *mut c_void) -> HRESULT,
    create_index_buffer: unsafe extern "system" fn(*mut c_void, u32, u32, D3DFORMAT, D3DPOOL, *mut *mut c_void) -> HRESULT,
    create_render_target: unsafe extern "system" fn(*mut c_void, u32, u32, D3DFORMAT, D3DMULTISAMPLE_TYPE, BOOL, *mut *mut c_void) -> HRESULT,
    create_depth_stencil_surface: unsafe extern "system" fn(*mut c_void, u32, u32, D3DFORMAT, D3DMULTISAMPLE_TYPE, *mut *mut c_void) -> HRESULT,
    create_image_surface: unsafe extern "system" fn(*mut c_void, u32, u32, D3DFORMAT, *mut *mut c_void) -> HRESULT,
    copy_rects: unsafe extern "system" fn(*mut c_void, *mut c_void, *const RECT, u32, *mut c_void, *const POINT) -> HRESULT,
    update_texture: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> HRESULT,
    get_front_buffer: unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
    set_render_target: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> HRESULT,
    get_render_target: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    get_depth_stencil_surface: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    begin_scene: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    end_scene: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    clear: unsafe extern "system" fn(*mut c_void, u32, *const D3DRECT, u32, u32, f32, u32) -> HRESULT,
    set_transform: unsafe extern "system" fn(*mut c_void, D3DTRANSFORMSTATETYPE, *const c_void) -> HRESULT,
    get_transform: unsafe extern "system" fn(*mut c_void, D3DTRANSFORMSTATETYPE, *mut c_void) -> HRESULT,
    multiply_transform: unsafe extern "system" fn(*mut c_void, D3DTRANSFORMSTATETYPE, *const c_void) -> HRESULT,
    set_viewport: unsafe extern "system" fn(*mut c_void, *const D3DVIEWPORT9) -> HRESULT,
    get_viewport: unsafe extern "system" fn(*mut c_void, *mut D3DVIEWPORT9) -> HRESULT,
    set_material: unsafe extern "system" fn(*mut c_void, *const D3DMATERIAL9) -> HRESULT,
    get_material: unsafe extern "system" fn(*mut c_void, *mut D3DMATERIAL9) -> HRESULT,
    set_light: unsafe extern "system" fn(*mut c_void, u32, *const D3DLIGHT9) -> HRESULT,
    get_light: unsafe extern "system" fn(*mut c_void, u32, *mut D3DLIGHT9) -> HRESULT,
    light_enable: unsafe extern "system" fn(*mut c_void, u32, BOOL) -> HRESULT,
    get_light_enable: unsafe extern "system" fn(*mut c_void, u32, *mut BOOL) -> HRESULT,
    set_clip_plane: unsafe extern "system" fn(*mut c_void, u32, *const f32) -> HRESULT,
    get_clip_plane: unsafe extern "system" fn(*mut c_void, u32, *mut f32) -> HRESULT,
    set_render_state: unsafe extern "system" fn(*mut c_void, u32, u32) -> HRESULT,
    get_render_state: unsafe extern "system" fn(*mut c_void, u32, *mut u32) -> HRESULT,
    begin_state_block: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    end_state_block: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
    apply_state_block: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    capture_state_block: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    delete_state_block: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    create_state_block: unsafe extern "system" fn(*mut c_void, D3DSTATEBLOCKTYPE, *mut u32) -> HRESULT,
    set_clip_status: unsafe extern "system" fn(*mut c_void, *const D3DCLIPSTATUS9) -> HRESULT,
    get_clip_status: unsafe extern "system" fn(*mut c_void, *mut D3DCLIPSTATUS9) -> HRESULT,
    get_texture: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT,
    set_texture: unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> HRESULT,
    get_texture_stage_state: unsafe extern "system" fn(*mut c_void, u32, u32, *mut u32) -> HRESULT,
    set_texture_stage_state: unsafe extern "system" fn(*mut c_void, u32, u32, u32) -> HRESULT,
    validate_device: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
    get_info: unsafe extern "system" fn(*mut c_void, u32, *mut c_void, u32) -> HRESULT,
    set_palette_entries: unsafe extern "system" fn(*mut c_void, u32, *const c_void) -> HRESULT,
    get_palette_entries: unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> HRESULT,
    set_current_texture_palette: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    get_current_texture_palette: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
    draw_primitive: unsafe extern "system" fn(*mut c_void, D3DPRIMITIVETYPE, u32, u32) -> HRESULT,
    draw_indexed_primitive: unsafe extern "system" fn(*mut c_void, D3DPRIMITIVETYPE, u32, u32, u32, u32) -> HRESULT,
    draw_primitive_up: unsafe extern "system" fn(*mut c_void, D3DPRIMITIVETYPE, u32, *const c_void, u32) -> HRESULT,
    draw_indexed_primitive_up: unsafe extern "system" fn(*mut c_void, D3DPRIMITIVETYPE, u32, u32, u32, *const c_void, D3DFORMAT, *const c_void, u32) -> HRESULT,
    process_vertices: unsafe extern "system" fn(*mut c_void, u32, u32, u32, *mut c_void, u32) -> HRESULT,
    create_vertex_shader: unsafe extern "system" fn(*mut c_void, *const u32, *const u32, *mut u32, u32) -> HRESULT,
    set_vertex_shader: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    get_vertex_shader: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
    delete_vertex_shader: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    set_vertex_shader_constant: unsafe extern "system" fn(*mut c_void, u32, *const c_void, u32) -> HRESULT,
    get_vertex_shader_constant: unsafe extern "system" fn(*mut c_void, u32, *mut c_void, u32) -> HRESULT,
    get_vertex_shader_declaration: unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *mut u32) -> HRESULT,
    get_vertex_shader_function: unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *mut u32) -> HRESULT,
    set_stream_source: unsafe extern "system" fn(*mut c_void, u32, *mut c_void, u32) -> HRESULT,
    get_stream_source: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void, *mut u32) -> HRESULT,
    set_indices: unsafe extern "system" fn(*mut c_void, *mut c_void, u32) -> HRESULT,
    get_indices: unsafe extern "system" fn(*mut c_void, *mut *mut c_void, *mut u32) -> HRESULT,
    create_pixel_shader: unsafe extern "system" fn(*mut c_void, *const u32, *mut u32) -> HRESULT,
    set_pixel_shader: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    get_pixel_shader: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
    delete_pixel_shader: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    set_pixel_shader_constant: unsafe extern "system" fn(*mut c_void, u32, *const c_void, u32) -> HRESULT,
    get_pixel_shader_constant: unsafe extern "system" fn(*mut c_void, u32, *mut c_void, u32) -> HRESULT,
    get_pixel_shader_function: unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *mut u32) -> HRESULT,
    draw_rect_patch: unsafe extern "system" fn(*mut c_void, u32, *const f32, *const c_void) -> HRESULT,
    draw_tri_patch: unsafe extern "system" fn(*mut c_void, u32, *const f32, *const c_void) -> HRESULT,
    delete_patch: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
}

const _: () = assert!(size_of::<Device8Vtbl>() == 97 * 4);

static DEVICE8_VTBL: Device8Vtbl = Device8Vtbl {
    query_interface: wrap_query_interface,
    add_ref: wrap_add_ref,
    release: device8_release,
    test_cooperative_level: device8_test_cooperative_level,
    get_available_texture_mem: device8_get_available_texture_mem,
    resource_manager_discard_bytes: device8_resource_manager_discard_bytes,
    get_direct3d: device8_get_direct3d,
    get_device_caps: device8_get_device_caps,
    get_display_mode: device8_get_display_mode,
    get_creation_parameters: device8_get_creation_parameters,
    set_cursor_properties: device8_set_cursor_properties,
    set_cursor_position: device8_set_cursor_position,
    show_cursor: device8_show_cursor,
    create_additional_swap_chain: device8_create_additional_swap_chain,
    reset: device8_reset,
    present: device8_present,
    get_back_buffer: device8_get_back_buffer,
    get_raster_status: device8_get_raster_status,
    set_gamma_ramp: device8_set_gamma_ramp,
    get_gamma_ramp: device8_get_gamma_ramp,
    create_texture: device8_create_texture,
    create_volume_texture: device8_create_volume_texture,
    create_cube_texture: device8_create_cube_texture,
    create_vertex_buffer: device8_create_vertex_buffer,
    create_index_buffer: device8_create_index_buffer,
    create_render_target: device8_create_render_target,
    create_depth_stencil_surface: device8_create_depth_stencil_surface,
    create_image_surface: device8_create_image_surface,
    copy_rects: device8_copy_rects,
    update_texture: device8_update_texture,
    get_front_buffer: device8_get_front_buffer,
    set_render_target: device8_set_render_target,
    get_render_target: device8_get_render_target,
    get_depth_stencil_surface: device8_get_depth_stencil_surface,
    begin_scene: device8_begin_scene,
    end_scene: device8_end_scene,
    clear: device8_clear,
    set_transform: device8_set_transform,
    get_transform: device8_get_transform,
    multiply_transform: device8_multiply_transform,
    set_viewport: device8_set_viewport,
    get_viewport: device8_get_viewport,
    set_material: device8_set_material,
    get_material: device8_get_material,
    set_light: device8_set_light,
    get_light: device8_get_light,
    light_enable: device8_light_enable,
    get_light_enable: device8_get_light_enable,
    set_clip_plane: device8_set_clip_plane,
    get_clip_plane: device8_get_clip_plane,
    set_render_state: device8_set_render_state,
    get_render_state: device8_get_render_state,
    begin_state_block: device8_begin_state_block,
    end_state_block: device8_end_state_block,
    apply_state_block: device8_apply_state_block,
    capture_state_block: device8_capture_state_block,
    delete_state_block: device8_delete_state_block,
    create_state_block: device8_create_state_block,
    set_clip_status: device8_set_clip_status,
    get_clip_status: device8_get_clip_status,
    get_texture: device8_get_texture,
    set_texture: device8_set_texture,
    get_texture_stage_state: device8_get_texture_stage_state,
    set_texture_stage_state: device8_set_texture_stage_state,
    validate_device: device8_validate_device,
    get_info: device8_get_info,
    set_palette_entries: device8_set_palette_entries,
    get_palette_entries: device8_get_palette_entries,
    set_current_texture_palette: device8_set_current_texture_palette,
    get_current_texture_palette: device8_get_current_texture_palette,
    draw_primitive: device8_draw_primitive,
    draw_indexed_primitive: device8_draw_indexed_primitive,
    draw_primitive_up: device8_draw_primitive_up,
    draw_indexed_primitive_up: device8_draw_indexed_primitive_up,
    process_vertices: device8_process_vertices,
    create_vertex_shader: device8_create_vertex_shader,
    set_vertex_shader: device8_set_vertex_shader,
    get_vertex_shader: device8_get_vertex_shader,
    delete_vertex_shader: device8_delete_vertex_shader,
    set_vertex_shader_constant: device8_set_vertex_shader_constant,
    get_vertex_shader_constant: device8_get_vertex_shader_constant,
    get_vertex_shader_declaration: device8_get_vertex_shader_declaration,
    get_vertex_shader_function: device8_get_vertex_shader_function,
    set_stream_source: device8_set_stream_source,
    get_stream_source: device8_get_stream_source,
    set_indices: device8_set_indices,
    get_indices: device8_get_indices,
    create_pixel_shader: device8_create_pixel_shader,
    set_pixel_shader: device8_set_pixel_shader,
    get_pixel_shader: device8_get_pixel_shader,
    delete_pixel_shader: device8_delete_pixel_shader,
    set_pixel_shader_constant: device8_set_pixel_shader_constant,
    get_pixel_shader_constant: device8_get_pixel_shader_constant,
    get_pixel_shader_function: device8_get_pixel_shader_function,
    draw_rect_patch: device8_draw_rect_patch,
    draw_tri_patch: device8_draw_tri_patch,
    delete_patch: device8_delete_patch,
};

unsafe extern "system" fn device8_release(this: *mut c_void) -> u32 {
    let Some(n) = (unsafe { com_header_release(this, "device8") }) else {
        return 0;
    };
    if n == 0 {
        let d = unsafe { device8(this) };
        info!(kind = "d3d8_device_released");
        // The dead wrapper stays behind to catch over-release.
        if !d.parent.is_null() {
            unsafe { d3d8_release(d.parent.cast()) };
        }
    }
    n
}

forward8!(device8_test_cooperative_level, dev9 / dev9_vt.base__.TestCooperativeLevel() -> HRESULT);
forward8!(device8_get_available_texture_mem, dev9 / dev9_vt.base__.GetAvailableTextureMem() -> u32);
// This is vacuously successful because D3D9Ex has no managed pool and `d3d9::translate_managed_pool` rewrites
// every `D3DPOOL_MANAGED` request to `DEFAULT | DYNAMIC`, so nothing is under management to discard.
forward8!(device8_resource_manager_discard_bytes, dev9 / dev9_vt.base__.EvictManagedResources(_bytes: u32) -> HRESULT => ());

unsafe extern "system" fn device8_get_direct3d(
    this: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    let d = unsafe { device8(this) };
    if out.is_null() || d.parent.is_null() {
        return D3DERR_INVALIDCALL;
    }
    unsafe {
        wrap_add_ref(d.parent.cast());
        *out = d.parent.cast();
    }
    D3D_OK
}

unsafe extern "system" fn device8_get_device_caps(
    this: *mut c_void,
    out: *mut D3DCaps8,
) -> HRESULT {
    if out.is_null() {
        return D3DERR_INVALIDCALL;
    }
    let p = unsafe { dev9(this) };
    let mut caps9 = D3DCAPS9::default();
    let hr = unsafe { (dev9_vt(p).base__.GetDeviceCaps)(p, &raw mut caps9) };
    if hr.is_ok() {
        unsafe { *out = caps_9_to_8(&caps9) };
    }
    hr
}

forward8!(device8_get_display_mode, dev9 / dev9_vt.base__.GetDisplayMode(out: *mut D3DDISPLAYMODE) -> HRESULT => (0, out));
forward8!(device8_get_creation_parameters, dev9 / dev9_vt.base__.GetCreationParameters(out: *mut D3DDEVICE_CREATION_PARAMETERS) -> HRESULT);
stub8!(device8_set_cursor_properties, "IDirect3DDevice8::SetCursorProperties"(_x: u32, _y: u32, _bitmap: *mut c_void) -> D3D_OK);
stub8!(device8_set_cursor_position, "IDirect3DDevice8::SetCursorPosition"(_x: u32, _y: u32, _flags: u32));
stub8!(device8_show_cursor, "IDirect3DDevice8::ShowCursor"(_show: BOOL) -> BOOL = BOOL(0));
stub8!(device8_create_additional_swap_chain, "IDirect3DDevice8::CreateAdditionalSwapChain"(_pp: *mut D3DPresentParameters8, _out: *mut *mut c_void) nulls _out -> D3DERR_NOTAVAILABLE);

unsafe extern "system" fn device8_reset(
    this: *mut c_void,
    pp8: *mut D3DPresentParameters8,
) -> HRESULT {
    let Some(pp8) = (unsafe { pp8.as_mut() }) else {
        return D3DERR_INVALIDCALL;
    };
    info!(kind = "d3d8_reset", pp8 = ?pp8);
    let d = unsafe { device8(this) };
    let mut pp9 = convert_present_params(pp8);
    let hr = unsafe { (dev9_vt(d.inner()).base__.Reset)(d.inner(), &raw mut pp9) };
    if hr.is_ok() {
        sync_present_params_back(pp8, &pp9);
    }
    hr
}

forward8!(device8_present, dev9 / dev9_vt.base__.Present(src: *const RECT, dst: *const RECT, window_override: HWND, dirty: *const RGNDATA) -> HRESULT);

unsafe extern "system" fn device8_get_back_buffer(
    this: *mut c_void,
    back_buffer: u32,
    ty: D3DBACKBUFFER_TYPE,
    out: *mut *mut c_void,
) -> HRESULT {
    let d = unsafe { device8(this) };

    let embedded = back_buffer == 0 && ty == D3DBACKBUFFER_TYPE_MONO;
    if embedded && out.is_null() {
        return D3DERR_INVALIDCALL;
    }

    let mut s9 = null_mut();
    let hr = unsafe {
        (dev9_vt(d.inner()).base__.GetBackBuffer)(d.inner(), 0, back_buffer, ty, &raw mut s9)
    };
    if !embedded {
        return unsafe {
            wrap_created(
                "IDirect3DDevice9::GetBackBuffer",
                hr,
                s9,
                (&raw const SURFACE8_VTBL).cast(),
                this.cast::<Device8>(),
                true,
                out,
            )
        };
    }
    if hr.is_err() {
        unsafe { *out = null_mut() };
        return hr;
    }
    if s9.is_null() {
        warn!(
            kind = "d3d9_null_on_success",
            call = "IDirect3DDevice9::GetBackBuffer",
        );
        unsafe { *out = null_mut() };
        return D3DERR_INVALIDCALL;
    }
    unsafe {
        d.back_buffer.adopt(s9);
        *out = (&raw const d.back_buffer).cast_mut().cast();
    }
    hr
}

forward8!(device8_get_raster_status, dev9 / dev9_vt.base__.GetRasterStatus(out: *mut D3DRASTER_STATUS) -> HRESULT => (0, out));

unsafe extern "system" fn device8_set_gamma_ramp(
    this: *mut c_void,
    flags: u32,
    ramp: *const D3DGAMMARAMP,
) {
    unsafe {
        let p = dev9(this);
        (dev9_vt(p).base__.SetGammaRamp)(p, 0, flags, ramp);
    }
}

unsafe extern "system" fn device8_get_gamma_ramp(this: *mut c_void, ramp: *mut D3DGAMMARAMP) {
    unsafe {
        let p = dev9(this);
        (dev9_vt(p).base__.GetGammaRamp)(p, 0, ramp);
    }
}

unsafe extern "system" fn device8_create_texture(
    this: *mut c_void,
    width: u32,
    height: u32,
    levels: u32,
    usage: u32,
    format: D3DFORMAT,
    pool: D3DPOOL,
    out: *mut *mut c_void,
) -> HRESULT {
    let d = unsafe { device8(this) };
    let mut t9 = null_mut();
    let hr = unsafe {
        (dev9_vt(d.inner()).base__.CreateTexture)(
            d.inner(),
            width,
            height,
            levels,
            usage,
            format,
            pool,
            &raw mut t9,
            null_mut(),
        )
    };
    unsafe {
        wrap_created(
            "IDirect3DDevice9::CreateTexture",
            hr,
            t9,
            (&raw const TEXTURE8_VTBL).cast(),
            this.cast::<Device8>(),
            false,
            out,
        )
    }
}

stub8!(device8_create_volume_texture, "IDirect3DDevice8::CreateVolumeTexture"(_w: u32, _h: u32, _d: u32, _levels: u32, _usage: u32, _format: D3DFORMAT, _pool: D3DPOOL, _out: *mut *mut c_void) nulls _out -> D3DERR_NOTAVAILABLE);
stub8!(device8_create_cube_texture, "IDirect3DDevice8::CreateCubeTexture"(_edge: u32, _levels: u32, _usage: u32, _format: D3DFORMAT, _pool: D3DPOOL, _out: *mut *mut c_void) nulls _out -> D3DERR_NOTAVAILABLE);

unsafe extern "system" fn device8_create_vertex_buffer(
    this: *mut c_void,
    length: u32,
    usage: u32,
    fvf: u32,
    pool: D3DPOOL,
    out: *mut *mut c_void,
) -> HRESULT {
    let d = unsafe { device8(this) };
    let mut vb9 = null_mut();
    let hr = unsafe {
        (dev9_vt(d.inner()).base__.CreateVertexBuffer)(
            d.inner(),
            length,
            usage,
            fvf,
            pool,
            &raw mut vb9,
            null_mut(),
        )
    };
    unsafe {
        wrap_created(
            "IDirect3DDevice9::CreateVertexBuffer",
            hr,
            vb9,
            (&raw const VERTEX_BUFFER8_VTBL).cast(),
            this.cast::<Device8>(),
            false,
            out,
        )
    }
}

stub8!(device8_create_index_buffer, "IDirect3DDevice8::CreateIndexBuffer"(_length: u32, _usage: u32, _format: D3DFORMAT, _pool: D3DPOOL, _out: *mut *mut c_void) nulls _out -> D3DERR_NOTAVAILABLE);

unsafe extern "system" fn device8_create_render_target(
    this: *mut c_void,
    width: u32,
    height: u32,
    format: D3DFORMAT,
    multi_sample: D3DMULTISAMPLE_TYPE,
    lockable: BOOL,
    out: *mut *mut c_void,
) -> HRESULT {
    let d = unsafe { device8(this) };
    let mut s9 = null_mut();
    let hr = unsafe {
        (dev9_vt(d.inner()).base__.CreateRenderTarget)(
            d.inner(),
            width,
            height,
            format,
            multi_sample,
            0,
            lockable,
            &raw mut s9,
            null_mut(),
        )
    };
    unsafe {
        wrap_created(
            "IDirect3DDevice9::CreateRenderTarget",
            hr,
            s9,
            (&raw const SURFACE8_VTBL).cast(),
            this.cast::<Device8>(),
            lockable.0 != 0,
            out,
        )
    }
}

// Every game's only depth buffer is the automatic one (`EnableAutoDepthStencil`, D16). No standalone depth-stencil surface is ever created.
stub8!(device8_create_depth_stencil_surface, "IDirect3DDevice8::CreateDepthStencilSurface"(_width: u32, _height: u32, _format: D3DFORMAT, _multi_sample: D3DMULTISAMPLE_TYPE, _out: *mut *mut c_void) nulls _out -> D3DERR_NOTAVAILABLE);

/// D3D8's `CreateImageSurface` can create lockable sysmem surfaces. The D3D9 equivalent is an offscreen plain sysmem surface.
unsafe extern "system" fn device8_create_image_surface(
    this: *mut c_void,
    width: u32,
    height: u32,
    format: D3DFORMAT,
    out: *mut *mut c_void,
) -> HRESULT {
    let d = unsafe { device8(this) };
    let mut s9 = null_mut();
    let hr = unsafe {
        (dev9_vt(d.inner()).base__.CreateOffscreenPlainSurface)(
            d.inner(),
            width,
            height,
            format,
            D3DPOOL_SYSTEMMEM,
            &raw mut s9,
            null_mut(),
        )
    };
    // Offscreen sysmem surfaces are always lockable.
    unsafe {
        wrap_created(
            "IDirect3DDevice9::CreateOffscreenPlainSurface",
            hr,
            s9,
            (&raw const SURFACE8_VTBL).cast(),
            this.cast::<Device8>(),
            true,
            out,
        )
    }
}

unsafe fn surface_desc9(s9: *mut c_void) -> Result<D3DSURFACE_DESC, HRESULT> {
    let mut d = D3DSURFACE_DESC::default();
    let hr = unsafe { (surf9_vt(s9).GetDesc)(s9, &raw mut d) };
    if hr.is_ok() { Ok(d) } else { Err(hr) }
}

/// Returns whether `r` is a positive-extent rect inside the source surface and `pt` places its extent inside the destination surface.
fn copy_rect_valid(r: &RECT, pt: &POINT, sd: &D3DSURFACE_DESC, dd: &D3DSURFACE_DESC) -> bool {
    let (w, h) = (
        i64::from(r.right) - i64::from(r.left),
        i64::from(r.bottom) - i64::from(r.top),
    );

    w > 0
        && h > 0
        && r.left >= 0
        && r.top >= 0
        && i64::from(r.right) <= i64::from(sd.Width)
        && i64::from(r.bottom) <= i64::from(sd.Height)
        && pt.x >= 0
        && pt.y >= 0
        && i64::from(pt.x) + w <= i64::from(dd.Width)
        && i64::from(pt.y) + h <= i64::from(dd.Height)
}

fn dest_rect(r: &RECT, pt: &POINT) -> RECT {
    RECT {
        left: pt.x,
        top: pt.y,
        right: pt.x + (r.right - r.left),
        bottom: pt.y + (r.bottom - r.top),
    }
}

// D3D9 splits D3D8's `CopyRects` into direction-specific calls; we dispatch on the source/dest pools.
unsafe fn copy_one_rect(
    dev9: *mut c_void,
    src9: *mut c_void,
    sd: &D3DSURFACE_DESC,
    dst9: *mut c_void,
    dd: &D3DSURFACE_DESC,
    r: &RECT,
    pt: &POINT,
) -> HRESULT {
    let vt = unsafe { dev9_vt(dev9) };
    // Each of D3D9's direction-specific calls carries a restriction D3D8's `CopyRects` doesn't:
    // `UpdateSurface` refuses a render-target destination (which the games' back buffer is),
    // `GetRenderTargetData` is whole-surface only, and `StretchRect` refuses some pool/usage pairs.
    // Rather than model every rule, we take the call that fits the pool pair and fall back to a locked copy.
    let hr = match (sd.Pool, dd.Pool) {
        (D3DPOOL_SYSTEMMEM, D3DPOOL_DEFAULT) => {
            Some(unsafe { (vt.base__.UpdateSurface)(dev9, src9, r, dst9, pt) })
        }
        (D3DPOOL_DEFAULT, D3DPOOL_SYSTEMMEM) => {
            // `GetRenderTargetData` is whole-surface only and needs identical dimensions.
            let full_surface = r.left == 0
                && r.top == 0
                && r.right.cast_unsigned() == sd.Width
                && r.bottom.cast_unsigned() == sd.Height
                && pt.x == 0
                && pt.y == 0
                && sd.Width == dd.Width
                && sd.Height == dd.Height;
            full_surface.then(|| unsafe { (vt.base__.GetRenderTargetData)(dev9, src9, dst9) })
        }
        (D3DPOOL_DEFAULT, D3DPOOL_DEFAULT) => {
            let dst_rect = dest_rect(r, pt);
            Some(unsafe {
                (vt.base__.StretchRect)(dev9, src9, r, dst9, &raw const dst_rect, D3DTEXF_NONE)
            })
        }
        _ => None,
    };

    if let Some(hr) = hr {
        if hr.is_ok() {
            return hr;
        }
        info!(kind = "d3d8_copy_rects_fallback", hr = %fmt_hr!(hr));
    }
    unsafe { copy_locked(src9, dst9, sd.Format, r, pt) }
}

/// An owned `LockRect` on a D3D9 surface. Dropping it performs the paired `UnlockRect`.
struct SurfaceLock {
    surface: *mut c_void,
    locked: D3DLOCKED_RECT,
}

impl SurfaceLock {
    /// # Safety
    /// `surface` must be a live `IDirect3DSurface9`.
    unsafe fn lock(surface: *mut c_void, rect: &RECT, flags: u32) -> Result<Self, HRESULT> {
        let mut locked = D3DLOCKED_RECT::default();
        let hr = unsafe { (surf9_vt(surface).LockRect)(surface, &raw mut locked, rect, flags) };
        if hr.is_ok() {
            unsafe { com_add_ref(surface) };
            Ok(Self { surface, locked })
        } else {
            Err(hr)
        }
    }
}

impl Drop for SurfaceLock {
    fn drop(&mut self) {
        unsafe {
            let _ = (surf9_vt(self.surface).UnlockRect)(self.surface);
            com_release(self.surface);
        }
    }
}

/// Same-format lock-and-copy fallback for copies not expressible as a single D3D9 device call
/// (e.g. sysmem -> sysmem, MANAGED sources/destinations). `format` is the surfaces' common format.
unsafe fn copy_locked(
    src9: *mut c_void,
    dst9: *mut c_void,
    format: D3DFORMAT,
    r: &RECT,
    pt: &POINT,
) -> HRESULT {
    let Some(bytes_pp) = bytes_per_pixel(format) else {
        warn!(
            kind = "copy_rects_unsupported_format",
            format = format_name(format),
            raw = format.0,
        );
        return D3DERR_INVALIDCALL;
    };
    // Each getter creates a fresh wrapper, so two D3D8 wrappers can hold the same inner surface.
    // However, none of the games self-copy, so we refuse rather than lock one surface twice.
    if src9 == dst9 {
        warn!(kind = "d3d8_copy_rects_self_copy_unsupported");
        return D3DERR_INVALIDCALL;
    }
    let width_bytes = (r.right - r.left).cast_unsigned() * bytes_pp;
    let rows = (r.bottom - r.top).cast_unsigned();
    let dst_rect = dest_rect(r, pt);

    let src_lock = match unsafe { SurfaceLock::lock(src9, r, D3DLOCK_READONLY.cast_unsigned()) } {
        Ok(lock) => lock,
        Err(hr) => return hr,
    };
    let dst_lock = match unsafe { SurfaceLock::lock(dst9, &dst_rect, 0) } {
        Ok(lock) => lock,
        Err(hr) => return hr,
    };

    for row in 0..rows {
        let src_row = unsafe {
            src_lock
                .locked
                .pBits
                .cast::<u8>()
                .offset((row.cast_signed() * src_lock.locked.Pitch) as isize)
        };
        let dst_row = unsafe {
            dst_lock
                .locked
                .pBits
                .cast::<u8>()
                .offset((row.cast_signed() * dst_lock.locked.Pitch) as isize)
        };
        unsafe { copy_nonoverlapping(src_row, dst_row, width_bytes as usize) };
    }

    D3D_OK
}

unsafe extern "system" fn device8_copy_rects(
    this: *mut c_void,
    src8: *mut c_void,
    rects: *const RECT,
    rect_count: u32,
    dst8: *mut c_void,
    points: *const POINT,
) -> HRESULT {
    let src9 = unsafe { unwrap8(src8) };
    let dst9 = unsafe { unwrap8(dst8) };

    if src9.is_null() || dst9.is_null() {
        return D3DERR_INVALIDCALL;
    }
    let (Ok(sd), Ok(dd)) = (unsafe { surface_desc9(src9) }, unsafe {
        surface_desc9(dst9)
    }) else {
        return D3DERR_INVALIDCALL;
    };
    if sd.Format != dd.Format {
        warn!(
            kind = "d3d8_copy_rects_format_mismatch",
            src = format_name(sd.Format),
            dst = format_name(dd.Format),
        );
        return D3DERR_INVALIDCALL;
    }

    let p = unsafe { dev9(this) };

    // Every call site in the games passes either zero or one rects.
    if rect_count > 1 {
        warn!(kind = "d3d8_copy_rects_multi_rect_unsupported", rect_count);
        return D3DERR_INVALIDCALL;
    }

    let (r, pt) = if rects.is_null() || rect_count == 0 {
        let r = RECT {
            left: 0,
            top: 0,
            right: sd.Width.cast_signed(),
            bottom: sd.Height.cast_signed(),
        };
        (r, POINT { x: 0, y: 0 })
    } else {
        let r = unsafe { *rects };
        // A null point array means the rect copies to its own top-left position.
        let pt = if points.is_null() {
            POINT {
                x: r.left,
                y: r.top,
            }
        } else {
            unsafe { *points }
        };
        (r, pt)
    };
    if !copy_rect_valid(&r, &pt, &sd, &dd) {
        warn!(kind = "d3d8_copy_rects_invalid_rect");
        return D3DERR_INVALIDCALL;
    }

    let hr = unsafe { copy_one_rect(p, src9, &sd, dst9, &dd, &r, &pt) };
    if hr.is_err() {
        log_at!(is_transient_device_error(hr) => debug / warn,
            kind = "d3d8_copy_rects_failed",
            hr = %fmt_hr!(hr),
            src_pool = sd.Pool.0,
            dst_pool = dd.Pool.0,
        );
    }
    hr
}

forward8!(device8_update_texture, dev9 / dev9_vt.base__.UpdateTexture(src8: *mut c_void, dst8: *mut c_void) -> HRESULT => (unwrap8(src8), unwrap8(dst8)));
stub8!(device8_get_front_buffer, "IDirect3DDevice8::GetFrontBuffer"(_dst8: *mut c_void) -> D3DERR_NOTAVAILABLE);
stub8!(device8_set_render_target, "IDirect3DDevice8::SetRenderTarget"(_rt8: *mut c_void, _zs8: *mut c_void) -> D3DERR_NOTAVAILABLE);
stub8!(device8_get_render_target, "IDirect3DDevice8::GetRenderTarget"(_out: *mut *mut c_void) nulls _out -> D3DERR_NOTAVAILABLE);
stub8!(device8_get_depth_stencil_surface, "IDirect3DDevice8::GetDepthStencilSurface"(_out: *mut *mut c_void) nulls _out -> D3DERR_NOTAVAILABLE);
forward8!(device8_begin_scene, dev9 / dev9_vt.base__.BeginScene() -> HRESULT);
forward8!(device8_end_scene, dev9 / dev9_vt.base__.EndScene() -> HRESULT);
forward8!(device8_clear, dev9 / dev9_vt.base__.Clear(count: u32, rects: *const D3DRECT, flags: u32, color: u32, z: f32, stencil: u32) -> HRESULT);
// D3D8's `D3DMATRIX` is layout-identical to `windows_numerics::Matrix4x4` used by the D3D9 `SetTransform` binding,
// so we pass the matrix as an opaque pointer and cast at the call boundary.
forward8!(device8_set_transform, dev9 / dev9_vt.base__.SetTransform(state: D3DTRANSFORMSTATETYPE, matrix: *const c_void) -> HRESULT => (state, matrix.cast()));
stub8!(device8_get_transform, "IDirect3DDevice8::GetTransform"(_state: D3DTRANSFORMSTATETYPE, _matrix: *mut c_void) -> D3DERR_NOTAVAILABLE);
stub8!(device8_multiply_transform, "IDirect3DDevice8::MultiplyTransform"(_state: D3DTRANSFORMSTATETYPE, _matrix: *const c_void) -> D3D_OK);
forward8!(device8_set_viewport, dev9 / dev9_vt.base__.SetViewport(viewport: *const D3DVIEWPORT9) -> HRESULT);
forward8!(device8_get_viewport, dev9 / dev9_vt.base__.GetViewport(viewport: *mut D3DVIEWPORT9) -> HRESULT);
stub8!(device8_set_material, "IDirect3DDevice8::SetMaterial"(_material: *const D3DMATERIAL9) -> D3D_OK);
stub8!(device8_get_material, "IDirect3DDevice8::GetMaterial"(_material: *mut D3DMATERIAL9) -> D3DERR_NOTAVAILABLE);
stub8!(device8_set_light, "IDirect3DDevice8::SetLight"(_index: u32, _light: *const D3DLIGHT9) -> D3D_OK);
stub8!(device8_get_light, "IDirect3DDevice8::GetLight"(_index: u32, _light: *mut D3DLIGHT9) -> D3DERR_NOTAVAILABLE);
stub8!(device8_light_enable, "IDirect3DDevice8::LightEnable"(_index: u32, _enable: BOOL) -> D3D_OK);
stub8!(device8_get_light_enable, "IDirect3DDevice8::GetLightEnable"(_index: u32, _enable: *mut BOOL) -> D3DERR_NOTAVAILABLE);
stub8!(device8_set_clip_plane, "IDirect3DDevice8::SetClipPlane"(_index: u32, _plane: *const f32) -> D3D_OK);
stub8!(device8_get_clip_plane, "IDirect3DDevice8::GetClipPlane"(_index: u32, _plane: *mut f32) -> D3DERR_NOTAVAILABLE);

unsafe extern "system" fn device8_set_render_state(
    this: *mut c_void,
    state: u32,
    value: u32,
) -> HRESULT {
    // These are D3D8 `D3DRENDERSTATETYPE` values that were removed in D3D9.
    const D3DRS8_LINEPATTERN: u32 = 10;
    const D3DRS8_ZVISIBLE: u32 = 30;
    const D3DRS8_EDGEANTIALIAS: u32 = 40;
    const D3DRS8_ZBIAS: u32 = 47;
    const D3DRS8_SOFTWAREVERTEXPROCESSING: u32 = 153;
    const D3DRS8_PATCHSEGMENTS: u32 = 164;

    match state {
        // th07/th08 unconditionally set this to 0 (disabled) at initialization, so dropping this is fine.
        D3DRS8_EDGEANTIALIAS => {
            debug!(kind = "d3d8_render_state_dropped", state, value);
            D3D_OK
        }
        // LINEPATTERN and ZVISIBLE have no D3D9 equivalent. ZBIAS has only a lossy heuristic translation to DEPTHBIAS, so it's dropped.
        D3DRS8_LINEPATTERN | D3DRS8_ZVISIBLE | D3DRS8_ZBIAS => {
            warn!(kind = "d3d8_render_state_dropped", state, value);
            D3D_OK
        }
        D3DRS8_SOFTWAREVERTEXPROCESSING => {
            let p = unsafe { dev9(this) };
            unsafe { (dev9_vt(p).base__.SetSoftwareVertexProcessing)(p, BOOL(value.cast_signed())) }
        }
        D3DRS8_PATCHSEGMENTS => {
            let p = unsafe { dev9(this) };
            unsafe { (dev9_vt(p).base__.SetNPatchMode)(p, f32::from_bits(value)) }
        }
        _ => {
            let p = unsafe { dev9(this) };
            unsafe {
                (dev9_vt(p).base__.SetRenderState)(
                    p,
                    D3DRENDERSTATETYPE(state.cast_signed()),
                    value,
                )
            }
        }
    }
}

stub8!(device8_get_render_state, "IDirect3DDevice8::GetRenderState"(_state: u32, _out: *mut u32) -> D3DERR_NOTAVAILABLE);
stub8!(device8_begin_state_block, "IDirect3DDevice8::BeginStateBlock"() -> D3DERR_NOTAVAILABLE);
stub8!(device8_end_state_block, "IDirect3DDevice8::EndStateBlock"(_out: *mut u32) -> D3DERR_NOTAVAILABLE);
stub8!(device8_apply_state_block, "IDirect3DDevice8::ApplyStateBlock"(_token: u32) -> D3DERR_NOTAVAILABLE);
stub8!(device8_capture_state_block, "IDirect3DDevice8::CaptureStateBlock"(_token: u32) -> D3DERR_NOTAVAILABLE);
stub8!(device8_delete_state_block, "IDirect3DDevice8::DeleteStateBlock"(_token: u32) -> D3DERR_NOTAVAILABLE);
stub8!(device8_create_state_block, "IDirect3DDevice8::CreateStateBlock"(_ty: D3DSTATEBLOCKTYPE, _out: *mut u32) -> D3DERR_NOTAVAILABLE);
stub8!(device8_set_clip_status, "IDirect3DDevice8::SetClipStatus"(_status: *const D3DCLIPSTATUS9) -> D3D_OK);
stub8!(device8_get_clip_status, "IDirect3DDevice8::GetClipStatus"(_status: *mut D3DCLIPSTATUS9) -> D3DERR_NOTAVAILABLE);
stub8!(device8_get_texture, "IDirect3DDevice8::GetTexture"(_stage: u32, _out: *mut *mut c_void) nulls _out -> D3DERR_NOTAVAILABLE);
forward8!(device8_set_texture, dev9 / dev9_vt.base__.SetTexture(stage: u32, texture8: *mut c_void) -> HRESULT => (stage, unwrap8(texture8)));
stub8!(device8_get_texture_stage_state, "IDirect3DDevice8::GetTextureStageState"(_stage: u32, _ty: u32, _out: *mut u32) -> D3DERR_NOTAVAILABLE);

unsafe extern "system" fn device8_set_texture_stage_state(
    this: *mut c_void,
    stage: u32,
    ty: u32,
    value: u32,
) -> HRESULT {
    let p = unsafe { dev9(this) };
    match tss_to_sampler_state(ty) {
        Some(sampler) => unsafe { (dev9_vt(p).base__.SetSamplerState)(p, stage, sampler, value) },
        None => unsafe {
            (dev9_vt(p).base__.SetTextureStageState)(
                p,
                stage,
                D3DTEXTURESTAGESTATETYPE(ty.cast_signed()),
                value,
            )
        },
    }
}

stub8!(device8_validate_device, "IDirect3DDevice8::ValidateDevice"(_passes: *mut u32) -> D3DERR_NOTAVAILABLE);
stub8!(device8_get_info, "IDirect3DDevice8::GetInfo"(_info_id: u32, _info: *mut c_void, _size: u32) -> S_FALSE);
stub8!(device8_set_palette_entries, "IDirect3DDevice8::SetPaletteEntries"(_num: u32, _entries: *const c_void) -> D3D_OK);
stub8!(device8_get_palette_entries, "IDirect3DDevice8::GetPaletteEntries"(_num: u32, _entries: *mut c_void) -> D3DERR_NOTAVAILABLE);
stub8!(device8_set_current_texture_palette, "IDirect3DDevice8::SetCurrentTexturePalette"(_num: u32) -> D3D_OK);
stub8!(device8_get_current_texture_palette, "IDirect3DDevice8::GetCurrentTexturePalette"(_num: *mut u32) -> D3DERR_NOTAVAILABLE);
forward8!(device8_draw_primitive, dev9 / dev9_vt.base__.DrawPrimitive(primitive_type: D3DPRIMITIVETYPE, start_vertex: u32, primitive_count: u32) -> HRESULT);
stub8!(device8_draw_indexed_primitive, "IDirect3DDevice8::DrawIndexedPrimitive"(_primitive_type: D3DPRIMITIVETYPE, _min_index: u32, _num_vertices: u32, _start_index: u32, _primitive_count: u32) -> D3DERR_NOTAVAILABLE);
forward8!(device8_draw_primitive_up, dev9 / dev9_vt.base__.DrawPrimitiveUP(primitive_type: D3DPRIMITIVETYPE, primitive_count: u32, vertex_data: *const c_void, vertex_stride: u32) -> HRESULT);
stub8!(device8_draw_indexed_primitive_up, "IDirect3DDevice8::DrawIndexedPrimitiveUP"(_primitive_type: D3DPRIMITIVETYPE, _min_vertex_index: u32, _num_vertex_indices: u32, _primitive_count: u32, _index_data: *const c_void, _index_data_format: D3DFORMAT, _vertex_data: *const c_void, _vertex_stride: u32) -> D3DERR_NOTAVAILABLE);
stub8!(device8_process_vertices, "IDirect3DDevice8::ProcessVertices"(_src_start: u32, _dest_index: u32, _count: u32, _dest_buffer: *mut c_void, _flags: u32) -> D3DERR_NOTAVAILABLE);
stub8!(device8_create_vertex_shader, "IDirect3DDevice8::CreateVertexShader"(_declaration: *const u32, _function: *const u32, _handle: *mut u32, _usage: u32) -> D3DERR_NOTAVAILABLE);

unsafe extern "system" fn device8_set_vertex_shader(this: *mut c_void, handle: u32) -> HRESULT {
    unsafe {
        let p = dev9(this);
        // The games never create shaders, so every handle is an FVF code.
        (dev9_vt(p).base__.SetFVF)(p, handle)
    }
}

stub8!(device8_get_vertex_shader, "IDirect3DDevice8::GetVertexShader"(_out: *mut u32) -> D3DERR_NOTAVAILABLE);
stub8!(device8_delete_vertex_shader, "IDirect3DDevice8::DeleteVertexShader"(_handle: u32) -> D3D_OK);
stub8!(device8_set_vertex_shader_constant, "IDirect3DDevice8::SetVertexShaderConstant"(_register: u32, _data: *const c_void, _count: u32) -> D3D_OK);
stub8!(device8_get_vertex_shader_constant, "IDirect3DDevice8::GetVertexShaderConstant"(_register: u32, _data: *mut c_void, _count: u32) -> D3DERR_NOTAVAILABLE);
stub8!(device8_get_vertex_shader_declaration, "IDirect3DDevice8::GetVertexShaderDeclaration"(_handle: u32, _data: *mut c_void, _size: *mut u32) -> D3DERR_NOTAVAILABLE);
stub8!(device8_get_vertex_shader_function, "IDirect3DDevice8::GetVertexShaderFunction"(_handle: u32, _data: *mut c_void, _size: *mut u32) -> D3DERR_NOTAVAILABLE);
forward8!(device8_set_stream_source, dev9 / dev9_vt.base__.SetStreamSource(stream: u32, vb8: *mut c_void, stride: u32) -> HRESULT => (stream, unwrap8(vb8), 0, stride));
stub8!(device8_get_stream_source, "IDirect3DDevice8::GetStreamSource"(_stream: u32, _out: *mut *mut c_void, _stride: *mut u32) nulls _out -> D3DERR_NOTAVAILABLE);
stub8!(device8_set_indices, "IDirect3DDevice8::SetIndices"(_ib8: *mut c_void, _base_vertex_index: u32) -> D3D_OK);
stub8!(device8_get_indices, "IDirect3DDevice8::GetIndices"(_out: *mut *mut c_void, _base: *mut u32) nulls _out -> D3DERR_NOTAVAILABLE);
stub8!(device8_create_pixel_shader, "IDirect3DDevice8::CreatePixelShader"(_function: *const u32, _handle: *mut u32) -> D3DERR_NOTAVAILABLE);
stub8!(device8_set_pixel_shader, "IDirect3DDevice8::SetPixelShader"(_handle: u32) -> D3D_OK);
stub8!(device8_get_pixel_shader, "IDirect3DDevice8::GetPixelShader"(_out: *mut u32) -> D3DERR_NOTAVAILABLE);
stub8!(device8_delete_pixel_shader, "IDirect3DDevice8::DeletePixelShader"(_handle: u32) -> D3D_OK);
stub8!(device8_set_pixel_shader_constant, "IDirect3DDevice8::SetPixelShaderConstant"(_register: u32, _data: *const c_void, _count: u32) -> D3D_OK);
stub8!(device8_get_pixel_shader_constant, "IDirect3DDevice8::GetPixelShaderConstant"(_register: u32, _data: *mut c_void, _count: u32) -> D3DERR_NOTAVAILABLE);
stub8!(device8_get_pixel_shader_function, "IDirect3DDevice8::GetPixelShaderFunction"(_handle: u32, _data: *mut c_void, _size: *mut u32) -> D3DERR_NOTAVAILABLE);
stub8!(device8_draw_rect_patch, "IDirect3DDevice8::DrawRectPatch"(_handle: u32, _num_segs: *const f32, _info: *const c_void) -> D3DERR_NOTAVAILABLE);
stub8!(device8_draw_tri_patch, "IDirect3DDevice8::DrawTriPatch"(_handle: u32, _num_segs: *const f32, _info: *const c_void) -> D3DERR_NOTAVAILABLE);
stub8!(device8_delete_patch, "IDirect3DDevice8::DeletePatch"(_handle: u32) -> D3DERR_NOTAVAILABLE);

#[repr(C)]
struct D3d8 {
    // `header.inner` is the wrapped `IDirect3D9Ex` with core vtable hooks installed.
    header: ComHeader,
}

impl D3d8 {
    fn inner(&self) -> *mut c_void {
        self.header.inner.get()
    }
}

unsafe fn d3d8<'a>(this: *mut c_void) -> &'a D3d8 {
    unsafe { &*this.cast() }
}

#[repr(C)]
#[rustfmt::skip]
struct D3d8Vtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    register_software_device: unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
    get_adapter_count: unsafe extern "system" fn(*mut c_void) -> u32,
    get_adapter_identifier: unsafe extern "system" fn(*mut c_void, u32, u32, *mut c_void) -> HRESULT,
    get_adapter_mode_count: unsafe extern "system" fn(*mut c_void, u32) -> u32,
    enum_adapter_modes: unsafe extern "system" fn(*mut c_void, u32, u32, *mut D3DDISPLAYMODE) -> HRESULT,
    get_adapter_display_mode: unsafe extern "system" fn(*mut c_void, u32, *mut D3DDISPLAYMODE) -> HRESULT,
    check_device_type: unsafe extern "system" fn(*mut c_void, u32, D3DDEVTYPE, D3DFORMAT, D3DFORMAT, BOOL) -> HRESULT,
    check_device_format: unsafe extern "system" fn(*mut c_void, u32, D3DDEVTYPE, D3DFORMAT, u32, u32, D3DFORMAT) -> HRESULT,
    check_device_multi_sample_type: unsafe extern "system" fn(*mut c_void, u32, D3DDEVTYPE, D3DFORMAT, BOOL, D3DMULTISAMPLE_TYPE) -> HRESULT,
    check_depth_stencil_match: unsafe extern "system" fn(*mut c_void, u32, D3DDEVTYPE, D3DFORMAT, D3DFORMAT, D3DFORMAT) -> HRESULT,
    get_device_caps: unsafe extern "system" fn(*mut c_void, u32, D3DDEVTYPE, *mut D3DCaps8) -> HRESULT,
    get_adapter_monitor: unsafe extern "system" fn(*mut c_void, u32) -> HMONITOR,
    create_device: unsafe extern "system" fn(*mut c_void, u32, D3DDEVTYPE, HWND, u32, *mut D3DPresentParameters8, *mut *mut c_void) -> HRESULT,
}

const _: () = assert!(size_of::<D3d8Vtbl>() == 16 * 4);

static D3D8_VTBL: D3d8Vtbl = D3d8Vtbl {
    query_interface: wrap_query_interface,
    add_ref: wrap_add_ref,
    release: d3d8_release,
    register_software_device: d3d8_register_software_device,
    get_adapter_count: d3d8_get_adapter_count,
    get_adapter_identifier: d3d8_get_adapter_identifier,
    get_adapter_mode_count: d3d8_get_adapter_mode_count,
    enum_adapter_modes: d3d8_enum_adapter_modes,
    get_adapter_display_mode: d3d8_get_adapter_display_mode,
    check_device_type: d3d8_check_device_type,
    check_device_format: d3d8_check_device_format,
    check_device_multi_sample_type: d3d8_check_device_multi_sample_type,
    check_depth_stencil_match: d3d8_check_depth_stencil_match,
    get_device_caps: d3d8_get_device_caps,
    get_adapter_monitor: d3d8_get_adapter_monitor,
    create_device: d3d8_create_device,
};

unsafe extern "system" fn d3d8_release(this: *mut c_void) -> u32 {
    let Some(n) = (unsafe { com_header_release(this, "d3d8") }) else {
        return 0;
    };
    if n == 0 {
        info!(kind = "d3d8_released");
    }
    n
}

stub8!(d3d8_register_software_device, "IDirect3D8::RegisterSoftwareDevice"(_init_fn: *mut c_void) -> D3D_OK);
stub8!(d3d8_get_adapter_count, "IDirect3D8::GetAdapterCount"() -> u32 = 0);
stub8!(d3d8_get_adapter_identifier, "IDirect3D8::GetAdapterIdentifier"(_adapter: u32, _flags: u32, _out: *mut c_void) -> D3DERR_NOTAVAILABLE);
stub8!(d3d8_get_adapter_mode_count, "IDirect3D8::GetAdapterModeCount"(_adapter: u32) -> u32 = 0);
stub8!(d3d8_enum_adapter_modes, "IDirect3D8::EnumAdapterModes"(_adapter: u32, _mode: u32, _out: *mut D3DDISPLAYMODE) -> D3DERR_NOTAVAILABLE);

unsafe extern "system" fn d3d8_get_adapter_display_mode(
    this: *mut c_void,
    adapter: u32,
    out: *mut D3DDISPLAYMODE,
) -> HRESULT {
    unsafe {
        let p = d3d8(this).inner();
        (d3d9_vt(p).base__.GetAdapterDisplayMode)(p, adapter, out)
    }
}

stub8!(d3d8_check_device_type, "IDirect3D8::CheckDeviceType"(_adapter: u32, _device_type: D3DDEVTYPE, _display_format: D3DFORMAT, _back_buffer_format: D3DFORMAT, _windowed: BOOL) -> D3DERR_NOTAVAILABLE);

unsafe extern "system" fn d3d8_check_device_format(
    this: *mut c_void,
    adapter: u32,
    device_type: D3DDEVTYPE,
    adapter_format: D3DFORMAT,
    usage: u32,
    rtype: u32,
    check_format: D3DFORMAT,
) -> HRESULT {
    unsafe {
        let p = d3d8(this).inner();
        (d3d9_vt(p).base__.CheckDeviceFormat)(
            p,
            adapter,
            device_type,
            adapter_format,
            usage,
            D3DRESOURCETYPE(rtype.cast_signed()),
            check_format,
        )
    }
}

stub8!(d3d8_check_device_multi_sample_type, "IDirect3D8::CheckDeviceMultiSampleType"(_adapter: u32, _device_type: D3DDEVTYPE, _surface_format: D3DFORMAT, _windowed: BOOL, _multi_sample_type: D3DMULTISAMPLE_TYPE) -> D3DERR_NOTAVAILABLE);
stub8!(d3d8_check_depth_stencil_match, "IDirect3D8::CheckDepthStencilMatch"(_adapter: u32, _device_type: D3DDEVTYPE, _adapter_format: D3DFORMAT, _render_target_format: D3DFORMAT, _depth_stencil_format: D3DFORMAT) -> D3DERR_NOTAVAILABLE);
// The games query capabilities through the device (see `device8_get_device_caps`), not the interface.
stub8!(d3d8_get_device_caps, "IDirect3D8::GetDeviceCaps"(_adapter: u32, _device_type: D3DDEVTYPE, _out: *mut D3DCaps8) -> D3DERR_NOTAVAILABLE);
stub8!(d3d8_get_adapter_monitor, "IDirect3D8::GetAdapterMonitor"(_adapter: u32) -> HMONITOR = HMONITOR(null_mut()));

unsafe extern "system" fn d3d8_create_device(
    this: *mut c_void,
    adapter: u32,
    device_type: D3DDEVTYPE,
    focus_window: HWND,
    behavior_flags: u32,
    pp8: *mut D3DPresentParameters8,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return D3DERR_INVALIDCALL;
    }
    unsafe { *out = null_mut() };

    let Some(pp8_ref) = (unsafe { pp8.as_mut() }) else {
        return D3DERR_INVALIDCALL;
    };
    info!(kind = "d3d8_create_device", pp8 = ?pp8_ref);

    let d = unsafe { d3d8(this) };
    let mut pp9 = convert_present_params(pp8_ref);
    let mut dev9_ptr = null_mut();
    let hr = unsafe {
        (d3d9_vt(d.inner()).base__.CreateDevice)(
            d.inner(),
            adapter,
            device_type,
            focus_window,
            behavior_flags,
            &raw mut pp9,
            &raw mut dev9_ptr,
        )
    };
    if hr.is_err() {
        warn!(kind = "d3d8_create_device_failed", hr = %fmt_hr!(hr));
        return hr;
    }
    if dev9_ptr.is_null() {
        warn!(
            kind = "d3d9_null_on_success",
            call = "IDirect3D9::CreateDevice",
        );
        warn!(kind = "d3d8_create_device_failed", hr = %fmt_hr!(D3DERR_INVALIDCALL));
        return D3DERR_INVALIDCALL;
    }

    sync_present_params_back(pp8_ref, &pp9);

    // We use a real reference on the parent for the device's lifetime
    // so doing `CreateDevice(...); parent->Release()` can't leave `GetDirect3D` dangling.
    unsafe { wrap_add_ref(this) };
    let device = Box::into_raw(Box::new(Device8 {
        header: ComHeader {
            vtbl: (&raw const DEVICE8_VTBL).cast(),
            refs: Cell::new(1),
            inner: Cell::new(dev9_ptr),
        },
        parent: this.cast(),
        back_buffer: Resource8 {
            header: ComHeader {
                vtbl: (&raw const SURFACE8_VTBL).cast(),
                refs: Cell::new(0),
                inner: Cell::new(null_mut()),
            },
            // Set below; the backpointer only exists once the box has an address.
            device: null_mut(),
            internal_flags: D3D8_INTERNAL_LOCKABLE,
        },
    }));
    unsafe {
        (*device).back_buffer.device = device;
        *out = device.cast();
    }

    info!(
        kind = "d3d8_device_created",
        device8 = format_args!("{device:p}"),
        device9 = format_args!("{dev9_ptr:p}"),
    );
    hr
}

iat_hook! {
    REAL_DIRECT3D_CREATE8 / real_direct3d_create8 : "Direct3DCreate8"
        as fn(sdk_version: u32) -> *mut c_void;
}

/// Optional callback run at `Direct3DCreate8` time, for work that must happen after the game loads its config
/// but before any window or device exists.
static PRE_CREATE_FN: OnceLock<fn()> = OnceLock::new();

/// Registers the pre-create callback; first caller wins. This is safe to call anywhere during process attachment
/// since the game can't reach `Direct3DCreate8` until `DllMain` returns.
pub fn set_pre_create_fn(f: fn()) {
    let _ = PRE_CREATE_FN.set(f);
}

/// IAT-hooks `Direct3DCreate8` against `host`'s import table.
/// The system `d3d8.dll` stays mapped but is never called into; the returned object translates everything to D3D9Ex.
///
/// # Safety
/// `host` must be a loaded module handle.
pub unsafe fn install(host: HMODULE) {
    unsafe { REAL_DIRECT3D_CREATE8.install(host, hook_direct3dcreate8) };
}

/// Returns a patch that rewrites a `Direct3DCreate8` call site to a direct call to our hook, bypassing any downstream IAT hook.
/// Accepts the same kinds of call sites as [`crate::d3d9::call_site_rewrite`].
#[must_use]
pub const fn call_site_rewrite<const N: usize>(
    addr: usize,
    expected: &'static [u8; N],
) -> PatchSite {
    PatchSite::call(
        addr,
        expected,
        hook_direct3dcreate8 as *mut (),
        "Direct3DCreate8 call-site rewrite",
    )
}

/// The present-parameter policy that every device created through this shim gets.
const SHIM_POLICY: PresentPolicy = PresentPolicy {
    // The games' statically-linked D3DX8 locks the back buffer (e.g. `D3DXLoadSurfaceFromSurface`),
    // so the flag they request must survive the rewrite.
    keep_lockable_back_buffer: true,
    // This risks diverging from a game's own copy of the present params, since the games rederive surface formats from that copy
    // and `CopyRects` requires matching formats. Game-specific crates using this must also fix the game's view if necessary
    // (e.g. th06's `InitD3dRendering` forces 32-bit).
    upgrade_16bit_back_buffer: true,
};

unsafe extern "system" fn hook_direct3dcreate8(sdk_version: u32) -> *mut c_void {
    if let Some(f) = PRE_CREATE_FN.get() {
        f();
    }

    let d3d9 = unsafe { create_hooked_d3d9_with(D3D_SDK_VERSION, SHIM_POLICY) };
    if d3d9.is_null() {
        warn!(
            kind = "d3d8_init_failed",
            sdk_version = format_args!("{sdk_version:#x}"),
        );
        return null_mut();
    }

    let wrapper = Box::into_raw(Box::new(D3d8 {
        header: ComHeader {
            vtbl: (&raw const D3D8_VTBL).cast(),
            refs: Cell::new(1),
            inner: Cell::new(d3d9),
        },
    }));

    info!(
        kind = "d3d8_init",
        sdk_version = format_args!("{sdk_version:#x}"),
        wrapper = format_args!("{wrapper:p}"),
    );

    wrapper.cast()
}

#[cfg(test)]
mod tests {
    use super::{
        ComHeader, D3D_OK, D3D8_INTERNAL_LOCKABLE, D3DERR_INVALIDCALL, D3DPresentParameters8,
        DEVICE8_VTBL, Device8, Resource8, SURFACE8_VTBL, caps_9_to_8, convert_present_params,
        copy_rect_valid, resource8_release, surface_desc_9_to_8, wrap_add_ref, wrap_created,
    };
    use std::cell::Cell;
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use windows::Win32::Foundation::{E_NOINTERFACE, HWND, POINT, RECT};
    use windows::Win32::Graphics::Direct3D9::{
        D3DCAPS9, D3DFMT_A1R5G5B5, D3DFMT_A8R8G8B8, D3DFMT_D16, D3DFMT_DXT1, D3DFMT_R5G6B5,
        D3DFMT_X1R5G5B5, D3DFMT_X8R8G8B8, D3DMULTISAMPLE_2_SAMPLES, D3DMULTISAMPLE_NONE,
        D3DPOOL_SYSTEMMEM, D3DPRESENT_INTERVAL_ONE, D3DPRESENTFLAG_LOCKABLE_BACKBUFFER,
        D3DSURFACE_DESC, D3DSWAPEFFECT_COPY, D3DSWAPEFFECT_DISCARD, D3DSWAPEFFECT_FLIP,
    };
    use windows::core::{BOOL, GUID, HRESULT, IUnknown_Vtbl};

    fn base_pp8() -> D3DPresentParameters8 {
        D3DPresentParameters8 {
            BackBufferWidth: 640,
            BackBufferHeight: 480,
            BackBufferFormat: D3DFMT_X8R8G8B8,
            BackBufferCount: 1,
            MultiSampleType: D3DMULTISAMPLE_NONE,
            SwapEffect: D3DSWAPEFFECT_COPY.0.cast_unsigned(),
            hDeviceWindow: HWND(null_mut()),
            Windowed: BOOL(1),
            EnableAutoDepthStencil: BOOL(1),
            AutoDepthStencilFormat: D3DFMT_D16,
            Flags: D3DPRESENTFLAG_LOCKABLE_BACKBUFFER,
            FullScreen_RefreshRateInHz: 0,
            FullScreen_PresentationInterval: 0,
        }
    }

    #[test]
    fn convert_present_params_fields() {
        let mut pp8 = base_pp8();
        pp8.FullScreen_PresentationInterval = D3DPRESENT_INTERVAL_ONE.cast_unsigned();
        let pp9 = convert_present_params(&pp8);
        assert_eq!(pp9.BackBufferWidth, 640);
        assert_eq!(pp9.BackBufferHeight, 480);
        assert_eq!(pp9.BackBufferFormat, D3DFMT_X8R8G8B8);
        assert_eq!(pp9.BackBufferCount, 1);
        assert_eq!(pp9.MultiSampleQuality, 0);
        assert_eq!(pp9.SwapEffect, D3DSWAPEFFECT_COPY);
        assert_eq!(pp9.Windowed, BOOL(1));
        assert_eq!(pp9.EnableAutoDepthStencil, BOOL(1));
        assert_eq!(pp9.AutoDepthStencilFormat, D3DFMT_D16);
        assert_eq!(pp9.Flags, D3DPRESENTFLAG_LOCKABLE_BACKBUFFER);
        assert_eq!(pp9.FullScreen_RefreshRateInHz, 0);
        assert_eq!(
            pp9.PresentationInterval,
            D3DPRESENT_INTERVAL_ONE.cast_unsigned(),
        );
    }

    #[test]
    fn convert_present_params_swap_effects() {
        let mut pp8 = base_pp8();

        pp8.SwapEffect = 4;
        assert_eq!(convert_present_params(&pp8).SwapEffect, D3DSWAPEFFECT_COPY);

        // `FLIP` survives translation so the games can still read the back buffer after `Present`.
        pp8.SwapEffect = D3DSWAPEFFECT_FLIP.0.cast_unsigned();
        pp8.MultiSampleType = D3DMULTISAMPLE_NONE;
        assert_eq!(convert_present_params(&pp8).SwapEffect, D3DSWAPEFFECT_FLIP);

        // ...except with multisampling, where D3D9 accepts nothing but `DISCARD`.
        pp8.MultiSampleType = D3DMULTISAMPLE_2_SAMPLES;
        assert_eq!(
            convert_present_params(&pp8).SwapEffect,
            D3DSWAPEFFECT_DISCARD,
        );
        pp8.MultiSampleType = D3DMULTISAMPLE_NONE;

        pp8.SwapEffect = D3DSWAPEFFECT_DISCARD.0.cast_unsigned();
        assert_eq!(
            convert_present_params(&pp8).SwapEffect,
            D3DSWAPEFFECT_DISCARD,
        );
    }

    #[test]
    fn convert_present_params_back_buffer_format() {
        for requested in [
            D3DFMT_R5G6B5,
            D3DFMT_X1R5G5B5,
            D3DFMT_A1R5G5B5,
            D3DFMT_X8R8G8B8,
            D3DFMT_A8R8G8B8,
        ] {
            let mut pp8 = base_pp8();
            pp8.BackBufferFormat = requested;
            pp8.Windowed = BOOL(0);
            assert_eq!(
                convert_present_params(&pp8).BackBufferFormat,
                requested,
                "format {requested:?}"
            );
        }
    }

    /// Minimal COM object standing in for a wrapped D3D9 interface.
    #[repr(C)]
    struct MockCom {
        vtbl: *const IUnknown_Vtbl,
        adds: Cell<u32>,
        releases: Cell<u32>,
    }

    impl MockCom {
        fn new() -> Self {
            Self {
                vtbl: &raw const MOCK_COM_VTBL,
                adds: Cell::new(0),
                releases: Cell::new(0),
            }
        }

        fn ptr(&self) -> *mut c_void {
            (&raw const *self).cast_mut().cast()
        }
    }

    unsafe extern "system" fn mock_com_query_interface(
        _this: *mut c_void,
        _riid: *const GUID,
        _out: *mut *mut c_void,
    ) -> HRESULT {
        E_NOINTERFACE
    }

    unsafe extern "system" fn mock_com_add_ref(this: *mut c_void) -> u32 {
        let m = unsafe { &*this.cast::<MockCom>() };
        m.adds.update(|n| n + 1);
        1
    }

    unsafe extern "system" fn mock_com_release(this: *mut c_void) -> u32 {
        let m = unsafe { &*this.cast::<MockCom>() };
        m.releases.update(|n| n + 1);
        0
    }

    static MOCK_COM_VTBL: IUnknown_Vtbl = IUnknown_Vtbl {
        QueryInterface: mock_com_query_interface,
        AddRef: mock_com_add_ref,
        Release: mock_com_release,
    };

    #[test]
    fn wrap_created_reject_success_with_null_interface() {
        let mut out = (&raw const MOCK_COM_VTBL).cast_mut().cast();
        let hr = unsafe {
            wrap_created(
                "test",
                D3D_OK,
                null_mut(),
                (&raw const SURFACE8_VTBL).cast(),
                null_mut(),
                false,
                &raw mut out,
            )
        };
        assert_eq!(hr, D3DERR_INVALIDCALL);
        assert!(out.is_null());
    }

    #[test]
    fn wrap_created_forward_original_failure() {
        const D3DERR_OUTOFVIDEOMEMORY: HRESULT = HRESULT(0x8876_017c_u32.cast_signed());

        let mut out = (&raw const MOCK_COM_VTBL).cast_mut().cast();
        let hr = unsafe {
            wrap_created(
                "test",
                D3DERR_OUTOFVIDEOMEMORY,
                null_mut(),
                (&raw const SURFACE8_VTBL).cast(),
                null_mut(),
                false,
                &raw mut out,
            )
        };
        assert_eq!(hr, D3DERR_OUTOFVIDEOMEMORY, "original error must survive");
        assert!(out.is_null());
    }

    #[test]
    fn wrap_created_wrap_live_interface() {
        let inner = MockCom::new();
        let mut out: *mut c_void = null_mut();
        let hr = unsafe {
            wrap_created(
                "test",
                D3D_OK,
                inner.ptr(),
                (&raw const SURFACE8_VTBL).cast(),
                null_mut(),
                true,
                &raw mut out,
            )
        };
        assert_eq!(hr, D3D_OK);
        assert!(!out.is_null());

        let wrapper = unsafe { &*out.cast::<Resource8>() };
        assert_eq!(wrapper.internal_flags, D3D8_INTERNAL_LOCKABLE);
        assert_eq!(wrapper.header.inner.get(), inner.ptr());

        // Releasing the wrapper returns the adopted D3D9 reference exactly once.
        assert_eq!(unsafe { resource8_release(out) }, 0);
        assert_eq!(inner.releases.get(), 1);
    }

    #[test]
    fn embedded_back_buffer_lifecycle() {
        let dev9 = MockCom::new();
        let surf_a = MockCom::new();
        let surf_b = MockCom::new();

        // Mirrors construction in `d3d8_create_device` with a mock D3D9 device.
        let device = Box::into_raw(Box::new(Device8 {
            header: ComHeader {
                vtbl: (&raw const DEVICE8_VTBL).cast(),
                refs: Cell::new(1),
                inner: Cell::new(dev9.ptr()),
            },
            parent: null_mut(),
            back_buffer: Resource8 {
                header: ComHeader {
                    vtbl: (&raw const SURFACE8_VTBL).cast(),
                    refs: Cell::new(0),
                    inner: Cell::new(null_mut()),
                },
                device: null_mut(),
                internal_flags: D3D8_INTERNAL_LOCKABLE,
            },
        }));

        unsafe {
            (*device).back_buffer.device = device;

            let wrapper = (&raw const (*device).back_buffer).cast_mut().cast();

            // `adopt` absorbs the pre-owned D3D9 reference and takes the device reference, entering the alive state.
            (*device).back_buffer.adopt(surf_a.ptr());
            assert_eq!((*device).back_buffer.header.refs.get(), 1);
            assert_eq!((*device).header.refs.get(), 2);
            assert_eq!(dev9.adds.get(), 1);

            // When another pre-owned D3D9 reference joins, a second device reference is not taken.
            (*device).back_buffer.adopt(surf_a.ptr());
            assert_eq!((*device).back_buffer.header.refs.get(), 2);
            assert_eq!((*device).header.refs.get(), 2);
            assert_eq!(dev9.adds.get(), 1);

            // A game-side AddRef on the live wrapper still references the inner surface.
            assert_eq!(wrap_add_ref(wrapper), 3);
            assert_eq!(surf_a.adds.get(), 1);

            // Reaching 0 returns the surface references and the device reference, entering the dead state.
            assert_eq!(resource8_release(wrapper), 2);
            assert_eq!(resource8_release(wrapper), 1);
            assert_eq!(resource8_release(wrapper), 0);
            assert_eq!(surf_a.releases.get(), 3);
            assert_eq!(dev9.releases.get(), 1);
            assert_eq!((*device).header.refs.get(), 1);

            // AddRef or Release calls after death are refused without touching the stale inner pointer.
            assert_eq!(resource8_release(wrapper), 0);
            assert_eq!(wrap_add_ref(wrapper), 0);
            assert_eq!(surf_a.releases.get(), 3);
            assert_eq!(surf_a.adds.get(), 1);

            // Adoption after a Reset wraps the freshly acquired surface, not the stale one.
            (*device).back_buffer.adopt(surf_b.ptr());
            assert_eq!((*device).back_buffer.header.inner.get(), surf_b.ptr());
            assert_eq!(dev9.adds.get(), 2);
            assert_eq!(resource8_release(wrapper), 0);
            assert_eq!(surf_b.releases.get(), 1);
            assert_eq!((*device).header.refs.get(), 1);

            drop(Box::from_raw(device));
        }
    }

    fn desc_with(width: u32, height: u32) -> D3DSURFACE_DESC {
        D3DSURFACE_DESC {
            Width: width,
            Height: height,
            ..Default::default()
        }
    }

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn copy_rect_validation() {
        let sd = desc_with(640, 480);
        let dd = desc_with(256, 256);
        let origin = POINT { x: 0, y: 0 };

        assert!(copy_rect_valid(
            &rect(0, 0, 640, 480),
            &POINT { x: 0, y: 0 },
            &sd,
            &desc_with(640, 480),
        ));
        assert!(copy_rect_valid(
            &rect(10, 20, 100, 120),
            &POINT { x: 166, y: 156 },
            &sd,
            &dd
        ));

        assert!(!copy_rect_valid(&rect(100, 0, 10, 50), &origin, &sd, &dd));
        assert!(!copy_rect_valid(&rect(0, 100, 50, 10), &origin, &sd, &dd));
        assert!(!copy_rect_valid(&rect(5, 5, 5, 50), &origin, &sd, &dd));
        assert!(!copy_rect_valid(&rect(-1, 0, 10, 10), &origin, &sd, &dd));
        assert!(!copy_rect_valid(&rect(0, 0, 641, 10), &origin, &sd, &dd));
        assert!(!copy_rect_valid(
            &rect(0, 0, 100, 100),
            &POINT { x: 200, y: 0 },
            &sd,
            &dd
        ));
        assert!(!copy_rect_valid(
            &rect(0, 0, 100, 100),
            &POINT { x: -1, y: 0 },
            &sd,
            &dd
        ));
        assert!(!copy_rect_valid(
            &rect(0, 0, i32::MAX, 1),
            &POINT { x: i32::MAX, y: 0 },
            &sd,
            &dd
        ));
    }

    #[test]
    fn caps_translation_fields() {
        let c9 = D3DCAPS9 {
            AdapterOrdinal: 3,
            PresentationIntervals: 0x8000_000f_u32.cast_signed().cast_unsigned(),
            TextureOpCaps: 0x42,
            MaxTextureWidth: 16384,
            MaxSimultaneousTextures: 8,
            PixelShader1xMaxValue: 8.0,
            ..Default::default()
        };
        let c8 = caps_9_to_8(&c9);
        assert_eq!(c8.AdapterOrdinal, 3);
        assert_eq!(c8.PresentationIntervals, c9.PresentationIntervals);
        assert_eq!(c8.TextureOpCaps, 0x42);
        assert_eq!(c8.MaxTextureWidth, 16384);
        assert_eq!(c8.MaxSimultaneousTextures, 8);
        assert_eq!(c8.MaxPixelShaderValue.to_bits(), 8.0f32.to_bits());
    }

    #[test]
    fn surface_desc_translation_sizes() {
        let d9 = D3DSURFACE_DESC {
            Format: D3DFMT_X8R8G8B8,
            Usage: 0,
            Pool: D3DPOOL_SYSTEMMEM,
            MultiSampleType: D3DMULTISAMPLE_NONE,
            MultiSampleQuality: 0,
            Width: 640,
            Height: 480,
            ..Default::default()
        };
        let d8 = surface_desc_9_to_8(&d9);
        assert_eq!(d8.Width, 640);
        assert_eq!(d8.Height, 480);
        assert_eq!(d8.Size, 640 * 480 * 4);
        assert_eq!(d8.Pool, D3DPOOL_SYSTEMMEM);

        let d9_16 = D3DSURFACE_DESC {
            Format: D3DFMT_R5G6B5,
            Width: 256,
            Height: 128,
            ..Default::default()
        };
        assert_eq!(surface_desc_9_to_8(&d9_16).Size, 256 * 128 * 2);

        let text_buffer = D3DSURFACE_DESC {
            Format: D3DFMT_A1R5G5B5,
            Width: 1024,
            Height: 64,
            ..Default::default()
        };
        assert_eq!(surface_desc_9_to_8(&text_buffer).Size, 1024 * 64 * 2);

        // We fall back for formats with nonlinear rows.
        let compressed = D3DSURFACE_DESC {
            Format: D3DFMT_DXT1,
            Width: 64,
            Height: 32,
            ..Default::default()
        };
        assert_eq!(surface_desc_9_to_8(&compressed).Size, 64 * 32 * 4);
    }
}
