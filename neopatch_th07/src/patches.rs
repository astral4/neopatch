//! Patches and hooks for th07.exe v1.00b.
//!
//! th07's frame loop (`CWindowManager::Update` at `0x4346e0`) runs draw -> calc -> limiter -> `Present`, so the presented frame trails
//! the freshest input by a frame, and pacing is split between a QPC/`timeGetTime` software limiter (windowed) and vsync (fullscreen).
//! The patches here restructure it into calc -> draw -> `Present` with our pacer as the only timing source.

use crate::state::{record_input, replay_keys};
use neopatch_core::config::{CONFIG, DisplayMode};
use neopatch_core::d3d8::{
    call_begin_scene, call_end_scene, call_set_texture, call_site_rewrite, set_pre_create_fn,
};
use neopatch_core::game_addr::{GameAddr, game_fn};
use neopatch_core::patches::PatchSite;
use neopatch_core::replay::set_probe;
use std::ffi::c_void;
use std::ptr::{null_mut, with_exposed_provenance_mut};
use std::sync::OnceLock;
use tracing::info;
use windows_sys::Win32::System::Threading::Sleep;

const WINDOW_UPDATE_FN: usize = 0x0043_46e0;
const RUN_CALC_CHAIN_FN: usize = 0x0042_fd60;
const RUN_DRAW_CHAIN_FN: usize = 0x0042_fe20;
const UPDATE_FOG_FN: usize = 0x0043_a207;
const TEXTURE_BEGIN_FN: usize = 0x0044_f580;
const TEXTURE_FLUSH_FN: usize = 0x0044_f5c0;
const INIT_D3D_RENDERING_FN: usize = 0x0043_4bd0;
const WRITE_DATA_TO_FILE_FN: usize = 0x0043_1540;
const GET_INPUT_FN: usize = 0x0043_0b50;

pub(crate) const G_ENGINE: usize = 0x0057_5950;
const D3D_DEVICE_VA: GameAddr<*mut c_void> = unsafe { GameAddr::new(G_ENGINE + 0x8) };
/// The original pre-calc draw block sets this to `0xff` right before `UpdateFog`, which then disables fog
/// via `SetRenderState` and clears the field.
const FOG_RESET_REQUEST_VA: GameAddr<u32> = unsafe { GameAddr::new(G_ENGINE + 0x2bc) };
const G_ENGINE_CFG: usize = G_ENGINE + 0x118;
// 0 = 32-bit; 1 = 16-bit; 0xff = auto-detect.
const CFG_COLOR_MODE_16BIT_VA: GameAddr<u8> = unsafe { GameAddr::new(G_ENGINE_CFG + 0x1e) };
const COLOR_MODE_AUTO: u8 = 0xff;
// 0 = fullscreen; 1 = windowed.
const CFG_WINDOWED_VA: GameAddr<u8> = unsafe { GameAddr::new(G_ENGINE_CFG + 0x22) };
const CFG_FRAMESKIP_VA: GameAddr<u8> = unsafe { GameAddr::new(G_ENGINE_CFG + 0x23) };
const CFG_OPTS_VA: GameAddr<u32> = unsafe { GameAddr::new(G_ENGINE_CFG + 0x34) };
const GCOS_FORCE_16BIT_COLOR_MODE_MASK: u32 = 1 << 2;

const G_WINDOW_MANAGER: usize = 0x0057_5c20;
// This is nonzero while the app holds focus.
const APP_ACTIVE_VA: GameAddr<i32> = unsafe { GameAddr::new(G_WINDOW_MANAGER + 0x8) };
const G_CHAIN_MANAGER: usize = 0x0062_6218;
const TEXTURE_MANAGER_VA: GameAddr<*mut c_void> = unsafe { GameAddr::new(0x004b_9e44) };

const DIRECT3DCREATE8_CALL_VA: usize = 0x0043_4a45;
const DIRECT3DCREATE8_CALL_BYTES: [u8; 5] = [0xe8, 0x52, 0xd0, 0x02, 0x00];

const DRAW_SKIP_VA: usize = 0x0043_4718;
const DRAW_SKIP_BYTES: [u8; 2] = [0x7f, 0x74];

const CALC_CALL_VA: usize = 0x0043_47df;
const CALC_CALL_BYTES: [u8; 5] = [0xe8, 0x7c, 0xb5, 0xff, 0xff];

const LIMITER_SKIP_VA: usize = 0x0043_4820;
const LIMITER_SKIP_BYTES: [u8; 7] = [0x0f, 0xb6, 0x05, 0x8a, 0x5a, 0x57, 0x00];
const LIMITER_SKIP_TARGET: usize = 0x0043_4a18;

const WINDOW_UPDATE_CALL_VA: usize = 0x0043_41f7;
const WINDOW_UPDATE_CALL_BYTES: [u8; 5] = [0xe8, 0xe4, 0x04, 0x00, 0x00];

const INIT_D3D_RENDERING_CALL_VA: usize = 0x0043_40eb;
const INIT_D3D_RENDERING_CALL_BYTES: [u8; 5] = [0xe8, 0xe0, 0x0a, 0x00, 0x00];

const CFG_WRITE_CALL_VA: usize = 0x0043_4433;
const CFG_WRITE_CALL_BYTES: [u8; 5] = [0xe8, 0x08, 0xd1, 0xff, 0xff];

const GET_INPUT_CALL_LIVE_VA: usize = 0x0043_7d80;
const GET_INPUT_CALL_LIVE_BYTES: [u8; 5] = [0xe8, 0xcb, 0x8d, 0xff, 0xff];
const GET_INPUT_CALL_REPLAY_VA: usize = 0x0043_7dfc;
const GET_INPUT_CALL_REPLAY_BYTES: [u8; 5] = [0xe8, 0x4f, 0x8d, 0xff, 0xff];

