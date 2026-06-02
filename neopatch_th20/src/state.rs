//! Direct reads of game state for th20.exe v1.00a.
//!
//! Replay speed control polls the configured focus key (Slow) and shoot key or `VK_CONTROL` (Skip)
//! via `GetAsyncKeyState`. The VKs come from the in-memory `th20.cfg` key table at `0x5c4f30`
//! ([shoot, bomb, focus, _, up, down, left, right]); the vanilla pacer `fcn.005081e0` fast-forwards
//! on the shoot bit.
//!
//! We poll the VKs rather than the per-frame input bitfield at `*0x5b889c` because of Wine/X11
//! weirdness. We also add Ctrl for replay skip because Ctrl is treated as a modifier, so holding
//! Ctrl translates to a continuous input signal where non-modifier keys would translate to
//! many discrete inputs per second and inconsistent speed control.

use crate::aslr::rebased_addr;
use neopatch_core::MainToken;
use neopatch_core::d3d9::{ReplayMode, set_replay_mode_fn};
use std::ptr::{read_volatile, with_exposed_provenance};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_SHIFT};

const MANAGER_PTR_VA: usize = 0x005c_60fc;
const MANAGER_MODE_OFFSET: usize = 0x10;
const VIEWER_MODE: i32 = 1;

// Rebindable keyboard VK table at `0x5c4f30` (8 u16 VKs: [shoot, bomb, focus, _, up, down, left, right]).
// We read shoot (index 0) and focus (index 2).
const SHOOT_VK_VA: usize = 0x005c_4f30;
const FOCUS_VK_VA: usize = 0x005c_4f34;

// Defaults if a config slot reads 0 (config unbound or not loaded).
const DEFAULT_SHOOT_VK: u16 = 0x5a; // 'Z'
const DEFAULT_FOCUS_VK: u16 = VK_SHIFT;

// The slide is a multiple of 0x10000, so it preserves the alignment of these offsets.
const _: () = {
    assert!(MANAGER_PTR_VA.is_multiple_of(4));
    assert!(MANAGER_MODE_OFFSET.is_multiple_of(4));
    assert!(SHOOT_VK_VA.is_multiple_of(2));
    assert!(FOCUS_VK_VA.is_multiple_of(2));
};

static SLIDE: OnceLock<usize> = OnceLock::new();

pub(crate) fn install(slide: usize) {
    _ = SLIDE.set(slide);
    set_replay_mode_fn(replay_mode);
}

fn replay_mode(_tok: &MainToken) -> ReplayMode {
    let slide = *SLIDE
        .get()
        .expect("state::install must run before the first replay_mode call");
    let mgr: *const u8 = unsafe { rebased_addr::<*const u8>(slide, MANAGER_PTR_VA) }.read();
    if mgr.is_null() {
        return ReplayMode::Normal;
    }
    // The mode field is at a pointer-derived address (`*mgr + offset`), so it stays a raw read.
    let mode_addr = mgr.addr().wrapping_add(MANAGER_MODE_OFFSET);
    let mode: i32 = unsafe { read_volatile(with_exposed_provenance::<i32>(mode_addr)) };
    if mode != VIEWER_MODE {
        return ReplayMode::Normal;
    }
    let (slow, skip) = poll_modifiers(slide);
    if slow {
        return ReplayMode::Slow;
    }
    if skip {
        return ReplayMode::Skip;
    }
    ReplayMode::Normal
}

// We throttle the `GetAsyncKeyState` poll interval because it races the game's own keyboard
// input delivery under Wine and can drop its input events.
const POLL_THROTTLE_MS: i64 = 25;
static LAST_POLL_QPC: AtomicI64 = AtomicI64::new(0);
static CACHED_SLOW: AtomicBool = AtomicBool::new(false);
static CACHED_SKIP: AtomicBool = AtomicBool::new(false);
static QPC_FREQ: AtomicI64 = AtomicI64::new(0);

fn poll_modifiers(slide: usize) -> (bool, bool) {
    let now = read_qpc();
    let last = LAST_POLL_QPC.load(Ordering::Relaxed);
    let freq = qpc_freq();
    if last != 0 && freq != 0 && now >= last && (now - last) * 1_000 / freq < POLL_THROTTLE_MS {
        return (
            CACHED_SLOW.load(Ordering::Relaxed),
            CACHED_SKIP.load(Ordering::Relaxed),
        );
    }
    let slow = key_held(configured_vk(slide, FOCUS_VK_VA, DEFAULT_FOCUS_VK));
    let skip =
        key_held(configured_vk(slide, SHOOT_VK_VA, DEFAULT_SHOOT_VK)) || key_held(VK_CONTROL);
    CACHED_SLOW.store(slow, Ordering::Relaxed);
    CACHED_SKIP.store(skip, Ordering::Relaxed);
    LAST_POLL_QPC.store(now, Ordering::Relaxed);
    (slow, skip)
}

/// The VK at `va` (rebased by `slide`), or `default` if it reads 0.
fn configured_vk(slide: usize, va: usize, default: u16) -> u16 {
    let vk = unsafe { rebased_addr::<u16>(slide, va) }.read();
    if vk == 0 { default } else { vk }
}

fn key_held(vk: u16) -> bool {
    unsafe { GetAsyncKeyState(i32::from(vk)).cast_unsigned() & 0x8000 != 0 }
}

fn read_qpc() -> i64 {
    let mut t: i64 = 0;
    unsafe { QueryPerformanceCounter(&raw mut t) };
    t
}

fn qpc_freq() -> i64 {
    let cached = QPC_FREQ.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let mut f: i64 = 0;
    unsafe { QueryPerformanceFrequency(&raw mut f) };
    QPC_FREQ.store(f, Ordering::Relaxed);
    f
}
