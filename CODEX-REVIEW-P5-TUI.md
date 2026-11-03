# Short review: TUI touch + close + restart (keep it short, Codex limit is low)

Commits (orchtool): 81d8d1d, 38e4cb7, 9356ef8, 92074ea, 359282c. Read-only review, no builds needed.

Scope — only these, find real bugs (P1/P2), max ~8 findings, each with file:line + failure scenario:
1. `crates/orch/src/tui.rs`: SGR mouse decoding (`Mouse::parse`, `next_token`), `mouse()`, `wheel()`,
   `forward_mouse()` (encodings, coordinates), scrollback view (`Scroll`, `Msg::History`), status-bar and
   sidebar hit testing (`status_spans`/`status_hit`, `sidebar_lines`) vs what `draw_*` renders,
   workspace-list action rows (`ws_actions`, `workspace_row`, `overlay_mouse`), confirm buttons.
2. Restart: TUI `Closing::Daemon` (StopDaemon{restart:true}, wait until socket gone, re-exec by argv0);
   `crates/orchd/src/restore.rs` + `main.rs` (save on stop, restore on start, read-once), `restart_argv`.
3. `crates/orchd/src/ui.rs` CloseTab / CloseWorkspace (`close_panes`).
4. `Stream::Screen.alt` (api.rs, server.rs send_frame).

Write the result to CODEX-REVIEW-P5-TUI-RESULT.md. Do not change code.
