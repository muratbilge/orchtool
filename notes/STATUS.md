# Status

## 2026-11-23 — Sleep: eleventh check (no recovery arguments to clients) — Claude (Opus)

- `SleptPane` serializes without `argv` / `agent_argv` by default (hand-written `Serialize` in orch-proto);
  only the daemon's sleeper-record write runs inside `layout::with_recovery_args`. No client sends a session to
  orchd, so every outward path (replies, events, snapshot, MCP, `orch watch`) is covered by the default. The
  autosave's copy of the sleepers is the public form too (start-up takes them from the sleeper record).
- A busy plan note names the program's base name only.
Test `m1_recovery_arguments_never_reach_a_client`.

## 2026-11-22 — Sleep: tenth check (wrapper variables, relative programs, note script, logs) — Claude (Opus)

- `agent_start::rebuild` refuses (note) a shell pane whose command line sets a variable or can't be read, also
  with a discovered agent argv; and a program word that is a relative path with a slash.
- The note is `/bin/sh -c <fixed script> orch-note <shell> <lines...>`: the lines are arguments, never script
  text; then `exec` of the pane's / login shell.
- Logs, the sleep plan and `orch ws wake` carry pane, kind and a fixed reason only, never the command line.
- Fact for the install decision: main's own restart (`restart_spec`, unchanged) already drops wrapper variables
  and replays a relative program from the pane's root folder (asserted in
  `agent_start::tests::wrapper_variables_and_relative_programs_are_refused`).
Tests `l2…`, `l3…`, log/plan checks in `k1…`/`k2…`, unit tests in `agent_start.rs`.

## 2026-11-22 — Sleep: ninth check (fail-safe agent start) — Claude (Opus)

