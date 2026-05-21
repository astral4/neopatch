# Changelog

All notable changes to neopatch will be documented in this file.

## [Unreleased]

### Added

- Support for Touhou 10 ~ Mountain of Faith (`th10.exe v1.00a`).

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
