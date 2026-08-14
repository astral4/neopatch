# Changelog

All notable changes to neopatch will be documented in this file.

## [Unreleased]

### Changed

- When another tool (e.g. [thcrap](https://github.com/thpatch/thcrap) or [thprac](https://github.com/touhouworldcup/thprac)) layers its own hook over one of neopatch's between two device creations, its hook is now left in place instead of silently displacing it.
- Session-log directory names are now based on UTC instead of the local timezone.

### Fixed

- Clicking no longer drops Touhou 6 through 10 out of fullscreen when another tool has taken neopatch's window-creation hook. The window is now shown after creating the graphics device. This resolves a regression introduced in version 0.11.0.
- The `[window]` frame, position, size, `always_on_top`, and title suffix are all applied even when another tool has taken neopatch's window-creation hook.
- Screenshots now support games running in 16-bit color (`R5G6B5`, `X1R5G5B5`, `A1R5G5B5`) instead of failing to save.
- Startup dialogs no longer abort the game at launch if neopatch's import-table lookup misses.
- D3DX9 texture loads onto a non-session device no longer have their memory pool rewritten, which could leave such a device unable to recover from a lost state.

## [0.12.0] - 2026-08-12

### Added

- Support for Touhou 9.5 ~ Shoot the Bullet (`th095.exe v1.02a`).
- Support for Touhou 12.5 ~ Double Spoiler (`th125.exe v1.00a`).

### Changed

- Pressing Ctrl-C while a quit is already in progress now prints a hint to the launching terminal instead of being absorbed silently.
- A patch write that fails read-back verification now terminates the game immediately with a logged error instead of continuing partially patched.
- The number of crash minidumps written per session is no longer capped.
- Screenshot capture performance has been optimized.

### Fixed

- The in-game UI for [thprac](https://github.com/touhouworldcup/thprac) is now displayed for the Direct3D 8 games (Touhou 6, 7, 8, and 9.5) when using neopatch.
- A nonzero frameskip value (描画間隔) set in `custom.exe` no longer causes Touhou 10 onward to run at 2× or 3× speed. The setting is now forced to zero in memory for the session.
- A `log_dir` path that cannot actually be claimed no longer silently disables logging and crash dumps.
- Log-session retention now deletes only directories containing a manifest or event log.
- Configuration files saved as UTF-16 are now decoded correctly instead of having every setting silently fall back to defaults.
- Unquoted configuration values containing an apostrophe are now parsed more robustly.
- Fullscreen refresh rate selection no longer rejects NTSC-skewed values under the `Native` and `Fixed` refresh rate modes.
- DirectInput calls are now properly forwarded per interface.
- Applying D3D9Ex tunables can no longer abort the game from inside a hook.
- A failed `CreateThread` call observed by the game now reports the operating system's error code instead of one overwritten by neopatch's own logging.
- A refused timer resolution request no longer suppresses the games' own timer-resolution calls.
- Crash reports no longer fabricate a zeroed stack readout when the faulting thread's stack is unreadable.

## [0.11.0] - 2026-08-07

### Added

- Support for Touhou 7 ~ Perfect Cherry Blossom (`th07.exe v1.00b`).
- Support for Touhou 8 ~ Imperishable Night (`th08.exe v1.00d`).

### Changed

- The game window is now created directly in its final styled state instead of being restyled after creation.
- `x` and `y` under the `[window]` configuration section are now unset by default, which lets the system and window manager place the game window instead of forcing it to the top-left corner.

### Fixed

- Touhou 10 through Touhou 18 now select the game's intended Japanese font and show dialog text without mojibake on non-Japanese system locales.
- Ctrl-C signals are now handled properly and trigger graceful termination instead of writing crash minidumps.

## [0.10.0] - 2026-07-30

### Added

- Support for Touhou 6 ~ Embodiment of Scarlet Devil (`東方紅魔郷.exe v1.02h`).
- Support for Touhou 12.8 ~ Great Fairy Wars (`th128.exe v1.00a`).

### Changed

- Releases are now packaged as a single ZIP archive with one directory per game instead of a separate archive per game.
- `[window]` geometry and frame settings now apply only to windowed-mode game windows.
- Every patch site is now verified before modifying anything. If a patch site does not match, then nothing is installed and the game runs exactly as it would without neopatch.
- A 16-bit back buffer refused by the adapter is now retried as 32-bit instead of immediately failing.

### Fixed

- Fullscreen refresh rate selection is now validated for support at the chosen resolution.
- D3D9 devices are now created with `D3DCREATE_MULTITHREADED` to prevent graphics-driver crashes at scene transitions, since the games use D3D from worker threads.
- Ctrl key detection for Touhou 18 and Touhou 20 no longer introduces a polling race that can drop keypress events.
- In Touhou 18 and Touhou 20, holding Ctrl no longer fast-forwards replays while the game window is inactive.
- Non-ASCII text in window titles and dialogs now correctly renders without mojibake even when using an incompatible system locale or ANSI code page.
- neopatch no longer risks faulting from unmapped memory when the `dinput8.dll` file is loaded by a non-game executable.
- D3D objects created by other software in the same process (e.g. overlays) are no longer accidentally affected by neopatch.

### Removed

- Removed the hang watchdog.

## [0.9.1] - 2026-06-02

### Fixed

- Games now have improved compatibility with [thcrap](https://github.com/thpatch/thcrap) due to reworking startup dialog auto-dismissal.

## [0.9.0] - 2026-06-02

### Added

- Support for Touhou 17 ~ Wily Beast and Weakest Creature (`th17.exe v1.00b`).
- Support for Touhou 18 ~ Unconnected Marketeers (`th18.exe v1.00a`).
- Support for Touhou 20 ~ Fossilized Wonders (`th20.exe v1.00a`).

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
- D3D9 device tunables (frame latency cap, GPU thread priority) are now reapplied after a successful `Reset` / `ResetEx` invocation. Previously, they were assumed to persist across swap chain reinitialization, which holds for D3D9Ex but isn't guaranteed across translation layers.

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
