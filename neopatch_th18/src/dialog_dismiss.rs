//! Logic for auto-dismissing th18's startup dialog.

use crate::config::CONFIG;
use neopatch_core::game_addr::GameAddr;
use neopatch_core::patches::patch_jmp;
use tracing::info;

const RADIO_INDEX_BYTE: GameAddr<u8> = unsafe { GameAddr::new(0x004c_d00b) };
const SCALE_INDEX_BYTE: GameAddr<u8> = unsafe { GameAddr::new(0x004c_d012) };
const DIALOG_LIFECYCLE_FLAGS: GameAddr<u32> = unsafe { GameAddr::new(0x0056_ac70) };
const DIALOG_LIFECYCLE_BITS: u32 = 0x300;

const FUN_00474850: usize = 0x0047_4850;
const FUN_00474850_PROLOGUE: [u8; 5] = [0x55, 0x8b, 0xec, 0x81, 0xec];

unsafe extern "stdcall" fn dialog_short_circuit() {
    let th18_cfg = CONFIG.get().unwrap();
    let mode = th18_cfg.display_mode;
    let idx = th18_cfg.resolution.radio_index(mode);
    let scale = th18_cfg.resolution.scale_index(mode);
    RADIO_INDEX_BYTE.write(idx);
    SCALE_INDEX_BYTE.write(scale);
    DIALOG_LIFECYCLE_FLAGS.write(DIALOG_LIFECYCLE_FLAGS.read() & !DIALOG_LIFECYCLE_BITS);
    info!(
        kind = "dialog_short_circuited",
        resolution = %th18_cfg.resolution,
        mode = %mode,
        radio_index = idx,
        scale_index = scale,
    );
}

pub(crate) unsafe fn install() {
    unsafe {
        patch_jmp(
            FUN_00474850,
            &FUN_00474850_PROLOGUE,
            dialog_short_circuit as *mut (),
            "dialog short-circuit (fcn.00474850)",
        );
    }
}
