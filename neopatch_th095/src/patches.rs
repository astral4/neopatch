//! Patches and hooks for th095.exe v1.02a.

use crate::state::replay_keys;
use neopatch_core::config::{CONFIG, DisplayMode};
use neopatch_core::d3d8::{call_site_rewrite, set_pre_create_fn};
use neopatch_core::game_addr::{GameAddr, game_fn};
use neopatch_core::patches::PatchSite;
use neopatch_core::replay::set_probe;
use std::ffi::c_void;
use std::ptr::with_exposed_provenance_mut;
use std::sync::OnceLock;
use tracing::info;

const INIT_D3D_RENDERING_FN: usize = 0x0042_0e20;
const WRITE_DATA_TO_FILE_FN: usize = 0x0041_ac50;

const G_CFG: usize = 0x004c_478c;
// 0 = 32-bit; 1 = 16-bit; 0xff = auto-detect.
const CFG_COLOR_MODE_16BIT_VA: GameAddr<u8> = unsafe { GameAddr::new(G_CFG + 0xac) };
const COLOR_MODE_AUTO: u8 = 0xff;
// 0 = fullscreen; 1 = windowed.
const CFG_WINDOWED_VA: GameAddr<u8> = unsafe { GameAddr::new(G_CFG + 0xaf) };
const CFG_FRAMESKIP_VA: GameAddr<u8> = unsafe { GameAddr::new(G_CFG + 0xb0) };
const CFG_OPTS_VA: GameAddr<u32> = unsafe { GameAddr::new(G_CFG + 0xc4) };
const GCOS_FORCE_16BIT_COLOR_MODE_MASK: u32 = 1 << 0;

const DIRECT3DCREATE8_CALL_VA: usize = 0x0042_0b58;
const DIRECT3DCREATE8_CALL_BYTES: [u8; 5] = [0xe8, 0x7f, 0x78, 0x03, 0x00];

const LIMITER_SKIP_VA: usize = 0x0042_07c3;
const LIMITER_SKIP_BYTES: [u8; 6] = [0x0f, 0x8a, 0x3d, 0x01, 0x00, 0x00];
const LIMITER_SKIP_TARGET: usize = 0x0042_07f0;

const INIT_D3D_RENDERING_CALL_VA: usize = 0x0042_033e;
const INIT_D3D_RENDERING_CALL_BYTES: [u8; 5] = [0xe8, 0xdd, 0x0a, 0x00, 0x00];

const CFG_WRITE_CALL_VA: usize = 0x0042_06ad;
const CFG_WRITE_CALL_BYTES: [u8; 5] = [0xe8, 0x9e, 0xa5, 0xff, 0xff];

const FRAME_LOOP_PATCHES: &[PatchSite] = &[
    call_site_rewrite(DIRECT3DCREATE8_CALL_VA, &DIRECT3DCREATE8_CALL_BYTES),
    PatchSite::jmp(
        LIMITER_SKIP_VA,
        &LIMITER_SKIP_BYTES,
        with_exposed_provenance_mut(LIMITER_SKIP_TARGET),
        "software frame limiter skip",
    ),
];

type InitD3dRenderingFn = unsafe extern "C" fn() -> i32;
type WriteDataToFileFn = unsafe extern "fastcall" fn(*const u8, *mut c_void, u32) -> i32;

/// The cfg bytes that neopatch overrides for the whole session.
#[derive(Clone, Copy)]
struct PinnedCfg {
    /// The user's original `cfg.windowed`.
    user_windowed: u8,
    /// The windowed value forced by neopatch.
    forced_windowed: u8,
    /// Whether the user's original `cfg.opts` had `GCOS_FORCE_16BIT_COLOR_MODE` set.
    /// neopatch always forces the bit clear, so the forced value is implicitly `false`.
    user_force_16bit: bool,
    /// The user's original `cfg.frameskip`. neopatch always forces 0.
    user_frameskip: u8,
}

static PINNED_CFG: OnceLock<PinnedCfg> = OnceLock::new();

/// Runs `f` with `cfg.colorMode16bit` forced to 32-bit, handing the player's value back afterwards.
fn with_32bit_color<R>(_pinned: PinnedCfg, f: impl FnOnce() -> R) -> R {
    let user_choice = CFG_COLOR_MODE_16BIT_VA.read();
    if user_choice != 0 {
        info!(kind = "color_mode_forced_32bit", user_choice);
        CFG_COLOR_MODE_16BIT_VA.write(0);
    }
    let result = f();
    CFG_COLOR_MODE_16BIT_VA.write(user_choice);
    result
}

