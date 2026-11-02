# Phase 4 review

Static review of `7f19152` / `89e23d6` and Kabuk through `e2b2b6a`, including updater/log commits. No cargo/gradle or code changes. Kabuk read-only.

Phase-3 findings, in original order:

1. **Closed at requested minimum:** MCP acknowledges after successful stdout flush; this is not a client receipt guarantee.
2. **Closed for transient failures:** Telegram retains the offset when application fails.
3. **Partial — `crates/orchd/src/bus.rs:414`:** dedup now covers unread answers, but a successful `ask_human`/`wait_for_answer` does not mark the answer read; asking the same question again can return the previous answer. Use explicit request IDs, not text/unread-state identity.
4. **Partial — `crates/orchd/src/server.rs:508`:** unknown states now fail closed, but a dialog can appear between the final idle check and input; use receiver-owned readiness/conditional delivery.
5. **Closed for original race:** failed layout insertion now attempts pane rollback.
6. **Closed:** removing an earlier tab preserves selection.
7. **Closed:** authoritative agent/question lists are reread on hints and lag.
8. **Closed for original queues:** transport/channel queues are bounded and slow delivery times out.
9. **Partial — `crates/orch-proto/src/mux.rs:130`:** clearing senders cannot cancel an already blocked socket write; transport EOF can still retain bridge tasks/sockets. Cancel/join bridges on transport teardown.
10. **Partial — `crates/orch/src/tui.rs:593`:** local focus fixes wrong-pane routing, but immediate typing after selecting an uncached tab is dropped because its view does not yet exist; queue until attached. Creation's three-second release can also send held input to the old pane before a slow operation completes (`:625`); timeout must fail closed.
11. **Closed for original scenario:** unexpected attach EOF/write errors remove the cached view for reopening.
12. **Closed:** bracketed paste is a separate overlay input event.
13. **Closed:** incomplete escape/UTF-8 input is retained across reads.

Phase-4 findings (Kabuk paths below are relative to `/projects/kabuk`):

- **P1 — `app/src/main/java/app/kabuk/agents/Orch.kt:109` → orch `crates/orch/src/bus.rs:174` — wrong bus answer.** Kabuk passes option text, but CLI interprets numeric text as an option index. With options `["2", "1"]`, tapping “2” stores “1”; numeric free-text replies are also rewritten. Add a literal-answer mode/protocol and use it for both UI and notification answers.

- **P1 — `app/src/main/java/app/kabuk/agents/Orch.kt:100` — screen fingerprint check is not atomic with sending.** Two clients can read the same dialog, then the second client's digit reaches the next dialog after the first answers. Per-tab dedup does not protect this. Provide conditional delivery tied to a question instance at the input owner; a separate SSH read followed by send cannot guarantee first-answer-wins.

- **P1 — `app/src/main/java/app/kabuk/update/Updater.kt:138` — updater fails on supported older Android.** `InputStream.readNBytes(int)` requires API 33, while minSdk is 26 and core-library desugaring is not configured. Checking updates on API 26–32 raises `NoSuchMethodError`, outside `catch (Exception)`, preventing self-update. Use a bounded `read` loop compatible with minSdk. [Android API reference](https://developer.android.com/reference/java/io/InputStream#readNBytes(int)).

- **P1 — orch `crates/orch/src/watch.rs:89`; `app/src/main/java/app/kabuk/agents/Orch.kt:79` — silent stalls never expire.** A snapshot request that stops responding blocks heartbeat/retry; the phone's blocking `readLine()` has no heartbeat deadline and keeps its last state live indefinitely. Bound snapshot requests and close/reconnect the watch after missed heartbeats, marking cached state stale and disabling answers.

- **P2 — `app/src/main/java/app/kabuk/term/SshTab.kt:394` — ended watches retain actionable stale data.** After a successful snapshot, EOF/failure followed by repeated unavailable/error connections leaves agents/questions/timeline unchanged; only `liveAgents` changes, while AgentUi still renders those lists. Clear or explicitly invalidate them and refresh/cancel notifications when the stream ends.

- **P1 — orch `crates/orch/src/main.rs:217` — remote selection bypassed.** Watch and herdr read/send return before using the remote connector. `orch --host remote send herdr:ID ...` operates on local herdr, potentially typing into an unrelated local agent with the same ID; watch likewise shows local state. Route these operations through the selected host, or reject `--host` explicitly until supported.

- **P2 — `scripts/logsink/logsink.py:135` / `:178` — dedup headers are unauthenticated.** HMAC covers timestamp/body only. On the default HTTP transport, replaying a captured valid batch with inflated matching sequence headers poisons that device/stream's high-water mark; subsequent genuine logs are acknowledged and discarded. Authenticate device, stream, sequence and once headers with the body, and use TLS.

No additional shell-quoting, bus first-writer transaction, Raise-sequence, manifest-signature/hash/APK-signer verification bypass found in this static pass. Runtime behavior remains untested.
