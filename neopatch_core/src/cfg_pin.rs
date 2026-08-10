//! Logic for disabling frameskip by hooking configuration file I/O.
//!
//! We need games to present every logic tick, since our pacer waits inside the `Present` hook and the per-game limiter patches
//! only affect the frame loop used when frameskip is disabled (i.e. set to 0). th11 onward also read the frameskip byte
//! before creating the render window, so the byte needs to already be 0 when the config loader returns.

use crate::iat_hook;
use crate::log::log_at;
use crate::untrusted::Untrusted;
use std::ffi::c_void;
use std::slice::{from_mut, from_raw_parts_mut};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};
use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, GetLastError, HANDLE, HMODULE, MAX_PATH, SetLastError,
};
use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::core::BOOL;

// `\\?\` prefix + drive path + NUL.
const MAX_PATH_LEN: u32 = 4 + MAX_PATH + 1;

static CFG: OnceLock<CfgFile> = OnceLock::new();
static PIN: OnceLock<PinState> = OnceLock::new();

/// A compare instruction from a game's cfg-validation chain.
#[derive(Clone, Copy)]
pub enum CfgCheck {
    /// `cmp byte [cfg+offset], bound` + `jae reject`: an unsigned byte below `bound`.
    ByteMax { offset: u32, bound: u8 },
    /// `cmp dword [cfg+offset], bound` + `jge reject`: a signed dword below `bound`.
    SdwordMax { offset: u32, bound: i32 },
    /// `cmp dword [cfg+offset], value` + `jne reject`: a dword equal to `value`.
    DwordEq { offset: u32, value: u32 },
}

impl CfgCheck {
    #[must_use]
    pub const fn byte_max(offset: u32, bound: u8) -> Self {
        Self::ByteMax { offset, bound }
    }

    #[must_use]
    pub const fn sdword_max(offset: u32, bound: i32) -> Self {
        Self::SdwordMax { offset, bound }
    }

    #[must_use]
    pub const fn dword_eq(offset: u32, value: u32) -> Self {
        Self::DwordEq { offset, value }
    }

    const fn offset(self) -> u32 {
        match self {
            Self::ByteMax { offset, .. }
            | Self::SdwordMax { offset, .. }
            | Self::DwordEq { offset, .. } => offset,
        }
    }

    const fn width(self) -> u32 {
        match self {
            Self::ByteMax { .. } => 1,
            Self::SdwordMax { .. } | Self::DwordEq { .. } => 4,
        }
    }

    /// Returns whether this check's field lies inside a `size`-byte image.
    const fn fits(self, size: u32) -> bool {
        self.width() <= size && self.offset() <= size - self.width()
    }

    /// Returns whether `bytes` (a whole cfg image) passes this check.
    fn passes(self, bytes: &[u8]) -> bool {
        let o = self.offset() as usize;
        match self {
            Self::ByteMax { bound, .. } => bytes[o] < bound,
            Self::SdwordMax { bound, .. } => i32::from_le_bytes(quad(bytes, o)) < bound,
            Self::DwordEq { value, .. } => u32::from_le_bytes(quad(bytes, o)) == value,
        }
    }
}

fn quad(bytes: &[u8], o: usize) -> [u8; 4] {
    [bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]
}

/// The frameskip byte details.
#[derive(Clone, Copy)]
pub struct ByteField {
    /// The byte offset of the field in the cfg image.
    pub offset: u32,
    /// The loader accepts values strictly below this bound.
    pub bound: u8,
}

impl ByteField {
    const fn as_check(self) -> CfgCheck {
        CfgCheck::ByteMax {
            offset: self.offset,
            bound: self.bound,
        }
    }
}

/// Information about a game's cfg file format.
#[derive(Clone, Copy)]
pub struct CfgFile {
    /// The base name of the cfg file in ASCII.
    pub file_name: &'static str,
    /// The magic/version dword at file offset 0.
    pub magic: u32,
    /// The exact byte size of a current-version cfg file.
    pub size: u32,
    /// The `frameskip` byte and the range the loader accepts for it.
    pub frameskip: ByteField,
    /// The loader's validation chain for the remaining fields, in the game's own check order.
    pub other_checks: &'static [CfgCheck],
}

