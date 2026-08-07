ship_toolchain := "nightly-2026-08-07"
ship_build_std := "-Zbuild-std=std,panic_abort"
ship_rustflags := "-Zunstable-options -Cpanic=immediate-abort -Clink-arg=-Wl,--enable-stdcall-fixup"

# 1 = run a copy of the game with the PE large-address-aware flag set
laa := env("NEOPATCH_LAA", "0")

build game:
    cargo build -p neopatch_{{game}} --release
    cp target/i686-pc-windows-gnu/release/neopatch_{{game}}.dll sandbox/games/{{game}}/dinput8.dll

build-ship game:
    RUSTFLAGS="{{ship_rustflags}}" cargo +{{ship_toolchain}} build -p neopatch_{{game}} --release {{ship_build_std}}
    cp target/i686-pc-windows-gnu/release/neopatch_{{game}}.dll sandbox/games/{{game}}/dinput8.dll

_test game:
    cargo +{{ship_toolchain}} test -p neopatch_{{game}} --release

test:
    cargo +{{ship_toolchain}} test --workspace --release

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

release:
    #!/usr/bin/env bash
    set -euo pipefail
    out="target/release-packages"
    rm -rf "${out}/neopatch" "${out}/neopatch.zip"
    for game in th06 th07 th08 th10 th11 th12 th128 th13 th14 th15 th16 th17 th18 th20; do
        name="neopatch_${game}"
        RUSTFLAGS="{{ship_rustflags}}" cargo +{{ship_toolchain}} build -p "${name}" --release {{ship_build_std}}
        mkdir -p "${out}/neopatch/${game}"
        cp "target/i686-pc-windows-gnu/release/${name}.dll" "${out}/neopatch/${game}/dinput8.dll"
        cp "${name}/neopatch.ini.example" "${out}/neopatch/${game}/neopatch.ini"
    done
    (cd "${out}" && zip -qr "neopatch.zip" "neopatch/")
    echo "Created ${out}/neopatch.zip"
