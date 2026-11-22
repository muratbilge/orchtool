# orchtool

`orch` runs your coding agents (Claude Code, Codex, plain shells) in panes on a dev server, keeps them running
while you are away, and gives them a bus to talk to each other and to you: messages, status, questions you can
answer from the TUI, from a script, or from a phone. [Kabuk](https://github.com/muratbilge/kabuk), the Android
terminal, drives it from the phone: agents, their permission dialogs and questions, and the workspaces, live.

Built in Rust (stable) by Claude Code and Codex with Murat Bilge steering, for one dev server first. It is young
(0.1.0): `notes/STATUS.md` is the running log, `notes/PLAN.md` the plan.

## Crates

- **`orchd`**, the daemon. Owns the PTYs (`portable-pty` + `vt100`), the workspaces and tabs, and a SQLite store
  for the bus (timeline, questions, answers). Reads each agent's status off its screen: working, blocked on a
  permission dialog, idle, done, or limited (a usage-limit notice; orchd types "continue" a minute after the reset
  unless `orch auto-continue <pane> off`; Codex's "switch to a cheaper model?" menu is answered "keep current
  model" the same way, and Codex's start-up "Update available" menu is skipped with Escape, so a restarted
  Codex pane does not wait there). Answers a dialog only if the screen still hashes to what the client
  saw (`send --expect`), and each screen once. `orch stop-daemon --restart` saves the layout and the next orchd
  recreates it, agents resumed; agents that were mid-turn get a note to carry on once they are back at their
  prompt. The layout also survives an unplanned stop (crash, `kill -9`, VM reset or shutdown): orchd keeps
  `autosave.json` next to its socket (2 s after a layout change, every 60 s for folders / agents / was-working,
  only when something changed, and once more on SIGTERM/SIGINT before the panes are killed); the next orchd
  restores it when there is no `restore.json` and agents that were working get a note that their last tool call
  and background work were lost. A plain `orch stop-daemon` removes it (the next orchd starts empty). A restore
  that does not finish is not tried again: the file is kept as `autosave.json.failed`. Questions from agents
  whose pane is gone expire by themselves. A workspace you don't need now can **sleep** (`orch ws sleep <label>`, TUI
  `S` or the `w` list): its layout and how to resume each agent are kept, its programs end and their memory is
  free; `orch ws wake <label>` (or Enter on it in the `w` list) brings it back with the agents resumed. It stays
  asleep across restarts and crashes, also a plain `stop-daemon`: sleeping workspaces have their own record
  next to the socket (`sleeping.a.json` / `sleeping.b.json`, two alternating generations; the newest valid one
  is used, the other is the predecessor), written and synced **before** any of the workspace's programs end. The
  layout files `restore.json` / `autosave.json` never contain them, so an older orchd (which reads and rewrites
  those) leaves the record alone; after a downgrade the sleepers are simply not shown, and they are back after
  the next upgrade. A wake writes its old→new pane map into the record before moving tasks and bus names, and
  drops the sleeper only after the awake layout is saved; an orchd that died half-way finishes or undoes it at
  the next start. A wake is all or nothing (`--partial` keeps panes that can't start asleep as `<label>-rest`);
  agents resume only a verified session id, else at their resume picker (Claude `--resume`, Codex `resume`).
  While asleep, an agent's bus name is reserved for it. orchd refuses (exit 15) while an agent in it
  works, waits on a dialog or a question, runs a task, or a program runs that can't be resumed; `--force`
  sleeps anyway (busy agents get a note on wake), `--dry-run` shows the plan. Mail, queued tasks and open
  questions for a sleeping agent wait for it; nothing wakes a workspace by itself, only on request:
  `orch msg --wake <agent> …` (the MCP `send_message` has `wake`) and `orch task add <ws> --wake …` wake the
  addressed agent's / the target's workspace (refused for `all`: a broadcast never wakes everything). A Codex
  without a verified session id comes back at its resume picker after a wake (never `resume --last`).
  **Auto-sleep** (off by default): `orch auto-sleep 30m` (or `2h`, `1h30m`, `90s`; `off`; without an argument it
  shows the setting; kept in the database) puts a workspace to sleep once it has been idle that long, never
  forced: a workspace a plain `orch ws sleep` would refuse is left awake and gets a new period.
  `orch ws keep-awake <ws> on|off` exempts one. *Idle* for the whole period means: no agent in it working
  (screen or bus status), no change of any of its panes' screens (the normalised `read --hash`, so spinners
  don't count), no input written into any of its panes, no client attached to any of its panes (a TUI showing
  it, `orch attach`), and it is not the workspace last raised (`orch focus`, Kabuk, a TUI's switch). Any of
  these starts a new period, and so does creating or waking a workspace, changing the setting, and starting
  orchd (after a restart every workspace gets a full period). Each automatic sleep is posted to the timeline
  (thread `sleep`, "… is asleep (auto-sleep: idle for 30m) …"). Starts on demand from `orch`, or as a user service (`contrib/orchd.service`).
- **`orch`**, the CLI and the TUI (`ratatui`; mouse and touch: tap a pane or a tab, `+` for a new tab, `☰` for
  the workspace list). Panes: `new`, `ls`, `attach`, `send`, `read` (`--unwrapped` joins soft-wrapped
  rows), `kill`, `focus`, `auto-continue`; layout: `ws` and `tab`
  (focus, new, rename, close, sleep, wake, by id); bus: `agents`, `tail`, `msg`, `inbox`, `questions`, `question close`, `answer`,
  `ask`, `status`, `hook`; `watch --json` streams everything for clients; `--host H` talks to an orchd on another
  machine through `ssh H orch proxy`; `ping` and `latency` measure the round trip.
- **`orch-mcp`**, an MCP server for agents on the bus: `send_message`, `read_inbox`, `ask_human`,
  `wait_for_answer`, `post_status`, `list_agents`.
- **`orch-proto`**, the versioned JSON-lines protocol between them.

## Install

```sh
scripts/install.sh        # cargo build --release, then orchd, orch, orch-mcp → ~/.local/bin
orch                      # the TUI; starts orchd if it isn't running (socket ~/.local/state/orchtool/orchd.sock)
scripts/check.sh          # fmt, clippy, build, tests
```

Rust stable via rustup with rustfmt and clippy (`rust-toolchain.toml`). The scripts take a build lock in
`/tmp/claude-1000/` because the dev server shares its RAM with other builds; drop that line elsewhere.

## Agents on the bus

- **Claude Code:** `orch hook` as the hook entry point registers the agent and keeps its status current; the MCP
  server gives it the bus tools. A pane without hooks is still listed, from its screen. Setup, in
  `~/.claude/settings.json` (the same entry for `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`,
  `Notification` and `Stop`; the hook never fails):

  ```json
  "hooks": {
    "Stop": [{ "hooks": [{ "type": "command", "command": "orch hook 2>/dev/null || true", "timeout": 5 }] }]
  }
  ```

  and the MCP server: `claude mcp add --scope user orch -- orch-mcp`. Set `ORCH_AGENT=<name>` before starting an
  agent to give it a fixed name on the bus.
- **Codex 0.166.0 (exact resume):** use `codex --no-daemon` in orch panes. The shared
  app-server runs hooks with its own environment, not the individual TUI's `ORCH_PANE`.
  orch refuses a session report unless the reporting process is in that pane's process tree.
  `--no-daemon` is preserved by restart along with your model/options. For resumed sessions:
  `codex --no-daemon resume <id>`. An already running shared-server session needs to be reopened
  this way; configuring a hook does not retroactively identify it.

  Merge this **additional** entry into `~/.codex/hooks.json`'s `hooks.SessionStart` array;
  retain the existing herdr entry and any other hooks. Use your installed absolute orch path:

  ```json
  { "hooks": [{ "type": "command", "command": "/home/murat/.local/bin/orch hook --kind codex", "timeout": 5 }] }
  ```

  Ensure `[features] hooks = true` in `~/.codex/config.toml` (already enabled on this machine).
  Review/trust the new command in Codex `/hooks`; do not copy another hook's trust hash or bypass
  hook trust. No `notify` replacement is needed. The helper reads `SessionStart.session_id` from
  stdin, reports identity only, and never starts orchd or changes Codex's working/idle status.
  It handles startup/resume/clear/fork/compact without a matcher. The payload contract and trust
  flow are documented in [OpenAI Hooks](https://learn.chatgpt.com/docs/hooks).

  If identity is unavailable or origin validation fails, orch logs why. A Codex pane that is the
  only Codex pane in its folder then comes back with `codex resume --last` (Codex's `--last` is per
  folder, so it cannot mean another pane); with several Codex panes in one folder it comes back at
  `codex resume` (the picker), **not** `--last`. Existing explicit resume IDs still work. No
  timestamp, newest-file, or shared app-server FD guessing. Private tests must pass `--socket`
  before `hook`, or export `ORCHD_SOCKET`; keep HOME separate. Neither config file is edited by
  the build/install scripts.
- **Any script or shell agent:** `orch ask "<question>"` waits for the human's answer; `orch status <name> working|idle|blocked|done [text]`;
  `orch msg -- <agent> "<text>"`.
- **You:** `orch questions`, `orch answer <id> <text or option number>`, `orch question close <id>`, `orch tail -f`,
  `orch inbox`.

## Clients

`orch watch --json` is the stream a client reads (agents, their screens and hashes, questions, timeline,
workspaces); actions are single `orch` commands. The contract Kabuk relies on, with the stream shape, the exact
commands, the exit codes and the shared fixture `crates/orch/tests/fixtures/watch-v1.json`, is
`notes/kabuk-contract-v2.md`.

Exit codes (`orch --help`): 0 done · 1 other error · 2 usage · 10 the screen changed, nothing sent · 11 answered
already · 12 the program exited · 13 the program isn't reading input · 14 no such pane, question, workspace or
tab · 15 `ws sleep`: the workspace is busy, nothing done · 20 orchd unreachable.

## License

Apache License 2.0, see `LICENSE`.
