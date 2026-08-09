//! DirectInput hooks for controller input.
//!
//! The games read `lX` and `lY` from `DIJOYSTATE` / `DIJOYSTATE2` for direction input, but not `rgdwPOV`,
//! which is for the D-pad on controllers. We convert POV into X/Y values so D-pad input translates into direction bits.
//!
//! When the D-pad is in a cardinal direction, we zero the perpendicular axis to account for stick drift.
//! Diagonals (POV at 45/135/225/315 degrees) set both axes. POV-centered passes the buffer through unchanged.

use crate::vtable::{IndexedFnSlots, hook_array8, install_vtable, vtable_field};
use std::ffi::c_void;
use std::mem::offset_of;
use std::ptr::NonNull;
use tracing::warn;
use windows::Win32::Devices::HumanInterfaceDevice::{
    DIJOYSTATE, DIJOYSTATE2, IDirectInput8A_Vtbl, IDirectInputDevice8A_Vtbl,
};
use windows::core::{GUID, HRESULT};

#[allow(clippy::cast_possible_truncation)]
const DIJOYSTATE_SIZE: u32 = size_of::<DIJOYSTATE>() as u32;
#[allow(clippy::cast_possible_truncation)]
const DIJOYSTATE2_SIZE: u32 = size_of::<DIJOYSTATE2>() as u32;

// `lX`, `lY`, and `rgdwPOV[0]` should be at the same offsets in both formats.
const LX_OFFSET: usize = offset_of!(DIJOYSTATE, lX);
const LY_OFFSET: usize = offset_of!(DIJOYSTATE, lY);
const POV0_OFFSET: usize = offset_of!(DIJOYSTATE, rgdwPOV);
const _: () = {
    assert!(LX_OFFSET == offset_of!(DIJOYSTATE2, lX));
    assert!(LY_OFFSET == offset_of!(DIJOYSTATE2, lY));
    assert!(POV0_OFFSET == offset_of!(DIJOYSTATE2, rgdwPOV));
};

// 36000 centidegrees = 360 degrees. The DirectInput spec uses `0xFFFFFFFF` for centered,
// but some drivers report `0xFFFF` or other out-of-range values, so anything past a full revolution is treated as centered.
const POV_FULL_REVOLUTION: u32 = 36000;

type DiCreateDeviceFn = unsafe extern "system" fn(
    this: *mut c_void,
    rguid: *const GUID,
    pp_device: *mut *mut c_void,
    p_unk_outer: *mut c_void,
) -> HRESULT;
type GetDeviceStateFn =
    unsafe extern "system" fn(this: *mut c_void, cb_data: u32, lpv_data: *mut c_void) -> HRESULT;

// Under Wine, winmm's joystick backend is built on DirectInput8 and creates a W object first. This means the game process can have
// multiple instances of `IDirectInput8`. Each patched interface vtable binds its displaced original to its own hook instance below.
static REAL_DI_CREATE_DEVICE: IndexedFnSlots<DiCreateDeviceFn, 8> =
    IndexedFnSlots::new("REAL_DI_CREATE_DEVICE", hook_array8!(hook_di_create_device));
static REAL_GET_DEVICE_STATE: IndexedFnSlots<GetDeviceStateFn, 8> =
    IndexedFnSlots::new("REAL_GET_DEVICE_STATE", hook_array8!(hook_get_device_state));

/// Registers a post-`DirectInput8Create` callback with the dinput8 proxy.
/// This must be called from `install_hooks` before the game calls `DirectInput8Create`.
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
    unsafe {
        install_vtable(vtbl, |scope| {
            scope.intercept_indexed(
                &REAL_DI_CREATE_DEVICE,
                vtable_field!(IDirectInput8A_Vtbl, CreateDevice),
                "IDirectInput8::CreateDevice",
            );
        });
    }
}

/// `CreateDevice` hook instance bound to entry `K` of [`REAL_DI_CREATE_DEVICE`].
/// Forwards through the original displaced from the vtable this instance was installed into, then patches the new device's vtable.
unsafe extern "system" fn hook_di_create_device<const K: usize>(
    this: *mut c_void,
    rguid: *const GUID,
    pp_device: *mut *mut c_void,
    p_unk_outer: *mut c_void,
) -> HRESULT {
    let hr = unsafe { REAL_DI_CREATE_DEVICE.get(K)(this, rguid, pp_device, p_unk_outer) };
    if hr.is_ok() && !pp_device.is_null() {
        // SAFETY: `hr` is a success code, so the real `CreateDevice` wrote the new device's pointer to `*pp_device`.
        let dev = unsafe { *pp_device };
        unsafe { patch_device_vtable(dev) };
    }
    hr
}

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
    unsafe {
        install_vtable(vtbl, |scope| {
            scope.intercept_indexed(
                &REAL_GET_DEVICE_STATE,
                vtable_field!(IDirectInputDevice8A_Vtbl, GetDeviceState),
                "IDirectInputDevice8::GetDeviceState",
            );
        });
    }
}

/// `GetDeviceState` hook instance bound to entry `K` of [`REAL_GET_DEVICE_STATE`].
/// Forwards through this vtable's own original, then converts POV to axis values in the returned state.
unsafe extern "system" fn hook_get_device_state<const K: usize>(
    this: *mut c_void,
    cb_data: u32,
    lpv_data: *mut c_void,
) -> HRESULT {
    let hr = unsafe { REAL_GET_DEVICE_STATE.get(K)(this, cb_data, lpv_data) };
    if hr.is_ok() {
        // SAFETY: On success, the real `GetDeviceState` should fill `cb_data` bytes at `lpv_data`.
        unsafe { convert_pov_in_state(cb_data, lpv_data) };
    }
    hr
}

