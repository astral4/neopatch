//! Patches and hooks for 東方紅魔郷.exe v1.02h.
//!
//! th06's frame loop (`GameWindow::Render` at `0x4206e0`) runs draw -> calc -> limiter -> `Present`, so the presented frame trails
//! the freshest input by a frame, and pacing is split between a `timeGetTime` software limiter (windowed / "force 60 fps") and vsync (fullscreen).
//! The patches here restructure it into calc -> draw -> `Present` with our pacer as the only timing source.

use crate::state::{record_input, replay_keys};
use neopatch_core::config::{CONFIG, DisplayMode};
use neopatch_core::d3d8::{
    D3DViewport8, call_begin_scene, call_clear, call_end_scene, call_set_texture,
    call_set_viewport, call_site_rewrite, set_pre_create_fn,
};
use neopatch_core::d3d9::active_back_buffer_format;
use neopatch_core::game_addr::{GameAddr, game_fn};
use neopatch_core::patches::PatchSite;
use neopatch_core::replay::set_probe;
use std::ffi::c_void;
use std::ptr::{null, null_mut, with_exposed_provenance_mut};
use std::sync::OnceLock;
use tracing::info;
use windows::Win32::Graphics::Direct3D9::{D3DCLEAR_TARGET, D3DCLEAR_ZBUFFER, D3DFORMAT};
use windows_sys::Win32::System::Threading::Sleep;

const RENDER_FN: usize = 0x0042_06e0;
const RUN_CALC_CHAIN_FN: usize = 0x0041_ca10;
const RUN_DRAW_CHAIN_FN: usize = 0x0041_cad0;
const WRITE_DATA_TO_FILE_FN: usize = 0x0041_e460;
const GET_INPUT_FN: usize = 0x0041_d820;
const INIT_D3D_RENDERING_FN: usize = 0x0042_0e60;

const G_CHAIN: usize = 0x0069_d918;
const G_GAME_WINDOW: usize = 0x006c_6bd4;
// This is nonzero while the app holds focus.
const APP_ACTIVE_VA: GameAddr<i32> = unsafe { GameAddr::new(G_GAME_WINDOW + 0x8) };
pub(crate) const G_SUPERVISOR: usize = 0x006c_6d18;
const G_SUPERVISOR_CFG: usize = G_SUPERVISOR + 0x114;
const D3D_DEVICE_VA: GameAddr<*mut c_void> = unsafe { GameAddr::new(G_SUPERVISOR + 0x8) };
const VIEWPORT_VA: GameAddr<D3DViewport8> = unsafe { GameAddr::new(G_SUPERVISOR + 0xc8) };
const BACK_BUFFER_FORMAT_VA: GameAddr<D3DFORMAT> =
    unsafe { GameAddr::new(G_SUPERVISOR + 0xe0 + 0x8) };
// 0 = 32-bit; 1 = 16-bit; 0xff = auto-detect.
const CFG_COLOR_MODE_16BIT_VA: GameAddr<u8> = unsafe { GameAddr::new(G_SUPERVISOR_CFG + 0x1a) };
const COLOR_MODE_AUTO: u8 = 0xff;
// 0 = fullscreen; 1 = windowed.
const CFG_WINDOWED_VA: GameAddr<u8> = unsafe { GameAddr::new(G_SUPERVISOR_CFG + 0x1e) };
const CFG_FRAMESKIP_VA: GameAddr<u8> = unsafe { GameAddr::new(G_SUPERVISOR_CFG + 0x1f) };
const CFG_OPTS_VA: GameAddr<u32> = unsafe { GameAddr::new(G_SUPERVISOR_CFG + 0x34) };
const GCOS_FORCE_16BIT_COLOR_MODE_MASK: u32 = 1 << 2;
const EFFECTIVE_FRAMERATE_MULTIPLIER_VA: GameAddr<f32> =
    unsafe { GameAddr::new(G_SUPERVISOR + 0x1a8) };
const FRAMERATE_MULTIPLIER_VA: GameAddr<f32> = unsafe { GameAddr::new(G_SUPERVISOR + 0x1ac) };
const SKY_FOG_COLOR_VA: GameAddr<u32> = unsafe { GameAddr::new(0x0048_7b60) };
const CLEAR_BACKBUFFER_OPTS_MASK: u32 = 0b11 << 3;

const DIRECT3DCREATE8_CALL_VA: usize = 0x0042_0bd5;
const DIRECT3DCREATE8_CALL_BYTES: [u8; 5] = [0xe8, 0xd2, 0xdf, 0x01, 0x00];

