# Kabuk ↔ orchtool contract, v2 (2026-11-03)

What the phone (Kabuk, `app/src/main/java/app/kabuk/agents/Orch.kt`) and orchtool (`orch` 0.1.0, `orchd`)
promise each other. Supersedes `kabuk-contract.md` (herdr era). The code this describes: `crates/orch/src/watch.rs`
(stream), `crates/orch/src/exit.rs` (exit codes), `crates/orchd/src/server.rs` (`pane.send --expect`,
`ask.answer`). Telegram is no longer part of orchtool (Murat, 2026-11-03); Kabuk keeps herdr only as a fallback
and still sees herdr panes through the stream.

## 1. Transport

- Everything runs as **SSH exec channels** on the DevBox: one long-lived channel for the stream, one short
  channel per action. There is no socket or JSON-RPC between the phone and orchd; `orch` is the only interface.
- Every command is `sh -c` of Kabuk's prefix `FIND` plus the `orch` invocation:
  `export PATH="$HOME/.local/bin:$PATH"; O=$(command -v orch) || exit 3; "$O" …` — a non-login shell often lacks
  `~/.local/bin`.
- Exit codes the **wrapper** owns, before `orch` runs: **3** = no `orch` binary (Kabuk treats the server as "no
  orchtool", not as an error), **4** = the folder for a new agent does not exist (`cd … || exit 4`). `orch` never
  produces 3 or 4.
- **2** = clap: unknown flag/subcommand. It means the two sides disagree on the CLI (one of them is outdated), never
  a semantic outcome.
- Kabuk's timeouts today: 8 s for `read`/`send`/`answer`/`msg`/`focus`, 10 s for `new`+`focus`, 15 s for the
  4000-line read; stream watchdog 150 s of silence → reconnect.

## 2. Stream: `orch watch --json`

Started as `FIND exec "$O" watch --json`. One JSON object per line, UTF-8, no embedded newlines; the phone reads
with `readLine()`. Closing the channel closes `orch`'s stdin and ends it at once.

- **A line is the full state, never a delta.** Printed when the state differs from the last line, and at least
  every **60 s** (heartbeat). orchd events are only hints (150 ms coalescing, 15 s refresh without events).
- **Fields** (serde_json sorts keys; order is irrelevant):

  | key | type | meaning |
  |-----|------|---------|
  | `v` | int | contract version, **1** |
  | `orch` | string | the `orch` binary's version (`CARGO_PKG_VERSION`), diagnostics only |
  | `human` | string | the human's bus name (`murat`) |
  | `agents` | array | see below, sorted blocked → working → done → idle → other, then by workspace number |
  | `questions` | array | **open** bus questions: `id` int, `ts_ms` int, `from`, `text`, `options` [string], `state` ("open"), `answer`/`answered_by`/`answered_ts_ms` null while open |
  | `screens` | object | pane name (as in `agents[].pane`) → the last **60 lines of recent text** of every agent whose status is `blocked`; nothing for the others |
  | `hashes` | object | (since 2026-11-03, additive) pane → the hash `orch read <pane> --hash --lines 60` would print for exactly that `screens` text; answer with one exec: `orch send <pane> <key> --expect <hash> --lines 60` |
  | `workspaces` | array | (since 2026-11-03, additive) the layout for a native workspace bar, in orchd's order: `{id, number, label, cwd, status, shown, active_tab, tabs:[{index, id, label, panes:["orch:pN"], focus, zoomed, status}]}`. `status` = most urgent agent status inside (blocked > working > idle, else null). `shown` = the workspace on screen in the TUI (last raise; the TUI raises on its own switches; first workspace before any raise). Control: `orch ws focus|new|rename|close`, `orch tab new|focus|rename|close` (workspace by id or label, tab by id; gone = exit 14; `new` prints the pane id) |
  | `timeline` | array | the last **40** bus rows, oldest first: `id`, `ts_ms`, `from`, `to`, `kind` (msg/ask/answer/status/event), `text`, `thread` (string/null), `question` (int/null) |

  `agents[]`: `name` (bus name, or `<kind>-<pane id>` for an unregistered agent pane), `kind` (claude/codex/shell…),
  `status` (blocked/working/idle/done/unknown — the screen's dialog detection wins over the bus for `blocked`),
  `status_text` (string/null), `pane` (`orch:pN`, `herdr:ID`, or null), `cwd`, `workspace` (label, "" when not
  placed), `workspace_number` (1-based, 0 when not placed), `focused` (bool: the pane is the focus of the active
  tab of the workspace last raised by `orch focus` — not what any particular TUI shows), `seen_ms` (int/null),
  `registered` (bool),
  `limit_reset` (ms/null) and `auto_continue` (bool) — since 2026-11-03, additive: status **`limited`** means the
  agent's screen ends with its usage-limit notice; `limit_reset` is the reset time read from it, `status_text`
  says "usage limit · continues HH:MM" (or "… resets HH:MM (auto-continue off)", "… no reset time on screen").
  orchd types a continue message into the pane one minute after the reset, once per pause, and posts a `msg`
  to murat (thread "limits") when the limit is hit and when it continued. Switch per pane:
  `orch auto-continue <pane> on|off` (without on/off: prints the state; exit 14 for an unknown pane).
- **Who is listed:** registered agents whose orch pane is running; herdr-pane agents whose pane herdr reports alive
  (when herdr cannot be asked they stay listed); pane-less agents (MCP only) seen within **6 h**; plus unregistered
  running agent panes. Everything else is dropped silently (no "gone" marker).
- **Error line:** `{"error": "<text>"}` when orchd cannot be reached or does not answer within **10 s**; the
  stream retries every **5 s** and the next good line is again full state. Kabuk clears what it shows on an error
  line. `--once` prints one line and exits (20 when orchd is unreachable).
- **Timing on the phone:** heartbeat 60 s vs. watchdog 150 s; SSH keepalive 5 s × 4; a reconnect longer than the
  notifier's 45 s grace re-posts an open question (it is still open, not a phantom).

## 3. Actions — the exact commands Kabuk sends

| action | command (after `FIND`) | prints |
|--------|------------------------|--------|
| re-read before answering a dialog | `"$O" read <pane> --lines 60 --hash` | line 1 `hash:<16 hex>` (FNV-1a 64 of the text), then the text |
| answer a dialog | `"$O" send <pane> <key> --expect <hash> --lines 60` | nothing |
| Esc on a dialog (the reply chips' Esc) | `"$O" send <pane> --key Escape --expect <hash> --lines 60` | nothing |
| answer a bus question | `"$O" answer --by kabuk --literal <id> -- <text>` | `answered #<id> → <agent>` |
| message an agent / everyone | `"$O" msg -- <to> <text>` | `sent #<id>` |
| start an agent | `cd <dir> 2>/dev/null \|\| exit 4; P=$("$O" new --cwd "$PWD" -- <tool>) \|\| exit 1; "$O" focus "$P"` | — |
| show an agent in the TUI | `"$O" focus <pane>` (orch panes only) | nothing |
| copy a pane's text | `"$O" read <pane> --lines 4000` | the text |

`<pane>` is the stream's `agents[].pane` verbatim (`orch:p3` or `herdr:…`); `orch` strips `orch:`. The `--hash`
must come from a read with the **same `--lines`** as the send. `--literal` makes a numeric answer the answer text,
not an option index.

### Exit codes (`crates/orch/src/exit.rs`, `orch --help`)

| code | meaning | the key/answer… |
|------|---------|-----------------|
| 0 | done | sent / answered |
| 1 | any other error, message on stderr (`orch: <code>: <message>`) | unknown |
| 2 | usage error (clap) — CLI mismatch, update one side | not sent |
| 10 | `send --expect`: the pane's screen changed | **not sent** |
| 11 | already answered: the screen (`send --expect`, within 30 s) or the question (`answer`; stderr: `#<id> was already answered by <who>: <answer>`) | **not sent** (someone else's answer stands) |
| 12 | the pane's program has exited | **not sent** |
| 13 | the program is not reading its input | **not sent** |
| 14 | no such pane / question / task / task target, or the pane is not in a workspace (`focus`) | **not sent** |
| 20 | orchd unreachable: not running, did not start, or did not answer | not sent when the connection failed; **unknown** when it broke mid-request |
| 3, 4 | Kabuk's wrapper (no `orch`, no folder) | not sent |

"Certainly not sent": 2, 3, 4, 10–14. An SSH timeout, 1 and 20 are **unsure**: Kabuk must not conclude "changed"
or "taken" from them, nor release its own answer ledger.

## 4. Guarantees

- **Atomic check-and-send, orch panes only:** `pane.send` with `expect` hashes the pane's recent text (the given
  `lines`) and writes the key under the terminal lock; nothing can change in between. Then the (pane, hash) is
  recorded for **30 s**: a second `send --expect` with the same hash on that pane gets `answered` (11), so phone,
  TUI and CLI cannot answer the same dialog twice. The window is per pane and keyed by the hash; after 30 s the
  same hash is accepted again (the dialog must then still be on screen for the hash to match).
- **herdr panes are best effort:** `send herdr:… --expect` re-reads through herdr, compares, then types — not
  atomic, and there is no answered record, so a second answer to an unchanged herdr screen goes through.
- **Bus questions: first answer wins** (`ask.answer` is one transaction); the loser gets 11 with the winning
  answer on stderr. The answerer's name (`--by kabuk`) lands in the timeline.
- `screens` carries the **recent 60 lines**, while orchd classifies `blocked` from the **visible screen**; they
  can disagree for a moment (the phone may see `blocked` with a screen whose dialog it cannot parse).
- `timeline` = last 40 rows, `questions` = open only. Answered questions leave `questions` and show up as an
  `answer` row in the timeline.
- Nothing in the stream is private to one client: `focused` and `orch focus` are global (every open TUI raises
  that workspace).

## 5. Versioning

- `v` bumps **only** when a field is removed, renamed or retyped, or an exit code changes meaning. Additions (new
  fields, new exit codes for new outcomes) do not bump it.
- Kabuk accepts unknown fields and any `v` ≤ its own; on a newer `v` it logs one line and tells Murat "orch is
  newer — update Kabuk" instead of parsing.
- `orch` is informational (shown in diagnostics); never branch on it.
- The `orch`/`orchd` wire protocol (`Frame.version`) is a DevBox-local matter; a mismatch there surfaces to the
  phone as exit 1 (`bad_request`/InvalidData), never as 10–20.

## 6. Fixture

`crates/orch/tests/fixtures/watch-v1.json` is a watch v1 example generated through production composition
(one blocked agent, its screen, an open question, timeline and queued task). The unit test in `watch.rs` builds a line through
`merge` + `compose` and asserts every top-level, agent, question and timeline key of the fixture is present and
`v == 1`. Kabuk's `app/src/test/resources/orch-watch.json` is the **same file**; its parse test loads it. Policy:
regenerate the fixture in orchtool on every stream change, copy it into Kabuk the same day, both suites green.

## 7. Logging

No screen text, question text or message text in logs on either side. orchtool logs nothing about `send`/`answer`
contents; Kabuk logs pane + key + outcome (exit code) only, and a malformed watch line as its class and length —
never the throwable (Android's `org.json` puts the whole input into the exception message).

## 8. Additions 2026-11-04

- `orch question close <id>`: dismiss an open question from the phone. Exit 0 closed, 14 gone, 11 already
  answered. Questions whose asker's pane exited (or killed, or bus agent stale > 6 h) now expire by themselves;
  watch keeps listing only open ones.
- `orch read <pane> --unwrapped [--lines N]`: soft-wrapped rows joined into real lines (N counts logical
  lines); for "Copy text…". orch panes only; not combinable with `--hash`/`--visible`.
- Codex's "Approaching rate limits / switch to <model>?" menu: orchd answers "2. Keep current model" by
  itself (same per-pane switch as auto-continue); the pane shows `blocked` for the few seconds before that.
- After `orch stop-daemon --restart`, agents that were working get a note typed in once they are back at
  their prompt; Murat gets "restart: resumed N agents, nudged M …" on the bus (thread "restart", from orchd).

## Task queues (P6A, additive)

`orch task add <workspace-label> "<text>" [--after ID]` prints a numeric task ID.
`label@tab-id` narrows the target to a tab. Labels are literal (not workspace numbers or pane
IDs); `@` is reserved for the separator. Bus names are not task targets in v1. The label and tab
IDs survive daemon layout restart. Renaming/removing a workspace leaves its queued tasks waiting;
requeue them under the new label. A target with zero or multiple live agents waits; never pick
an arbitrary agent or whichever pane happens to be focused. Overlapping workspace/tab targets
also cannot run two tasks in the same pane.

`orch task ls` lists queued/running work; `--all` includes all history. `orch task cancel ID`
cancels queued/running scheduling, without interrupting the agent's current work. `orch task done [ID]`
completes running work; omitted ID resolves via ORCH_PANE. The MCP `task_done` tool takes optional
integer `id` with the same meaning. Unknown task/dependency/target is exit **14**; invalid text or
an invalid state transition is **1**. Terminal states cannot be changed. An unmet `--after` waits
until that task is `done`; cancelled/failed dependencies never release their dependents. Dispatch
chooses the oldest eligible task; a blocked dependency does not hold up later independent work.

Dispatch requires three seconds of continuously idle screen detection, no working/blocked bus
status, no open human question, no limit or model menu. Input is sent as one paste followed by a
separate Enter after 300 ms. Multiline text requires the terminal's bracketed-paste mode; otherwise
the task fails without typing. CRLF/CR normalize to LF; terminal control bytes are rejected.
Input interruption fails the task and pauses further task dispatch to that pane: inspect its
prompt and recreate the pane before requeueing. No automatic replay of uncertain input.

MCP registration opts a pane into **explicit completion**; its dispatched text includes the task ID
and `task_done` instruction. Other agents use observed working → three seconds stable idle. Merely
sending text or initially seeing an idle prompt never completes a task. Limits keep work running and require fresh observed work after the pause before heuristic completion.
Pane exit makes running work failed. On layout restore running reservations map to new panes;
they are not resent and need fresh observed work or explicit completion. If the daemon dies
between its durable claim and the PTY write, operator inspection/explicit completion is required:
SQLite and terminal input cannot be one atomic transaction. A cold start without restored panes
fails orphaned running tasks and retains queued tasks.

Watch v1 adds `tasks`: open tasks plus the last 20 terminal tasks, ordered by ID. Each row has
`id`, `target`, `text`, `state` (queued/running/done/failed/cancelled), `created_by`, `created_ms`,
`started_ms`, `finished_ms`, `pane` (raw pN or null), `after` (ID or null), `error` (or null), and
`saw_working` (boolean). Times are Unix milliseconds. Start/done/failed/cancelled transitions emit
a message to murat, thread `tasks`, atomically with the task transition. Subscribe snapshots and
`tasks` events contain the same rows. The TUI workspace list counts queued tasks, including tabs.

Fixture policy: regenerate on additive changes too. The deterministic production `merge`/`compose`
fixture generator is `ORCH_UPDATE_WATCH_FIXTURE=1 cargo test -p orch --bin orch
watch::tests::line_keeps_every_key_of_the_v1_fixture`; it includes a queued task and checks every
serialized task key. P6A uses this generator because private sockets are unavailable in Codex's
sandbox. On the integration host, also capture a real `orch --socket <private> watch --once` and
validate it. Copy the regenerated fixture into Kabuk and run its parser tests before release;
that cross-repository step is still required, not implied by orchtool's unit test passing.
