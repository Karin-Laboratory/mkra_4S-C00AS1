#!/bin/sh
# Test the dependency-free source, then use the same ARM build as CI.
set -eu
cd "$(dirname "$0")/.."
cargo test --locked
./build-device.sh --locked
sha256sum target/armv7-unknown-linux-musleabihf/release/mkra