const DRAW_SKIP_VA: usize = 0x0042_071a;
const DRAW_SKIP_BYTES: [u8; 6] = [0x0f, 0x8f, 0xed, 0x00, 0x00, 0x00];
const DRAW_SKIP_TARGET: usize = 0x0042_080d;

const CALC_CALL_VA: usize = 0x0042_0853;
const CALC_CALL_BYTES: [u8; 5] = [0xe8, 0xb8, 0xc1, 0xff, 0xff];

const LIMITER_SKIP_VA: usize = 0x0042_08ed;
const LIMITER_SKIP_BYTES: [u8; 8] = [0x6a, 0x01, 0xff, 0x15, 0x50, 0xa2, 0x46, 0x00];
const LIMITER_SKIP_TARGET: usize = 0x0042_0990;

const RENDER_CALL_VA: usize = 0x0042_04ff;
const RENDER_CALL_BYTES: [u8; 5] = [0xe8, 0xdc, 0x01, 0x00, 0x00];

const CFG_WRITE_CALL_VA: usize = 0x0042_0661;
const CFG_WRITE_CALL_BYTES: [u8; 5] = [0xe8, 0xfa, 0xdd, 0xff, 0xff];

const GET_INPUT_CALL_VA: usize = 0x0042_3361;
const GET_INPUT_CALL_BYTES: [u8; 5] = [0xe8, 0xba, 0xa4, 0xff, 0xff];

const INIT_D3D_RENDERING_CALL_VA: usize = 0x0042_040d;
const INIT_D3D_RENDERING_CALL_BYTES: [u8; 5] = [0xe8, 0x4e, 0x0a, 0x00, 0x00];

const STANDALONE_PATCHES: &[PatchSite] = &[PatchSite::replace(
    0x0041_dc58,
    &[0x1e, 0x00, 0x07, 0x80, 0x75],
    &[0x00, 0x00, 0x00, 0x00, 0x74],
    "keyboard re-acquire on any input failure",
)];

