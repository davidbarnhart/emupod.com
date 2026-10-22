#!/bin/sh
# Build the Rust core's Python module (emupod/emupod_rs.abi3.so), which emupod.emu_rs loads.
set -e
cd "$(dirname "$0")"
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release -p emupod-rs
cp target/release/libemupod_rs.so ../emupod/emupod_rs.abi3.so
