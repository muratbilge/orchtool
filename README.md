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
  free; `orch ws wake <label>` (or Enter on it in the `w` list) brings it back with the agents resumed. orchd
  refuses (exit 15) while an agent in it works, waits on a dialog or a question, runs a task, or a program runs
  that can't be resumed; `--force` sleeps anyway (busy agents get a note on wake), `--dry-run` shows the plan.
  Sleeping workspaces have their own record next to the socket (`sleeping.a.json` / `sleeping.b.json`, two
  alternating generations, the newest valid one used), written and synced before any of the workspace's programs
  end (a failed write, folder sync included, ends nothing). Restarts, crashes and a plain `stop-daemon` keep it;
  the restart path itself (`restore.json`, the autosave) is unchanged. The sleeper record is the truth for
  its panes: if orchd dies before the autosave has caught up with a sleep or a wake (about 2 s), the restore
  leaves those panes out of whatever layout it reads (in memory; no file is rewritten), also ones whose program
  had already ended before the sleep, so they are never started twice, and the workspace comes back asleep (wake it again). A wake drops the sleeper only once the autosave holds the
  woken workspace; a woken pane that ends before that is not waited for, and the workspace is never held
  reserved for more than 30 s. A wake is all or nothing: if a pane can't be
  started, none is and the sleeper stays as it was; a started pane that then can't be ended keeps running
  outside any workspace (`orch ls`, `orch kill`) and the workspace can't wake until it is gone. Agents resume
  only a verified session id, else at their resume picker (Claude `--resume`, Codex `resume`, never `--last`).
  While a workspace with a Claude (or Codex) agent sleeps, a restart gives every awake agent of that kind without
  a known conversation its picker too (`claude --resume`, `codex resume`), never `--continue` / `resume --last`
  (also not by replaying a saved shell command word for word), since "the latest" could be the sleeper's.
  Such an agent gets a fresh command: its verified id, else a target written on a command orch can parse, else
  the picker; an agent that ran in a shell comes back as `<shell> -lc '<agent command>; exec <shell>'` (the
  wrapper's other commands, e.g. a `cd`, are not kept).
  **Fail-safe start (wake, and a restart beside a sleeper):** an agent is started only from a command that parses
  completely: its program word is `claude`/`codex`, written as a bare name or an absolute path (a relative path
  like `./bin/codex` is refused: it would be looked up from the pane's folder, not necessarily where it ran) and
  kept as written; every option is recognised and carried over (sandbox, approval, model ...). A shell pane's
  command line must set nothing: a plain shell, or a `-c` script whose first command is the agent itself with no
  variable other than orch's own `ORCH_AGENT` (no `cd`, `export`, `env X=`, `$...`), also when orch saw the
  agent's own command line. Otherwise the agent is **not started**: the pane comes back as a plain shell in its
  folder (its own shell, or your login shell for a direct agent pane). A fixed `/bin/sh` script first prints
  `orch: <kind> was not restarted: its start command could not be reconstructed safely (<reason>).`, the saved
  command line (shell-quoted, to run or adapt yourself) and the verified conversation id if known; the lines are
  passed as arguments, so nothing of the saved text is run, whatever your shell. The saved command line appears
  only in that pane: the daemon log, the sleep plan and `orch ws wake` name the pane, the agent kind and the
  reason (a command line can carry credentials). The wake still succeeds.
  A sleeping workspace's recovery arguments (each pane's saved command line and the agent's discovered command
  line) stay in orchd's own sleeper record (`sleeping.a.json`/`sleeping.b.json`, 0600). Clients (session replies,
  `ws sleep`/`ws wake` and dry-run replies, the event stream, `orch watch`, the MCP tools) get the sleeper's
  workspace, layout, position, folder, and per pane its name, agent kind, bus name and flags, without them.
  Known limits: the program word is used as written (as main's restart does); a bare `codex`/`claude` is found
  on the shell's PATH at that time, which is not checked against the program that ran before. Variables set
  interactively in a shell pane (or by orch's spawn environment) are not seen; main's restart drops them too.
  While asleep, an agent's bus name is reserved for it; mail, queued tasks and open questions wait for it.
  Nothing wakes a workspace by itself, only on request: `orch msg --wake <agent> …` (the MCP `send_message` has
  `wake`) and `orch task add <ws> --wake …` (refused for `all`: a broadcast never wakes everything). A record
  slot that is damaged or in an unknown format is left alone and blocks sleep / wake (`save_failed`); moving it
  away is enough while it was not the newest record, otherwise (or after a repaired slot) restart orchd to load
  it. Known limits: if the folder sync after a record's rename fails, the sleep is reported failed and nothing
  ends, but that record may still be on disk, so after a restart the workspace can come back asleep; and a
  repaired slot carrying the same generation number as the one loaded is not told apart; when SQLite can't
  hand out pane ids, the inherited in-memory fallback can repeat an id across starts and the restore then also
  leaves out an unrelated pane with a sleeper's old id; and if every woken pane exits before the next
  autosave, the sleeper is retired without a checkpoint (a crash right then could start a program that already
  ran from an older autosave). Starts on demand from `orch`, or as a user service (`contrib/orchd.service`).
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
