//! Logic for auto-dismissing th18's startup dialog.
//!
//! Unlike th14-th17's modeless `CreateDialogParamA`, th18 wraps the dialog in a
//! self-contained `__stdcall` function (`fcn.00474850`) with its own
//! `PeekMessageA` loop, so an IAT hook on `CreateDialogParamA` is too late.
//! We splice the function entry instead and mirror the bytes the OK handler
//! (`fcn.004747d0`) writes: `[0x4cd00b]` (radio index 0..9 into the array at
//! `0x4b4280`) and `[0x4cd012]` (scale byte from `0x4b7fbc`), plus a clear of
//! `[0x56ac70]` bits 0x300 (the dialog-lifecycle flags `WM_INITDIALOG` would
//! have set; the caller bails out at `0x471758` if either bit is observed).

use crate::config::CONFIG;
use neopatch_core::game_addr::GameAddr;
use neopatch_core::patches::patch_jmp;
use tracing::info;

const RADIO_INDEX_BYTE: GameAddr<u8> = unsafe { GameAddr::new(0x004c_d00b) };
const SCALE_INDEX_BYTE: GameAddr<u8> = unsafe { GameAddr::new(0x004c_d012) };
const DIALOG_LIFECYCLE_FLAGS: GameAddr<u32> = unsafe { GameAddr::new(0x0056_ac70) };

/// Entry of th18's startup dialog. The 5-byte prologue is the first half of
/// `push ebp; mov ebp, esp; sub esp, 0x184`; the `0x184` immediate at +5..+9
/// becomes dead after the splice.
const FUN_00474850: usize = 0x0047_4850;
const FUN_00474850_PROLOGUE: [u8; 5] = [0x55, 0x8b, 0xec, 0x81, 0xec];

unsafe extern "stdcall" fn dialog_short_circuit() {
    let th18_cfg = CONFIG.get().unwrap();
    let mode = th18_cfg.display_mode;
    let idx = th18_cfg.resolution.radio_index(mode);
    let scale = th18_cfg.resolution.scale_byte(mode);
    RADIO_INDEX_BYTE.write(idx);
    SCALE_INDEX_BYTE.write(scale);
    DIALOG_LIFECYCLE_FLAGS.write(DIALOG_LIFECYCLE_FLAGS.read() & 0xffff_fcff);
    info!(
        kind = "dialog_short_circuited",
        resolution = %th18_cfg.resolution,
        mode = %mode,
        radio_index = idx,
        scale_byte = scale,
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
