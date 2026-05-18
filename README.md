# neopatch

neopatch is a Touhou game mod for input-to-display latency reductions, optimizations, and other fixes.

Currently, only Touhou 15 (`th15.exe v1.00b`) is supported. Support for more games is planned for the near future.

## Usage

neopatch ships as a DLL file. The file should be named `dinput8.dll` and placed in the game directory alongside the game executable.

neopatch is configured through a `neopatch.ini` file, which should also be placed in the game directory. Please see [`neopatch.ini.example`](neopatch.ini.example) for information on all of the settings and what's possible with neopatch!

neopatch is not compatible with similar mods like vpatch and OpenInputLagPatch (OILP). Attempting to use neopatch with them may cause hard-to-troubleshoot issues.

## Development

neopatch is cross-compiled from Linux to `i686-pc-windows-gnu`. Build hosts need a mingw-w64 i686 toolchain. [Wine](https://www.winehq.org/) is used as the Cargo test runner. 

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
