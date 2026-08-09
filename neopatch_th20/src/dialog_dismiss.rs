//! Auto-dismiss th20's startup dialog.

use crate::CONFIG;
use crate::aslr::{current_slide, rebased_addr};
use neopatch_core::patches::PatchSite;
use tracing::info;

const RADIO_INDEX_VA: usize = 0x005c_4f80;
const SCALE_INDEX_VA: usize = 0x005c_4f8a;
const DIALOG_LIFECYCLE_FLAGS_VA: usize = 0x005b_87e8;
const DIALOG_LIFECYCLE_BITS: u32 = 0x18;

const DIALOG_DISPATCH_VA: usize = 0x0041_ae70;
const DIALOG_DISPATCH_PROLOGUE: [u8; 5] = [0x55, 0x8b, 0xec, 0x81, 0xec];

unsafe extern "stdcall" fn dialog_short_circuit() {
    let slide = current_slide();

    let cfg = CONFIG.get().unwrap();
    let mode = cfg.display_mode;
    let radio_index = cfg.resolution.radio_index(mode);
    let scale_index = cfg.resolution.scale_index(mode);
    let radio_addr = unsafe { rebased_addr(RADIO_INDEX_VA, slide) };
    let scale_addr = unsafe { rebased_addr(SCALE_INDEX_VA, slide) };
    let lifecycle = unsafe { rebased_addr::<u32>(DIALOG_LIFECYCLE_FLAGS_VA, slide) };

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

pub(crate) fn sites(slide: usize) -> [PatchSite; 1] {
    [PatchSite::jmp(
        DIALOG_DISPATCH_VA,
        &DIALOG_DISPATCH_PROLOGUE,
        dialog_short_circuit as *mut (),
        "dialog short-circuit (fcn.0041ae70)",
    )
    .rebase(slide)]
}
