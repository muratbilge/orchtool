# P5C result

Implemented; socket integration validation remains blocked in this sandbox. Live orchd untouched.

- Part 1: `bbba8e0` — `[codex] Preserve per-pane conversation IDs and agent resume options`.
  Saves process-owned transcript IDs / Claude hook session IDs and actual agent argv; restores
  exact IDs with options, prompt removal, explicit-ID fallback, shell quoting and ORCH_AGENT.
  No newest-transcript scan. Fake HOME/process tests distinguish two agents in one cwd;
  argv tables and snapshot compatibility/quoting tests pass.
- Part 2: `[codex] Order inbox reads after previously flushed delivery receipts` (this commit).
  Cause: the next MCP call can query orchd before the preceding response's post-flush ACK.
  MCP records receipts under a lock spanning output/flush; the next inbox request carries them,
  and orchd applies them before querying. Failed output is not acknowledged; ACKs remain
  idempotent. The original integration test is unchanged; no test sleeps added.

Validation:
- `scripts/check.sh` run before each commit: formatting, workspace Clippy and binary builds pass;
  test stage stops on Unix socket `EPERM` in the two existing orch-proto mux tests.
- Separate tests: 36 orchd unit tests and 2 MCP delivery tests pass. Final shell/hook fallback
  edit rechecked with all 5 restore tests. Deterministic regressions cover late ACK, unfinished
  output, cancellation/failure, and preserving newly arrived unread messages.
- Baseline and final attempts at `two_agents_talk_and_one_asks_murat` both fail at private daemon
  startup (`EPERM`), before the race assertion. **No 200-run before/after success claim.**
  On a socket-capable host, run on baseline `4cdf445` and the final revision:
  `cargo build --workspace --bins`, then
  `for i in $(seq 1 200); do cargo test -p orchd --test bus two_agents_talk_and_one_asks_murat -- --exact --test-threads=8 || exit 1; done`.
- Read-only local CLI help and transcript filenames checked. Host agent processes are hidden
  here, so live process-to-transcript ownership could not be verified. Missing/ambiguous FDs
  use hook IDs if available, otherwise prior resume fallback. Unsupported CLI syntax retains
  the legacy fallback rather than guessing option arity; live resume validation remains open.

The workspace `.git` is read-only: commits are in `/tmp/orch-p5c-commits`; importable two-commit
series: `/tmp/orch-p5c-series.patch`. Working-tree edits are also left in place. Nothing pushed.
Logs: `/tmp/orch-p5c-{before,after,final-check,part2-unit,restore-final}.log`.
