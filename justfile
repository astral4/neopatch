build game:
    cargo build -p neopatch_{{game}} --release
    cp target/i686-pc-windows-gnu/release/neopatch_{{game}}.dll sandbox/games/{{game}}/dinput8.dll

_test game:
    cargo +nightly test -p neopatch_{{game}} --release -Zpanic-abort-tests

test: (_test "core") (_test "th10") (_test "th11") (_test "th12") (_test "th128") (_test "th13") (_test "th14") (_test "th15") (_test "th16") (_test "th17") (_test "th18") (_test "th20")

_clippy game:
    cargo clippy -p neopatch_{{game}} --release --all-targets -- -D warnings

clippy: (_clippy "core") (_clippy "th10") (_clippy "th11") (_clippy "th12") (_clippy "th128") (_clippy "th13") (_clippy "th14") (_clippy "th15") (_clippy "th16") (_clippy "th17") (_clippy "th18") (_clippy "th20")

doc:
    cargo doc --no-deps --workspace

fmt:
    cargo fmt --all

clean:
    cargo clean

run game:
    #!/usr/bin/env bash
    set -euo pipefail
    just build {{game}}
    cd sandbox/games/{{game}}
    WINEDLLOVERRIDES="mscoree=,mshtml=,winemenubuilder.exe=d" wine {{game}}.exe

release:
    #!/usr/bin/env bash
    set -euo pipefail
    out="target/release-packages"
    rm -rf "${out}/neopatch" "${out}/neopatch.zip"
    for game in th10 th11 th12 th128 th13 th14 th15 th16 th17 th18 th20; do
        name="neopatch_${game}"
        cargo build -p "${name}" --release
        mkdir -p "${out}/neopatch/${game}"
        cp "target/i686-pc-windows-gnu/release/${name}.dll" "${out}/neopatch/${game}/dinput8.dll"
        cp "${name}/neopatch.ini.example" "${out}/neopatch/${game}/neopatch.ini"
    done
    (cd "${out}" && zip -qr "neopatch.zip" "neopatch/")
    echo "Created ${out}/neopatch.zip"