- New `agent_start.rs`, used by manual wake (`wake_start`) and by the restart beside a sleeper
  (`Daemon::restart_for`): an agent starts only from a completely parsed command (program word = the agent's,
  kept as written; every option recognised; a wrapper must be one plain command without variables other than
  `ORCH_AGENT`, unless the agent's own argv was discovered). No bare-name fallback, no retry with argv[0] only.
- Otherwise: a shell in the pane's folder printing the note (why, saved command, agent command, conversation id);
  nothing of the saved text runs. Wake succeeds; result `not_started`, `orch ws wake` prints `not started: …`,
  the sleep plan says "will not be started", the daemon logs it. The restore does not count/nudge such panes.
- `restore.rs` against main: SavedPane re-export, `skip_asleep`, `restart_for` call, `started &&` on the agent
  bookkeeping. `restart_spec` (main's) runs whenever no agent of that kind sleeps.
- Test fakes are now named `claude`/`codex` (the program word must be the agent's).
Tests `k1…`–`k4…`, `i_c…`/`j_a…` now expect the note; unit tests in `agent_start.rs` (main's argv as literals).

## 2026-11-22 — Sleep: eighth check (fresh command instead of a text scan) — Claude (Opus)

- `restart_spec` is main's function again, word for word; `restart_spec_for(sp, false)` calls it.
- With `no_latest`, `fresh_spec` builds a new agent command, never replaying saved shell text: the verified
  session id; else a target the resume parser establishes (discovered agent argv, or the pane's command /
  its shell wrapper when that is one simple command, known options kept); else the picker. Shell panes get
  `<shell> -lc '<agent>; exec <shell>'`; other wrapper commands are dropped. `has_latest_selector` is gone.
- `bash -c 'cd . && claude --resume <id>'` without a verified id: the parser can't establish the id (first
  command is `cd`), so the picker. With a verified id: that id. The unknown-option fallback uses the verified id.
Tests `j_a…`, unit `no_latest_builds_a_fresh_command_on_every_branch` (main's argv as literals).

## 2026-11-22 — Sleep: seventh check (no latest-session selector beside a sleeper) — Claude (Opus)

- One rule, no folder matching: when the loaded sleepers hold a pane of an agent kind (claude / codex), every
  awake pane of that kind is restored with a known id or its picker, on every branch of `restart_spec_for`
  (direct and in-shell rewrite, word-for-word wrapper replay, shell fallback, the unknown-command fallback):
  `no_latest_selector` turns Claude `-c`/`--continue` into `--resume` and drops Codex `--last`; a wrapper
  replay holding such a selector falls back to the picker form. Decided by `Daemon::no_latest`. Without such
  a sleeper `restart_spec_for(sp, false)` is `restart_spec` (main) unchanged. The sixth check's folder rule is
  replaced by this one.
Tests `i_a…`–`i_d…`, unit `no_latest_selector_on_any_branch_while_one_of_the_kind_sleeps`.

## 2026-11-22 — Sleep: sixth-check fixes (ended panes, lone Codex beside a sleeper) — Claude (Opus)

- **Ended panes:** the start-up filter also leaves out every pane id in a sleeper's saved tab trees, so a pane
  whose program ended before the sleep (no restartable record) is not run again from a stale autosave. A wake
  still starts only panes with a record.
- **Codex `--last`:** (replaced by the seventh check's rule) after main's `mark_sole_codex`, an awake Codex pane
  in a folder where a loaded sleeper has a Codex pane is not alone there: picker, not `resume --last`. With no sleepers the decision is main's. A wake
  never uses `--last` (unchanged).
- Sweep (bus names, tasks, questions, labels): already counted sleepers. Pane names: a new awake pane may take a
  sleeping pane's name; its wake then fails (all or nothing, sleeper kept) until one is renamed. Not changed.
Tests `h1_…`, `h2_…` (two).

## 2026-11-22 — Sleep: fifth-check fixes (in-memory start-up filter, wake-bound confirmer) — Claude (Opus)

- **A1/A2:** no layout file is rewritten or set aside at start-up any more. `restore_layout` calls
  `skip_asleep` on the layout it parsed (the planned file or the autosave it fell back to): the loaded
  sleepers' panes are left out, other workspaces stay; with no sleepers it returns at once.
- **C3:** the wake confirmation is bound to its wake's pane map; the sleeper is removed only if it still
  carries that map (checked in the same commit), and it stops once that sleeper is gone or changed. The 30 s
  reservation bound is a timer of its own, so a stalled read or commit can't hold it longer.
- Known limits (README): pane-id reuse by the inherited counter fallback; every woken pane exiting before the
  next autosave.
Tests `g5_…` (two), `g7_…`.


## 2026-11-22 — Sleep: fourth-check fixes (start-up rule, split by name, wake confirmation) — Claude (Opus)

- **Start-up rule (finding 5):** the sleeper record always wins. `load_sleepers` takes every pane of a sleeper
  (old ids, and the new ids of an interrupted wake) out of the layout file the restore is about to read (atomic
  rewrite, only when one overlaps; if that write fails the file is set aside as `*.sleeper-conflict` and not
  restored). The restore never starts them; start-up never retires a sleeper. A crash after a wake's autosave
  but before its sleeper is dropped brings it back asleep.
- **Finding 6:** pane-addressed layout changes resolve a pane name to its canonical id before admission and
  carry that id through placement.
- **Wake confirmation:** a woken pane that has ended is no longer waited for; the reservation is released at
  confirmation or after 30 s; a sleep of a workspace whose wake is not confirmed yet is refused.
- Known limits (README): a failed folder sync after rename can leave the record on disk (comes back asleep after
  a restart); a repaired slot with the same generation number is not told apart.
Tests `e5_…` (four cases), `e6_…`, `e7_…` in `crates/orchd/tests/sleep.rs`.


## 2026-11-22 — Sleep cut down to manual sleep / wake — Claude (Opus)

Murat's decision after Codex's third re-check: smaller feature instead of a fourth fix round. **Supersedes the
sleep entries below** (auto-sleep, keep-awake, partial wake, `-stuck` / `-rest` workspaces, pane lineage,
`unrestored*.json` retry, `restore.json.restoring`, durable-only pane counter: all removed).
- The restart path is main's: `autosave.rs`, `restore.rs` (except `SavedPane` moved to orch-proto) and the pane
  id allocation are unchanged; `main.rs` only loads the sleeper record before the restore and installs it after.
- Kept: `orch ws sleep|wake [--force|--dry-run]`, `msg --wake`, `task add --wake`, the two-slot record, plain
  stop keeps sleepers, verified-id / picker resume, name reservation, id-before-label lookup, stop/reservation
  admission, single-pass commits, the in-flight send/kill guard (now also layout changes, through placement).
- Start-up rule (one): a sleeper whose panes are in the layout file the restore used came back awake (crash
  before the autosave caught up with a sleep, or after a wake's autosave) and is dropped; duplicate sleepers of one
  workspace id keep the first; an interrupted wake moves its owners back. A wake drops its sleeper only once the
  autosave on disk holds the new panes. A failed folder sync is a failed sleep. A blocked record store unblocks
  only when the record on disk is the one it loaded (else: restart).
Tests: `crates/orchd/tests/sleep.rs` (manual sleep/wake, faults, d5/d6/d_store for the remaining findings).


## 2026-11-22 — Sleep: second re-check (1, 3–8, 14, store, commit ids) — Claude (Opus)

The class fix Codex asked for: one authority per pane, reconciled by **lineage** (`Pane::origin`, kept in every
saved record as `origin`: the first pane id of a chain of restarts, sleeps, wakes and retries). Start-up
(`autosave::restore_at_start`) reads the sleeper record, the awake layout file and the single retry file
`unrestored.json`, and `reconcile_sleepers` gives each lineage to the highest owner (sleeper unless its wake
committed, then the awake layout, then the retry file) before anything starts; it then writes the sleeper
record, the autosave and the retry file in that order and only then drops the source. Replaying after a
crash between any two of these finds the overlap and resolves it the same way.
- **1:** commits run once: `f` returns a `Change` computed from the live layout with ids from the live
  counter, that change is written and exactly it is published. A failed folder sync after the rename counts
  as written: no compensating write.
- **3:** a malformed `restore.json` no longer removes the autosave; failed sources are kept under unique names.
- **4:** survivors are guarded by lineage (a live pane of the sleeper's lineage blocks its wake), which also
  covers a partial wake's remainder and does not depend on a record write.
- **5:** an awake copy of a workspace whose wake did not commit is dropped by lineage.
- **6:** sends and kills count as in flight until their last byte; a sleep waits for them; woken panes join the
  reservation when they are created. **7:** keep-awake and an agent's ask go through the reservation.
- **8:** both restore paths schedule a saved continue. **14:** the retry file is single, rewritten after the
  autosave, deduplicated by lineage, raises the id counter, and is dropped by a plain stop.
- **Store:** a blocked store re-checks its slots before every write; a wake checks writability before starting
  anything.
Tests `c1_…`–`c14_…`, `cstore_…`, `cid_…` in `crates/orchd/tests/sleep.rs`.


## 2026-11-22 — Sleep: Codex re-check fixes (1, 2, 3–8, 10, 11, 13, 14, store) — Claude (Opus)

- **14 (ordinary restart):** a restored pane whose start fails (e.g. no durable pane id) no longer consumes the
  layout: the failed panes go to `unrestored-*.json` (never written by the autosave or an older orchd) and are
  tried again at every start. `restore.json` is renamed to `restore.json.restoring` (loop guard) instead of
  unlinked, and the source (`restore.json.restoring` / `autosave.json.restoring`) goes only after the
  sleepers taken from it are in their record, the leftovers are kept and the restored layout is in the
  autosave; otherwise it is kept as `*.failed`.
- **1:** sleeper changes go through `Daemon::commit`: computed on a copy, written, and only then applied to the
  live layout; snapshots wait on a publish gate while a write is in flight; a write that failed after its
  rename is superseded by the unchanged state.
- **2:** a failed planned-restart save resets `stopping_soon`; reservations are admitted under the lock the stop
  checks under.
- **3:** a stale awake copy of a sleeper is recognised by its panes, not its id (an older orchd's reused id
  keeps its workspace; the sleeper is renumbered); legacy sleepers keep their source until written.
- **4:** wake-rollback survivors are kept visible and block the next wake; a shell wrapper only counts as
  started when the agent program itself is its child.
- **5:** a failed undo keeps the wake map in the record (the next start retries); the question sweep starts
  after the sleepers are reconciled.
- **6/7:** reservations hold their pane ids until the transition ends (send, kill, attach refused even after the
  workspace left the layout); a person arriving (attach, input, layout change) stops an automatic sleep that has
  not started to commit; questions are checked at the auto-sleep commit.
- **8:** due usage-limit continues of woken panes are in the awake layout before the wake commits.
- **10:** during a wake only the replacement of the agent's own pane may take its name. **11:** `task add --wake`
  wakes the sleeper the task's label resolves to. **13:** record writes run on blocking threads.
- **Store:** a slot that exists but is damaged or unsupported is left alone and blocks writes.
Tests `r1_…`–`r14_…`, `rstore_…` in `crates/orchd/tests/sleep.rs`.


## 2026-11-22 — Sleep: Codex review fixes (13 findings) — Claude (Opus)

Codex's static review ("Safe to install: no", findings 1–13) checked against the code: all 13 hold; all fixed.
- **Persistence (1–3):** sleepers have their own record, `sleeping.a.json` / `sleeping.b.json` next to the
  socket (`sleepstore.rs`): alternating slots with a generation, tmp+fsync+rename+folder fsync, newest valid
  slot wins, the other is kept; never deleted. Each write is of the current state under the store lock (no
  stale overtaking). The layout files never carry sleepers (an older orchd leaves the record alone). Sleep:
  reserve → check → move into `sleeping` → **confirmed write** → only then end the programs (failed write =
  rolled back, nothing ended). Start-up loads the record first (the panes count as asleep at once), drops
  every asleep workspace from a stale awake layout, finishes or undoes an interrupted wake. Plain stop waits
  for transitions and never touches the record. Autosave writes are version-checked; `write_atomic` syncs the folder.
- **Wake (4, 5, 9):** all or nothing (`--partial` keeps failed panes asleep as `<label>-rest`); an agent must
  still be up after a grace (in a shell wrapper: as the shell's child); a missing folder fails the pane (no
  home fallback). Order: start → write old→new map → move tasks and bus names → write awake layout →
  mark committed → drop the sleeper; a crash before "committed" moves owners back at the next start. Agents
  resume only a verified session id, else their picker (Claude `--resume`, Codex `resume`); a saved command
  line is never replayed.
- **Transitions (6, 7, 13):** per-workspace reservation (layout ops, send, kill, task dispatch, nudges refused
  or skipped; the global task gate is only touched briefly); the commit checks the pane set is unchanged;
  panes end in parallel; SIGKILL survivors stay visible in `<label>-stuck` and block the wake. Auto-sleep
  re-samples at the commit (input, screen, clients, focus, exemption, setting, bus status) and aborts on any
  change or failed read; a bus status "working" also blocks a manual sleep.
- **8:** a due usage-limit continue keeps a workspace awake for auto-sleep; the continue rechecks the live
  auto-continue setting before typing and goes into the layout snapshot (`continue_at`) to survive a restart.
- **10:** a sleeping agent's explicit bus name is reserved (`name_reserved`); wake refuses on a live holder.
- **11:** ids resolve before labels across awake and sleeping workspaces.
- **12:** no pane without a durable id from the counter (no in-memory fallback).
Failure injection: `ORCHD_FAULT` (`fault.rs`). Tests `f1_…`–`f13_…` in `crates/orchd/tests/sleep.rs`.


## 2026-11-22 — Sleep follow-ups: wake on request, Codex picker, auto-sleep — Claude (Opus)

Murat's answers: "1 yes 2. yes 3. with request, 4. yes". (1) plain `stop-daemon` keeps sleeping workspaces
(unchanged). (2) Wake never uses `codex resume --last`: an unidentified Codex comes back at its picker (restart
unchanged). (3) `orch msg --wake` / MCP `send_message` `wake` / `orch task add --wake` wake the addressed
workspace; refused for `all`; default off. (4) `orch auto-sleep <duration>|off` (kv `auto_sleep_ms`, off by
default) and `orch ws keep-awake` (kv `keep_awake:<ws id>`): `crates/orchd/src/autosleep.rs` tracks per
workspace the panes' screen hashes, inputs written, attached clients, working agents and the raised workspace;
idle for the whole period → a normal (unforced) sleep with a timeline entry; refused → a new period; created,
woken, setting changed or orchd started → a new period. Watch: `auto_sleep_ms`, `keep_awake` (additive). Tests
in `crates/orchd/tests/sleep.rs` (9 now). Not installed; not tried with real agents, a real TUI or Kabuk.


## 2026-11-22 — Workspaces sleep and wake — Claude (Opus, standing in for Codex)

Murat: "we should be able to close and reopen the workspaces for efficiency". `orch ws sleep <ws>` keeps the
workspace's layout (ids, tabs, splits, sizes, labels) and each pane's `restore.json` record in the layout itself
(`Session::sleeping`, so `restore.json` / `autosave.json` carry it), writes the autosave at once, then ends its
programs. `orch ws wake <ws>` starts them again through the restart code (`restore::spawn_saved`, shared with
`restore_layout`), puts the workspace back at its old place, moves the agents' bus names to the new panes and
tells an agent what it missed once it is at its prompt. Refused with `ws_busy` / exit 15 while an agent works,
waits on a dialog or a question, runs a task, or a program runs that can't be resumed (`--force`, `--dry-run`).
Mail, queued/running tasks and open questions wait (no expiry while asleep); a limited agent is continued after
its reset once awake; nothing wakes by itself. Survives restart, crash and a plain `stop-daemon` (which now
keeps an autosave with only the sleeping workspaces). TUI: `S`, `☾ sleep workspace…` and sleeping rows (Enter
wakes) in the `w` list; a refusal turns into a forced confirmation. Watch: additive top-level `sleeping`
(kabuk-contract-v2.md, fixture regenerated). Tests: `crates/orchd/tests/sleep.rs` (5, fake agents) + TUI/watch
unit tests. Not installed; not tried with a real Claude/Codex resume or Kabuk.

## 2026-11-17 — P7: the layout survives an unplanned stop — Claude (Opus, standing in for Codex)

orchd now keeps `autosave.json` (restore.json format, 0600, atomic) next to its socket: 2 s after a layout
change, every 60 s otherwise, skipped when unchanged; a final save on SIGTERM/SIGINT before `kill_all`. Start-up:
`restore.json` (planned restart, wins and removes the autosave), else the autosave, moved to
`autosave.json.restoring` while it is restored; a marker found at start-up → kept as `autosave.json.failed`, no
restore. Plain `stop-daemon` removes the autosave. Recovered agents that were working get `RECOVERY_NOTE`, the bus
summary says "recovered after an unplanned stop". New tests in `crates/orchd/tests/autosave.rs`. Not installed;
the live orchd needs an install + one planned restart (see the untracked CODEX-REVIEW-P7-RESULT.md).

## 2026-11-11 — Codex's update menu is skipped automatically — Claude

After today's restart both Codex panes sat at Codex's "Update available" menu (it comes before
`codex resume`). orchd now presses Escape (= skip) on that menu, once per appearance, also when
the pane's agent is not known yet; `orch auto-continue <pane> off` leaves it alone; Murat gets a
bus message (thread limits). Detected only as the active bottom menu (`detect::codex_update_question`).

## 2026-11-11 — P6B reviewed and merged; lone Codex pane keeps `--last` — Claude

Reviewed Codex's P6B outside its sandbox: `scripts/check.sh` green, the two-pane resume test passes,
`nudges_only_reach_idle_panes` 50/50 with 4 in parallel; `--no-daemon` and `resume <id>` confirmed
on codex-cli 0.166.0. One change on top: a Codex pane without a verified id that is the only Codex
pane in its folder comes back with `codex resume --last` (per folder, so unambiguous) instead of
stopping at the picker with nobody at the board; several Codex panes in one folder still get the
picker. Open: start Codex panes with `--no-daemon` and add the SessionStart hook (README) — needs
Murat's trust step in Codex `/hooks`; until then exact resume only applies to the lone-pane case.

## 2026-11-06 — P6B: verified Codex resume and nudge test readiness — Codex

Codex SessionStart reports now require a dedicated `--no-daemon` client and matching process
ancestry; shared-server hook environments are not pane-specific (verified with Codex 0.166.0).
Unknown sessions use the picker, never automatic `--last`. README has the opt-in hook setup.
The nudge test now waits for detected prompt/dialog readiness before sending. Socket tests and
the 50-run load comparison remain blocked by sandbox EPERM; see CODEX-REVIEW-P6B-RESULT.md.

## 2026-11-04 — P6A task queue — Codex

Implemented durable workspace/tab task queues, guarded dispatch, explicit MCP completion and
Codex working→idle fallback, CLI/watch/TUI counts. Unit validation and a private fake-agent
integration test added. Socket integration/release validation remains blocked by sandbox EPERM;
see untracked CODEX-REVIEW-P6A-RESULT.md. No live daemon changes.

Older Codex briefs and review results (`CODEX-*.md`, `HANDOFF.md`) were removed from the tree on 2026-11-04;
they are in git history (`git log --all -- 'CODEX-*.md'`). New briefs stay untracked (`.gitignore`).

## 2026-11-04 — P5A–P5C merged, restart note, model menu, exact resume — Claude

All on main, `scripts/check.sh` green, installed; live orchd restarted at 09:1x onto P5A/P5B.
- **P5A** (Codex): review fixes (dialog hash, inherited ORCH_AGENT, login-env deadline, tab rename by id);
  expired questions + `orch question close`; one frame per key; `│` split border; phone help; 20 ms Enter
  except Codex/multi-line; `orch read --unwrapped`.
- **Limits** (Claude): `limited` status, auto-continue after the reset (`orch auto-continue`).
- **P5B**: after `stop-daemon --restart`, agents that were working get a note once idle (Codex); orchd answers
  Codex's "switch to a cheaper model?" menu with "keep current model" (Codex + Claude).
- **P5C** (Codex, reviewed by Claude): restore saves each agent's conversation id (Claude: from the hook's
  session id; Codex: from its open rollout file) and real argv, resumes `--resume <id>` with the options kept.
  Inbox race fixed (inbox requests carry flushed receipts). Proof under 4× parallel load of the bus suite:
  before 1 in 15 runs failed `two_agents_talk_and_one_asks_murat`, after 0 in 100.
- **Open:**
  - Codex keeps its rollout files open in the shared `codex app-server` daemon, not in the pane's process,
    so the id lookup finds nothing for Codex; a plain `codex` comes back as `codex resume --last` (wrong if two
    Codex share a cwd). Panes started as `codex resume <id>` are exact. Lead: Codex hook / session id.
  - `nudges_only_reach_idle_panes` fails under 4× parallel load (timing); fine in normal check runs.
  - Live check of the restart note and exact resume at the next restart.

## 2026-11-04 — P5A implemented; full validation blocked — Codex

Part A review/fixes and all six polish items are implemented; commits and open issues in the entry above. Formatting/clippy/build pass, and 49 unit tests pass. Full `scripts/check.sh` is **not GREEN**: this sandbox rejects Unix-socket creation, preventing mux/integration tests and the real Claude/shell smoke check. The reported inbox/ACK test flake is listed, not suppressed.

`.git` is read-only here. Separate `[codex]` commits were preserved in `/tmp/orch-p5a-commits.yhmUjF`; `/tmp/orch-p5a-series.patch` exports them for a clean checkout at `860d4bc`. The main branch is unchanged and its working tree contains the implementation. Live orchd and Kabuk were not touched.

## 2026-11-03 (evening) — Touch TUI, restart with layout, review council — Claude (orchtool session)

Handover state for whichever session continues. Everything below is committed, `scripts/check.sh` green (65 tests),
installed in ~/.local/bin, and the live orchd was restarted at 17:50 onto it (layout restored, 3 panes).

- **TUI on the phone** (`crates/orch/src/tui.rs`): SGR mouse on.
  - Taps: focus a pane, select a tab, the `+` in the status bar opens a new tab, `☰ <workspace>` opens the workspace list.
  - The workspace list has `+ new workspace / tab / agent`, `× close tab / workspace` and `↻ restart orchd`. It scrolls, and confirmations have tappable Yes/Cancel buttons.
  - The wheel scrolls a shell back through orchd's scrollback, sends arrow keys to programs on the alternate screen, and passes mouse events through to programs that ask for them.
  - The status bar degrades at narrow widths, and there is no idle redraw.
- **orchd**:
  - `UiOp::CloseTab{workspace,index,tab}` (the tab id decides) and `UiOp::CloseWorkspace`.
  - `Stream::Screen.alt`.
  - `daemon.stop{restart}` saves `restore.json` before stopping (a failed save refuses the stop) and removes the socket first. The next orchd recreates the layout (`crates/orchd/src/restore.rs`):
    - same folders, sizes and programs;
    - agents resume: `claude --continue`, `codex resume --last`, also when they were typed into a shell;
    - each agent gets its old bus name (`ORCH_AGENT`).
    - CLI: `orch stop-daemon --restart`.
  - NOTE: today's restart was saved by the old orchd, so agents came back as plain shells and got new bus names. From now on restarts keep them.
- **Environment**: `orch_proto::env::full_env()` fills PATH and profile variables from `$SHELL -lc env` when orch or orchd were started from a bare SSH command. Panes and `orch new` use it.
- **Bus**: a new agent name does not inherit earlier `all` broadcasts.
- **watch**:
  - `seen_ms` is rounded to whole minutes (the line had churned about once a second).
  - herdr's pane list is cached for 15 s.
  - New field `hashes` (pane → screen hash).
  - `text_hash` hashes `hash_normalize(text)`, ignoring pulsing bullets, spinners and spinner status lines. `read --hash`, watch `hashes`, `send --expect` and the answered-once memory all share it.
- **Review council** (report: https://claude.ai/artifact/XS5yAuejFeobjU3gBnZX4J; Codex: CODEX-REVIEW-P5-TUI-RESULT.md):
  - The orchtool findings are fixed.
  - The Kabuk findings are in notes/kabuk-council-findings.md. The Kabuk session applied 13 of the 17 in 0.5.4–0.5.6.
- **Open, Murat's call**:
  - Native workspace/tab bar. orchtool half: a `workspaces` array in watch, plus `orch ws` and `orch tab` CLI over the existing UiOps.
  - Long-press agent menu in Kabuk.
  - Timeline only when changed (a contract change).
  - Questions from agents that are gone (expiry or Dismiss).
  - Phase 5: task queue, `limited` status, `orch wait`, worktrees.
  - Smaller items: one TUI frame per key, the help screen at phone width, a `│` split border, `read` without hard line breaks, and the 250 ms Enter delay only for Codex panes.

## 2026-11-03 — Telegram removed; honest Kabuk contract, phase 1 — Claude

- **Telegram is out of orchtool** (Murat's decision, 2026-11-03): `orchd/src/telegram.rs`, `orchd --no-telegram`,
  `orch telegram pair`, the `telegram.pair` request and the bus's Telegram bookkeeping (`tg_map_*`, pairing kv) are
  gone; `ureq` dropped. `~/.config/orchtool/telegram.env` is no longer read — Murat can delete it. The SQLite
  tables stay (`kv` still holds the pane counter, `dedup` the dedup keys, `tg_map` is unused; no drop migration).
  **The running orchd keeps the bridge until its next restart** (old binary in memory). Kabuk keeps herdr as a
  fallback; it still sees herdr panes through `orch watch`.
- `orch` exit codes (crates/orch/src/exit.rs, `orch --help`): 10 screen changed, 11 already answered (screen or
  question; stderr carries the answer), 12 pane exited, 13 input full, 14 no such pane/question/not placed,
  20 orchd unreachable, 1 other, 2 clap; 3/4 reserved for Kabuk's wrapper. orchd answers a missing question
  with `no_question`.
- `orch watch` line: `"orch": "<version>"` next to `"v": 1`; `snapshot()` split into a pure `compose()`;
  `crates/orch/tests/fixtures/watch-v1.json` (a real 0.1.0 `--once` line, same file as Kabuk's
  `orch-watch.json` plus the `orch` key) asserted by a unit test. Contract note: `notes/kabuk-contract-v2.md`.



## 2026-11-03 — Phase 4 code done: Kabuk ↔ orchtool (Claude; Codex reviewed)

orchtool (`89e23d6`, fixes `5cdcce0`, check green, installed):
- `orch watch --json`: one JSON line per change — agents (bus joined with orch panes/layout, unregistered agent
  panes, dead panes/stale MCP agents dropped), dialog screens of blocked agents, open questions, timeline tail;
  events as hints, 10 s request timeout, 15 s refresh, 60 s heartbeat, error line + 5 s retry, exits on stdin EOF.
- `orch focus` (`UiOp::Raise`, `Session.raised`): open TUIs switch to that workspace.
- `orch read --hash` / `orch send --expect`: orchd checks the screen and sends under the terminal lock and
  answers each screen state once (30 s) — first answer wins across phone/TUI/CLI. herdr panes: best effort.
- `orch answer --literal --by`, `orch read/send herdr:ID`, `--host` refused where it would act locally.
- Review leftovers from phase 3 closed: answer marked read after the MCP delivered it; mux teardown cancels
  stuck bridges/connects; TUI holds keys for a still-attaching view and drops them after 3 s (fail closed).

Kabuk 0.5.0 (22) (`/projects/kabuk`: `5d7bcbf` updater, `2e89041` live log — Sonnet subagent; `e2b2b6a`
orch backend, `cbc617a` review fixes — Claude): 66 unit tests, 0 failures.
- Server "Start with: orch" runs the orch TUI; agents panel from `orch watch`: Asks you (option buttons +
  Reply…), Comms timeline + Message…, dialog answers, New agent in an orch pane, notifications with answer
  buttons for bus questions.
- In-app updater (signed update.json, own RSA key ~/.android-keys/kabuk-update-manifest.pem, APK cert = debug
  keystore as before) via the relay server /apk/kabuk/; live log → container kabuk-logsink :8792
  (`scripts/livelog`), Settings → Diagnostics switch, on by default.
- Live-tested here with Kabuk's exact shell commands in a bare env (quoting, first-answer-wins, exit 4 for a
  missing folder, literal numeric answers, stale-hash refusal). Not yet on the phone.

Open: one-time bootstrap install of 0.5.0 (0.4.4 has no updater); phone test; phase-3 nudge race remains
documented (screen-based, fail closed).

## 2026-11-02 — Dev release installed (Murat's OK) — Claude

- `scripts/install.sh`: orchd, orch, orch-mcp 0.1.0 (release) in ~/.local/bin; real orchd running on the
  default socket (pong 0.24 ms).
- Claude: `claude mcp add -s user orch ~/.local/bin/orch-mcp` (✔ Connected); `orch hook` on SessionStart,
  UserPromptSubmit, PreToolUse, PostToolUse, Notification, Stop in ~/.claude/settings.json (timeout 5 s,
  `|| true`, silent without orchd).
- Codex: `[mcp_servers.orch]` with `env_vars = ["ORCH_PANE", "HERDR_PANE_ID"]` in ~/.codex/config.toml
  (`codex mcp list` shows it enabled).
- Backups: ~/.claude/settings.json, ~/.claude.json, ~/.codex/config.toml → `*.bak-orch-20261102-231148`.
- No laptop binary: Murat reaches DevBox from the phone via Kabuk (phase 4).
- Rollback: `claude mcp remove orch -s user`, restore the three backups.

## 2026-11-02 — Phase 3 review fixes (CODEX-REVIEW-P3-RESULT.md, 13 findings) — Claude

All fixed: (1) orch-mcp acks inbox rows only after the result was written+flushed to the client;
(2) Telegram: invalid updates ignored, store errors never advance the offset; (3) ask retry returns
the original question until the asker has read its answer; (4) nudges fail closed — Idle needs a
positive prompt-footer match ("for shortcuts", "⏵⏵", "shift+tab to cycle"), Enter only while still
Idle / herdr still idle; (5) spawn+place rolls back (kills the pane) on failure; (6) removing an
earlier tab keeps the active tab; (7) event stream sends authoritative agent/question lists, full
resync on lag; (8) mux: bounded queues with backpressure to orchd, stalled channel closed after 5 s;
(9) mux: transport EOF tears down every channel; (10) TUI: focus/tab switches applied locally at
once, keys after split/new tab held until the new pane's view exists; (11) dead attach streams are
reported and reattached; (12) bracketed paste is one token (overlay: text, never submit);
(13) streaming key decoder (split CSI/UTF-8/paste kept across reads, 40 ms Escape timeout).
Checks: `scripts/check.sh` exit 0 — proto 10, orch 2, orchd unit 18, panes 10, ui 3, bus 9, spike 3.
Live: keys typed in the same chunk as `prefix s` land in the new pane.
Known limit (documented, accepted): nudges still read the screen; Claude's primary delivery is the
Stop hook, nudges are a best-effort wake-up for idle agents without hooks (Codex).

## 2026-11-02 — Phase 3 (TUI + agent status) — code done, awaiting Codex review + Murat's day of use — Claude

Phase-2 review (CODEX-REVIEW-P2-RESULT.md): all 9 findings fixed in 741d00a, each with a test
(acked delivery, one-transaction state changes, identity by pane/instance, ordered rows, Telegram
dedup/durable sync/sender auth, idempotent ask, compare-and-set nudges with pre-typing checks).

Phase 3 built
- Layout in orchd (orch-proto/src/layout.rs, orchd/src/ui.rs): workspaces → tabs → split tree, atomic
  `ui.op` (new workspace/tab, split, close, focus, select tab, rename, resize, zoom); `orch new`
  panes are placed by cwd; kill removes from the layout; detach/reattach keeps it.
- `events.subscribe`: snapshot + changes (panes, session, agents, questions, messages).
- Screen-based status (orchd/src/detect.rs): working ("esc to interrupt"), blocked (Claude
  permission/trust, Codex approval, y/n), idle; agent kind from argv or from registration.
- TUI = plain `orch` (crates/orch/src/tui.rs): ratatui, each visible pane an attach stream into a
  local vt100 screen (views stay attached → instant tab switches), sidebar (workspaces + agents with
  status), status bar (tabs, ⚑ needs-you count, working/idle counts, clock), overlays: help, workspace
  list, needs-you (answer questions / jump to blocked agents), messages (@agent text), prompts.
  Keys like herdr: prefix (Ctrl-b, $ORCH_PREFIX) + c v s h/j/k/l H/J/K/L x z n p 1-9 w ( ) N A , $ a m b t d ?
- Remote: `orch --host H` opens ONE `ssh H orch proxy --mux`; all connections are channels in it
  (orch-proto/src/mux.rs).

Checks
- `scripts/check.sh` → exit 0: proto 9 (layout 5, mux 1), orch 2, orchd unit 18, panes 10, ui 3,
  bus 9, spike 3.
- Live in a herdr test pane (ORCH_PREFIX=C-a): auto workspace, typing, split right/down, focus
  h/l, new tab, tab select, zoom, help, `A claude` → detected ⚠ blocked on the trust dialog →
  needs-you lists it → answered in the pane → ○ idle; detach + reopen keeps 3 tabs.
- Remote through the mux (stand-in ssh): split + typing OK; one proxy process.
- Bandwidth: a pane printing `seq 1 300000` (2.1 MB) while attached remotely → 37.5 KB over the
  connection for the whole session (≈55× less; latest-state sync).

Open / for Murat
- Real-SSH comparison with herdr from the laptop (needs the install + laptop binary).
- Acceptance: one day of `orch` instead of herdr.

## 2026-11-02 — Phase 2 GREEN (agent bus, ask_human, Telegram) — Claude

Built
- orchd bus (crates/orchd/src/bus.rs): SQLite at ~/.local/state/orchtool/orchtool.db (`--db`), agents /
  messages / per-agent read marks / questions. Persist first, then notify; first answer wins atomically;
  waits are resumable (ask.wait, msg.inbox with wait); timeline follow stream catches up on lag.
- Identity: server-side, keyed by pane (`orch:pN`, `herdr:wX:pY`), name `<project>-<kind>`, numbered on
  collision, `$ORCH_AGENT` overrides. orch-mcp registers at initialize, and again on every call.
- orch-mcp (hand-written MCP stdio JSON-RPC, no SDK): send_message, read_inbox, ask_human, wait_for_answer,
  post_status, list_agents; one task per call, notifications/cancelled aborts. Default waits: Claude 600 s
  (max 1800), Codex 50 s (max 55, its tool timeout is 60 s) — the answer is never lost, the agent calls
  wait_for_answer or finds it in read_inbox.
- Delivery: explicit inbox; Claude Stop hook (`orch hook`) hands unread messages over at turn end
  ({"decision":"block"}); idle-only nudges typed into the agent's pane (never when status is unknown /
  working / blocked, so nothing is typed into a permission prompt).
- Telegram (crates/orchd/src/telegram.rs): own bot, paired chat only (`orch telegram pair` → `/pair CODE`
  from a private chat, or TG_CHAT), option buttons + reply-to-answer, answered-elsewhere edits the message,
  agent→murat messages forwarded and replies routed back; token scrubbed from errors; no terminal output.
- CLI: agents, tail [-f], msg, inbox, questions, answer (number = option), ask, status, hook, telegram pair.

Checks
- `scripts/check.sh` → exit 0: proto 3, orchd unit 14 (5 bus), panes 10, bus e2e 6, spike 3. bus+panes
  re-run 3× green.
- e2e (real orchd + orch-mcp + orch binaries): two agents talk, inbox wait wakes, ask_human answered while
  blocking, second answer refused, timeline order; question survives orch-mcp death + orchd restart and
  wait_for_answer resumes, answer in inbox exactly once; MCP cancel + unknown method; Stop hook delivers
  once, silent without orchd; Telegram via mock Bot API: stranger ignored, button answers, message edited,
  reply routed to the agent; one-time pairing code.
- Live demo (test daemon): real Claude (`--mcp-config`) asked real Codex (`-c mcp_servers.orch…`)
  "17*23" → "391" → ask_human to murat → answered via `orch answer 1 1` → second answer refused → Claude
  reported "yes". Idle nudge: Codex got "[orch] new message from murat…", read inbox, replied PONG.

Found in the live demo and fixed
- `send --enter`: Codex's paste detection swallowed a CR 60 ms after the text → 250 ms.
- Agents were only reachable after their first tool call → orch-mcp registers at initialize.
- Codex starts MCP servers with a clean env → config must forward the pane:
  `env_vars = ["ORCH_PANE", "HERDR_PANE_ID"]`.

Not done / for Murat
- Telegram not tried against the real API: needs the bot token (BotFather → ~/.config/orchtool/telegram.env).
- Claude/Codex MCP + hook config is not installed globally (needs Murat's OK). Codex asks approval per
  orch tool call unless "Allow for this session"/"Always allow" is chosen.

## 2026-11-02 — Phase 1 review fixes (Codex review → CODEX-REVIEW-P1-RESULT.md) — Claude

All 8 findings confirmed and fixed, each with a regression test:
1. Mouse modes now sent as explicit set+reset state (vt100's formatter never reset them); detach also resets ?9.
2. Wide glyph cut by an observer's right edge → blank at an explicit position (no wrap/scroll). Found a second
   bug via the new clipped random test: blank runs are erase sequences that don't move the cursor.
3. Controller watch subscribed before registering + reconciled after the attach reply.
4. Resize: PTY ioctl, parser size and gen updated together under the terminal lock; `resize_for(client)`
   re-checks control under the lock.
5. Kill: SIGHUP → SIGTERM(group) → SIGKILL(group), 1.5 s each; pane kept + `kill_failed` if it survives.
6. Input budget 1 MiB per pane; `send` returns `input_full`, attach drops keys; never blocks.
7. Exit published only after the reader hit PTY EOF (max 2 s wait for descendants holding the PTY).
   Reader/waiter threads hold Weak<Pane>.
8. Raw-mode guard created right after enabling raw mode.
Checks: `scripts/check.sh` exit 0 (unit 9, integration 10, proto 3, spike 3); integration ×6 → 10/10 each.
Live: codex still renders in an orch pane; `orch kill` of a HUP+TERM-ignoring loop → gone in 3.0 s.

## 2026-11-02 — Phase 1 GREEN (PTY core) — Claude

Built: orchd (panes in PTYs, vt100 state behind `term.rs`, query replies, scrollback, per-client row-diff
renderer, controller/observer + takeover, JSON-lines socket 0600 + peer-uid check), `orch` CLI
(new/ls/attach/send/read/kill/proxy/ping/latency/stop-daemon, `--host H` via `$ORCH_SSH H orch proxy`,
autostarts orchd with setsid), `scripts/install.sh`, `contrib/orchd.service` (optional, not enabled).

Design notes
- Socket: `~/.local/state/orchtool/orchd.sock` (not $XDG_RUNTIME_DIR: Linger=no, it vanishes at logout).
- Sync: renderer keeps what each client has (formatted rows + modes + cursor); a frame rewrites changed rows
  only, ≤ 25 fps, nothing queued → a slow client jumps to the latest state; frames carry gen/base/rev,
  client asks `resnapshot` on a gap; resize bumps gen → full redraw. View clipped to the client's size.
- Size: controller's terminal decides; observers never resize; detached panes keep the last size.
- Terminal queries answered by orchd (DSR 5/6, DA1, DA2, XTVERSION, DECRQM, OSC 10/11): Codex's TUI starts
  with no client attached. Kitty keyboard query left unanswered (= unsupported).
- `orch new` passes the caller's env minus HERDR_*/TMUX*/Claude-session vars (CLAUDECODE,
  CLAUDE_CODE_CHILD_SESSION, CLAUDE_CODE_MESSAGING_TOKEN, …) — otherwise a pane started from Claude
  inherits "transcript saving off" and Claude's messaging token.
- Daemon restart kills panes (accepted for now); SIGHUP is ignored.

Checks run (2026-11-02)
- `scripts/check.sh` → exit 0 (fmt ok, clippy -D warnings ok; tests: proto 3, orchd unit 6, integration 7,
  terminal_spike 3 — all passed). Integration suite re-run 5× → 7/7 each time.
- Unit: render random-diff property test (39 seeds × 60 steps, skipped frames, alt screen, wide chars,
  insert-line) — client vt100 equals server cell by cell; clipped observer; query replies incl. fragmented;
  scrollback read order.
- Integration (real orchd): echo+exit code, send+enter, two clients mirror, observer input ignored,
  detach/reattach keeps state + frees controller slot, takeover + controller sizing, resize storm under
  output ends consistent, query reply with no client, kill + name resolution.
- Live (debug build, test socket): bash, `vim -u NONE` (insert + Esc), `claude` (trust dialog navigated with
  Down/Enter, typing), `codex` (TUI renders, typing) in orch panes. Full-screen `orch attach` from two herdr
  panes: mirror OK, observer typing ignored, prefix-t takeover resized codex to the new controller, prefix-d
  detach restores the terminal, agent keeps running. `kill -HUP orchd` → keeps running.
- Proxy: `orch --host localhost …` through a stand-in ssh (real `ssh localhost` is refused: DevBox's own key
  is not in authorized_keys — not changed). new/ls/latency/attach over the proxy OK.

Measurements (debug build, DevBox)
- orchd RSS with 5 idle panes (bash, vim, cat, claude, codex): 5.9 MB.
- Full scrollback costs ~13 MB per pane (5000 lines, 80 cols; vt100 cells are big): 59 MB with 9 panes,
  4 of them full. Default scrollback lowered to 3000 (`--scrollback` / `ORCHD_SCROLLBACK`). A compact
  scrollback store is a later optimisation if needed.
- Local: ping 0.2–0.4 ms; attach → first frame 1.8 ms; keystroke → echo frame median 0.8 ms (max 5.2).
- Via proxy pipe: attach 8.7 ms; keystroke → echo median 1.2 ms. Real network numbers need the laptop.

Open / for Murat
- SSH-logout survival not tested with a real logout: orchd is setsid'd, parent 1, KillUserProcesses is the
  Debian default (no) — same situation as herdr's server. Please confirm once: start `orch new -- bash`
  over SSH, log out, log in, `orch ls`.
- Laptop test: `ORCH_REMOTE_BIN=~/.local/bin/orch orch --host devbox attach <pane>` after `scripts/install.sh`.

## 2026-11-02 — Phase 0 GREEN (Claude finished the blocked steps)

Codex's sandbox had no network / home writes, so Claude installed rustup (stable 1.99.1-era cargo, minimal +
rustfmt + clippy, `~/.cargo`, PATH not modified — check.sh sources `~/.cargo/env`) and ran `git init -b main`.
- `cargo fmt --all` (fixed formatting in examples/terminal_spike.rs)
- `scripts/check.sh` → exit 0: fmt ok, clippy -D warnings ok, tests: orch-proto 3 passed, terminal_spike 3 passed.
- Spike A (notes/kabuk-contract.md): complete (source inspection).
- Spike B (notes/terminal-spike.md): synthetic vt100 tests pass; live bash/vim/claude/codex checks moved into
  Phase 1 acceptance (they need the real PTY driver anyway).

## 2026-11-02 — Scope update: Codex stops at Phase 0; validation still BLOCKED

Murat assigned phases 1 and 2 to Claude; Codex will review them later. Do not
start Phase 1 as part of this task. The Phase 0 skeleton, `scripts/check.sh`, and
both spike notes are present. Rechecked `scripts/check.sh` after this scope change:
exit 127 (`Rust is required: install stable rustup with rustfmt and clippy.`).
Rust remains absent and `.git` remains read-only. No build/test or live terminal
spike success can be reported, so Phase 0 is not marked GREEN. Claude's handoff
includes resolving these Phase 0 validation blockers before building on it.

## 2026-11-02 — Phase 0 BLOCKED (not GREEN); Phase 1 NOT STARTED

Read draft-3 HANDOFF/PLAN and accepted engineering defaults. No herdr commands,
socket connections, service changes or agent messages have been made.

- Rust/rustup/cargo are absent from PATH; ~/.cargo and ~/.rustup do not exist.
- Toolchain download probe: `curl --proto '=https' --tlsv1.2 -I --max-time 15 https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init`
  failed with curl exit 6, `Could not resolve host: static.rust-lang.org`.
- `git init -b main` failed: `.git/hooks/: Read-only file system`. The session's
  permission profile explicitly makes `.git` read-only; commits cannot be made.
- Home-directory writes are also outside this session's writable roots. A normal
  user-level rustup installation needs an environment permitting those writes.

Prepared work:

- Four-crate Cargo workspace, stable/minimal toolchain declaration, explicit binary
  stubs, Tokio daemon entry point, versioned JSON-lines envelope with framing,
  version and size tests. No PTY service is implemented yet.
- `scripts/check.sh` runs fmt, clippy with warnings denied, and tests across all
  targets; holds the shared flock and sets CARGO_BUILD_JOBS=2.
- Spike A complete as source inspection: `notes/kabuk-contract.md` covers the
  watcher, CLI actions, fields, events, errors and reconnect behavior. Found that
  socket redirection alone cannot migrate the phone's hard-coded herdr CLI actions.
- Spike B prepared: vt100 replay example and synthetic mode/Unicode/resize tests.
  `notes/terminal-spike.md` explicitly lists the unrun real-application and query
  reply checks. Emulator verdict remains provisional.

Checks actually run:

- `bash -n scripts/check.sh` — exit 0, shell syntax passes.
- `scripts/check.sh` — exit 127, Rust missing; fmt/clippy/tests did NOT run.
- Python `tomllib` manifest/member inspection — passed for workspace, toolchain
  and all four package manifests (see command below).
- `git init -b main` — exit 1, read-only `.git/hooks/`; no commits possible.
- Download probe above — curl exit 6. No installation was attempted after that
  prerequisite failed. No Cargo.lock could be generated.

Static manifest check command:

```sh
python3 - <<'PY'
from pathlib import Path
import tomllib
root = Path('.')
workspace = tomllib.loads((root / 'Cargo.toml').read_text())
tomllib.loads((root / 'rust-toolchain.toml').read_text())
for member in workspace['workspace']['members']:
    package = tomllib.loads((root / member / 'Cargo.toml').read_text())
    assert package['package']['name'] == Path(member).name
    assert (root / member / 'src').is_dir()
print('PASS: workspace, toolchain, and four package manifests')
PY
```

Continuation needs an execution environment with Rust download/dependency access,
user-level toolchain installation access, and writable repository Git metadata.
Then install stable/minimal + rustfmt/clippy, initialize Git, build/fix/check this
uncompiled scaffold, complete the live terminal spike, and commit small verified
steps. No product decision is pending. Do not mark Phase 0 GREEN merely because
both note files exist. Phase 1 and its live logout/SSH measurements must follow
successful Phase 0 validation.
