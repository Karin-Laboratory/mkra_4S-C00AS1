#!/bin/sh
# Cross-build the device binary from macOS without Docker or a cross gcc.
#
# The crate is pure Rust with no C dependencies, so rust-lld links it directly.
# Rust's musl targets default to link-self-contained, which supplies crt1.o and
# libc.a from the bundled musl, so no sysroot is needed.
#
# Settings live here rather than in .cargo/config.toml so that a checkout has
# no hidden build state.
set -eu

TARGET=armv7-unknown-linux-musleabihf
cd "$(dirname "$0")"

export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=rust-lld
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_RUSTFLAGS="-C target-feature=+crt-static -C link-arg=--gc-sections"

cargo build --release --target "$TARGET" "$@"

BIN="target/$TARGET/release/mkra"
echo
ls -l "$BIN"
file "$BIN" 2>/dev/null || true
if command -v /opt/homebrew/opt/llvm/bin/llvm-readelf >/dev/null 2>&1; then
    echo "--- dynamic dependencies (must be none) ---"
    /opt/homebrew/opt/llvm/bin/llvm-readelf -d "$BIN" 2>/dev/null | grep -i needed || echo "  (none: static)"
fi
if command -v md5 >/dev/null 2>&1; then
    echo "--- md5 (compare against the device after upload) ---"
    md5 -q "$BIN"
fi
