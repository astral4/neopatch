# Changelog

All notable changes to neopatch will be documented in this file.

## [Unreleased]

### Added

- Support for Touhou 17 ~ Wily Beast and Weakest Creature (`th17.exe v1.00b`).

### Fixed

- Touhou 10, Touhou 11, Touhou 12, Touhou 13, Touhou 14, Touhou 15, and Touhou 16 no longer deadlock from an I/O loader synchronization fix. This resolves a regression introduced in version 0.7.0.

## [0.8.0] - 2026-05-29

### Added

- Support for Touhou 16 ~ Hidden Star in Four Seasons (`th16.exe v1.00a`).

## [0.7.0] - 2026-05-28

### Added

- Support for Touhou 14 ~ Double Dealing Character (`th14.exe v1.00b`).

### Changed

- D3D9 hooks now defend against downstream IAT hijacks of `Direct3DCreate9` for Touhou 10, 11, and 12. Previously, only Touhou 13 and 15 had this protection.
- D3D9 device tunables (frame latency cap, GPU thread priority) are now reapplied after a successful `Reset`/`ResetEx` invocation. Previously, they were assumed to persist across swap chain reinitialization, which holds for D3D9Ex but isn't guaranteed across translation layers.

### Fixed

- Replay speed control now works for Touhou 12.
- Touhou 10 screenshot capture no longer silently drops a still-pending screenshot if the screenshot key fires twice in quick succession.
- Touhou 10 screenshot capture no longer captures the wrong frame if the D3D9 device is recreated between the trigger and the next `Present` invocation.
- Touhou 10, Touhou 11, Touhou 12, and Touhou 15 no longer race their BGM-init and I/O loader threads at startup. Touhou 13 already had this fix.
- Touhou 11, Touhou 12, and Touhou 13 no longer risk deadlocking at scene transitions when the `AsciiInf` text-renderer destructor blocks main while its worker thread is still preloading `.anm` assets. Touhou 15 already had this fix.

## [0.6.0] - 2026-05-27

### Added

- Support for Touhou 13 ~ Ten Desires (`th13.exe v1.00c`).

### Changed

- Session log writing now accounts for UAC virtualization. neopatch first tries `<install>\neopatch_logs\`, then `%LOCALAPPDATA%\neopatch_logs\`, and finally `%TEMP%\neopatch_logs\`.

### Fixed

- Screenshot functionality is fixed across all games. This resolves a regression introduced in version 0.1.0.

## [0.5.1] - 2026-05-23

### Fixed

- `A8R8G8B8` (with transparency) backbuffer format usage is no longer forcibly converted to `X8R8G8B8` (without transparency). This resolves a regression introduced in version 0.3.0.

## [0.5.0] - 2026-05-23

### Added

- Support for Touhou 12 ~ Undefined Fantastic Object (`th12.exe v1.00b`).

### Fixed

- Z (depth) coordinates of certain sprites and transformation matrices in the vanilla games are now computed correctly.

## [0.4.1] - 2026-05-23

### Changed

- More detailed thread information is logged when the process becomes stuck before the render thread has been identified.

### Fixed

- Launching a game via the [thprac](https://github.com/touhouworldcup/thprac) launcher no longer causes an abort.
- MMCSS "Games" task registration is now applied to the render thread regardless of which thread loaded neopatch. Previously, under loaders that inject via `CreateRemoteThread` (e.g. the thprac launcher), the registration was applied to the short-lived injection thread and lost as soon as that thread exited.

## [0.4.0] - 2026-05-22

### Added

- Support for Touhou 11 ~ Subterranean Animism (`th11.exe v1.00a`).
- Support for D-pad input from controllers. The vanilla games only read the analog stick, so the D-pad on modern gamepads was previously silently dropped.

### Changed

- Log write operations are now unbuffered, so a process abort no longer silently truncates the session log. Every event that completes before the panic/abort should be on disk.

### Fixed

- Recapturing vtable slots no longer aborts the process.
- Reinstalling vtable intercepts no longer aborts the process.

## [0.3.0] - 2026-05-20

### Added

- Support for Touhou 10 ~ Mountain of Faith (`th10.exe v1.00a`).

### Changed

- Color depth is now normalized to 32-bit regardless of what the game requests.

## [0.2.0] - 2026-05-17

### Added

- Patch verification: every patch now compares the bytes at its target address to the expected pattern before writing. In the case of a mismatch, the patch is not applied and the mismatch is logged.

### Changed

- Session log directories are now named `YYYYMMDD_HHMMSS_pPID`. Two concurrent launches in the same second no longer overwrite each other's logs.
- `[log] sessions_to_keep = 0` now falls back to the default of `10` instead of being treated as `1`. To disable logging entirely, use `level = off`.

### Fixed

- When the number of logged sessions reaches the configured limit, neopatch now only deletes directories with the expected naming structure. User files dropped into the log root are preserved.
- Crash diagnostics no longer risk deadlocking when a fault occurs during a log write.

## [0.1.0] - 2026-05-16

### Added

- Initial release!
