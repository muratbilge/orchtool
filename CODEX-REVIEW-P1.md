# Codex — review Phase 1 (commit 30dd490), keep it short: your weekly limit is scarce

Claude built Phase 1 (see STATUS.md top entry). Review ONLY for real bugs, in this order, and stop after ~15 min:
1. crates/orchd/src/render.rs — can the client's screen ever diverge from the server's (row diff, cursor,
   modes, clipping, wide chars at the edge, gen/resize)?
2. crates/orchd/src/server.rs `attach_loop` + crates/orchd/src/pane.rs — races (resize vs render, takeover,
   detach, exit), lock misuse, anything that can block the tokio runtime or leak panes/threads.
3. crates/orch/src/attach.rs — terminal restore, prefix handling, resnapshot.
No style nits, no rewrites. Don't run cargo (no Rust in your sandbox). Write findings to CODEX-REVIEW-P1-RESULT.md
as: file:line — bug — concrete failure scenario — suggested fix. Then say "REVIEW DONE".
