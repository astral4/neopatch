# neopatch

neopatch is a Touhou game mod for end-to-end latency reductions, optimizations, and other fixes.

Currently supported: Touhou 10 (`th10.exe v1.00a`), Touhou 11 (`th11.exe v1.00a`), Touhou 12 (`th12.exe v1.00b`), Touhou 13 (`th13.exe v1.00c`), Touhou 14 (`th14.exe v1.00b`), and Touhou 15 (`th15.exe v1.00b`). Support for more games is planned for the near future.

## Usage

neopatch ships as a DLL file. The file should be named `dinput8.dll` and placed in the game directory alongside the game executable.

neopatch is configured through a `neopatch.ini` file, which should also be placed in the game directory. Please check the per-game example files to see what's possible with neopatch!

- Touhou 10: [`neopatch.ini.example`](neopatch_th10/neopatch.ini.example)
- Touhou 11: [`neopatch.ini.example`](neopatch_th11/neopatch.ini.example)
- Touhou 12: [`neopatch.ini.example`](neopatch_th12/neopatch.ini.example)
- Touhou 13: [`neopatch.ini.example`](neopatch_th13/neopatch.ini.example)
- Touhou 14: [`neopatch.ini.example`](neopatch_th14/neopatch.ini.example)
- Touhou 15: [`neopatch.ini.example`](neopatch_th15/neopatch.ini.example)

neopatch is not compatible with similar mods like vpatch and OpenInputLagPatch (OILP). Attempting to use neopatch with them may cause hard-to-troubleshoot issues.

## Development

neopatch is cross-compiled from Linux to `i686-pc-windows-gnu`. Build hosts need a mingw-w64 i686 toolchain. [Wine](https://www.winehq.org/) is used as the Cargo test runner.

An example with the [`neopatch_th15`](neopatch_th15/) crate:

```
cargo build -p neopatch_th15 --target i686-pc-windows-gnu --release
```

See [`justfile`](justfile) for commands you can run via [`just`](https://github.com/casey/just).

## Acknowledgements

neopatch would not be possible without these projects:

- [OpenInputLagPatch](https://github.com/khang06/OpenInputLagPatch) by Khangaroo
- vpatch by swmpLV/75E

And special thanks to all pre-release testers!

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or https://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or https://opensource.org/licenses/MIT)

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
