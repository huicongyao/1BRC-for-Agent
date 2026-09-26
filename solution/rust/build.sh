#!/usr/bin/env bash
# Build the Rust track. Must produce ./target/release/onebrc relative to this
# directory. You may edit profile flags in Cargo.toml within the rules in AGENTS.md.
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release --offline
