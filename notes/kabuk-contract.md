> **Superseded** by `kabuk-contract-v2.md` (orchtool era). This is the herdr-era contract, kept for history.

# Kabuk → herdr contract (source inspection, 2026-11-02)

Inspected `/projects/kabuk` at commit
`bde5fc7ef998fe50f971e06043a567d7bb0896d3`; no calls to herdr, no changes to Kabuk.
Sources: `app/src/main/assets/herdr_watch.py`,
`app/src/main/java/app/kabuk/agents/Herdr.kt`,
`app/src/main/java/app/kabuk/term/SshTab.kt`,
`app/src/main/java/app/kabuk/text/QuickReplies.kt`, and existing
`HerdrParseTest.kt`, `HerdrLiveTest.kt`, `herdr-agents.json`, `herdr-workspaces.json`.
This describes what the client consumes, not a verified complete server specification.

## Transport and framing

Kabuk runs its bundled Python watcher with `python3 -u -c ...` on a separate SSH
exec channel. There is no WebSocket transport in this path. The watcher uses Unix
sockets: existing `HERDR_SOCKET_PATH`, otherwise
`${XDG_CONFIG_HOME:-~/.config}/herdr/herdr.sock`.

Each ordinary RPC opens a fresh connection, sets a five-second timeout, sends one
UTF-8 JSON line, reads until the accumulated bytes end in newline, then closes.
Request: `{"id":"k","method":"agent.list","params":{}}\n`.
No `jsonrpc` or version member. Replies wrap data in `result`, alongside `id`.
Do not emit unrelated events on these one-shot RPC connections: the Python reader
attempts to parse the whole received buffer as one JSON document.

Subscriptions use their own persistent nonblocking connections, request ID `w`:
`{"id":"w","method":"events.subscribe","params":{"subscriptions":[{"type":"pane.agent_status_changed","pane_id":"wE:p1"}]}}\n`.
The watcher drains event bytes without decoding envelopes or checking IDs. Events
are invalidation hints; it fetches authoritative snapshots afterward. Exact event
and server-error payload schemas are not established by this source inspection.

## Required socket responses

| Method | Parameters / fields consumed |
|---|---|
| `agent.list` | `{}` → `result.agents[]`: `terminal_id`, `pane_id`, `agent`, `agent_status`, `cwd`, `workspace_id`, `focused`. Existing fixtures also have `tab_id`, `foreground_cwd`, `revision`, and `result.type = agent_list`. |
| `workspace.list` | `{}` → `result.workspaces[]`: `workspace_id`, `label`, `number`; fixtures use `result.type = workspace_list`. |
| `pane.list` | `{}` → `result.panes[].pane_id`; CLI focused-read additionally consumes `focused`. |
| `pane.read` | `{"pane_id":"…","source":"recent","lines":60}` → `result.read.text`; fetched for each blocked agent. |

IDs are opaque strings. Fixtures use panes `wE:p1`, terminals `term_…`, workspaces
`wE`; retain stable identities and joins, not assumptions about numeric IDs.
Phone sorting is blocked → working → done → idle → everything else, then workspace
number. `agent` defaults to `agent`, status to `unknown`; missing list/result
containers fail parsing. A `name` field does not replace `agent` in this parser.

The watcher writes one outer JSON line per changed snapshot:
`{"agents":<whole agent reply>,"workspaces":<whole workspace reply>,"questions":{"<pane_id>":"<plain terminal text>"}}`.
An error line is `{"error":"<exception text>"}`; the phone treats it as unavailable
and clears agents/questions. There is no structured ask_human object yet.

## Events, refresh and reconnect

One structure subscription requests `pane.created`, `pane.closed`, `pane.exited`,
`pane.agent_detected`, `pane.focused`, `workspace.created`, `workspace.closed`,
`workspace.renamed`. One extra subscription per pane requests
`pane.agent_status_changed` filtered by pane ID.

The watcher coalesces bursts for 150 ms, reconciles pane subscriptions, rereads
snapshots, and suppresses identical output. Quiet refresh is 15 seconds; forced
heartbeat is about 60 seconds. Comments explicitly say herdr replays history on
subscribe, so no event is itself proof of current state. Stdin EOF ends the
watcher. Structure-stream EOF restarts the watch loop after an error line and a
five-second delay; pane-stream EOF removes that subscription before reconciliation.

Android restarts an ended watcher after three seconds. Python missing (exit 3
before any output) switches to CLI polling every three seconds, or 60 seconds when
unavailable. SSH connection failures use exponential retry capped at 30 seconds
when auto-reconnect is enabled; normal remote-command exit waits for manual
Reconnect. Login cancellation/authentication errors do not auto-retry.

## CLI compatibility is also required

`Herdr.kt` resolves `command -v herdr`, falling back to `$HOME/.local/bin/herdr`.
Pointing only `HERDR_SOCKET_PATH` at orchd does **not** migrate these commands.

| Phone action | CLI contract |
|---|---|
| Fallback list | `agent list` and `workspace list`: one JSON reply line each, exit 0. |
| Answer | `pane read ID --source recent --lines 60` returns plain text; then `pane send-text ID KEY`, exit 0. No automatic Enter is appended by Kabuk. |
| Focus | `agent focus TERMINAL_ID`, exit 0. |
| Start agent | Validate cwd, `workspace create --cwd … --label … --focus`; parse `result.root_pane.pane_id`; then `pane run ID TOOL`. Missing directory is surfaced via exit 4. |
| Read focused pane | `pane list` JSON with `focused`; `pane read ID --source recent-unwrapped --lines 4000` returns plain text. |

Most SSH exec timeouts are eight seconds; workspace creation ten, focused read
fifteen. Nonzero exit means failure. Numeric/menu answers are accepted only after
rereading and comparing `QuickReplies.pending`'s fingerprint (prompt plus choice
keys/labels), then checking the selected key still exists. Phone-side duplicate
suppression is in-memory, capped at 200 entries. Read/check/send is not atomic:
it cannot guarantee first-answer-wins across clients.

## Phase 3 acceptance implications

Add a configurable executable/endpoint to Kabuk through its own release flow,
or an explicitly selected compatibility launcher; do not replace the existing
herdr executable/socket. Support the consumed CLI surface as well as socket RPCs.
Replay the existing fixture shapes and verify subscriptions/reconnect with the
watcher before claiming compatibility. Keep errors explicit; a current snapshot
must recover after missed events. Live phone/server validation remains a phase-3
gate; this spike is complete as read-only source inspection.