/// Applies [`convert_pov`] in place to a `DIJOYSTATE` / `DIJOYSTATE2` buffer. Any other size (or a null buffer) passes through untouched.
///
/// # Safety
/// `lpv_data` must be null or point to `cb_data` readable and writable bytes.
#[allow(clippy::similar_names)]
unsafe fn convert_pov_in_state(cb_data: u32, lpv_data: *mut c_void) {
    if lpv_data.is_null() || !matches!(cb_data, DIJOYSTATE_SIZE | DIJOYSTATE2_SIZE) {
        return;
    }
    let ptr: *mut u8 = lpv_data.cast();
    let pov = unsafe { ptr.add(POV0_OFFSET).cast::<u32>().read_unaligned() };
    let lx = unsafe { ptr.add(LX_OFFSET).cast::<i32>().read_unaligned() };
    let ly = unsafe { ptr.add(LY_OFFSET).cast::<i32>().read_unaligned() };
    let (new_lx, new_ly) = convert_pov(pov, lx, ly);
    unsafe {
        ptr.add(LX_OFFSET).cast::<i32>().write_unaligned(new_lx);
        ptr.add(LY_OFFSET).cast::<i32>().write_unaligned(new_ly);
    }
}

/// Converts a POV value into `(lX, lY)` axis values. `pov` is the centidegree angle from `rgdwPOV[0]`:
/// `0` for N; `9000` for E; `18000` for S; `27000` for W; `36000` or more for centered.
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
    use super::{
        DiCreateDeviceFn, GetDeviceStateFn, REAL_DI_CREATE_DEVICE, REAL_GET_DEVICE_STATE,
        convert_pov,
    };
    use crate::vtable::hook_array8;
    use std::ffi::c_void;
    use std::num::NonZero;
    use std::ptr::null_mut;
    use windows::core::{GUID, HRESULT};

    fn nz(addr: usize) -> NonZero<usize> {
        NonZero::new(addr).unwrap()
    }

    #[test]
    fn convert_pov_directions() {
        // The four cardinals and the four diagonals sitting on the boundaries where two cardinal ranges meet.
        // The input axes are nonzero throughout, so every row also proves the game's originals get overwritten.
        for (pov, expected) in [
            (0, (0, i32::MIN)),            // N
            (4500, (i32::MAX, i32::MIN)),  // NE
            (9000, (i32::MAX, 0)),         // E
            (13500, (i32::MAX, i32::MAX)), // SE
            (18000, (0, i32::MAX)),        // S
            (22500, (i32::MIN, i32::MAX)), // SW
            (27000, (i32::MIN, 0)),        // W
            (31500, (i32::MIN, i32::MIN)), // NW
        ] {
            assert_eq!(convert_pov(pov, 12345, -6789), expected, "pov={pov}");
        }
    }

    #[test]
    fn convert_pov_centered_passes_axes_through() {
        for centered in [0xffff_ffffu32, 0xffff, 36000, 99999] {
            assert_eq!(
                convert_pov(centered, 123, -456),
                (123, -456),
                "pov={centered:#x}",
            );
        }
        // Extreme axis values survive the pass-through untouched.
        assert_eq!(
            convert_pov(u32::MAX, i32::MIN, i32::MIN),
            (i32::MIN, i32::MIN),
        );
    }

    #[test]
    fn convert_pov_just_before_diagonal_is_cardinal() {
        assert_eq!(convert_pov(4499, 100, 200), (0, i32::MIN));
        assert_eq!(convert_pov(4501, 100, 200), (i32::MAX, 0));
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    unsafe extern "system" fn stub_state<const K: usize>(
        _: *mut c_void,
        _: u32,
        _: *mut c_void,
    ) -> HRESULT {
        HRESULT(K as i32)
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    unsafe extern "system" fn stub_create<const K: usize>(
        _: *mut c_void,
        _: *const GUID,
        _: *mut *mut c_void,
        _: *mut c_void,
    ) -> HRESULT {
        HRESULT(K as i32)
    }

    #[test]
    fn get_device_state_instances_pairing() {
        const STUBS: [GetDeviceStateFn; 8] = hook_array8!(stub_state);
        for (k, &stub) in STUBS.iter().enumerate() {
            assert_eq!(REAL_GET_DEVICE_STATE.claim(nz(0x1000 + k), stub), Some(k));
        }
        for k in 0..STUBS.len() {
            let hr = unsafe { REAL_GET_DEVICE_STATE.hook(k)(null_mut(), 0, null_mut()) };
            assert_eq!(hr.0, i32::try_from(k).unwrap());
        }
    }

    #[test]
    fn create_device_instances_pairing() {
        use std::ptr::{null, null_mut};

        const STUBS: [DiCreateDeviceFn; 8] = hook_array8!(stub_create);
        for (k, &stub) in STUBS.iter().enumerate() {
            assert_eq!(REAL_DI_CREATE_DEVICE.claim(nz(0x2000 + k), stub), Some(k));
        }
        for k in 0..STUBS.len() {
            let hr = unsafe {
                REAL_DI_CREATE_DEVICE.hook(k)(null_mut(), null(), null_mut(), null_mut())
            };
            assert_eq!(hr.0, i32::try_from(k).unwrap());
        }
    }
}
