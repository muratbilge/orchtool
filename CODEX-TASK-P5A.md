# Codex task P5A: review today's work, then the polish items (orchtool)

Murat activated an extra limit reset, so you get a bigger task this time. Work in /projects/orchtool.
Run `scripts/check.sh` before every commit; it takes the build lock itself, so don't wrap it in flock.
Commit locally with the prefix `[codex]`. Don't push, and don't restart or touch the live orchd
(`~/.local/state/orchtool/orchd.sock`): it runs Murat's and other agents' panes. For live tests, start a
private daemon with its own HOME and database, e.g.
`HOME=$D/home orchd --socket $D/s.sock --db $D/db`, and always pass `--socket` to `orch`. Without it,
`orch` autostarts an orchd that uses the real database. Kill only daemons you started.

## Part A — review (write CODEX-REVIEW-P5A-RESULT.md, max ~12 findings, P1/P2 only)

Review the orchtool commits from 359282c to 1f30b5f (`git log 359282c^..1f30b5f`). Focus on:
- `crates/orchd/src/restore.rs` and `main.rs`: saving inside `daemon.stop{restart}`, read-once
  restore, `restart_spec` (an agent in a shell is resumed in that shell with `ORCH_AGENT`).
- `crates/orch-proto/src/env.rs`: merging in the login-shell environment; its use in
  `orchd/src/pane.rs` and `orch/src/main.rs` `client_env`. Also 0cb8f45, which strips the
  agent-session variables.
- `crates/orch-proto/src/api.rs` `hash_normalize` / `text_hash`: can two different dialogs get the
  same hash? Can a still-changing dialog keep changing its hash?
- `crates/orch/src/watch.rs`: `hashes`, `workspaces`, minute-rounded `seen_ms`, the herdr cache.
- `crates/orch/src/ws.rs` (`orch ws` / `orch tab`) and the TUI's raise on workspace switch
  (`show_workspace`, `focus_local` in `tui.rs`).
- `crates/orchd/src/bus.rs`: a new agent doesn't inherit old broadcasts.

Also do a short read-only review of Kabuk's native workspace bar (`/projects/kabuk`, commit
fa2b9ef), only in how it calls orch (commands, quoting, exit codes, confirmation before close).
Don't change Kabuk; the Kabuk Claude fixes its own code.

Fix P1 findings in orchtool yourself, with tests. List P2s in the result file, and fix them where
the fix is small and clear.

## Part B — implement (each with tests, one commit each)

1. **Questions from agents that are gone.** An open `ask_human` whose asker's pane has exited, or
   whose bus agent is stale more than 6 h, should leave "Asks you". Add a question state
   `expired` (or close it with answered_by "orchd"), set when the asker's orch pane exits or the
   pane is killed. Add `orch question close <id>` so the phone can dismiss one (exit 14 if gone,
   11 if already answered). Watch lists only open questions, as now.
2. **One TUI frame per typed key.** `crates/orch/src/tui.rs` run loop: `Msg::Keys` that are only
   forwarded to the pane shouldn't mark the UI dirty. The echo comes back as a frame. Measured
   now: two frames per key, about 98 B instead of about 34 B.
3. **Visible split border.** Draw `│` in the one-cell gap between side-by-side panes (and keep the
   gap for stacked ones). Kabuk's text selection finds pane borders by that glyph.
4. **Help at phone width.** At under ~72 columns, the help popup becomes one column and scrolls,
   or shows the most-used keys first.
5. **Enter delay only where needed.** `orchd/src/server.rs` `ENTER_DELAY_MS = 250` applies to
   every `send --enter`. Keep it for Codex panes (paste-burst detection) and for text longer than
   one line. Use about 20 ms for others. Verify with a private daemon that Claude Code and a
   shell still get the Enter.
6. **`orch read --unwrapped`.** Join soft-wrapped rows (vt100 `Screen::row_wrapped`) in
   `term.rs` `recent_text`, as a new `ReadSource`/flag. "Copy text…" then gets real lines, not
   lines hard-wrapped at pane width.

When done, add a short STATUS.md entry and write a summary for Claude to the end of
CODEX-REVIEW-P5A-RESULT.md: commits, what was fixed, what is open.
