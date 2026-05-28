build game:
    cargo build -p neopatch_{{game}} --target i686-pc-windows-gnu --release
    cp target/i686-pc-windows-gnu/release/neopatch_{{game}}.dll sandbox/games/{{game}}/dinput8.dll

_test game:
    cargo test -p neopatch_{{game}} --target i686-pc-windows-gnu --release

test: (_test "core") (_test "th10") (_test "th11") (_test "th12") (_test "th13") (_test "th14") (_test "th15")

_clippy game:
    cargo clippy -p neopatch_{{game}} --target i686-pc-windows-gnu --release --all-targets -- -D warnings

clippy: (_clippy "core") (_clippy "th10") (_clippy "th11") (_clippy "th12") (_clippy "th13") (_clippy "th14") (_clippy "th15")

doc:
    cargo doc --no-deps --workspace --target i686-pc-windows-gnu

fmt:
    cargo fmt --all

clean:
    cargo clean

run game:
    #!/usr/bin/env bash
    set -euo pipefail
    just build {{game}}
    cd sandbox/games/{{game}}
    WINEDLLOVERRIDES="mscoree=,mshtml=" wine {{game}}.exe

_release game:
    #!/usr/bin/env bash
    set -euo pipefail
    out="target/release-packages"
    name="neopatch_{{game}}"
    cargo build -p ${name} --target i686-pc-windows-gnu --release
    rm -rf "${out}/${name}" "${out}/${name}.zip"
    mkdir -p "${out}/${name}"
    cp "target/i686-pc-windows-gnu/release/${name}.dll" "${out}/${name}/dinput8.dll"
    cp ${name}/neopatch.ini.example "${out}/${name}/neopatch.ini"
    (cd "${out}" && zip -qr "${name}.zip" "${name}/")
    echo "Created ${out}/${name}.zip"

release: (_release "th10") (_release "th11") (_release "th12") (_release "th13") (_release "th14") (_release "th15")
