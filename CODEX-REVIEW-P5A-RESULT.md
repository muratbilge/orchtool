# P5A review and implementation handoff

Part A reviewed `359282c^..1f30b5f`, including Kabuk `fa2b9ef` read-only. Findings below describe the reviewed code; resolution and commits will be recorded after checks.

1. **P1 — `crates/orch-proto/src/api.rs:338` — meaningful dialogs collapse to the same hash.** Any line ending in `(digits + s/m/h...)` is discarded, including `Delete backups (1h)` versus `Delete production (1h)`. Trimming all indentation and removing arbitrary leading `*` also conflates code/commands. A stale conditional answer can therefore pass. Restrict animation normalization to recognizable status/tool decoration and preserve meaningful whitespace/text.
2. **P1 — `crates/orch-proto/src/env.rs:19` — inherited bus identity survives environment sanitization.** `ORCH_AGENT` is not excluded; an agent launching a new pane passes its explicit bus name to the new agent, which can replace the original registration and consume its inbox. Strip inherited `ORCH_AGENT`, while retaining deliberate request/restore overrides.
3. **P2 — `crates/orch-proto/src/env.rs:81` — login environment collection can hang beyond its three-second deadline.** A shell exits after launching a background child that retains stdout; `reader.join()` waits indefinitely for EOF. Apply the deadline to reading as well as waiting for the shell, and bound captured output.
4. **P2 — `crates/orch/src/ws.rs:175` — tab rename resolves a stable ID back to a mutable index.** Another client closes an earlier tab between UiGet and RenameTab; a different tab receives the label. Pass and resolve the stable tab ID on the server, as CloseTab already does.
5. **P2 — `crates/orchd/src/restore.rs:98` — ordinary option values prevent agent resume.** For example, `claude --model x` is left unchanged because `only_flags` rejects `x`; restart starts a fresh invocation despite promising continuation. A proper agent argument/session contract is needed; do not guess which arbitrary positional values are prompts.

No additional P1/P2 found in the scoped Kabuk workspace-bar calls: IDs/labels are shell-quoted, close actions have confirmation, and outcomes reach its layout error handler. The new-agent broadcast cutoff is serialized with registration under the bus lock. Initial TUI selection is already raised. Minute rounding and the bounded herdr cache behave as intended. Automatic continuation still does not persist a distinct conversation ID per pane.

## Implementation handoff

Pending implementation and validation. Tests must use private sockets, HOME and databases; the live orchd must not be contacted.
