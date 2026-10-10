#!/usr/bin/env bash
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
export CARGO_HOME="$repo/compositor/cargo-home"
export PKG_CONFIG_PATH="$repo/lab/sysroot/usr/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
if [[ -f "$repo/lab/sysroot/usr/lib/pkgconfig/libseat.pc" ]]; then
    # Relocate the extracted package, never install it on the host.
    sed -i "s|^prefix=.*|prefix=$repo/lab/sysroot/usr|" "$repo/lab/sysroot/usr/lib/pkgconfig/libseat.pc"
fi
cd "$repo/compositor/upstream"
cargo build --locked --no-default-features "$@"
