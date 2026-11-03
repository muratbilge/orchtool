#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."

export CARGO_BUILD_JOBS=2
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
command -v cargo >/dev/null || { echo 'Rust is required: install stable rustup with rustfmt and clippy.' >&2; exit 127; }
# One lock covers the entire check, including every compilation below.
exec 9>/tmp/claude-1000/hardio-heavy.lock
flock 9
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
# The end-to-end tests run the real orch / orch-mcp binaries: build them first, or they test
# whatever stale binary is in target/debug.
cargo build --workspace --bins
cargo test --workspace --all-targets
