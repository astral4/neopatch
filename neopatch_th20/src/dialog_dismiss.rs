//! Auto-dismiss th20's startup dialog.

use crate::aslr::{rebased_addr, rebased_patch_jmp};
use crate::config::CONFIG;
use std::sync::OnceLock;
use tracing::info;

const RADIO_INDEX_VA: usize = 0x005c_4f80;
const SCALE_INDEX_VA: usize = 0x005c_4f8a;
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
    let radio_index = cfg.resolution.radio_index(mode);
    let scale_index = cfg.resolution.scale_index(mode);
    let radio_addr = unsafe { rebased_addr(slide, RADIO_INDEX_VA) };
    let scale_addr = unsafe { rebased_addr(slide, SCALE_INDEX_VA) };
    let lifecycle = unsafe { rebased_addr::<u32>(slide, DIALOG_LIFECYCLE_FLAGS_VA) };

    radio_addr.write(radio_index);
    scale_addr.write(scale_index);
    lifecycle.write(lifecycle.read() & !DIALOG_LIFECYCLE_BITS);

    info!(
        kind = "dialog_short_circuited",
        resolution = %cfg.resolution,
        mode = %mode,
        radio_index,
        scale_index,
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
