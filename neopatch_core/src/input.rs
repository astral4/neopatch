//! Joystick input hooks.
//!
//! The games read `lX` and `lY` from `DIJOYSTATE`/`DIJOYSTATE2` for direction input,
//! but not `rgdwPOV`, which is for the D-pad on controllers. We convert POV into X/Y values
//! so D-pad input translates into direction bits.
//!
//! When the D-pad is in a cardinal direction, we zero the perpendicular axis
//! to account for stick drift. Diagonals (POV at 45/135/225/315 degrees) set both axes.
//! POV-centered passes the buffer through unchanged.

use crate::vtable::{install_vtable, vtable_slot, vtbl_field};
use std::ffi::c_void;
use std::mem::offset_of;
use std::ptr::NonNull;
use tracing::{info, warn};
use windows::Win32::Devices::HumanInterfaceDevice::{
    DIJOYSTATE, DIJOYSTATE2, IDirectInput8A_Vtbl, IDirectInputDevice8A_Vtbl,
};
use windows::core::{GUID, HRESULT};

// `cb_data` discriminants for `GetDeviceState`.
#[allow(clippy::cast_possible_truncation)]
const DIJOYSTATE_SIZE: u32 = size_of::<DIJOYSTATE>() as u32;
#[allow(clippy::cast_possible_truncation)]
const DIJOYSTATE2_SIZE: u32 = size_of::<DIJOYSTATE2>() as u32;

// `lX`, `lY`, and `rgdwPOV[0]` should be at the same offsets in both formats.
const LX_OFFSET: usize = offset_of!(DIJOYSTATE, lX);
const LY_OFFSET: usize = offset_of!(DIJOYSTATE, lY);
const POV0_OFFSET: usize = offset_of!(DIJOYSTATE, rgdwPOV);
const _: () = {
    assert!(offset_of!(DIJOYSTATE, lX) == offset_of!(DIJOYSTATE2, lX));
    assert!(offset_of!(DIJOYSTATE, lY) == offset_of!(DIJOYSTATE2, lY));
    assert!(offset_of!(DIJOYSTATE, rgdwPOV) == offset_of!(DIJOYSTATE2, rgdwPOV));
};

// 36000 centidegrees = 360 degrees. The DirectInput spec uses `0xFFFFFFFF` for centered,
// but some drivers report `0xFFFF` or other out-of-range values,
// so anything past a full revolution is treated as centered.
const POV_FULL_REVOLUTION: u32 = 36000;

vtable_slot! {
    REAL_DI_CREATE_DEVICE / call_real_di_create_device :
        as fn(
            this: *mut c_void,
            rguid: *const GUID,
            pp_device: *mut *mut c_void,
            p_unk_outer: *mut c_void,
        ) -> HRESULT;
}
vtable_slot! {
    REAL_GET_DEVICE_STATE / call_real_get_device_state :
        as fn(
            this: *mut c_void,
            cb_data: u32,
            lpv_data: *mut c_void,
        ) -> HRESULT;
}

/// Registers a post-`DirectInput8Create` callback with the dinput8 proxy.
/// This should be called from `install_hooks` before the game calls `DirectInput8Create`.
pub fn install() {
    crate::dinput8::set_on_created(on_directinput_created);
}

unsafe fn on_directinput_created(di: *mut c_void) {
    let Some(di) = NonNull::new(di) else { return };
    // SAFETY: `di` points to an `IDirectInput8A` whose first slot is the vtable pointer.
    let vtbl: *mut IDirectInput8A_Vtbl = unsafe { *di.as_ptr().cast() };
    let Some(vtbl) = NonNull::new(vtbl) else {
        warn!(kind = "dinput_vtbl_null", di = format_args!("{di:p}"));
        return;
    };
    let result = unsafe {
        install_vtable(vtbl, |scope| {
            scope.intercept(
                &REAL_DI_CREATE_DEVICE,
                vtbl_field!(IDirectInput8A_Vtbl, CreateDevice),
                "IDirectInput8::CreateDevice",
                hook_di_create_device,
            );
        })
    };
    info!(
        kind = "dinput_hooks_installed",
        protect_ok = result.is_some()
    );
}

unsafe extern "system" fn hook_di_create_device(
    this: *mut c_void,
    rguid: *const GUID,
    pp_device: *mut *mut c_void,
    p_unk_outer: *mut c_void,
) -> HRESULT {
    let hr = unsafe { call_real_di_create_device(this, rguid, pp_device, p_unk_outer) };
    if hr.is_ok() && !pp_device.is_null() {
        // SAFETY: DirectInput guarantees `pp_device` is non-null and points at the new device.
        let dev = unsafe { *pp_device };
        unsafe { patch_device_vtable(dev) };
    }
    hr
}

/// Patches the class-shared `IDirectInputDevice8A` vtable. Called for every
/// device created through our hook (keyboard then joystick on TH15, similar on
/// TH10); `install_vtable`'s idempotency makes second-and-later calls no-ops.
unsafe fn patch_device_vtable(dev: *mut c_void) {
    let Some(dev) = NonNull::new(dev) else { return };
    // SAFETY: `dev` points to an `IDirectInputDevice8A` whose first slot is the vtable pointer.
    let vtbl: *mut IDirectInputDevice8A_Vtbl = unsafe { *dev.as_ptr().cast() };
    let Some(vtbl) = NonNull::new(vtbl) else {
        warn!(
            kind = "dinput_device_vtbl_null",
            dev = format_args!("{dev:p}"),
        );
        return;
    };
    let result = unsafe {
        install_vtable(vtbl, |scope| {
            scope.intercept(
                &REAL_GET_DEVICE_STATE,
                vtbl_field!(IDirectInputDevice8A_Vtbl, GetDeviceState),
                "IDirectInputDevice8::GetDeviceState",
                hook_get_device_state,
            );
        })
    };
    info!(
        kind = "dinput_device_hooks_installed",
        protect_ok = result.is_some(),
    );
}

