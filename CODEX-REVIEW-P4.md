# Codex — review phase 4 (Kabuk ↔ orchtool). Short: limit is scarce.

~20 min, real bugs only (no style). Don't run cargo/gradle. In this order:

1. **Phase-3 fixes** (`7f19152`): did they close YOUR findings in CODEX-REVIEW-P3-RESULT.md? One line each.
2. **orchtool server side** (`89e23d6`): `crates/orch/src/watch.rs` (`orch watch --json`: snapshot merge, staleness,
   coalescing/heartbeat, error/retry, stdin-EOF exit), `UiOp::Raise` in `crates/orchd/src/ui.rs` + TUI follow in
   `crates/orch/src/tui.rs` (search `raise_seq`), herdr-pane `read`/`send` delegation in `crates/orch/src/main.rs`.
3. **Kabuk** (`/projects/kabuk`, read-only for you; uncommitted or latest commit on main):
   `app/src/main/java/app/kabuk/agents/Orch.kt` (shell commands over SSH: quoting, exit codes, fingerprint
   check before answering a dialog, first-answer-wins for bus questions), `term/SshTab.kt` (`watchOrch`,
   `answerQuestion`, `sendBus`, backend dispatch), `agents/AgentNotifier.kt` + `agents/AnswerReceiver.kt`
   (question notifications/buttons), `data/Hosts.kt` (`ORCH` remote command), `ui/AgentUi.kt` (Asks you / Comms).
   Also the updater + live log commits `5d7bcbf`, `2e89041` (`update/*`, `log/*`, `scripts/publish_update.py`,
   `scripts/logsink/logsink.py`): signature/APK verification holes, token leaks, anything that can brick updates.

Focus: answering the wrong question/agent, double answers, lost answers, shell injection, a phone that shows stale
state forever, updater accepting a bad APK.

Write findings to CODEX-REVIEW-P4-RESULT.md (file:line — bug — scenario — fix), then "REVIEW DONE".
