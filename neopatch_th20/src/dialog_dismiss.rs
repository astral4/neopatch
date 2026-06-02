//! Auto-dismiss th20's startup dialog.
//!
//! th20's dialog runs inside a self-contained `__stdcall fcn.0041ae70` with its own `PeekMessageW`
//! loop. We splice the function entry and mirror the bytes the OK handler `fcn.0041ab30` would have
//! written, then clear the lifecycle bits main checks on return (`(DAT_005b87e8 >> 3) & 3 == 0`).

use crate::aslr::{rebased_addr, rebased_patch_jmp};
use crate::config::CONFIG;
use std::sync::OnceLock;
use tracing::info;

const RADIO_INDEX_BYTE_VA: usize = 0x005c_4f80;
const SCALE_INDEX_BYTE_VA: usize = 0x005c_4f8a;
const DIALOG_LIFECYCLE_FLAGS_VA: usize = 0x005b_87e8;

/// The 5-byte prologue is the first half of `push ebp; mov ebp, esp; sub esp, 0x15c`.
const DIALOG_DISPATCH_VA: usize = 0x0041_ae70;
const DIALOG_DISPATCH_PROLOGUE: [u8; 5] = [0x55, 0x8b, 0xec, 0x81, 0xec];

static SLIDE: OnceLock<usize> = OnceLock::new();

unsafe extern "stdcall" fn dialog_short_circuit() {
    let slide = *SLIDE
        .get()
        .expect("dialog_short_circuit fired before install");
    let cfg = CONFIG.get().unwrap();
    let mode = cfg.display_mode;
    let idx = cfg.resolution.radio_index(mode);
    let scale = cfg.resolution.scale_byte(mode);
    let radio = unsafe { rebased_addr::<u8>(slide, RADIO_INDEX_BYTE_VA) };
    let scale_addr = unsafe { rebased_addr::<u8>(slide, SCALE_INDEX_BYTE_VA) };
    let lifecycle = unsafe { rebased_addr::<u32>(slide, DIALOG_LIFECYCLE_FLAGS_VA) };
    radio.write(idx);
    scale_addr.write(scale);
    lifecycle.write(lifecycle.read() & !0x18);
    info!(
        kind = "dialog_short_circuited",
        resolution = %cfg.resolution,
        mode = %mode,
        radio_index = idx,
        scale_byte = scale,
    );
}

pub(crate) unsafe fn install(slide: usize) {
    _ = SLIDE.set(slide);
    unsafe {
        rebased_patch_jmp(
            slide,
            DIALOG_DISPATCH_VA,
            &DIALOG_DISPATCH_PROLOGUE,
            dialog_short_circuit as *mut (),
            "dialog short-circuit (fcn.0041ae70)",
        );
    }
}
