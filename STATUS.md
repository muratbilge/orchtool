# Status

## 2026-11-02 — Phase 0 GREEN (Claude finished the blocked steps)

Codex's sandbox had no network / home writes, so Claude installed rustup (stable 1.99.1-era cargo, minimal +
rustfmt + clippy, `~/.cargo`, PATH not modified — check.sh sources `~/.cargo/env`) and ran `git init -b main`.
- `cargo fmt --all` (fixed formatting in examples/terminal_spike.rs)
- `scripts/check.sh` → exit 0: fmt ok, clippy -D warnings ok, tests: orch-proto 3 passed, terminal_spike 3 passed.
- Spike A (notes/kabuk-contract.md): complete (source inspection).
- Spike B (notes/terminal-spike.md): synthetic vt100 tests pass; live bash/vim/claude/codex checks moved into
  Phase 1 acceptance (they need the real PTY driver anyway).

## 2026-11-02 — Scope update: Codex stops at Phase 0; validation still BLOCKED

Murat assigned phases 1 and 2 to Claude; Codex will review them later. Do not
start Phase 1 as part of this task. The Phase 0 skeleton, `scripts/check.sh`, and
both spike notes are present. Rechecked `scripts/check.sh` after this scope change:
exit 127 (`Rust is required: install stable rustup with rustfmt and clippy.`).
Rust remains absent and `.git` remains read-only. No build/test or live terminal
spike success can be reported, so Phase 0 is not marked GREEN. Claude's handoff
includes resolving these Phase 0 validation blockers before building on it.

## 2026-11-02 — Phase 0 BLOCKED (not GREEN); Phase 1 NOT STARTED

Read draft-3 HANDOFF/PLAN and accepted engineering defaults. No herdr commands,
socket connections, service changes or agent messages have been made.

- Rust/rustup/cargo are absent from PATH; ~/.cargo and ~/.rustup do not exist.
- Toolchain download probe: `curl --proto '=https' --tlsv1.2 -I --max-time 15 https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init`
  failed with curl exit 6, `Could not resolve host: static.rust-lang.org`.
- `git init -b main` failed: `.git/hooks/: Read-only file system`. The session's
  permission profile explicitly makes `.git` read-only; commits cannot be made.
- Home-directory writes are also outside this session's writable roots. A normal
  user-level rustup installation needs an environment permitting those writes.

Prepared work:

- Four-crate Cargo workspace, stable/minimal toolchain declaration, explicit binary
  stubs, Tokio daemon entry point, versioned JSON-lines envelope with framing,
  version and size tests. No PTY service is implemented yet.
- `scripts/check.sh` runs fmt, clippy with warnings denied, and tests across all
  targets; holds the shared flock and sets CARGO_BUILD_JOBS=2.
- Spike A complete as source inspection: `notes/kabuk-contract.md` covers the
  watcher, CLI actions, fields, events, errors and reconnect behavior. Found that
  socket redirection alone cannot migrate the phone's hard-coded herdr CLI actions.
- Spike B prepared: vt100 replay example and synthetic mode/Unicode/resize tests.
  `notes/terminal-spike.md` explicitly lists the unrun real-application and query
  reply checks. Emulator verdict remains provisional.

Checks actually run:

- `bash -n scripts/check.sh` — exit 0, shell syntax passes.
- `scripts/check.sh` — exit 127, Rust missing; fmt/clippy/tests did NOT run.
- Python `tomllib` manifest/member inspection — passed for workspace, toolchain
  and all four package manifests (see command below).
- `git init -b main` — exit 1, read-only `.git/hooks/`; no commits possible.
- Download probe above — curl exit 6. No installation was attempted after that
  prerequisite failed. No Cargo.lock could be generated.

Static manifest check command:

```sh
python3 - <<'PY'
from pathlib import Path
import tomllib
root = Path('.')
workspace = tomllib.loads((root / 'Cargo.toml').read_text())
tomllib.loads((root / 'rust-toolchain.toml').read_text())
for member in workspace['workspace']['members']:
    package = tomllib.loads((root / member / 'Cargo.toml').read_text())
    assert package['package']['name'] == Path(member).name
    assert (root / member / 'src').is_dir()
print('PASS: workspace, toolchain, and four package manifests')
PY
```

Continuation needs an execution environment with Rust download/dependency access,
user-level toolchain installation access, and writable repository Git metadata.
Then install stable/minimal + rustfmt/clippy, initialize Git, build/fix/check this
uncompiled scaffold, complete the live terminal spike, and commit small verified
steps. No product decision is pending. Do not mark Phase 0 GREEN merely because
both note files exist. Phase 1 and its live logout/SSH measurements must follow
successful Phase 0 validation.
