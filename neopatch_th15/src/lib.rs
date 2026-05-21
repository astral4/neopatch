//! neopatch_th15: latency reductions, optimizations, and other fixes for Touhou 15.
//!
//! Shipped as `dinput8.dll` next to `th15.exe`.

#[cfg(all(not(panic = "abort"), not(test), not(doc)))]
compile_error!("neopatch_th15 requires `panic = \"abort\"`");

mod config;
mod dialog_dismiss;
mod patches;
mod state;

use crate::config::{self as th15_config, Th15Config};
use neopatch_core::config::{self as core_config, CoreConfig};
use neopatch_core::pacer::{PACER, Pacer, PacingPolicy};
use neopatch_core::{
    crash, d3d9, d3dx9, dinput8, dinput8_export, exit_hooks, gdi_caps, log, process, thread,
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
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

/// `0x0047158C` in the game is `FF 15 disp32` (6-byte indirect call to `Direct3DCreate9`).
/// We rewrite it to `E8 disp32 90` (5-byte direct call to our hook plus a trailing NOP).
/// This is defense for the IAT hook in case another program IAT-hooks
/// `Direct3DCreate9` after us, so the main call still lands on us.
///
/// A second `FF 15 [iat]` site exists at `0x00472e72` in what seems like
/// dead error-recovery code. We intentionally don't patch this site.
/// The IAT hook covers it for any live caller.
const TH15_DIRECT3DCREATE9_CALL_ADDR: usize = 0x0047_158c;
const TH15_DIRECT3DCREATE9_CALL_BYTES: [u8; 6] = [0xff, 0x15, 0xb0, 0xe2, 0x4b, 0x00];

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

        let (th15_cfg, core_cfg): (Th15Config, CoreConfig) = exe_dir
            .and_then(|d| read(d.join("neopatch.ini")).ok())
            .map_or_else(
                || (Th15Config::default(), CoreConfig::default()),
                |b| th15_config::parse(&core_config::decode_text(&b)),
            );
        // Set core first because `log::init` reads from it via `core_cfg_ref`.
        drop(core_config::CONFIG.set(core_cfg));
        drop(config::CONFIG.set(th15_cfg));
        let core_cfg_ref = core_config::CONFIG.get().unwrap();
        let th15_cfg_ref = config::CONFIG.get().unwrap();

        // Initialize logging first so the earliest install events are captured.
        // Minidumps land in `log::dump_dir`, the per-session directory next to `events.log`.
        let install_dir = exe_dir.map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        log::init(
            &install_dir,
            &core_cfg_ref.log,
            host_exe_path.as_deref(),
            |w| th15_config::write_manifest_extras(w, core_cfg_ref, th15_cfg_ref),
        );

        // `DllMain` runs on the `LoadLibrary` caller.
        // For a statically-imported DLL this is the process' main thread.
        // We do this before `watchdog::install` so the watchdog has the TID at startup.
        thread::set_main_id(GetCurrentThreadId());

        crash::install_handlers();
        // The watchdog only emits at INFO level anyway.
        if core_cfg_ref.log.level >= LevelFilter::INFO {
            watchdog::install();
        }

        // Important: IAT patches should operate on th15.exe's import table, not ours!
        // Passing our `hinst` would walk the wrong import directory
        // and silently no-op for symbols we don't import ourselves.
        let host_exe: HMODULE = GetModuleHandleW(null());

        process::apply(&core_cfg_ref.process);

        timer_period::install(host_exe);
        gdi_caps::install(host_exe);
        window::install(
            host_exe,
            &core_cfg_ref.window,
            th15_cfg_ref.resolution.dimensions(),
            core_cfg_ref.display.mode,
        );
        dialog_dismiss::install(host_exe);
        exit_hooks::install(host_exe);
        d3dx9::install(host_exe);

        // Wire the replay-mode probe before any `Present` can fire.
        d3d9::set_replay_mode_fn(state::replay_mode);

        // We do this before `d3d9::install` because that call
        // wires `Present` into `hook_present`, which unwraps `PACER.get()`.
        _ = PACER.set(Pacer::new(PacingPolicy::LiveInput {
            target_fps: core_cfg_ref.framerate.game_fps,
        }));

        d3d9::install(host_exe);
        // We rewrite a live `Direct3DCreate9` call site in th15 so a downstream IAT hijack
        // can't reroute the main call away from us.
        d3d9::install_call_site_rewrite(
            TH15_DIRECT3DCREATE9_CALL_ADDR,
            &TH15_DIRECT3DCREATE9_CALL_BYTES,
        );

        patches::apply_basic();
        patches::install_destructor_hook();
    }
}
