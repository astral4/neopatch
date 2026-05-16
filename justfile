build-th15:
    cargo build -p neopatch_th15 --target i686-pc-windows-gnu --release
    mv target/i686-pc-windows-gnu/release/dinput8.dll sandbox/games/th15/dinput8.dll

test: test-th15

test-th15:
    cargo test -p neopatch_th15 --target i686-pc-windows-gnu --release

fmt:
    cargo fmt --all

clippy: clippy-th15

clippy-th15:
    cargo clippy -p neopatch_th15 --target i686-pc-windows-gnu --release --all-targets -- -D warnings

clean:
    cargo clean
