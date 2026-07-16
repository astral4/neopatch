//! Auto-dismiss th20's startup dialog.

use crate::aslr::{rebased_addr, rebased_patch_jmp};
use crate::config::CONFIG;
use std::sync::OnceLock;
use tracing::info;

const RADIO_INDEX_BYTE_VA: usize = 0x005c_4f80;
const SCALE_INDEX_BYTE_VA: usize = 0x005c_4f8a;
const DIALOG_LIFECYCLE_FLAGS_VA: usize = 0x005b_87e8;
const DIALOG_LIFECYCLE_BITS: u32 = 0x18;

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
    let scale = cfg.resolution.scale_index(mode);
    let radio = unsafe { rebased_addr::<u8>(slide, RADIO_INDEX_BYTE_VA) };
    let scale_addr = unsafe { rebased_addr::<u8>(slide, SCALE_INDEX_BYTE_VA) };
    let lifecycle = unsafe { rebased_addr::<u32>(slide, DIALOG_LIFECYCLE_FLAGS_VA) };
    radio.write(idx);
    scale_addr.write(scale);
    lifecycle.write(lifecycle.read() & !DIALOG_LIFECYCLE_BITS);
    info!(
        kind = "dialog_short_circuited",
        resolution = %cfg.resolution,
        mode = %mode,
        radio_index = idx,
        scale_index = scale,
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
