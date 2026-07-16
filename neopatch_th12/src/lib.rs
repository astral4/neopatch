//! neopatch_th12: neopatch for Touhou 12.

#[cfg(all(not(panic = "abort"), not(test), not(doc)))]
compile_error!("neopatch_th12 requires `panic = \"abort\"`");

mod dialog_dismiss;
mod patches;
mod state;

use neopatch_core::config::{CONFIG, CoreConfig, decode_text, parse_core_only};
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
use tracing::level_filters::LevelFilter;
use windows_sys::Win32::Foundation::{HINSTANCE, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{DisableThreadLibraryCalls, GetModuleHandleW};
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

// The render resolution is always 640x480.
const FRAMEBUFFER_SIZE: (u32, u32) = (640, 480);

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
        vtable::set_our_dll_handle(hinst as HMODULE);
        dinput8::init();
        install_hooks();
    }
    1
}

unsafe fn install_hooks() {
    unsafe {
        let host_exe_path = current_exe().ok();
        let exe_dir = host_exe_path.as_deref().and_then(Path::parent);

        let core_cfg = exe_dir
            .and_then(|d| read(d.join("neopatch.ini")).ok())
            .map_or_else(CoreConfig::default, |b| parse_core_only(&decode_text(&b)));
        drop(CONFIG.set(core_cfg));
        let core_cfg = CONFIG.get().unwrap();

        let install_dir = exe_dir.map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        log::init(&install_dir, core_cfg, host_exe_path.as_deref(), |_| Ok(()));

        crash::install_handlers();
        if core_cfg.log.level >= LevelFilter::INFO {
            watchdog::install();
        }

        let host_exe = GetModuleHandleW(null());

        process::apply(&core_cfg.process);

        timer_period::install(host_exe);
        gdi_caps::install(host_exe);
        window::install(
            host_exe,
            &core_cfg.window,
            window::WindowPolicy::Restyle {
                framebuffer: FRAMEBUFFER_SIZE,
                display_mode: core_cfg.display.mode,
            },
            window::WindowApi::Ansi,
        );
        dialog_dismiss::install(host_exe);
        exit_hooks::install(host_exe);
        d3dx9::install(host_exe);

        d3d9::set_replay_mode_fn(state::replay_mode);
        _ = PACER.set(Pacer::new(PacingPolicy::LiveInput {
            target_fps: core_cfg.framerate.game_fps,
        }));
        d3d9::install(host_exe);

        if core_cfg.input.dpad {
            input::install();
        }

        patches::install();
    }
}
