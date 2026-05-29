//! neopatch_th16: latency reductions, optimizations, and other fixes for Touhou 16.
//!
//! Shipped as `dinput8.dll` next to `th16.exe`.

#[cfg(all(not(panic = "abort"), not(test), not(doc)))]
compile_error!("neopatch_th16 requires `panic = \"abort\"`");

mod config;
mod dialog_dismiss;
mod patches;
mod state;

use crate::config::{self as th16_config, Th16Config};
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
        // Cache the real `DirectInput8Create` before `install_hooks`
        // so the proxy export works even if hook installation fails.
        dinput8::init();
        install_hooks();
    }
    1
}

unsafe fn install_hooks() {
    unsafe {
        // If `current_exe` fails, the configuration path is `None`
        // and `install_dir` falls back to "." for the log root.
        let host_exe_path = current_exe().ok();
        let exe_dir = host_exe_path.as_deref().and_then(Path::parent);

        let (th16_cfg, core_cfg): (Th16Config, CoreConfig) = exe_dir
            .and_then(|d| read(d.join("neopatch.ini")).ok())
            .map_or_else(
                || (Th16Config::default(), CoreConfig::default()),
                |b| th16_config::parse(&core_config::decode_text(&b)),
            );
        drop(core_config::CONFIG.set(core_cfg));
        drop(config::CONFIG.set(th16_cfg));
        let core_cfg = core_config::CONFIG.get().unwrap();
        let th16_cfg = config::CONFIG.get().unwrap();

        // Initialize logging first so the earliest install events are captured.
        let install_dir = exe_dir.map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        log::init(&install_dir, core_cfg, host_exe_path.as_deref(), |w| {
            th16_config::write_manifest_extras(w, th16_cfg)
        });

        crash::install_handlers();
        // The watchdog only emits at INFO level anyway.
        if core_cfg.log.level >= LevelFilter::INFO {
            watchdog::install();
        }

        // Important: IAT patches operate on th16.exe's import table, not ours.
        let host_exe: HMODULE = GetModuleHandleW(null());

        process::apply(&core_cfg.process);

        timer_period::install(host_exe);
        gdi_caps::install(host_exe);
        window::install(
            host_exe,
            &core_cfg.window,
            th16_cfg.resolution.dimensions(),
            core_cfg.display.mode,
        );
        dialog_dismiss::install(host_exe);
        exit_hooks::install(host_exe);
        d3dx9::install(host_exe);

        // Wire the replay-mode probe before any `Present` can fire.
        d3d9::set_replay_mode_fn(state::replay_mode);

        // Construct the pacer before `d3d9::install`, whose `hook_present` unwraps `PACER.get()`.
        _ = PACER.set(Pacer::new(PacingPolicy::LiveInput {
            target_fps: core_cfg.framerate.game_fps,
        }));

        d3d9::install(host_exe);
        patches::install_d3d9_call_site_rewrite();

        // TODO(th16): audit th16's input-writer function for `rgdwPOV[]` reads before trusting
        // `dpad = on` (ARCHITECTURE's bring-up rule). Until then
        // the default is kept; a user whose stick stops while the D-pad is held sets `dpad = off`.
        if core_cfg.input.dpad {
            input::install();
        }
        patches::apply_basic();
        patches::install_destructor_hook();
        patches::install_loader_sync_hooks();
        patches::install_anm_matrix_tz_fix();
        patches::install_screenshot_hook();
    }
}