const STANDALONE_PATCHES: &[PatchSite] = &[PatchSite::replace(
    0x0043_0f03,
    &[0x1e, 0x00, 0x07, 0x80, 0x75],
    &[0x00, 0x00, 0x00, 0x00, 0x74],
    "keyboard re-acquire on any input failure",
)];

const FRAME_LOOP_PATCHES: &[PatchSite] = &[
    call_site_rewrite(DIRECT3DCREATE8_CALL_VA, &DIRECT3DCREATE8_CALL_BYTES),
    PatchSite::replace(
        DRAW_SKIP_VA,
        &DRAW_SKIP_BYTES,
        &[0xeb, 0x74],
        "draw-before-calc skip",
    ),
    PatchSite::call(
        CALC_CALL_VA,
        &CALC_CALL_BYTES,
        calc_then_draw_hook as *mut (),
        "calc-then-draw reorder",
    ),
    PatchSite::jmp(
        LIMITER_SKIP_VA,
        &LIMITER_SKIP_BYTES,
        with_exposed_provenance_mut(LIMITER_SKIP_TARGET),
        "software frame limiter skip",
    ),
];

type RunChainFn = unsafe extern "thiscall" fn(*mut c_void) -> i32;
type TextureBatchFn = unsafe extern "thiscall" fn(*mut c_void);
type UpdateFogFn = unsafe extern "thiscall" fn(*mut c_void) -> i32;
type WindowUpdateFn = unsafe extern "thiscall" fn(*mut c_void) -> i32;
type InitD3dRenderingFn = unsafe extern "C" fn() -> i32;
type WriteDataToFileFn = unsafe extern "fastcall" fn(*const u8, *mut c_void, u32) -> i32;
type GetInputFn = unsafe extern "system" fn() -> u16;

/// Replaces the `CChainManager::UpdateCalcChain` call inside `CWindowManager::Update`.
unsafe extern "C" fn calc_then_draw_hook() -> i32 {
    let run_calc: RunChainFn = unsafe { game_fn(RUN_CALC_CHAIN_FN) };
    let res = unsafe { run_calc(with_exposed_provenance_mut(G_CHAIN_MANAGER)) };
    // 0 and -1 mean exit or display-mode restart; the game tears down before another frame would be drawn.
    if res != 0 && res != -1 {
        unsafe { draw_frame() };
    }
    res
}

/// The original pre-calc draw block, relocated to run after calc.
unsafe fn draw_frame() {
    let dev = D3D_DEVICE_VA.read();
    let tex_mgr = TEXTURE_MANAGER_VA.read();
    if dev.is_null() || tex_mgr.is_null() {
        return;
    }

    let texture_begin: TextureBatchFn = unsafe { game_fn(TEXTURE_BEGIN_FN) };
    let texture_flush: TextureBatchFn = unsafe { game_fn(TEXTURE_FLUSH_FN) };
    let update_fog: UpdateFogFn = unsafe { game_fn(UPDATE_FOG_FN) };
    let run_draw: RunChainFn = unsafe { game_fn(RUN_DRAW_CHAIN_FN) };
    unsafe {
        call_begin_scene(dev);
        texture_begin(tex_mgr);
        FOG_RESET_REQUEST_VA.write(0xff);
        update_fog(with_exposed_provenance_mut(G_ENGINE));
        run_draw(with_exposed_provenance_mut(G_CHAIN_MANAGER));
        texture_flush(tex_mgr);
        call_set_texture(dev, 0, null_mut());
        call_end_scene(dev);
    }
}

/// Replaces both `GetInput` calls inside `CEngine::OnUpdate`, forwarding to the real function and mirroring
/// the returned raw hardware bitmask into [`crate::state`] for the replay-speed probe.
unsafe extern "system" fn get_input_observer() -> u16 {
    let get_input: GetInputFn = unsafe { game_fn(GET_INPUT_FN) };
    let input = unsafe { get_input() };
    record_input(input);
    input
}

/// Replaces the `CWindowManager::Update` call in `WinMain`.
unsafe extern "C" fn window_update_gate_hook() -> i32 {
    if APP_ACTIVE_VA.read() == 0 {
        // The original `Update` returns immediately while the app is inactive, so without `Sleep` the message loop busy-spins.
        unsafe { Sleep(16) };
    }
    let update: WindowUpdateFn = unsafe { game_fn(WINDOW_UPDATE_FN) };
    unsafe { update(with_exposed_provenance_mut(G_WINDOW_MANAGER)) }
}

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

/// Applies the pinned configuration overrides. This occurs at `Direct3DCreate8` time when `th07.cfg` is loaded
/// but no window or device exists yet.
fn apply_display_override() {
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

/// Replaces the `th07.cfg` write-back call in `WinMain`'s exit path.
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
        WINDOW_UPDATE_CALL_VA,
        &WINDOW_UPDATE_CALL_BYTES,
        window_update_gate_hook as *mut (),
        "window-update gate (inactive sleep)",
    ),
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
    PatchSite::call(
        GET_INPUT_CALL_LIVE_VA,
        &GET_INPUT_CALL_LIVE_BYTES,
        get_input_observer as *mut (),
        "input observer (live)",
    ),
    PatchSite::call(
        GET_INPUT_CALL_REPLAY_VA,
        &GET_INPUT_CALL_REPLAY_BYTES,
        get_input_observer as *mut (),
        "input observer (replay)",
    ),
];

pub(crate) const PATCH_GROUPS: &[&[PatchSite]] =
    &[STANDALONE_PATCHES, FRAME_LOOP_PATCHES, HOOK_PATCHES];

pub(crate) fn install() {
    set_pre_create_fn(apply_display_override);
    set_probe(replay_keys);
}
