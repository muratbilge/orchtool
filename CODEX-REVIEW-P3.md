# Codex — review the phase-2 fixes + phase 3 (commits 741d00a, b6fc0d8). Short: limit is scarce.

~20 min, real bugs only (no style), in this order:
1. Did the fixes for YOUR phase-2 findings (CODEX-REVIEW-P2-RESULT.md) actually close them?
   crates/orchd/src/bus.rs, telegram.rs, server.rs (start_nudger/nudge/pane_idle), orch-mcp main.rs, orch/src/bus.rs hook.
2. crates/orchd/src/ui.rs + crates/orch-proto/src/layout.rs — layout ops atomicity, pane/session consistency
   when panes exit or are killed, event stream correctness.
3. crates/orch-proto/src/mux.rs — channel lifecycle, close races, backpressure/unbounded memory.
4. crates/orch/src/tui.rs — anything that can hang, lose input, corrupt the terminal, or attach the wrong pane.
Don't run cargo. Write findings to CODEX-REVIEW-P3-RESULT.md (file:line — bug — scenario — fix), then "REVIEW DONE".
