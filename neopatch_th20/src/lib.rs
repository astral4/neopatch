//! neopatch_th20: neopatch for Touhou 20.

#[cfg(all(not(panic = "abort"), not(test), not(doc)))]
compile_error!("neopatch_th20 requires `panic = \"abort\"`");

mod aslr;
mod config;
mod dialog_dismiss;
mod patches;
mod state;

use crate::aslr::{PREFERRED_IMAGE_BASE, host_slide};
use crate::config::{CONFIG, Th20Config, Th20DisplayMode, parse_config, write_manifest_extras};
use crate::dialog_dismiss::sites as dialog_sites;
use crate::patches::sites;
use neopatch_core::config::{CONFIG as CORE_CONFIG, CoreConfig, decode_text};
use neopatch_core::pacer::{PACER, Pacer, PacingPolicy};
use neopatch_core::patches::install_all;
use neopatch_core::{
    crash, d3d9, d3dx9, dinput8, dinput8_export, exit_hooks, gdi_caps, input, log, process,
    timer_period, vtable, window,
};
use std::env::current_exe;
use std::ffi::c_void;
use std::fs::read;
use std::path::{Path, PathBuf};
use std::ptr::null;
use tracing::info;
use windows_sys::Win32::Foundation::{HINSTANCE, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{DisableThreadLibraryCalls, GetModuleHandleW};
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

dinput8_export!();

#[unsafe(no_mangle)]
unsafe extern "system" fn DllMain(hinst: HINSTANCE, reason: u32, _reserved: *mut c_void) -> i32 {
    if reason != DLL_PROCESS_ATTACH {
        return 1;
    }
    unsafe { DisableThreadLibraryCalls(hinst as HMODULE) };
    vtable::set_our_dll_handle(hinst as HMODULE);
    dinput8::init();
    unsafe { install_hooks() };
    1
}

unsafe fn install_hooks() {
    let host_exe_path = current_exe().ok();
    let exe_dir = host_exe_path.as_deref().and_then(Path::parent);

    let (th20_cfg, core_cfg) = exe_dir
        .and_then(|d| read(d.join("neopatch.ini")).ok())
        .map_or_else(
            || (Th20Config::default(), CoreConfig::default()),
            |b| parse_config(&decode_text(&b)),
        );
    drop(CORE_CONFIG.set(core_cfg));
    drop(CONFIG.set(th20_cfg));
    let core_cfg = CORE_CONFIG.get().unwrap();
    let th20_cfg = CONFIG.get().unwrap();

    let install_dir = exe_dir.map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    log::init(&install_dir, core_cfg, host_exe_path.as_deref(), |w| {
        write_manifest_extras(w, th20_cfg)
    });

    let host_exe = unsafe { GetModuleHandleW(null()) };
    let slide = host_slide(host_exe);
    info!(
        kind = "aslr_slide",
        host_base = format_args!("{:#010x}", host_exe.addr()),
        preferred_base = format_args!("{:#010x}", PREFERRED_IMAGE_BASE),
        slide = format_args!("{slide:#010x}"),
    );

    let installed = unsafe { install_all(&[&sites(slide), &dialog_sites(slide)]) };
    if !installed {
        return;
    }

    crash::install_handlers();

    process::apply(&core_cfg.process);

    unsafe {
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
        exit_hooks::install(host_exe);
        d3dx9::install(host_exe);
    }

    state::install(slide);
    _ = PACER.set(Pacer::new(PacingPolicy::LiveInput {
        target_fps: core_cfg.framerate.game_fps,
    }));
    unsafe { d3d9::install(host_exe) };

    if core_cfg.input.dpad {
        input::install();
    }
}
