# Changelog

All notable changes to neopatch will be documented in this file.

## [Unreleased]

### Changed

- Session log directories are now named `YYYYMMDD_HHMMSS_pPID`. Two concurrent launches in the same second no longer overwrite each other's logs.

### Fixed

- When the number of logged sessions reaches the configured limit, neopatch now only deletes directories with the expected naming structure. User files dropped into the log root are preserved.
- Crash diagnostics no longer risk deadlocking when a fault occurs during a log write.

## [0.1.0] - 2026-05-16

### Added

- Initial release!
