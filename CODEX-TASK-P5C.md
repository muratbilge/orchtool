# Codex task P5C: exact resume after restart + the flaky bus test (orchtool)

Same rules as P5A/P5B: /projects/orchtool main, `scripts/check.sh` before each commit, prefix `[codex]`,
no push, never touch/restart the live orchd; private daemon with own HOME/db for live tests, always
`orch --socket`. If .git/sockets are blocked: private repo + `/tmp/orch-p5c-series.patch`, say so in the
result file. P5B is done (see CODEX-REVIEW-P5B-RESULT.md); don't redo it.

## 1. Resume the exact conversation, keep the options (your P5A finding 5)

`crates/orchd/src/restore.rs` `restart_spec` / `restart_argv`: an agent started with ordinary options
(`claude --model x`, `codex -m y`, `claude --dangerously-skip-permissions`) comes back as a fresh session
or loses its options, and `claude --continue` / `codex resume --last` pick the *newest* conversation in
that cwd — wrong when two agents share a cwd (e.g. two Claudes in /projects/kabuk).

Wanted: at save time (`snapshot_layout`) record per agent pane the conversation id it is running, and
restore with `claude --resume <id>` / `codex resume <id>` plus the original options (drop only prompt
positionals and `--continue`/`-c`/`--resume X`/`resume X`/`--last`). Leads for finding the id (pick what is
reliable, verify on this machine, read-only):
- Claude Code: transcripts `~/.claude/projects/<cwd with / → ->/<session-uuid>.jsonl`; the pane's claude
  process (pid from the pane's process tree) has its transcript open or writes it; or a SessionStart hook /
  orch-mcp registration could report the session id to orchd (orch-mcp runs inside each Claude).
- Codex: `~/.codex/sessions/YYYY/MM/DD/rollout-*-<uuid>.jsonl`, same pid / open-file approach.
Fall back to today's behaviour when no id is found. Keep `restart_spec`'s ORCH_AGENT handling and the
"already a resume of an id" case. Unit tests for argv rewriting (table of cases); a test for id discovery
with a fake HOME.

## 2. Flaky `crates/orchd/tests/bus.rs` `two_agents_talk_and_one_asks_murat`

Failed once in ~10 runs at the second `read_inbox`, right after delivery/ACK. Find the race (concurrent
inbox read vs. the post-flush ACK / delivered marking), fix the cause in orchd (not by sleeping in the
test), and prove it: loop the test e.g. 200× (`cargo test ... -- --test-threads=8` in a shell loop) before
and after.

When done: `CODEX-REVIEW-P5C-RESULT.md` with commits, evidence, anything open. Short.