impl CfgFile {
    /// Checks that this cfg file format specification satisfies invariants for correctness.
    /// This should be invoked from a `const _: () = ...` block at each site declaring a `CfgFile`.
    ///
    /// # Panics
    /// Panics if:
    /// - `size` is too small for the magic dword
    /// - a field is outside the image
    pub const fn validate(self) {
        assert!(self.size >= 4, "cfg image must hold the magic dword");
        assert!(
            self.frameskip.as_check().fits(self.size),
            "frameskip byte is outside the cfg image"
        );
        let mut i = 0;
        while i < self.other_checks.len() {
            assert!(
                self.other_checks[i].fits(self.size),
                "check is outside the cfg image"
            );
            i += 1;
        }
    }
}

struct PinState {
    /// The player's on-disk frameskip value, restored into every outgoing identified cfg image.
    user: u8,
    /// The forced image delivered to the game at load time.
    forced_image: Vec<u8>,
    /// Whether the first identified cfg write was already compared against `forced_image`.
    first_write_checked: AtomicBool,
}

iat_hook! {
    REAL_CFG_READ_FILE / real_cfg_read_file : "ReadFile"
        as fn(
            file: HANDLE,
            buffer: *mut c_void,
            bytes_to_read: u32,
            bytes_read: *mut u32,
            overlapped: *mut OVERLAPPED,
        ) -> BOOL;
}
iat_hook! {
    REAL_CFG_WRITE_FILE / real_cfg_write_file : "WriteFile"
        as fn(
            file: HANDLE,
            buffer: *const c_void,
            bytes_to_write: u32,
            bytes_written: *mut u32,
            overlapped: *mut OVERLAPPED,
        ) -> BOOL;
}

/// IAT-hooks `ReadFile`/`WriteFile` against `host`'s import table and registers the game's cfg file format details.
///
/// # Safety
/// `host` must be a loaded module handle.
pub unsafe fn install(host: HMODULE, cfg: CfgFile) {
    let read = unsafe { REAL_CFG_READ_FILE.install(host, hook_read_file) };
    let write = unsafe { REAL_CFG_WRITE_FILE.install(host, hook_write_file) };
    let viable = read && write;
    if viable {
        let _ = CFG.set(cfg);
    }
    log_at!(viable => info / warn,
        kind = "cfg_pin_installed",
        file = cfg.file_name,
        read,
        write,
    );
}

/// Returns true if `path` ends with `expect` (ASCII case-insensitive) as a whole path component.
fn is_cfg_path(path: &[u16], expect: &str) -> bool {
    let expect = expect.as_bytes();
    if path.len() < expect.len() {
        return false;
    }
    let (head, tail) = path.split_at(path.len() - expect.len());
    let tail_matches = tail
        .iter()
        .zip(expect)
        .all(|(&c, &e)| u8::try_from(c).is_ok_and(|c| c.eq_ignore_ascii_case(&e)));
    tail_matches
        && head
            .last()
            .is_none_or(|&s| s == u16::from(b'\\') || s == u16::from(b'/'))
}

/// Checks whether `file` currently refers to the game's cfg file.
fn identify_cfg_handle(file: HANDLE, cfg: &CfgFile) -> Result<bool, u32> {
    let mut buf = [0u16; MAX_PATH_LEN as usize];
    let n = unsafe { GetFinalPathNameByHandleW(file, buf.as_mut_ptr(), MAX_PATH_LEN, 0) };
    if n == 0 {
        return Err(unsafe { GetLastError() });
    }
    if n < MAX_PATH_LEN {
        return Ok(is_cfg_path(&buf[..n as usize], cfg.file_name));
    }

    // `n` is the required length including the NUL.
    let mut buf = vec![0u16; n as usize];
    let m = unsafe { GetFinalPathNameByHandleW(file, buf.as_mut_ptr(), n, 0) };
    if m == 0 {
        return Err(unsafe { GetLastError() });
    }
    if m >= n {
        return Err(ERROR_INSUFFICIENT_BUFFER);
    }
    Ok(is_cfg_path(&buf[..m as usize], cfg.file_name))
}

