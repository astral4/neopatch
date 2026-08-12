triple := "i686-pc-windows-gnu"
ship_toolchain := "nightly-2026-08-12"
ship_build_std := "-Zbuild-std=std,panic_abort"
ship_rustflags := "-Zunstable-options -Cpanic=immediate-abort -Clink-arg=-Wl,--enable-stdcall-fixup"

# 1 = run a copy of the game with the PE large-address-aware flag set
laa := env("NEOPATCH_LAA", "0")

build game:
    cargo build -p neopatch_{{game}} --release
    cp target/{{triple}}/release/neopatch_{{game}}.dll sandbox/games/{{game}}/dinput8.dll

_ensure-ship-toolchain:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! rustup toolchain list | grep -q "^{{ship_toolchain}}-"; then
        echo "installing toolchain {{ship_toolchain}}" >&2
        rustup toolchain install {{ship_toolchain}} --profile minimal
    fi
    if ! rustup component list --installed --toolchain {{ship_toolchain}} | grep -qx rust-src; then
        echo "adding rust-src to {{ship_toolchain}}" >&2
        rustup component add rust-src --toolchain {{ship_toolchain}}
    fi
    if ! rustup target list --installed --toolchain {{ship_toolchain}} | grep -qx "{{triple}}"; then
        echo "adding {{triple}} to {{ship_toolchain}}" >&2
        rustup target add {{triple}} --toolchain {{ship_toolchain}}
    fi

build-ship game: _ensure-ship-toolchain
    RUSTFLAGS="{{ship_rustflags}}" cargo +{{ship_toolchain}} build -p neopatch_{{game}} --release {{ship_build_std}}
    cp target/{{triple}}/release/neopatch_{{game}}.dll sandbox/games/{{game}}/dinput8.dll

_test game:
    cargo +{{ship_toolchain}} test -p neopatch_{{game}} --release

test:
    cargo +{{ship_toolchain}} test --workspace --release

_miri flags:
    MIRIFLAGS="-Zmiri-ignore-leaks -Zmiri-strict-provenance -Zmiri-symbolic-alignment-check \
    -Zmiri-address-reuse-rate=1.0 -Zmiri-retag-fields {{ flags }}" \
    CARGO_UNSTABLE_PANIC_ABORT_TESTS=false \
    CARGO_TARGET_I686_PC_WINDOWS_GNU_RUNNER="cargo-miri runner" \
    cargo +nightly miri test -p neopatch_core --target {{triple}} d3d8::tests

miri: (_miri "") (_miri "-Zmiri-tree-borrows")

_clippy game:
    cargo clippy -p neopatch_{{game}} --release --all-targets -- -D warnings

clippy:
    cargo clippy --workspace --release --all-targets -- -D warnings

doc:
    cargo doc --no-deps --workspace --document-private-items

fmt:
    cargo fmt --all

clean:
    cargo clean

run game:
    #!/usr/bin/env bash
    set -euo pipefail
    just build {{game}}
    cd sandbox/games/{{game}}
    exe="{{game}}"
    if [ "${exe}" = "th06" ]; then exe="東方紅魔郷"; fi
    launch="${exe}.exe"
    if [ "{{laa}}" != "0" ]; then
        launch="${exe}_laa.exe"
        rm -f "${launch}"
        cp "${exe}.exe" "${launch}"
        # e_lfanew at 0x3c locates the PE signature; IMAGE_FILE_HEADER.Characteristics sits 22 bytes past it,
        # and bit 0x20 is IMAGE_FILE_LARGE_ADDRESS_AWARE.
        pe=$(( $(od -An -tu4 -j 60 -N 4 "${launch}") ))
        if [ "$(od -An -tx1 -j "${pe}" -N 4 "${launch}" | tr -d ' ')" != "50450000" ]; then
            echo "${exe}.exe: not a PE image" >&2
            exit 1
        fi
        off=$(( pe + 22 ))
        characteristics=$(( $(od -An -tu2 -j "${off}" -N 2 "${launch}") | 0x20 ))
        printf "\\x$(printf %02x $(( characteristics & 0xff )))\\x$(printf %02x $(( characteristics >> 8 )))" \
            | dd of="${launch}" bs=1 seek="${off}" conv=notrunc status=none
    fi
    WINEDLLOVERRIDES="mscoree=,mshtml=,winemenubuilder.exe=d" wine "${launch}"

release: _ensure-ship-toolchain
    #!/usr/bin/env bash
    set -euo pipefail
    out="target/release-packages"
    rm -rf "${out}/neopatch" "${out}/neopatch.zip"
    for name in neopatch_th*/; do
        name="${name%/}"
        game="${name#neopatch_}"
        RUSTFLAGS="{{ship_rustflags}}" cargo +{{ship_toolchain}} build -p "${name}" --release {{ship_build_std}}
        mkdir -p "${out}/neopatch/${game}"
        cp "target/{{triple}}/release/${name}.dll" "${out}/neopatch/${game}/dinput8.dll"
        cp "${name}/neopatch.ini.example" "${out}/neopatch/${game}/neopatch.ini"
    done
    (cd "${out}" && zip -qr "neopatch.zip" "neopatch/")
    echo "Created ${out}/neopatch.zip"
