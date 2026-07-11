build game:
    cargo build -p neopatch_{{game}} --release
    cp target/i686-pc-windows-gnu/release/neopatch_{{game}}.dll sandbox/games/{{game}}/dinput8.dll

_test game:
    cargo +nightly test -p neopatch_{{game}} --release -Zpanic-abort-tests

test: (_test "core") (_test "th10") (_test "th11") (_test "th12") (_test "th13") (_test "th14") (_test "th15") (_test "th16") (_test "th17") (_test "th18") (_test "th20")

_clippy game:
    cargo clippy -p neopatch_{{game}} --release --all-targets -- -D warnings

clippy: (_clippy "core") (_clippy "th10") (_clippy "th11") (_clippy "th12") (_clippy "th13") (_clippy "th14") (_clippy "th15") (_clippy "th16") (_clippy "th17") (_clippy "th18") (_clippy "th20")

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

_release game:
    #!/usr/bin/env bash
    set -euo pipefail
    out="target/release-packages"
    name="neopatch_{{game}}"
    cargo build -p ${name} --release
    rm -rf "${out}/${name}" "${out}/${name}.zip"
    mkdir -p "${out}/${name}"
    cp "target/i686-pc-windows-gnu/release/${name}.dll" "${out}/${name}/dinput8.dll"
    cp ${name}/neopatch.ini.example "${out}/${name}/neopatch.ini"
    (cd "${out}" && zip -qr "${name}.zip" "${name}/")
    echo "Created ${out}/${name}.zip"

release: (_release "th10") (_release "th11") (_release "th12") (_release "th13") (_release "th14") (_release "th15") (_release "th16") (_release "th17") (_release "th18") (_release "th20")
