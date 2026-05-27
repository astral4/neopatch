# Changelog

All notable changes to neopatch will be documented in this file.

## [Unreleased]

### Changed

- Session log writing now accounts for UAC virtualization. neopatch first tries `<install>\neopatch_logs\`, then `%LOCALAPPDATA%\neopatch_logs\`, and finally `%TEMP%\neopatch_logs\`.

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
