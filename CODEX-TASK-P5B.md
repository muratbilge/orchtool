# Codex task P5B: resumed agents carry on after `orch stop-daemon --restart`

Work in /projects/orchtool (main, at 1765ea3 or later). Same rules as P5A: run `scripts/check.sh`
before each commit (it takes the build lock itself), commit locally with prefix `[codex]`, don't
push, never touch or restart the live orchd (`~/.local/state/orchtool/orchd.sock`). For live tests use
a private daemon: `HOME=$D/home orchd --socket $D/s.sock --db $D/db`, and always `orch --socket`.
If .git or sockets are blocked in your sandbox again, do as in P5A: commits in a private repo +
`/tmp/orch-p5b-series.patch`, and say so in the result file. Claude imports them and runs the full checks.

## Problem

`orch stop-daemon --restart` saves the layout (`crates/orchd/src/restore.rs`, `SavedPane`) and the new
orchd brings agents back (`restart_spec`: `claude --continue` / `--resume <id>`, `codex resume <id>`).
But a resumed agent only reloads its conversation: a turn that was running when orchd stopped is
cut off ("Tool call interrupted"), and the agent then idles until Murat types. Murat restarts from
his phone and expects working agents to keep working.

## Wanted

1. At save time (`snapshot_layout`), record per pane whether its agent was **working** (busy, not
   idle at its prompt / not waiting for an answer). Use the existing screen detection
   (`detect.rs`, the same status `orch watch` shows). New `SavedPane` field with `#[serde(default)]`
   so old restore.json files still load.
2. After `restore_layout`, for each restored agent pane that was working: wait until the agent
   shows its idle prompt again (detection, with a timeout of ~90 s; give up silently on timeout,
   and log a line), then type a note and Enter, e.g.
   "orchd was restarted while you were working (orch stop-daemon --restart); the restart is done.
   Your last tool call was interrupted: check its outcome, then continue where you stopped."
   Use the same text/Enter path as `limits.rs` (text, short pause, `\r`), once per pane.
   Idle, limited or asking agents get nothing. Plain shells and other programs get nothing.
3. One bus message to murat (thread "restart"), e.g. "restart: resumed 3 agents, nudged 2 that were
   working (orchtool-claude-3, Orchtool · Codex)".
4. Tests: unit test for the new field's serde default; an integration test with a private daemon
   where a fake "agent" (shell script that prints a Claude/Codex-like prompt screen) was working,
   is restarted, and receives the note exactly once, while an idle one receives nothing.
   Look at `a_limited_codex_is_continued_once_after_its_reset` in `tests/panes.rs` for the pattern.

Note the session that runs `stop-daemon --restart` is itself one of the panes (Claude in p54
ran it this morning and was cut off mid-call); that case must be covered: it was working.

When done, write `CODEX-REVIEW-P5B-RESULT.md`: commits, what you tested, anything open.
Keep it short.
