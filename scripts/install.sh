#!/usr/bin/env bash
# Build release binaries and copy them to ~/.local/bin (on PATH; herdr lives there too).
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
export CARGO_BUILD_JOBS=2
flock /tmp/claude-1000/hardio-heavy.lock cargo build --release -p orchd -p orch
install -d "$HOME/.local/bin"
for b in orchd orch; do
  install -m 755 "target/release/$b" "$HOME/.local/bin/$b.new"
  mv -f "$HOME/.local/bin/$b.new" "$HOME/.local/bin/$b"
done
echo "installed: $(~/.local/bin/orchd --version) and orch into ~/.local/bin"
echo "note: a running orchd keeps the old binary until 'orch stop-daemon' (that kills its panes)."
