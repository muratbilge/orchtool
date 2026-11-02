# HANDOFF — Codex, phases 0 + 1 (draft 3, 2026-11-02)

Read PLAN.md (phase table "draft 3"), your own CODEX-OPINION.md (its engineering defaults are now accepted),
notes/inventory.md, notes/research.md. Work in /projects/orchtool.

Murat's goal: manage ALL his agents in Rust — like herdr but better and faster — and let them talk to each other,
report to him and ask him. Build toward that; keep each step small and working.

## Rules
- Rust stable via rustup (user-level, no sudo). Cargo builds: `CARGO_BUILD_JOBS=2`, wrapped in
  `flock /tmp/claude-1000/hardio-heavy.lock …` (RAM is tight; lock shared with gradle builds).
- Small commits, clear messages. Tests for every behaviour. Don't touch herdr or its running server.
- Progress + anything Murat must decide go into STATUS.md (top = latest). Phase done → mark `GREEN` with the exact
  commands run and their result. Then stop and say "PHASE N GREEN".
- Your weekly limit is low (~24%): prefer finishing a phase cleanly over starting the next; write STATUS.md often so
  a stand-in can continue.

## Phase 0 — toolchain, skeleton, spikes
1. rustup (stable, minimal + rustfmt + clippy). `git init`, .gitignore (target/).
2. Cargo workspace: `orch-proto`, `orchd`, `orch` (bin), `orch-mcp` (bin, stub). tokio, versioned JSON-lines protocol.
3. `scripts/check.sh`: fmt --check, clippy -D warnings, test — under the flock + JOBS=2.
4. Spike A → notes/kabuk-contract.md: how Kabuk really talks to herdr (find its source/watcher on DevBox, e.g.
   search /projects and ~/bin for kabuk); framing, envelopes, ids, fields, errors, events, reconnect. Read-only.
5. Spike B → notes/terminal-spike.md: vt100 in a tiny test harness with bash, vim, `claude`, `codex` (alt screen,
   unicode width, bracketed paste, mouse, resize, terminal query replies). Verdict vt100 vs alacritty_terminal.

## Phase 1 — PTY core (slim)
- orchd: Unix socket `$XDG_RUNTIME_DIR/orchtool/orchd.sock` (fallback `~/.local/state/orchtool/`), mode 0600.
  Spawn panes with portable-pty (cwd, env, argv; blocking reads off the tokio scheduler), terminal state behind an
  adapter (vt100 unless spike B says otherwise), bounded scrollback, resize, exit status, kill.
- One controlling client (input + size) + observers, explicit takeover; detached panes keep their last size.
- Attach: full snapshot, then diffs carrying pane generation, dims, base + result revision; resnapshot on skipped
  base or resize; coalesce (start ~25 fps), bounded outgoing bytes, slow client → resnapshot.
- `orch` CLI: `new [--name N] [--cwd D] -- argv`, `ls`, `attach <pane>` (detach Ctrl-b d), `send <pane> <text> [--enter]`,
  `read <pane> [--lines N]`, `kill <pane>`, `proxy` (stdio ⇄ socket for `ssh host orch proxy`).
- systemd --user unit for orchd; verify panes survive SSH logout.
- Tests: diff/revision unit tests incl. skipped frames + resize races; integration: spawn orchd, `sh -c 'echo hi'`,
  read "hi"; detach/reattach keeps state; two clients identical; reconnect resnapshots.
- Done when: bash, vim, claude and codex run in orch panes, survive detach, two terminals mirror. Measure in
  STATUS.md: orchd RSS idle with 5 panes and with full scrollback, attach latency (local and via ssh proxy).
