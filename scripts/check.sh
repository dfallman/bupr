#!/bin/sh
# Local CI gate (spec §9).
set -e
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