/// Replaces the `InitD3dRendering` call in `WinMain`, running it under the pinned cfg overrides.
/// The 32-bit color-depth override is scoped to the call, and the pinned display mode is re-asserted.
unsafe extern "C" fn init_d3d_rendering_hook() -> i32 {
    let init: InitD3dRenderingFn = unsafe { game_fn(INIT_D3D_RENDERING_FN) };
    let Some(pinned) = PINNED_CFG.get() else {
        return unsafe { init() };
    };

    if CFG_WINDOWED_VA.read() != pinned.forced_windowed {
        info!(
            kind = "display_mode_reasserted",
            cfg_windowed = CFG_WINDOWED_VA.read(),
            forced = pinned.forced_windowed,
        );
        CFG_WINDOWED_VA.write(pinned.forced_windowed);
    }

    with_32bit_color(*pinned, || unsafe { init() })
}

/// Applies the pinned configuration overrides. This occurs at `Direct3DCreate8` time when `th095.cfg` is loaded
/// but no window or device exists yet.
pub(crate) fn apply_display_override() {
    let user_windowed = CFG_WINDOWED_VA.read();

    let opts = CFG_OPTS_VA.read();
    let user_force_16bit = opts & GCOS_FORCE_16BIT_COLOR_MODE_MASK != 0;

    let user_frameskip = CFG_FRAMESKIP_VA.read();

    let forced_windowed = match CONFIG.get().map(|c| c.display.mode) {
        Some(DisplayMode::Fullscreen) => 0,
        // The default display mode in the neopatch configuration is windowed.
        Some(DisplayMode::Windowed) | None => 1,
    };

    let _ = PINNED_CFG.set(PinnedCfg {
        user_windowed,
        forced_windowed,
        user_force_16bit,
        user_frameskip,
    });

    if user_windowed != forced_windowed {
        info!(
            kind = "display_mode_override",
            cfg_windowed = user_windowed,
            forced = forced_windowed,
        );
        CFG_WINDOWED_VA.write(forced_windowed);
    }

    if user_force_16bit {
        info!(kind = "force_16bit_opt_cleared");
        CFG_OPTS_VA.write(opts & !GCOS_FORCE_16BIT_COLOR_MODE_MASK);
    }

    if user_frameskip != 0 {
        info!(kind = "frameskip_forced_off", user_frameskip);
        CFG_FRAMESKIP_VA.write(0);
    }

    resolve_color_mode_sentinel();
}

/// Resolves `cfg.colorMode16bit`'s `0xff` "auto-detect" sentinel to 32-bit.
fn resolve_color_mode_sentinel() {
    if CFG_COLOR_MODE_16BIT_VA.read() == COLOR_MODE_AUTO {
        info!(kind = "color_mode_sentinel_resolved");
        CFG_COLOR_MODE_16BIT_VA.write(0);
    }
}

/// Replaces the `th095.cfg` write-back call in `WinMain`'s exit path.
unsafe extern "fastcall" fn cfg_write_hook(path: *const u8, data: *mut c_void, size: u32) -> i32 {
    if let Some(pinned) = PINNED_CFG.get() {
        // We only restore where the game still holds what we forced, so anything it changed since then is preserved.
        if CFG_WINDOWED_VA.read() == pinned.forced_windowed {
            CFG_WINDOWED_VA.write(pinned.user_windowed);
        }

        let opts = CFG_OPTS_VA.read();
        if pinned.user_force_16bit && opts & GCOS_FORCE_16BIT_COLOR_MODE_MASK == 0 {
            CFG_OPTS_VA.write(opts | GCOS_FORCE_16BIT_COLOR_MODE_MASK);
        }

        if CFG_FRAMESKIP_VA.read() == 0 {
            CFG_FRAMESKIP_VA.write(pinned.user_frameskip);
        }
    }

    let write_data_to_file: WriteDataToFileFn = unsafe { game_fn(WRITE_DATA_TO_FILE_FN) };
    unsafe { write_data_to_file(path, data, size) }
}

const HOOK_PATCHES: &[PatchSite] = &[
    PatchSite::call(
        INIT_D3D_RENDERING_CALL_VA,
        &INIT_D3D_RENDERING_CALL_BYTES,
        init_d3d_rendering_hook as *mut (),
        "display and color depth scope (InitD3dRendering)",
    ),
    PatchSite::call(
        CFG_WRITE_CALL_VA,
        &CFG_WRITE_CALL_BYTES,
        cfg_write_hook as *mut (),
        "config write-back restore",
    ),
];

pub(crate) const PATCH_GROUPS: &[&[PatchSite]] = &[FRAME_LOOP_PATCHES, HOOK_PATCHES];

pub(crate) fn install() {
    set_pre_create_fn(apply_display_override);
    set_probe(replay_keys);
}
