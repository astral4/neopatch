build game:
    cargo build -p neopatch_{{game}} --release
    cp target/i686-pc-windows-gnu/release/neopatch_{{game}}.dll sandbox/games/{{game}}/dinput8.dll

_test game:
    cargo +nightly test -p neopatch_{{game}} --release -Zpanic-abort-tests

test:
    cargo +nightly test --workspace --release -Zpanic-abort-tests

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
    WINEDLLOVERRIDES="mscoree=,mshtml=,winemenubuilder.exe=d" wine "${exe}.exe"

release:
    #!/usr/bin/env bash
    set -euo pipefail
    out="target/release-packages"
    rm -rf "${out}/neopatch" "${out}/neopatch.zip"
    for game in th06 th10 th11 th12 th128 th13 th14 th15 th16 th17 th18 th20; do
        name="neopatch_${game}"
        cargo build -p "${name}" --release
        mkdir -p "${out}/neopatch/${game}"
        cp "target/i686-pc-windows-gnu/release/${name}.dll" "${out}/neopatch/${game}/dinput8.dll"
        cp "${name}/neopatch.ini.example" "${out}/neopatch/${game}/neopatch.ini"
    done
    (cd "${out}" && zip -qr "neopatch.zip" "neopatch/")
    echo "Created ${out}/neopatch.zip"