unsafe extern "system" fn hook_get_device_state(
    this: *mut c_void,
    cb_data: u32,
    lpv_data: *mut c_void,
) -> HRESULT {
    let hr = unsafe { call_real_get_device_state(this, cb_data, lpv_data) };
    if hr.is_ok() && !lpv_data.is_null() && matches!(cb_data, DIJOYSTATE_SIZE | DIJOYSTATE2_SIZE) {
        unsafe {
            let ptr = lpv_data.cast::<u8>();
            let pov: u32 = ptr.add(POV0_OFFSET).cast::<u32>().read_unaligned();
            let lx: i32 = ptr.add(LX_OFFSET).cast::<i32>().read_unaligned();
            let ly: i32 = ptr.add(LY_OFFSET).cast::<i32>().read_unaligned();
            let (new_lx, new_ly) = convert_pov(pov, lx, ly);
            ptr.add(LX_OFFSET).cast::<i32>().write_unaligned(new_lx);
            ptr.add(LY_OFFSET).cast::<i32>().write_unaligned(new_ly);
        };
    }
    hr
}

/// Converts a POV value into `(lX, lY)` axis values.
///
/// `pov` is the centidegree angle from `rgdwPOV[0]`: `0` for N, `9000` for E,
/// `18000` for S, `27000` for W; `36000` or more for centered.
#[must_use]
#[allow(clippy::similar_names)]
fn convert_pov(pov: u32, lx: i32, ly: i32) -> (i32, i32) {
    if pov >= POV_FULL_REVOLUTION {
        return (lx, ly);
    }
    let up = pov <= 4500 || pov >= 31500;
    let right = (4500..=13500).contains(&pov);
    let down = (13500..=22500).contains(&pov);
    let left = (22500..=31500).contains(&pov);
    let new_lx = if left {
        i32::MIN
    } else if right {
        i32::MAX
    } else {
        0
    };
    let new_ly = if up {
        i32::MIN
    } else if down {
        i32::MAX
    } else {
        0
    };
    (new_lx, new_ly)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_pov_cardinal_up_zeroes_lx() {
        let (lx, ly) = convert_pov(0, 12345, -6789);
        assert_eq!(lx, 0);
        assert_eq!(ly, i32::MIN);
    }

    #[test]
    fn convert_pov_cardinal_right_zeroes_ly() {
        let (lx, ly) = convert_pov(9000, -12345, 6789);
        assert_eq!(lx, i32::MAX);
        assert_eq!(ly, 0);
    }

    #[test]
    fn convert_pov_cardinal_down_zeroes_lx() {
        let (lx, ly) = convert_pov(18000, 12345, -6789);
        assert_eq!(lx, 0);
        assert_eq!(ly, i32::MAX);
    }

    #[test]
    fn convert_pov_cardinal_left_zeroes_ly() {
        let (lx, ly) = convert_pov(27000, 12345, 6789);
        assert_eq!(lx, i32::MIN);
        assert_eq!(ly, 0);
    }

    #[test]
    fn convert_pov_diagonal_ne_sets_both_axes() {
        assert_eq!(convert_pov(4500, 0, 0), (i32::MAX, i32::MIN));
    }

    #[test]
    fn convert_pov_diagonal_se_sets_both_axes() {
        assert_eq!(convert_pov(13500, 0, 0), (i32::MAX, i32::MAX));
    }

    #[test]
    fn convert_pov_diagonal_sw_sets_both_axes() {
        assert_eq!(convert_pov(22500, 0, 0), (i32::MIN, i32::MAX));
    }

    #[test]
    fn convert_pov_diagonal_nw_sets_both_axes() {
        assert_eq!(convert_pov(31500, 0, 0), (i32::MIN, i32::MIN));
    }

    #[test]
    fn convert_pov_centered_passes_axes_through() {
        for centered in [0xFFFF_FFFFu32, 0xFFFF, 36000, 99999] {
            let (lx, ly) = convert_pov(centered, 123, -456);
            assert_eq!(lx, 123, "pov={centered:#x}");
            assert_eq!(ly, -456, "pov={centered:#x}");
        }
    }

    #[test]
    fn convert_pov_just_before_diagonal_is_cardinal() {
        let (lx, ly) = convert_pov(4499, 100, 200);
        assert_eq!(lx, 0);
        assert_eq!(ly, i32::MIN);
        let (lx, ly) = convert_pov(4501, 100, 200);
        assert_eq!(lx, i32::MAX);
        assert_eq!(ly, 0);
    }

    #[test]
    fn convert_pov_preserves_negative_inputs_when_centered() {
        let (lx, ly) = convert_pov(u32::MAX, i32::MIN, i32::MIN);
        assert_eq!(lx, i32::MIN);
        assert_eq!(ly, i32::MIN);
    }
}
