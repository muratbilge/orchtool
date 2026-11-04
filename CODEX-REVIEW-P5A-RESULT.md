# P5A review and implementation handoff

Part A reviewed `359282c^..1f30b5f`, including Kabuk `fa2b9ef` read-only. Findings below describe the reviewed code; findings 1–4 are fixed below; finding 5 remains open.

1. **P1 — `crates/orch-proto/src/api.rs:338` — meaningful dialogs collapse to the same hash.** Any line ending in `(digits + s/m/h...)` is discarded, including `Delete backups (1h)` versus `Delete production (1h)`. Trimming all indentation and removing arbitrary leading `*` also conflates code/commands. A stale conditional answer can therefore pass. Restrict animation normalization to recognizable status/tool decoration and preserve meaningful whitespace/text.
2. **P1 — `crates/orch-proto/src/env.rs:19` — inherited bus identity survives environment sanitization.** `ORCH_AGENT` is not excluded; an agent launching a new pane passes its explicit bus name to the new agent, which can replace the original registration and consume its inbox. Strip inherited `ORCH_AGENT`, while retaining deliberate request/restore overrides.
3. **P2 — `crates/orch-proto/src/env.rs:81` — login environment collection can hang beyond its three-second deadline.** A shell exits after launching a background child that retains stdout; `reader.join()` waits indefinitely for EOF. Apply the deadline to reading as well as waiting for the shell, and bound captured output.
4. **P2 — `crates/orch/src/ws.rs:175` — tab rename resolves a stable ID back to a mutable index.** Another client closes an earlier tab between UiGet and RenameTab; a different tab receives the label. Pass and resolve the stable tab ID on the server, as CloseTab already does.
5. **P2 — `crates/orchd/src/restore.rs:98` — ordinary option values prevent agent resume.** For example, `claude --model x` is left unchanged because `only_flags` rejects `x`; restart starts a fresh invocation despite promising continuation. A proper agent argument/session contract is needed; do not guess which arbitrary positional values are prompts.

No additional P1/P2 found in the scoped Kabuk workspace-bar calls: IDs/labels are shell-quoted, close actions have confirmation, and outcomes reach its layout error handler. The new-agent broadcast cutoff is serialized with registration under the bus lock. Initial TUI selection is already raised. Minute rounding and the bounded herdr cache behave as intended. Automatic continuation still does not persist a distinct conversation ID per pane.

## Implementation handoff

All six Part B items are implemented in the working tree. No live orchd contact, installation, restart, push, or Kabuk modification was performed.

The sandbox mounts this repository's `.git` read-only (`index.lock`: read-only filesystem). Consequently the requested commits exist in the private local repository `/tmp/orch-p5a-commits.yhmUjF`, based on `860d4bc`; the original branch remains at `860d4bc` with these edits uncommitted. A format-patch series is exported to `/tmp/orch-p5a-series.patch`. Import into a **clean** checkout at the base; the current working tree already contains the implementation.

| Commit | Work |
| --- | --- |
| `054d83c` | Part A: conservative dialog normalization, strip inherited ORCH_AGENT, bound login stdout wait/capture, stable-ID tab rename; private HOME for pane tests. |
| `b4a5db5` | B1: expired questions on pane exit/kill or >6 h staleness, `orch question close ID`, exit 14 gone / 11 answered, terminal-state handling for MCP. |
| `9489222` | B2: forwarded keyboard input does not dirty the UI; local changes still redraw. |
| `eb4adb1` | B3: vertical `│` split borders from the same geometry used for panes; stacked gaps unchanged. |
| `93d3bec` | B4: one-column phone help, arrow/wheel scrolling, rendering test. |
| `f6bc3b1` | B5: 20 ms for ordinary single-line sends; 250 ms for Codex or multiline input. Nudges retain their safety delay. |
| `d845bf7` | B6: `orch read --unwrapped`, `recent_unwrapped` source, logical-line limits, soft-wrap joining across scrollback pages. |

Validation: `scripts/check.sh` was run before each commit. Formatting, clippy, binaries and all test targets compile. The final run passes 10 orch tests and 13 non-socket proto tests, then fails the two existing mux tests because Unix-socket bind returns EPERM. Separately, all 26 orchd library tests pass, including expiry, Enter timing and unwrapped text (49 passing unit tests in total). Integration tests are added and compiled, but could not run here. Logs: `/tmp/orch-p5a-b6.log`, `/tmp/orch-p5a-lib.log`; previous steps have `/tmp/orch-p5a-check.log` and `/tmp/orch-p5a-b1.log` through `b5.log`. **Not GREEN:** full checks and real shell/Claude Code smoke verification remain required outside this socket-restricted sandbox. The Claude-classified shell test is explicitly not a real Claude Code test.

Open / follow-up for Claude:

- Finding 5: resume with ordinary option values still needs an explicit argument/session contract. Per-pane conversation IDs are not persisted.
- User-reported flake: `tests/bus.rs::two_agents_talk_and_one_asks_murat` failed once in roughly ten runs at the second `read_inbox`, immediately after delivery/ACK. Not reproduced here because socket tests cannot run; not skipped, weakened or declared fixed. Investigate concurrent inbox reads versus the post-flush ACK.
- Hash normalization now preserves indentation, interior blank lines and meaningful `*`; visibly changing content can therefore cause safe stale-screen refusals. A disappearing tool bullet may also change the hash. Refresh the watch snapshot and retry; do not broaden normalization to discard arbitrary content.
- `--unwrapped` preserves hard newlines and joins only vt100-marked wraps. This vt100 version does not mark the early wrap caused by a wide glyph in the final column; that edge retains a break. A regression documents this limitation instead of guessing away real newlines. `--unwrapped` conflicts with `--hash`/`--visible` and rejects herdr panes, whose wrap metadata is unavailable.
- Run full checks on a clean import and test actual Claude Code plus a shell using a private socket, HOME and db. Do not use or restart the live daemon. Kabuk can adopt `question close` and `read --unwrapped` separately.

