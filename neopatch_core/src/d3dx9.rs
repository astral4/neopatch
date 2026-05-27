//! Hooks for `d3dx9_43.dll` texture-creation entries.
//!
//! d3dx9 validates `Pool` against the D3D9Ex device internally
//! before dispatching to `CreateTexture`, so the `MANAGED` -> `DEFAULT` translation
//! in our `CreateTexture` vtable hook never fires for the d3dx9 path.
//! We do the same translation at the d3dx9 entry points themselves.

use crate::d3d9::{fmt_hr, format_name, out_ptr, translate_managed_pool};
use crate::iat_hook;
use crate::log_cap::LogCap;
use std::ffi::c_void;
use std::num::NonZero;
use tracing::info;
use windows::Win32::Graphics::Direct3D9::{D3DFORMAT, D3DPOOL};
use windows::core::HRESULT;
use windows_sys::Win32::Foundation::HMODULE;

iat_hook! {
    REAL_D3DX_CREATE_TEX / real_d3dx_create_texture : "D3DXCreateTexture"
        as fn(
            device: *mut c_void,
            width: u32,
            height: u32,
            mip_levels: u32,
            usage: u32,
            format: D3DFORMAT,
            pool: D3DPOOL,
            pp_texture: *mut *mut c_void,
        ) -> HRESULT;
}

iat_hook! {
    REAL_D3DX_CREATE_TEX_FROM_FILE_IN_MEM_EX / real_d3dx_create_texture_from_file_in_memory_ex
        : "D3DXCreateTextureFromFileInMemoryEx"
        as fn(
            device: *mut c_void,
            src_data: *const c_void,
            src_data_size: u32,
            width: u32,
            height: u32,
            mip_levels: u32,
            usage: u32,
            format: D3DFORMAT,
            pool: D3DPOOL,
            filter: u32,
            mip_filter: u32,
            color_key: u32,
            src_info: *mut c_void,
            palette: *mut c_void,
            pp_texture: *mut *mut c_void,
        ) -> HRESULT;
}

static D3DX_CREATE_TEX_LOG: LogCap = LogCap::new(NonZero::new(32).unwrap());
static D3DX_CREATE_FROM_MEM_LOG: LogCap = LogCap::new(NonZero::new(32).unwrap());

/// IAT-hooks `D3DXCreateTexture` and `D3DXCreateTextureFromFileInMemoryEx`
/// against `host`'s import table for every `d3dx9_*.dll` version.
///
/// # Safety
/// `host` must be a loaded module handle.
pub unsafe fn install(host: HMODULE) {
    unsafe {
        REAL_D3DX_CREATE_TEX.install(host, hook_d3dx_create_texture);
        REAL_D3DX_CREATE_TEX_FROM_FILE_IN_MEM_EX
            .install(host, hook_d3dx_create_texture_from_file_in_memory_ex);
    }
}

unsafe extern "system" fn hook_d3dx_create_texture(
    device: *mut c_void,
    width: u32,
    height: u32,
    mip_levels: u32,
    mut usage: u32,
    format: D3DFORMAT,
    mut pool: D3DPOOL,
    pp_texture: *mut *mut c_void,
) -> HRESULT {
    unsafe {
        let pool_orig = pool;
        let usage_orig = usage;
        let translated = translate_managed_pool(&mut pool, &mut usage);

        let hr = real_d3dx_create_texture(
            device, width, height, mip_levels, usage, format, pool, pp_texture,
        );
        if let Some(n) = D3DX_CREATE_TEX_LOG.tick() {
            let returned = out_ptr(pp_texture);
            info!(
                kind = "d3dx_create_texture",
                n = n + 1,
                width,
                height,
                mip_levels,
                format = format_name(format),
                format_n = format.0,
                pool_in = pool_orig.0,
                pool_out = pool.0,
                usage_in = format_args!("{usage_orig:#x}"),
                usage_out = format_args!("{usage:#x}"),
                translated,
                hr = fmt_hr!(hr),
                ptr = format_args!("{returned:p}"),
            );
        }
        hr
    }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn hook_d3dx_create_texture_from_file_in_memory_ex(
    device: *mut c_void,
    src_data: *const c_void,
    src_data_size: u32,
    width: u32,
    height: u32,
    mip_levels: u32,
    mut usage: u32,
    format: D3DFORMAT,
    mut pool: D3DPOOL,
    filter: u32,
    mip_filter: u32,
    color_key: u32,
    src_info: *mut c_void,
    palette: *mut c_void,
    pp_texture: *mut *mut c_void,
) -> HRESULT {
    unsafe {
        let pool_orig = pool;
        let usage_orig = usage;
        let translated = translate_managed_pool(&mut pool, &mut usage);

        let hr = real_d3dx_create_texture_from_file_in_memory_ex(
            device,
            src_data,
            src_data_size,
            width,
            height,
            mip_levels,
            usage,
            format,
            pool,
            filter,
            mip_filter,
            color_key,
            src_info,
            palette,
            pp_texture,
        );
        if let Some(n) = D3DX_CREATE_FROM_MEM_LOG.tick() {
            let returned = out_ptr(pp_texture);
            info!(
                kind = "d3dx_create_texture_from_mem",
                n = n + 1,
                src = format_args!("{src_data:p}"),
                src_size = src_data_size,
                width,
                height,
                mip_levels,
                format = format_name(format),
                format_n = format.0,
                pool_in = pool_orig.0,
                pool_out = pool.0,
                usage_in = format_args!("{usage_orig:#x}"),
                usage_out = format_args!("{usage:#x}"),
                translated,
                hr = fmt_hr!(hr),
                ptr = format_args!("{returned:p}"),
            );
        }
        hr
    }
}
