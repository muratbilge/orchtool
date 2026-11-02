# Codex — review Phase 2 (commit 2e7f901), short: your weekly limit is scarce (~19%)

Review ONLY for real bugs, ~15 min, in this order:
1. crates/orchd/src/bus.rs — lost wake-ups in inbox_wait/ask_wait, first-answer-wins, read marks, name
   resolution (register), anything that can lose or duplicate a message/answer.
2. crates/orchd/src/telegram.rs — security (who can answer, pairing, token leakage), duplicate/late updates,
   offset handling, blocking calls.
3. crates/orch-mcp/src/main.rs — JSON-RPC correctness, cancellation, waits vs client tool timeouts.
4. crates/orchd/src/server.rs `start_nudger`/`nudge` and crates/orch/src/bus.rs `hook` — can a nudge or
   hook ever type into / block an agent wrongly?
No style nits, no rewrites, don't run cargo. Write findings to CODEX-REVIEW-P2-RESULT.md as:
file:line — bug — concrete failure scenario — suggested fix. Then say "REVIEW DONE".