fn has_magic(cfg: &CfgFile, bytes: &[u8]) -> bool {
    bytes.len() >= 4 && bytes[..4] == cfg.magic.to_le_bytes()
}

/// What the read hook should do with an identified cfg image.
#[derive(Debug, PartialEq, Eq)]
enum ReadVerdict {
    /// The image is valid with frameskip already 0, so there's nothing to do.
    AlreadyOff,
    /// The image passes the game's whole validator and holds this nonzero frameskip, so we force the zero value.
    Force(u8),
    /// The check at this offset fails, so the game will reset everything to defaults.
    /// We leave the image untouched, preserving the vanilla outcome.
    Reject { offset: u32 },
}

fn read_verdict(cfg: &CfgFile, bytes: &[u8]) -> ReadVerdict {
    for check in cfg.other_checks {
        if !check.passes(bytes) {
            return ReadVerdict::Reject {
                offset: check.offset(),
            };
        }
    }
    match bytes[cfg.frameskip.offset as usize] {
        0 => ReadVerdict::AlreadyOff,
        user if cfg.frameskip.as_check().passes(bytes) => ReadVerdict::Force(user),
        _ => ReadVerdict::Reject {
            offset: cfg.frameskip.offset,
        },
    }
}

/// Restores the player's frameskip value into an outgoing cfg image, in place. If `Err` is returned, then the image was not touched.
fn restore_in_place(cfg: &CfgFile, bytes: &mut [u8], user: u8) -> Result<(), &'static str> {
    let offset = cfg.frameskip.offset as usize;
    if bytes[offset] != 0 {
        return Err("byte_not_forced");
    }
    bytes[offset] = user;
    Ok(())
}

unsafe extern "system" fn hook_read_file(
    file: HANDLE,
    buffer: *mut c_void,
    bytes_to_read: u32,
    bytes_read: *mut u32,
    overlapped: *mut OVERLAPPED,
) -> BOOL {
    let ok = unsafe { real_cfg_read_file(file, buffer, bytes_to_read, bytes_read, overlapped) };
    if let Some(cfg) = CFG.get()
        && ok != 0
        && overlapped.is_null()
        && !bytes_read.is_null()
        && !buffer.is_null()
        && bytes_to_read >= cfg.size
        // We do a raw deref here instead of using `Untrusted` because the real `ReadFile` just wrote through the pointer.
        && unsafe { *bytes_read } == cfg.size
    {
        // SAFETY: The real `ReadFile` just wrote `cfg.size` bytes through `buffer`, so the range is valid.
        // The slice length is bounded by `bytes_to_read` and `bytes_read`. We assume this is the game's cfg image if there are magic bytes.
        let bytes = unsafe { from_raw_parts_mut(buffer.cast::<u8>(), cfg.size as usize) };
        if has_magic(cfg, bytes) {
            let error = unsafe { GetLastError() };
            force_frameskip(file, cfg, bytes);
            unsafe { SetLastError(error) };
        }
    }
    ok
}

fn force_frameskip(file: HANDLE, cfg: &CfgFile, bytes: &mut [u8]) {
    match identify_cfg_handle(file, cfg) {
        Ok(true) => match read_verdict(cfg, bytes) {
            ReadVerdict::AlreadyOff => {
                info!(kind = "frameskip_already_off");
            }
            ReadVerdict::Force(user) => {
                bytes[cfg.frameskip.offset as usize] = 0;
                // The first force operation wins. This assumes that no game reads its cfg file more than once.
                drop(PIN.set(PinState {
                    user,
                    forced_image: bytes.to_vec(),
                    first_write_checked: AtomicBool::new(false),
                }));
                info!(kind = "frameskip_forced_off", user_frameskip = user);
            }
            ReadVerdict::Reject { offset } => {
                warn!(
                    kind = "cfg_read_rejected",
                    offset = format_args!("{offset:#x}"),
                );
            }
        },
        Ok(false) => {}
        Err(query_error) => {
            warn!(kind = "cfg_identify_failed", op = "read", query_error);
        }
    }
}

