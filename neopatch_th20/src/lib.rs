//! neopatch_th20: latency reductions, optimizations, and other fixes for Touhou 20.
//!
//! Shipped as `dinput8.dll` next to `th20.exe`.
//!
//! Targets `th20.exe v1.00a`; the in-game title misreports "v1.00c" (copied from th19).

#[cfg(all(not(panic = "abort"), not(test), not(doc)))]
compile_error!("neopatch_th20 requires `panic = \"abort\"`");

mod aslr;
mod config;
mod dialog_dismiss;
mod patches;
mod state;

use crate::aslr::host_slide;
use crate::config::{self as th20_config, Th20Config, Th20DisplayMode};
use neopatch_core::config::{self as core_config, CoreConfig};
use neopatch_core::pacer::{PACER, Pacer, PacingPolicy};
use neopatch_core::{
    crash, d3d9, d3dx9, dinput8, dinput8_export, exit_hooks, gdi_caps, input, log, process,
    timer_period, vtable, watchdog, window,
};
use std::env::current_exe;
use std::ffi::c_void;
use std::fs::read;
use std::path::{Path, PathBuf};
use std::ptr::null;
use tracing::info;
use tracing::level_filters::LevelFilter;
use windows_sys::Win32::Foundation::{HINSTANCE, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{DisableThreadLibraryCalls, GetModuleHandleW};
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

dinput8_export!();

#[unsafe(no_mangle)]
#[allow(non_snake_case, clippy::missing_safety_doc)]
pub unsafe extern "system" fn DllMain(
    hinst: HINSTANCE,
    reason: u32,
    _reserved: *mut c_void,
) -> i32 {
    if reason != DLL_PROCESS_ATTACH {
        return 1;
    }
    unsafe {
        DisableThreadLibraryCalls(hinst as HMODULE);
        // Lets the vtable patcher distinguish "already our hook" (idempotent re-entry)
        // from a shim-layer chain like `apphelp.dll`'s `CreateDevice` hijack.
        vtable::set_our_dll_handle(hinst as HMODULE);
        dinput8::init();
        // Cache the real `DirectInput8Create` before `install_hooks`
        // so the proxy export works even if hook installation fails.
        install_hooks();
    }
    1
}

unsafe fn install_hooks() {
    unsafe {
        let host_exe_path = current_exe().ok();
        let exe_dir = host_exe_path.as_deref().and_then(Path::parent);

        let (th20_cfg, core_cfg) = exe_dir
            .and_then(|d| read(d.join("neopatch.ini")).ok())
            .map_or_else(
                || (Th20Config::default(), CoreConfig::default()),
                |b| th20_config::parse(&core_config::decode_text(&b)),
            );
        drop(core_config::CONFIG.set(core_cfg));
        drop(config::CONFIG.set(th20_cfg));
        let core_cfg = core_config::CONFIG.get().unwrap();
        let th20_cfg = config::CONFIG.get().unwrap();

        let install_dir = exe_dir.map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        log::init(&install_dir, core_cfg, host_exe_path.as_deref(), |w| {
            th20_config::write_manifest_extras(w, th20_cfg)
        });

        crash::install_handlers();
        if core_cfg.log.level >= LevelFilter::INFO {
            watchdog::install();
        }

        let host_exe = GetModuleHandleW(null());
        let slide = host_slide(host_exe);
        info!(
            kind = "aslr_slide",
            host_base = format_args!("{:#010x}", host_exe as usize),
            preferred_base = format_args!("{:#010x}", aslr::PREFERRED_IMAGE_BASE),
            slide = format_args!("{slide:#010x}"),
        );

        process::apply(&core_cfg.process);

        timer_period::install(host_exe);
        gdi_caps::install(host_exe);
        window::install(
            host_exe,
            &core_cfg.window,
            match th20_cfg.display_mode {
                Th20DisplayMode::Borderless => window::WindowPolicy::DeferToGame,
                Th20DisplayMode::Windowed | Th20DisplayMode::Fullscreen => {
                    window::WindowPolicy::Restyle {
                        framebuffer: th20_cfg.resolution.dimensions(),
                        display_mode: core_cfg.display.mode,
                    }
                }
            },
            window::WindowApi::Wide,
        );
        dialog_dismiss::install(slide);
        exit_hooks::install(host_exe);
        d3dx9::install(host_exe);

        // Wire the replay-mode probe before any `Present` can fire.
        state::install(slide);

        _ = PACER.set(Pacer::new(PacingPolicy::LiveInput {
            target_fps: core_cfg.framerate.game_fps,
        }));

        d3d9::install(host_exe);
        patches::install_d3d9_call_site_rewrite(slide);

        // th20's input writer at `fcn.00421b00` does not read `DIJOYSTATE2` offsets
        // 0x20-0x30 (`rgdwPOV[]`), so the default of `dpad = true` is safe.
        if core_cfg.input.dpad {
            input::install();
        }
        patches::apply_basic(slide);
        patches::install_screenshot_hook(slide);
    }
}
