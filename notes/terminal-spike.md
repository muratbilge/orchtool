# Terminal spike — prepared, execution BLOCKED

2026-11-02: Rust is absent and the Rust download host cannot resolve in the build
environment. No vt100 test or real-application compatibility result is claimed.

Prepared `crates/orchd/examples/terminal_spike.rs`: reads raw output in chunks,
feeds vt100, prints the final screen and reports cursor, alternate-screen,
bracketed-paste and mouse states. Unit tests cover alternate-screen restoration,
fragmented UTF-8 with wide/combining characters, mode transitions and resize.
This is a replay harness, not yet an interactive PTY emulator. It cannot validate
terminal query replies or interactive input forwarding by itself.

The current [vt100 Parser API](https://docs.rs/vt100/latest/vt100/struct.Parser.html)
uses nonzero dimensions and supports callbacks. Its
[Screen API](https://docs.rs/vt100/latest/vt100/struct.Screen.html) exposes modes
and screen state. Those APIs informed the source; they are not execution evidence.

## Required execution before GREEN

1. Install stable rustup (minimal profile, rustfmt, clippy), resolve dependencies
   and retain Cargo.lock. Run `scripts/check.sh` under its shared build lock.
2. Run the example tests and add a tiny portable-pty driver to exercise actual
   bash, vim, claude and codex; inspect available CLI flags before invocation.
   Use a scratch cwd, preserve existing agent sessions, and do not submit tasks.
3. For each app, record installed version, argv, terminal dimensions, initial
   rendered screen, redraw after resize and exit cleanup. Exercise interactive
   prompt/editor behavior, not just `--version` output. Exercise Unicode, paste
   and mouse behavior where supported. Do not commit credentials/session text.
4. Capture terminal queries (cursor position, device attributes, colors and
   other queries actually emitted). Verify that the adapter responds correctly,
   including fragmented sequences; replay parsing alone does not test this.
5. Re-run regressions on captured sanitized output and record failures here.
   Compare alacritty_terminal only if vt100 exposes a material compatibility gap.

Planned commands after toolchain setup:

```sh
scripts/check.sh
CARGO_BUILD_JOBS=2 flock /tmp/claude-1000/hardio-heavy.lock cargo test -p orchd --example terminal_spike
CARGO_BUILD_JOBS=2 flock /tmp/claude-1000/hardio-heavy.lock cargo run -p orchd --example terminal_spike < /path/to/sanitized-raw-output
```

**Verdict pending:** vt100 remains the approved provisional choice behind an
adapter. There is insufficient experimental evidence to accept it for Phase 1
or to justify switching to alacritty_terminal. bash/vim/claude/codex live runs,
query-response behavior and memory measurements are all outstanding.