unsafe extern "system" fn hook_write_file(
    file: HANDLE,
    buffer: *const c_void,
    bytes_to_write: u32,
    bytes_written: *mut u32,
    overlapped: *mut OVERLAPPED,
) -> BOOL {
    if let Some(cfg) = CFG.get()
        && overlapped.is_null()
        && !buffer.is_null()
        && bytes_to_write == cfg.size
        && let Some(pin) = PIN.get()
    {
        let error = unsafe { GetLastError() };
        let mut image = vec![0u8; cfg.size as usize];
        if Untrusted::from_raw(buffer.cast::<u8>()).safe_read(&mut image) != image.len() {
            warn!(
                kind = "frameskip_restore_skipped",
                reason = "unreadable_buffer",
                user_frameskip = pin.user,
            );
        } else if has_magic(cfg, &image) {
            let restore = match identify_cfg_handle(file, cfg) {
                Ok(true) => {
                    check_first_write(cfg, pin, &image);
                    true
                }
                // We don't patch what the OS says is a different file.
                Ok(false) => false,
                // The query failed, so the identity is unknown. However, the image already matched the cfg's exact size and magic bytes
                // while a force operation was live, which was sufficient evidence for us to arm the pin in the first place.
                // We restore here because passing the game's buffer through would preserve our forced zero over the user's saved value.
                Err(query_error) => {
                    warn!(kind = "cfg_identify_failed", op = "write", query_error);
                    true
                }
            };
            if restore
                && let Ok(()) = restore_in_place(cfg, &mut image, pin.user).inspect_err(|reason| {
                    warn!(
                        kind = "frameskip_restore_skipped",
                        reason,
                        user_frameskip = pin.user
                    );
                })
            {
                unsafe { SetLastError(error) };
                let ret = unsafe {
                    real_cfg_write_file(
                        file,
                        image.as_ptr().cast(),
                        bytes_to_write,
                        bytes_written,
                        overlapped,
                    )
                };
                let mut written = 0u32;
                let written_ok = bytes_written.is_null()
                    || (Untrusted::from_raw(bytes_written.cast_const())
                        .safe_read(from_mut(&mut written))
                        == 1
                        && written == cfg.size);
                let ok = ret != 0 && written_ok;
                let error = unsafe { GetLastError() };
                log_at!(ok => info / warn,
                    kind = "frameskip_write_back",
                    user_frameskip = pin.user,
                    ok,
                );
                unsafe { SetLastError(error) };
                return ret;
            }
        }
        unsafe { SetLastError(error) };
    }
    unsafe { real_cfg_write_file(file, buffer, bytes_to_write, bytes_written, overlapped) }
}

