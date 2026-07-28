//! Logic for auto-dismissing th18's startup dialog.

use crate::config::CONFIG;
use neopatch_core::game_addr::GameAddr;
use neopatch_core::patches::PatchSite;
use tracing::info;

const RADIO_INDEX_VA: GameAddr<u8> = unsafe { GameAddr::new(0x004c_d00b) };
const SCALE_INDEX_VA: GameAddr<u8> = unsafe { GameAddr::new(0x004c_d012) };
const DIALOG_LIFECYCLE_FLAGS_VA: GameAddr<u32> = unsafe { GameAddr::new(0x0056_ac70) };
const DIALOG_LIFECYCLE_BITS: u32 = 0x300;

const FUN_00474850: usize = 0x0047_4850;
const FUN_00474850_PROLOGUE: [u8; 5] = [0x55, 0x8b, 0xec, 0x81, 0xec];

unsafe extern "stdcall" fn dialog_short_circuit() {
    let cfg = CONFIG.get().unwrap();
    let mode = cfg.display_mode;
    let radio_index = cfg.resolution.radio_index(mode);
    let scale_index = cfg.resolution.scale_index(mode);

    RADIO_INDEX_VA.write(radio_index);
    SCALE_INDEX_VA.write(scale_index);
    DIALOG_LIFECYCLE_FLAGS_VA.write(DIALOG_LIFECYCLE_FLAGS_VA.read() & !DIALOG_LIFECYCLE_BITS);
    info!(
        kind = "dialog_short_circuited",
        resolution = %cfg.resolution,
        mode = %mode,
        radio_index,
        scale_index,
    );
}

pub(crate) const DIALOG_PATCHES: &[PatchSite] = &[PatchSite::jmp(
    FUN_00474850,
    &FUN_00474850_PROLOGUE,
    dialog_short_circuit as *mut (),
    "dialog short-circuit (fcn.00474850)",
)];
