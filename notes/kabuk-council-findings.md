# Kabuk findings from the review council (2026-11-03)

From three read-only reviewers (correctness, speed, native UX) plus Codex. orchtool-side findings
are fixed in orchtool `cacb20f` + `292b591`; everything below is Kabuk code. Verify each claim
against the current code before changing it (line numbers were taken before your 0.5.3 commits).

## Correctness (P1/P2)
1. **Start orch through a login shell** — `data/Hosts.kt:45-48` runs `exec "$O"` with the bare
   non-login PATH (live orchd had PATH=/usr/local/bin:/usr/bin:/bin). orchtool now completes the
   env from `$SHELL -lc env` itself, but please run it as `exec "${SHELL:-/bin/sh}" -lc 'exec "$0"' "$O"`
   (or equivalent) so the TUI and orchd start with the real environment.
2. **Agents started from Kabuk miss profile vars** — `agents/Orch.kt` FIND only adds ~/.local/bin;
   `orch new` passes that env to the pane (no JAVA_HOME/ANDROID_HOME/npm-global). orch now merges the
   login env into `orch new`'s env too, but running the wrapper under `bash -lc` is the clean fix.
3. **Retry misreports / double-types** — `Orch.kt` answerScreen retry (`once()`): on orch panes a
   retry that gets 10 (changed) after the first try landed shows "Not sent: question changed" and
   releases the ledger although the key was sent; treat 10 on a retry as UNSURE. On herdr panes
   `orch send herdr:` has no answered record and herdr calls can take ~5 s each, so a retry can type
   the digit twice: never retry herdr panes.
4. **Ledger released on unsure outcomes** — `term/SshTab.kt` (~420, ~450) releases on everything
   but SENT, incl. UNSURE/FAILED/DAEMON. Contract v2 §3: 1, 20 and timeout keep the ledger; release
   only on 2,3,4,10,12,13,14. `outcomeOf(20)` says "orchd isn't running" but 20 can mean the
   connection broke mid-request → UNSURE wording. Also `showQuestions(emptyMap())` on every stream
   gap prunes all screen keys (AnswerLedger.prune).
5. **Notification buttons can answer on the wrong server** — question buttons carry only tab id +
   question id (`agents/AgentNotifier.kt` ~114-119, `AnswerReceiver.kt` ~12-20); tab ids restart at 1
   per process (`TabManager.kt` ~69/94). After Android kills the app, a stale button can answer
   question #N on another host. Put host id (+ question ts_ms) in the intent and verify, or cancel
   question notifications at init.
6. **Two notifications per ask_human** — orchd marks the asker blocked, so `AgentNotifier` posts a
   button-less "X needs you" next to the question notification. Suppress it while that bus name has
   an open question (or status_text "waiting for Murat").
7. **Receiver time budget (risk)** — AnswerReceiver uses goAsync; worst case read 8 s + send 8 s +
   retry 8 s. Hand the work to SessionService and finish the receiver at once, or cap under ~9 s.
8. **Old orch without `watch`** — clap exits 2 with no output; `Orch.kt` treats only 3 as
   UNAVAILABLE, so it reconnects every 3 s forever with an empty sheet. Exit 2 + no lines → orchNote
   "orch too old" + 60 s back-off.
9. **ORCH host without orch** — terminal falls back to herdr (Hosts.kt) but the sheet stays on
   watchOrch (SshTab ~72/348); switch to the herdr watcher on UNAVAILABLE.

## Speed
10. **Skip unchanged lines** — SshTab (~381-399) parses, runs QuickReplies over every screen, sets 4
    StateFlows and calls `TabManager.agentsChanged` on every line. Skip when nothing used changed.
    (orch watch no longer churns: seen_ms is now whole minutes; measured 1 line/30 s, was 69/min.)
11. **One SSH exec per dialog answer** (needs orchtool too, not done yet) — if watch puts each
    blocked pane's 60-line hash next to `screens`, Kabuk can skip the `read --hash` exec and send
    `orch send --expect` directly. Say if you want it; I'll add the field (additive, no v bump).
12. **Timeline resent in every line** (79% of the line) — would need a contract change (send only
    when the last id changes / only new rows). Proposal, not done.

## Native UX
13. **Native workspace/tab bar** — biggest win. Needs from orchtool: `workspaces` array in watch
    (`[{id, number, label, cwd, status, active_tab, tabs:[{index, id, label, panes, focus}]}]`) and
    `orch ws|tab` CLI (new/rename/close/select/split) over the existing UiOps. Then Kabuk: chips above
    the terminal or a sheet section; tap = `orch focus <pane>`, long-press = rename/close/new tab.
    Tell me and I'll do the orchtool half first.
14. Herdr-pane agents (workspace "") can't be focused via orch: show "in herdr" instead of the
    "Couldn't switch" toast (AgentUi ~146-149, Orch.kt ~219).
15. Long-press agent menu: Kill (`orch kill`), Last 60 lines (`orch read`), Message, Restart here
    (`orch new --cwd … -- claude --continue`) — all commands exist.
16. "New agent" hint text (AgentUi ~307) says "herdr or orch" but orch adds a tab to the folder's
    workspace without a label.
17. After an orchd restart pane ids change (they never repeat); bus names are now kept by orchtool
    (ORCH_AGENT on restore), so Message chips and question ownership survive.
