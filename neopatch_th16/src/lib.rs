//! neopatch_th16: neopatch for Touhou 16.

#[cfg(all(not(panic = "abort"), not(test), not(doc)))]
compile_error!("neopatch_th16 requires `panic = \"abort\"`");

mod dialog_dismiss;
mod patches;
mod state;

use crate::dialog_dismiss::DIALOG_PATCHES;
use crate::patches::PATCHES;
use crate::state::replay_keys;
use neopatch_core::config::{CONFIG as CORE_CONFIG, ResolutionConfig, read_ini_text};
use neopatch_core::pacer::{PACER, Pacer, PacingPolicy};
use neopatch_core::patches::install_all;
use neopatch_core::{
    ansi, console, crash, d3d9, d3dx9, dinput8_export, exit_hooks, gdi_caps, input, log, process,
    replay, timer_period, vtable, window,
};
use std::env::current_exe;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::null;
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{HINSTANCE, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{DisableThreadLibraryCalls, GetModuleHandleW};
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

dinput8_export!();

/// Process-wide handle to the game-specific configuration.
pub(crate) static CONFIG: OnceLock<ResolutionConfig> = OnceLock::new();

#[unsafe(no_mangle)]
unsafe extern "system" fn DllMain(hinst: HINSTANCE, reason: u32, _reserved: *mut c_void) -> i32 {
    if reason != DLL_PROCESS_ATTACH {
        return 1;
    }
    unsafe { DisableThreadLibraryCalls(hinst as HMODULE) };
    vtable::set_our_dll_handle(hinst as HMODULE);
    unsafe { install_hooks() };
    1
}

unsafe fn install_hooks() {
    let host_exe_path = current_exe().ok();
    let exe_dir = host_exe_path.as_deref().and_then(Path::parent);

    let (game_cfg, core_cfg) = ResolutionConfig::parse(&read_ini_text(exe_dir));
    let core_cfg = CORE_CONFIG.get_or_init(|| core_cfg);
    let game_cfg = CONFIG.get_or_init(|| game_cfg);

    let install_dir = exe_dir.map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    log::init(&install_dir, core_cfg, host_exe_path.as_deref(), |w| {
        game_cfg.write_manifest_extras(w)
    });

    let installed = unsafe { install_all(&[PATCHES, DIALOG_PATCHES]) };
    if !installed {
        return;
    }

    crash::install_handlers();
    console::install();
    process::apply(&core_cfg.process);

    let host_exe = unsafe { GetModuleHandleW(null()) };

    unsafe {
        ansi::install(host_exe, ansi::CP_SHIFT_JIS);
        timer_period::install(host_exe);
        gdi_caps::install(host_exe);
        window::install(
            host_exe,
            &core_cfg.window,
            game_cfg.window_policy(core_cfg.display.mode),
            window::WindowApi::Ansi,
        );
        exit_hooks::install(host_exe);
        d3dx9::install(host_exe);
    }

    replay::set_probe(replay_keys);
    _ = PACER.set(Pacer::new(PacingPolicy::LiveInput {
        target_fps: core_cfg.framerate.game_fps,
    }));
    unsafe { d3d9::install(host_exe) };

    if core_cfg.input.dpad {
        input::install();
    }

    unsafe { dialog_dismiss::install(host_exe) };
}