const FRAME_LOOP_PATCHES: &[PatchSite] = &[
    call_site_rewrite(DIRECT3DCREATE8_CALL_VA, &DIRECT3DCREATE8_CALL_BYTES),
    PatchSite::jmp(
        DRAW_SKIP_VA,
        &DRAW_SKIP_BYTES,
        with_exposed_provenance_mut(DRAW_SKIP_TARGET),
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
    PatchSite::nop(
        0x0042_09a2,
        &[0x7d, 0x02],
        "frameskip catch-up loop skip (windowed)",
    ),
    PatchSite::replace(
        0x0042_09ff,
        &[0x7c, 0x0a],
        &[0xeb, 0x0a],
        "frameskip catch-up loop skip (fullscreen)",
    ),
];

type RunChainFn = unsafe extern "thiscall" fn(*mut c_void) -> i32;
type RenderFn = unsafe extern "thiscall" fn(*mut c_void) -> i32;
type WriteDataToFileFn = unsafe extern "C" fn(*const u8, *mut c_void, u32) -> i32;
type GetInputFn = unsafe extern "system" fn() -> u16;
type InitD3dRenderingFn = unsafe extern "C" fn() -> i32;

/// Replaces the `RunCalcChain` call inside `Render`.
unsafe extern "C" fn calc_then_draw_hook() -> i32 {
    // We write these values every frame since game code rewrites them. This keeps game speed 1:1 with presented frames.
    FRAMERATE_MULTIPLIER_VA.write(1.);
    EFFECTIVE_FRAMERATE_MULTIPLIER_VA.write(1.);

    let run_calc: RunChainFn = unsafe { game_fn(RUN_CALC_CHAIN_FN) };
    let res = unsafe { run_calc(with_exposed_provenance_mut(G_CHAIN)) };
    // 0 and -1 mean exit; the game tears down before another frame would be drawn.
    if res != 0 && res != -1 {
        unsafe { draw_frame() };
    }
    res
}

/// The original pre-calc draw block, relocated to run after calc.
unsafe fn draw_frame() {
    const CLEAR_FLAGS: u32 = (D3DCLEAR_TARGET | D3DCLEAR_ZBUFFER).cast_unsigned();

    let dev = D3D_DEVICE_VA.read();
    if dev.is_null() {
        return;
    }

    if CFG_OPTS_VA.read() & CLEAR_BACKBUFFER_OPTS_MASK != 0 {
        let full = D3DViewport8 {
            X: 0,
            Y: 0,
            Width: crate::FRAMEBUFFER_SIZE.0,
            Height: crate::FRAMEBUFFER_SIZE.1,
            MinZ: 0.,
            MaxZ: 1.,
        };
        unsafe {
            call_set_viewport(dev, &raw const full);
            // The flags match the original block.
            call_clear(dev, 0, null(), CLEAR_FLAGS, SKY_FOG_COLOR_VA.read(), 1., 0);
        }
        let saved = VIEWPORT_VA.read();
        unsafe { call_set_viewport(dev, &raw const saved) };
    }

    let run_draw: RunChainFn = unsafe { game_fn(RUN_DRAW_CHAIN_FN) };
    unsafe {
        call_begin_scene(dev);
        run_draw(with_exposed_provenance_mut(G_CHAIN));
        call_end_scene(dev);
        call_set_texture(dev, 0, null_mut());
    }
}

/// Replaces the `GetInput` call inside `Supervisor::OnUpdate`, forwarding to the real `Controller::GetInput`
/// and mirroring the returned bitmask into [`crate::state`] for the replay-speed probe.
unsafe extern "system" fn get_input_observer() -> u16 {
    let get_input: GetInputFn = unsafe { game_fn(GET_INPUT_FN) };
    let input = unsafe { get_input() };
    record_input(input);
    input
}

/// Replaces the `Render` call in `WinMain`.
unsafe extern "C" fn render_gate_hook() -> i32 {
    if APP_ACTIVE_VA.read() == 0 {
        // The original `Render` returns immediately while the app is inactive, so without `Sleep` the message loop busy-spins.
        unsafe { Sleep(16) };
    }
    let render: RenderFn = unsafe { game_fn(RENDER_FN) };
    unsafe { render(with_exposed_provenance_mut(G_GAME_WINDOW)) }
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

/// Replaces the `InitD3dRendering` call in `WinMain`, applying the cfg overrides the game reads inside that call,
/// then reconciling its cached present params with the device that actually came back.
unsafe extern "C" fn init_d3d_rendering_hook() -> i32 {
    let result = unsafe { init_d3d_rendering() };
    if result == 0 {
        reconcile_cached_back_buffer_format();
    }
    result
}

/// Runs the game's `InitD3dRendering` under the pinned cfg overrides, scoping the 32-bit color-depth override to the call
/// and re-asserting the pinned display mode.
unsafe fn init_d3d_rendering() -> i32 {
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

/// Points `g_Supervisor.presentParameters.BackBufferFormat` at the format the device actually got.
///
/// The game caches its present params in this field before creating the device,
/// so a format that we substitute during creation never reaches that copy otherwise.
fn reconcile_cached_back_buffer_format() {
    let Some(actual) = active_back_buffer_format() else {
        return;
    };
    let cached = BACK_BUFFER_FORMAT_VA.read();
    if cached != actual {
        info!(
            kind = "cached_back_buffer_format_reconciled",
            cached = cached.0,
            actual = actual.0,
        );
        BACK_BUFFER_FORMAT_VA.write(actual);
    }
}

/// Applies the pinned configuration overrides. This occurs at `Direct3DCreate8` time when `東方紅魔郷.cfg` is loaded
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

/// Replaces the `東方紅魔郷.cfg` write-back call in `WinMain`'s exit path.
unsafe extern "C" fn cfg_write_hook(path: *const u8, data: *mut c_void, size: u32) -> i32 {
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
        RENDER_CALL_VA,
        &RENDER_CALL_BYTES,
        render_gate_hook as *mut (),
        "render gate (inactive sleep)",
    ),
    PatchSite::call(
        CFG_WRITE_CALL_VA,
        &CFG_WRITE_CALL_BYTES,
        cfg_write_hook as *mut (),
        "config write-back restore",
    ),
    PatchSite::call(
        INIT_D3D_RENDERING_CALL_VA,
        &INIT_D3D_RENDERING_CALL_BYTES,
        init_d3d_rendering_hook as *mut (),
        "color depth scope (InitD3dRendering)",
    ),
    PatchSite::call(
        GET_INPUT_CALL_VA,
        &GET_INPUT_CALL_BYTES,
        get_input_observer as *mut (),
        "input observer (replay speed)",
    ),
];

pub(crate) const PATCH_GROUPS: &[&[PatchSite]] =
    &[STANDALONE_PATCHES, FRAME_LOOP_PATCHES, HOOK_PATCHES];

pub(crate) fn install() {
    set_pre_create_fn(apply_display_override);
    set_probe(replay_keys);
}
