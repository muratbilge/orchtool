# Short P5 TUI review

Static review of the requested touch/close/restart changes; line numbers refer to the current checkout. No builds or code changes.

1. **P1 — `crates/orch/src/tui.rs:2338`, `crates/orchd/src/ui.rs:213` — close confirmation identifies a tab by mutable index.** Open confirmation for B in A/B/C; another client closes A before confirmation is submitted/processed. Index 1 now identifies C, whose processes are killed instead. Carry the stable tab ID through confirmation and CloseTab, and reject a vanished target.

2. **P1 — `crates/orchd/src/main.rs:104` — failed restart save still destroys every pane.** Disk-full, permissions or rename failure merely logs an error, then `kill_all()` runs; the replacement starts without the promised layout. Persist successfully before committing shutdown, and return the save failure to the requesting client while leaving the daemon running.

3. **P1 — `crates/orch/src/tui.rs:2358`, `crates/orchd/src/main.rs:99` — connection refusal is mistaken for completed shutdown.** The server is aborted before saving the layout and killing panes. During a slow save, the TUI can observe refusal and start a replacement before `restore.json` exists; the old daemon can subsequently unlink the replacement's socket. Wait for an explicit completed-shutdown boundary (with daemon lifetime ownership/locking), not merely failed connect, and fail rather than re-exec on an unconfirmed timeout.

4. **P2 — `crates/orch/src/tui.rs:1092` / `:1121` — mouse release is routed by current pointer position, not the pressed pane.** Press inside a mouse-enabled pane and release over the sidebar, title, status bar or another pane: the original application never receives release and can retain a pressed/selection state. Route release to `mouse_down` before hit testing, clamp coordinates to that pane, and clear capture reliably.

5. **P2 — `crates/orch/src/tui.rs:2271` — workspace actions disappear below the popup with no scrolling.** With enough workspaces or a short phone viewport, close/restart rows are clipped. Wheel/arrow input changes selection but never scrolls rendering, so touch cannot reach those actions and keyboard selection becomes invisible. Maintain a viewport offset shared by drawing and mouse hit testing, keeping selection visible.

6. **P2 — `crates/orch/src/tui.rs:2576` — confirmation text overwrites its buttons on narrow screens.** Buttons are drawn first on the last row, then wrapped explanatory text is drawn over the entire rectangle. At phone widths the restart explanation exceeds the five rows above the buttons; hidden button coordinates remain destructive click targets. Reserve a separate text rectangle excluding the button row, with clipping/scrolling as needed.

7. **P2 — `crates/orchd/src/server.rs:716` — alternate-screen changes can be suppressed with unchanged pixels.** `send_frame` returns when the renderer finds no row/cursor/input-mode changes; the renderer does not track `alternate_screen()`. Switching between identical main/alternate buffers leaves the client's `alt` stale until another visual change, so wheel input uses scrollback instead of arrows, or sends arrows into the shell. Track `alt` in frame dirtiness and emit a metadata update even when terminal bytes are unchanged.