fn check_first_write(cfg: &CfgFile, pin: &PinState, outgoing: &[u8]) {
    if pin.first_write_checked.swap(true, Ordering::Relaxed) {
        return;
    }
    if pin.forced_image != outgoing {
        warn!(
            kind = "frameskip_first_write_diverged",
            file = cfg.file_name
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ByteField, CfgCheck, CfgFile, ReadVerdict, has_magic, is_cfg_path, read_verdict,
        restore_in_place,
    };

    const CFG: CfgFile = CfgFile {
        file_name: "game.cfg",
        magic: 0xfee1_900d,
        size: 0x40,
        frameskip: ByteField {
            offset: 0x20,
            bound: 3,
        },
        other_checks: &[CfgCheck::byte_max(0x1c, 2), CfgCheck::byte_max(0x21, 3)],
    };
    const _: () = CFG.validate();

    const CFG_WIDE: CfgFile = CfgFile {
        file_name: "wide.cfg",
        magic: 0x0abc_0001,
        size: 0x30,
        frameskip: ByteField {
            offset: 0x10,
            bound: 3,
        },
        other_checks: &[CfgCheck::dword_eq(0x4, 0x30), CfgCheck::sdword_max(0x8, 10)],
    };
    const _: () = CFG_WIDE.validate();

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn image(cfg: &CfgFile, frameskip: u8) -> Vec<u8> {
        let mut bytes = vec![0u8; cfg.size as usize];
        bytes[..4].copy_from_slice(&cfg.magic.to_le_bytes());
        for check in cfg.other_checks {
            if let CfgCheck::DwordEq { offset, value } = *check {
                bytes[offset as usize..offset as usize + 4].copy_from_slice(&value.to_le_bytes());
            }
        }
        bytes[cfg.frameskip.offset as usize] = frameskip;
        bytes
    }

    #[test]
    fn cfg_path_checking() {
        for path in [
            "game.cfg",
            "GAME.CFG",
            r"C:\users\user\AppData\Roaming\ShanghaiAlice\game\game.cfg",
            r"\\?\Z:\games\game\game.cfg",
            "./game.cfg",
        ] {
            assert!(is_cfg_path(&to_wide(path), CFG.file_name), "{path}");
        }
        for path in ["xgame.cfg", "game.cfg.bak", "game.cf", "score.dat", ""] {
            assert!(!is_cfg_path(&to_wide(path), CFG.file_name), "{path}");
        }
    }

    #[test]
    fn magic_gate() {
        assert!(has_magic(&CFG, &image(&CFG, 0)));
        let mut wrong = image(&CFG, 0);
        wrong[0] ^= 0xff;
        assert!(!has_magic(&CFG, &wrong));
        assert!(!has_magic(&CFG, &[]));
    }

    #[test]
    fn only_force_validator_accepted_values() {
        assert_eq!(read_verdict(&CFG, &image(&CFG, 0)), ReadVerdict::AlreadyOff);
        assert_eq!(read_verdict(&CFG, &image(&CFG, 1)), ReadVerdict::Force(1));
        assert_eq!(read_verdict(&CFG, &image(&CFG, 2)), ReadVerdict::Force(2));
        assert_eq!(
            read_verdict(&CFG, &image(&CFG, 3)),
            ReadVerdict::Reject { offset: 0x20 }
        );
        assert_eq!(
            read_verdict(&CFG, &image(&CFG, 0xff)),
            ReadVerdict::Reject { offset: 0x20 }
        );
    }

    #[test]
    fn reject_force_if_check_fails() {
        let mut bad_other_field = image(&CFG, 2);
        bad_other_field[0x1c] = 2;
        assert_eq!(
            read_verdict(&CFG, &bad_other_field),
            ReadVerdict::Reject { offset: 0x1c }
        );
    }

    #[test]
    fn sdword_max_check() {
        let mut negative = image(&CFG_WIDE, 2);
        negative[0x8..0xc].copy_from_slice(&(-1i32).to_le_bytes());
        assert_eq!(read_verdict(&CFG_WIDE, &negative), ReadVerdict::Force(2));

        let mut low_byte_small = image(&CFG_WIDE, 2);
        low_byte_small[0x8..0xc].copy_from_slice(&0x0000_0105u32.to_le_bytes());
        assert_eq!(
            read_verdict(&CFG_WIDE, &low_byte_small),
            ReadVerdict::Reject { offset: 0x8 }
        );
    }

    #[test]
    fn dword_eq_check() {
        let mut wrong_header = image(&CFG_WIDE, 2);
        wrong_header[0x4] = 0x31;
        assert_eq!(
            read_verdict(&CFG_WIDE, &wrong_header),
            ReadVerdict::Reject { offset: 0x4 }
        );
    }

    #[test]
    fn write_back_restore_frameskip_only() {
        let mut forced = image(&CFG, 2);
        forced[CFG.frameskip.offset as usize] = 0;
        forced[5] = 0xab;

        let mut restored = forced.clone();
        restore_in_place(&CFG, &mut restored, 2).unwrap();

        let mut expected = forced;
        expected[CFG.frameskip.offset as usize] = 2;
        assert_eq!(restored, expected);
    }

    #[test]
    fn write_back_preserve_bytes_changed_by_game() {
        assert_eq!(
            restore_in_place(&CFG, &mut image(&CFG, 1), 2),
            Err("byte_not_forced")
        );
    }
}
