# Changelog

All notable changes to neopatch will be documented in this file.

## [Unreleased]

### Fixed

- `neopatch_logs/` retention sweep now only deletes directories created
  by neopatch; user files dropped into the log root are preserved.
- Crash diagnostics no longer risk hanging when a fault occurs during a
  log write.

## [0.1.0] - 2026-05-16

### Added

- Initial release!
