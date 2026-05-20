build-th15:
    cargo build -p neopatch_th15 --target i686-pc-windows-gnu --release
    mv target/i686-pc-windows-gnu/release/dinput8.dll sandbox/games/th15/dinput8.dll

_release game:
    #!/usr/bin/env bash
    set -euo pipefail
    out="target/release-packages"
    name="neopatch_{{game}}"
    cargo build -p ${name} --target i686-pc-windows-gnu --release
    rm -rf "${out}/${name}" "${out}/${name}.zip"
    mkdir -p "${out}/${name}"
    cp target/i686-pc-windows-gnu/release/${name}.dll "${out}/${name}/dinput8.dll"
    cp ${name}/neopatch.ini.example "${out}/${name}/neopatch.ini"
    (cd "${out}" && zip -qr "${name}.zip" "${name}/")
    echo "Created ${out}/${name}.zip"

release-th15: (_release "th15")

release: release-th15

test: test-th15

test-th15:
    cargo test -p neopatch_th15 --target i686-pc-windows-gnu --release

fmt:
    cargo fmt --all

clippy: clippy-th15

clippy-th15:
    cargo clippy -p neopatch_th15 --target i686-pc-windows-gnu --release --all-targets -- -D warnings

doc:
    cargo doc --no-deps --workspace --target i686-pc-windows-gnu

clean:
    cargo clean
