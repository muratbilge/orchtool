# orchtool

`orch` runs your coding agents (Claude Code, Codex, plain shells) in panes on a dev server, keeps them running
while you are away, and gives them a bus to talk to each other and to you: messages, status, questions you can
answer from the TUI, from a script, or from a phone. [Kabuk](https://github.com/muratbilge/kabuk), the Android
terminal, drives it from the phone: agents, their permission dialogs and questions, and the workspaces, live.

Built in Rust (stable) by Claude Code and Codex with Murat Bilge steering, for one dev server first. It is young
(0.1.0): `STATUS.md` is the running log, `PLAN.md` the plan, `CODEX-*.md` the review rounds.

## Crates

- **`orchd`**, the daemon. Owns the PTYs (`portable-pty` + `vt100`), the workspaces and tabs, and a SQLite store
  for the bus (timeline, questions, answers). Reads each agent's status off its screen: working, blocked on a
  permission dialog, idle, done, or limited (a usage-limit notice; orchd types "continue" a minute after the reset
  unless `orch auto-continue <pane> off`; Codex's "switch to a cheaper model?" menu is answered "keep current
  model" the same way). Answers a dialog only if the screen still hashes to what the client
  saw (`send --expect`), and each screen once. `orch stop-daemon --restart` saves the layout and the next orchd
  recreates it, agents resumed; agents that were mid-turn get a note to carry on once they are back at their
  prompt. Questions from agents whose pane is gone expire by themselves. Starts on demand from `orch`, or as a user service (`contrib/orchd.service`).
- **`orch`**, the CLI and the TUI (`ratatui`; mouse and touch: tap a pane or a tab, `+` for a new tab, `☰` for
  the workspace list). Panes: `new`, `ls`, `attach`, `send`, `read` (`--unwrapped` joins soft-wrapped
  rows), `kill`, `focus`, `auto-continue`; layout: `ws` and `tab`
  (focus, new, rename, close, by id); bus: `agents`, `tail`, `msg`, `inbox`, `questions`, `question close`, `answer`,
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
tab · 20 orchd unreachable.

## License

Apache License 2.0, see `LICENSE`.
